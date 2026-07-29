//! The live-handle table.
//!
//! A handle is an opaque token pairing a slot index with a generation counter,
//! never an address; see the module docs in `super` for why. This module owns
//! the table, the per-connection lock groups keyed off it, and the cancel
//! tokens `SQLCancel` reads without taking any group lock.

use std::any::Any;
use std::ffi::c_void;
// Deliberately `std::sync::Arc`, not `crate::sync::Arc`: see `Slot::cancel`
// below for why this one field is the exception to importing locks from
// `crate::sync`.
use std::sync::Arc as StdArc;

use crate::sync::{Arc, Mutex, MutexGuard, RwLock, TryLockError};

/// Which kind of ODBC handle a registry slot holds.
///
/// The kind lives in the registry rather than in the allocation, so checking
/// it never touches the caller's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    /// `SQL_HANDLE_ENV`
    Env,
    /// `SQL_HANDLE_DBC`
    Dbc,
    /// `SQL_HANDLE_STMT`
    Stmt,
    /// `SQL_HANDLE_DESC`
    Desc,
}

/// Bits of a token given to the slot index; the rest hold the generation.
///
/// On a 64-bit target this is a 32/32 split: four billion concurrent handles
/// and four billion reuses of each. On 32-bit — which ODBC very much still has,
/// since Excel and Access are 32-bit on Windows — it is 16/16, so 65 535
/// concurrent handles and 65 535 reuses per slot. A slot whose generation would
/// wrap is retired rather than reused, which keeps the scheme sound at the cost
/// of the table growing slowly under extreme churn.
const TOKEN_INDEX_BITS: u32 = usize::BITS / 2;

/// Largest slot index a token can encode.
const MAX_SLOT_INDEX: usize = (1usize << TOKEN_INDEX_BITS) - 1;

/// Largest generation a token can encode. A slot reaching this is retired.
const MAX_GENERATION: u32 = MAX_SLOT_INDEX as u32;

/// Encode a slot index and generation as the opaque value handed to the
/// application.
///
/// Generations start at 1, so a valid token is never zero and stays
/// distinguishable from `SQL_NULL_HANDLE`.
pub(super) fn encode_token(index: usize, generation: u32) -> *mut c_void {
    (((generation as usize) << TOKEN_INDEX_BITS) | index) as *mut c_void
}

/// Split a token back into its slot index and generation.
fn decode_token(token: *mut c_void) -> (usize, u32) {
    let raw = token as usize;
    (raw & MAX_SLOT_INDEX, (raw >> TOKEN_INDEX_BITS) as u32)
}

/// The lock guarding every handle in one group.
///
/// A group is a connection and all of its statements and descriptors, so one
/// acquisition covers a call that touches a statement and its parent — which is
/// why there is no lock ordering to get wrong outside `SQLEndTran(SQL_HANDLE_ENV)`.
///
/// It contains `()` deliberately: the handles live in their own `Box`
/// allocations outside the mutex, so this is a lock *token*, not a container
/// for the state it protects.
///
/// Recovery from poisoning stays enabled here, and deliberately so, not
/// because there is nothing at stake: the state this lock actually guards is
/// those handle allocations, so a panic while the lock is held mid-mutation
/// of one can leave it inconsistent. A closure panic on the ordinary
/// `panic_safe` path (`panic.rs`) does *not* poison this lock at all:
/// its `catch_unwind` lives in `panic_safe`'s own frame, so the unwind
/// never reaches the guard held there and it drops normally when that
/// function returns. The two paths that *can* poison a group lock are
/// `HandleScope::with_child_group` (`handles/scope.rs`) — whose guard sits
/// below `catch_unwind`, so a panic inside its nested closure unwinds through
/// its `drop(guard)` — and a panic inside `push_diagnostic` itself, which runs
/// outside `catch_unwind` on `panic_safe`'s error and panic arms.
/// Either way, refusing every later call on that connection for the rest of
/// the process is the worse of the two outcomes available once poisoning has
/// happened; handing the group to the next caller, who may find a handle
/// still recovering from the failed call, is the lesser one. The payload
/// being `()` does not make recovering *safe* — it only means there is no
/// half-written value of a real type sitting in the guard for that next
/// caller to read out.
pub(crate) struct GroupLock {
    inner: Mutex<()>,
}

impl GroupLock {
    /// A fresh group, for a new environment or connection.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(()),
        })
    }

    /// Block until the group is free.
    pub(crate) fn lock(&self) -> MutexGuard<'_, ()> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the group only if it is free.
    ///
    /// `SQLCancel` uses this to answer the one question that separates the
    /// spec's two cancel cases: is another thread inside this connection?
    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, ()>> {
        match self.inner.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }
}

/// One entry in the handle registry.
struct Slot {
    /// Incremented on every free. A token carrying a different value is stale.
    generation: u32,
    /// `None` when the slot is free and available for reuse.
    kind: Option<HandleKind>,
    /// Address of the `Box`-allocated handle this crate owns.
    ///
    /// Stored as `usize` rather than a raw pointer so `Slot` stays `Send`;
    /// it is only ever produced by `Box::into_raw` in an `alloc_*` function.
    addr: usize,
    /// The lock guarding this handle. Shared with the whole group.
    group: Arc<GroupLock>,
    /// Token of the parent handle: the environment for a connection, the
    /// connection for a statement, the statement for a descriptor. `None` for
    /// an environment, which has no parent.
    ///
    /// A `usize` for the same reason `addr` is one — `Slot` must stay `Send`.
    /// A token is an encoded index and generation rather than an address, so
    /// this costs nothing but the encode/decode already in this module.
    parent: Option<usize>,
    /// Type-erased `Arc<B::CancelToken>`.
    ///
    /// It lives here rather than in `StatementHandle` because `&mut
    /// StatementHandle` asserts exclusive access to every field: a concurrent
    /// read from `SQLCancel` would be undefined behaviour no matter which field
    /// it touched.
    ///
    /// `std::sync::Arc`, deliberately not `crate::sync::Arc`: this is a
    /// refcounted payload, not a lock, so instrumenting it under loom would
    /// buy nothing for the lock discipline `crate::sync` exists to model —
    /// and loom's `Arc` cannot even hold one, since it has no
    /// `CoerceUnsized` impl (loom's own docs carry a `compile_fail` doctest
    /// for exactly this, pointing at `Arc::from_std` as the escape hatch).
    /// `Arc::new(x) as Arc<dyn Any + Send + Sync>` — the coercion `set_cancel`
    /// callers rely on — only exists for `std::sync::Arc`.
    cancel: Option<StdArc<dyn Any + Send + Sync>>,
}

/// The live-handle table.
///
/// Read on every FFI call and written only when a handle is allocated or
/// freed, so it is read-mostly by a wide margin.
///
/// A type rather than a bare static so that a loom model can construct its
/// own `Registry` and drive its methods (`register`, `resolve`, `group_of`,
/// and the rest) directly, inside the model closure. That reach stops at this
/// type's own methods: it does not extend to `alloc_environment`,
/// `alloc_connection`, `alloc_statement`, or the `free_*` functions in
/// `handles::mod`, which resolve the process-wide singleton via `registry()`
/// below rather than taking a `Registry` as an argument — see that function's
/// doc comment for why a model must not call through to it. loom's
/// primitives are also not const-constructible, so a bare `static` cannot
/// hold one directly; a type gives loom code a `Registry` it builds for
/// itself instead.
pub(crate) struct Registry {
    slots: RwLock<Vec<Slot>>,
}

impl Registry {
    /// An empty table.
    pub(crate) fn new() -> Self {
        Self {
            slots: RwLock::new(Vec::new()),
        }
    }

    /// Resolve a token to a live handle's address, checking its kind **and**
    /// that it belongs to `group`, in a single pass over the table.
    ///
    /// What [`HandleScope::get`] needs, and the reason it is one method rather
    /// than a [`Self::group_of`] followed by a [`Self::resolve`]: every FFI
    /// entry point in the crate goes through that call, so it paid two
    /// acquisitions of this lock, two token decodes and two bounds checks to
    /// answer one question about one slot. It also paid an `Arc` clone —
    /// `group_of` hands back a counted reference purely so the caller can
    /// compare it and drop it, which is an atomic increment and decrement on a
    /// refcount every other thread on the connection is touching. Comparing
    /// with [`Arc::ptr_eq`] against the borrowed `group` while the read guard
    /// is already held answers the same question and clones nothing.
    ///
    /// [`HandleScope::get`]: crate::handles::scope::HandleScope::get
    pub(crate) fn resolve_in_group(
        &self,
        token: *mut c_void,
        expected: HandleKind,
        group: &Arc<GroupLock>,
    ) -> Option<usize> {
        if token.is_null() {
            return None;
        }
        let (index, generation) = decode_token(token);
        let slots = self.read();
        let slot = slots.get(index)?;
        if slot.generation != generation
            || slot.kind != Some(expected)
            || !Arc::ptr_eq(&slot.group, group)
        {
            return None;
        }
        Some(slot.addr)
    }

    /// [`Self::resolve_in_group`] for a caller that does not know the kind in
    /// advance and wants it back.
    ///
    /// The diagnostic-queue lookups, which accept any handle type and dispatch
    /// on what they find.
    pub(crate) fn resolve_any_in_group(
        &self,
        token: *mut c_void,
        group: &Arc<GroupLock>,
    ) -> Option<(HandleKind, usize)> {
        if token.is_null() {
            return None;
        }
        let (index, generation) = decode_token(token);
        let slots = self.read();
        let slot = slots.get(index)?;
        if slot.generation != generation || !Arc::ptr_eq(&slot.group, group) {
            return None;
        }
        Some((slot.kind?, slot.addr))
    }

    /// Register a freshly allocated handle and return its token.
    ///
    /// Reuses a free slot when one is available, otherwise appends. Returns
    /// `None` if the table is exhausted, which the caller reports as an
    /// allocation failure rather than handing back an ambiguous token.
    ///
    /// `group` is the lock the new handle joins — a fresh one for an
    /// environment or connection, shared with the parent for a statement or
    /// descriptor. `parent` is the owning handle's token, or `None` for an
    /// environment.
    pub(crate) fn register(
        &self,
        kind: HandleKind,
        addr: usize,
        group: Arc<GroupLock>,
        parent: Option<usize>,
    ) -> Option<(*mut c_void, u32, u32)> {
        let mut slots = self.write();

        if let Some((index, slot)) = slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.kind.is_none() && slot.generation < MAX_GENERATION)
        {
            slot.generation += 1;
            slot.kind = Some(kind);
            slot.addr = addr;
            slot.group = group;
            slot.parent = parent;
            slot.cancel = None;
            let generation = slot.generation;
            return Some((encode_token(index, generation), index as u32, generation));
        }

        let index = slots.len();
        if index > MAX_SLOT_INDEX {
            tracing::error!("handle registry exhausted at {index} slots");
            return None;
        }
        slots.push(Slot {
            generation: 1,
            kind: Some(kind),
            addr,
            group,
            parent,
            cancel: None,
        });
        Some((encode_token(index, 1), index as u32, 1))
    }

    /// Retire a handle's slot, so every outstanding token for it is rejected.
    ///
    /// Returns the address that was registered, or `None` if the token was
    /// already stale — which is what makes a double free a refusal rather than
    /// a second deallocation.
    ///
    /// Resets `group` to a fresh lock and clears `parent` and `cancel`, so a
    /// slot handed to a future `register` call inherits nothing from the
    /// handle that used to live in it.
    pub(crate) fn unregister(&self, token: *mut c_void, expected: HandleKind) -> Option<usize> {
        if token.is_null() {
            return None;
        }
        let (index, generation) = decode_token(token);
        let mut slots = self.write();
        let slot = slots.get_mut(index)?;
        if slot.generation != generation || slot.kind != Some(expected) {
            return None;
        }
        slot.kind = None;
        // Bumped here as well as on reuse so that the token is dead the
        // instant the handle is freed, not merely once the slot is handed out
        // again.
        slot.generation = slot.generation.saturating_add(1);
        slot.group = GroupLock::new();
        slot.parent = None;
        slot.cancel = None;
        Some(slot.addr)
    }

    /// The lock group a token belongs to, or `None` if the token is stale.
    ///
    /// Kind-agnostic: a caller that also needs to know the token names a
    /// handle of a specific kind wants [`Self::group_of_kind`] instead.
    pub(crate) fn group_of(&self, token: *mut c_void) -> Option<Arc<GroupLock>> {
        if token.is_null() {
            return None;
        }
        let (index, generation) = decode_token(token);
        let slots = self.read();
        let slot = slots.get(index)?;
        if slot.generation != generation || slot.kind.is_none() {
            return None;
        }
        Some(Arc::clone(&slot.group))
    }

    /// The lock group a token belongs to, if it is live **and** of the expected
    /// kind.
    ///
    /// The kind check is what keeps the parentage tree well-formed: a
    /// connection's parent must be an environment and a statement's a
    /// connection, or a handle joins a lock group it has no relationship to.
    /// Folding the check into the same lookup that hands back the group keeps
    /// validation a single pass over the slot, still without dereferencing
    /// `token`.
    pub(crate) fn group_of_kind(
        &self,
        token: *mut c_void,
        expected: HandleKind,
    ) -> Option<Arc<GroupLock>> {
        if token.is_null() {
            return None;
        }
        let (index, generation) = decode_token(token);
        let slots = self.read();
        let slot = slots.get(index)?;
        if slot.generation != generation || slot.kind != Some(expected) {
            return None;
        }
        Some(Arc::clone(&slot.group))
    }

    /// Every live handle whose parent is `token`, as an owned snapshot.
    ///
    /// Owned, not borrowed: a caller iterating this while another call frees a
    /// child is the shape that made `SQLEndTran` unsound when the list was a
    /// field of the handle.
    pub(crate) fn children_of(&self, token: *mut c_void) -> Vec<*mut c_void> {
        if token.is_null() {
            return Vec::new();
        }
        let parent = token as usize;
        let slots = self.read();
        slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.kind.is_some() && slot.parent == Some(parent))
            .map(|(index, slot)| encode_token(index, slot.generation))
            .collect()
    }

    /// Attach a cancel token to a live handle. Replaces any previous one.
    pub(crate) fn set_cancel(&self, token: *mut c_void, cancel: StdArc<dyn Any + Send + Sync>) {
        if token.is_null() {
            return;
        }
        let (index, generation) = decode_token(token);
        let mut slots = self.write();
        if let Some(slot) = slots.get_mut(index)
            && slot.generation == generation
            && slot.kind.is_some()
        {
            slot.cancel = Some(cancel);
        }
    }

    /// Clone out a handle's cancel token.
    ///
    /// The clone is what lets `SQLCancel` keep the token alive across a
    /// concurrent `SQLFreeHandle` or `SQLDisconnect` — the case SQLite's
    /// documentation calls out as unsafe ("a database connection that is
    /// closed or might close before `sqlite3_interrupt()` returns").
    pub(crate) fn cancel_of(&self, token: *mut c_void) -> Option<StdArc<dyn Any + Send + Sync>> {
        if token.is_null() {
            return None;
        }
        let (index, generation) = decode_token(token);
        let slots = self.read();
        let slot = slots.get(index)?;
        if slot.generation != generation || slot.kind.is_none() {
            return None;
        }
        slot.cancel.clone()
    }

    /// Recover the table after a panic poisoned it.
    ///
    /// The table is a plain `Vec` of integers and `Arc`s; a panic mid-update
    /// cannot leave it in a state that makes the checks unsound, and refusing
    /// every handle for the life of the process because one call panicked
    /// would be far worse.
    ///
    /// Returns `impl Deref` rather than naming a guard type because `slots` is
    /// `crate::sync::RwLock`, whose guard is `std::sync::RwLockReadGuard`
    /// normally but loom's own guard type under `--cfg loom`. One signature
    /// covers both instead of splitting the method on `#[cfg]`.
    fn read(&self) -> impl std::ops::Deref<Target = Vec<Slot>> + '_ {
        self.slots.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The write-locking counterpart to [`Self::read`]; see its doc comment
    /// for why the return type is `impl DerefMut`.
    fn write(&self) -> impl std::ops::DerefMut<Target = Vec<Slot>> + '_ {
        self.slots.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// The process-wide table every FFI entry point uses.
///
/// Defined the same way whether or not loom is enabled — `std::sync::OnceLock`
/// is not itself a loom-tracked primitive, only the `Registry` it lazily
/// builds is — but a loom model must never call this function, or anything in
/// `handles::mod` that resolves it internally (`alloc_environment`,
/// `alloc_connection`, `alloc_statement`, the `free_*` functions). The first
/// thing that goes wrong is a panic, not a subtler correctness gap: loom's
/// primitives register with the execution of whichever `loom::model` closure constructs them
/// (`loom::sync::RwLock::new` asserts one exists), so calling
/// `Registry::new()` — which this function does, lazily, the first time it
/// runs — outside an active model panics immediately. And even granting an
/// active model for that first call, a `static` still only runs its
/// construction once for the life of the process, while loom replays the same
/// closure many times to explore interleavings; every replay after the first
/// would be handed a `Registry` still wired to that first replay's now-defunct
/// execution. A loom model must build its own `Registry::new()` inside the
/// closure instead, never reaching for this one.
pub(crate) fn registry() -> &'static Registry {
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Registry::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A statement shares its connection's group, so one acquire covers both.
    /// This is what dissolves the stmt-then-parent-connection pattern in
    /// `ffi/metadata.rs`.
    #[test]
    fn a_statement_shares_its_connections_group() {
        let reg = Registry::new();
        let group = GroupLock::new();
        let (conn, _, _) = reg
            .register(HandleKind::Dbc, 0x1000, Arc::clone(&group), None)
            .expect("registered");
        let (stmt, _, _) = reg
            .register(
                HandleKind::Stmt,
                0x2000,
                Arc::clone(&group),
                Some(conn as usize),
            )
            .expect("registered");

        let conn_group = reg.group_of(conn).expect("live");
        let stmt_group = reg.group_of(stmt).expect("live");
        assert!(
            Arc::ptr_eq(&conn_group, &stmt_group),
            "a statement must share its connection's lock, not have its own"
        );
    }

    /// Parentage lives in the registry, so a child list is a snapshot the
    /// caller owns rather than a borrow of a handle field.
    #[test]
    fn children_are_derived_from_the_registry() {
        let reg = Registry::new();
        let env_group = GroupLock::new();
        let (env, _, _) = reg
            .register(HandleKind::Env, 0x1000, env_group, None)
            .expect("registered");

        let conn_group = GroupLock::new();
        let (conn_a, _, _) = reg
            .register(
                HandleKind::Dbc,
                0x2000,
                Arc::clone(&conn_group),
                Some(env as usize),
            )
            .expect("registered");
        let (conn_b, _, _) = reg
            .register(
                HandleKind::Dbc,
                0x3000,
                GroupLock::new(),
                Some(env as usize),
            )
            .expect("registered");

        let mut children = reg.children_of(env);
        children.sort();
        let mut expected = vec![conn_a, conn_b];
        expected.sort();
        assert_eq!(children, expected);
    }

    /// Freeing a child removes it from its parent's list with no `retain` on a
    /// handle field, which is what makes the iterate-while-mutating hazard in
    /// `SQLEndTran` unrepresentable.
    #[test]
    fn freeing_a_child_removes_it_from_its_parent() {
        let reg = Registry::new();
        let (env, _, _) = reg
            .register(HandleKind::Env, 0x1000, GroupLock::new(), None)
            .expect("registered");
        let (conn, _, _) = reg
            .register(
                HandleKind::Dbc,
                0x2000,
                GroupLock::new(),
                Some(env as usize),
            )
            .expect("registered");

        assert_eq!(reg.children_of(env), vec![conn]);
        reg.unregister(conn, HandleKind::Dbc).expect("was live");
        assert!(
            reg.children_of(env).is_empty(),
            "a freed child must not remain in its parent's list"
        );
    }

    /// The cancel token outlives the handle: cloning it out is what protects a
    /// concurrent `SQLCancel` from a `SQLDisconnect` on another thread.
    #[test]
    fn a_cloned_cancel_token_survives_the_handle_being_freed() {
        let reg = Registry::new();
        let (stmt, _, _) = reg
            .register(HandleKind::Stmt, 0x1000, GroupLock::new(), None)
            .expect("registered");

        let token: StdArc<dyn Any + Send + Sync> = StdArc::new(42u32);
        reg.set_cancel(stmt, token);

        let held = reg.cancel_of(stmt).expect("token stored");
        reg.unregister(stmt, HandleKind::Stmt).expect("was live");

        assert_eq!(
            held.downcast_ref::<u32>().copied(),
            Some(42),
            "the clone must stay usable after the slot is retired"
        );
        assert!(reg.cancel_of(stmt).is_none(), "the slot itself is gone");
    }

    /// Every production call site that obtains a [`GroupLock`] from the
    /// registry, and what it does with the one it gets.
    ///
    /// Obtaining a group from the registry is the **only** way to reach one
    /// that is not already held: `GroupLock::new` creates a fresh group for a
    /// handle being allocated, and `HandleScope` can only relock what it
    /// already has. So this list is the complete set of places the crate can
    /// take a group lock, which makes it the complete set of places the lock
    /// discipline can be broken.
    ///
    /// Counts rather than line numbers, because a line number is stale the
    /// moment anything above it moves.
    const GROUP_ACQUISITION_SITES: &[(&str, usize, &str)] = &[
        (
            "src/panic.rs",
            1,
            "panic_safe locks the target handle's group. The entry lock for \
             every FFI function except SQLCancel; acquires exactly one group \
             and nests nothing.",
        ),
        (
            "src/ffi/cursor.rs",
            1,
            "sql_cancel, and it try_locks rather than locking: taking the \
             group unconditionally would make cancelling a query wait for the \
             query it was asked to cancel. Cannot deadlock, because it never \
             blocks.",
        ),
        (
            "src/handles/mod.rs",
            2,
            "both group_of_kind, and neither locks. alloc_connection checks \
             the parent environment is live; alloc_statement takes the \
             connection's group to *share* with the new statement, which is \
             what puts a statement and its parent in one group.",
        ),
        (
            "src/handles/scope.rs",
            2,
            "holds_in compares a token's group against the held one without \
             locking, and with_child_group_in is the crate's one nested \
             acquisition — environment then connection, SQLEndTran's path. \
             Modelled by env_before_connection_cannot_deadlock.",
        ),
    ];

    /// The list above is complete: no other module acquires a group lock.
    ///
    /// This guards the failure mode a loom model structurally cannot catch. A
    /// model proves things about the code it calls; it says nothing about a
    /// *new* nesting site added somewhere it does not reach, which would sit
    /// green next to a second, unmodelled lock order. That is not hypothetical
    /// — `env_before_connection_cannot_deadlock` spent its whole life passing
    /// while proving a property of its own test code, and no amount of running
    /// it would have said so.
    ///
    /// A failure here is not necessarily a bug. It means someone added a place
    /// that can take a group lock, and the question to answer is whether it
    /// nests a second one: if it does, it needs a loom model and an entry in
    /// the ordering rule in AGENTS.md; if it does not, it needs a line in the
    /// list above saying so.
    ///
    /// Not run under Miri: it reads the source tree, which is slow under
    /// interpretation and contains no `unsafe` for Miri to check.
    #[test]
    #[cfg_attr(miri, ignore = "reads the source tree; no unsafe to check")]
    fn the_set_of_group_lock_acquisition_sites_is_closed() {
        // Test code is excluded: the models and unit tests take group locks
        // freely and legitimately, and it is production nesting the rule is
        // about. Every file in the crate puts its unit tests behind this exact
        // two-line sequence, so it is a reliable cut.
        const TEST_MODULE: &str = "#[cfg(test)]\nmod tests {";

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found: Vec<(String, usize)> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src is readable") {
                let path = entry.expect("readable entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("readable source");
                let production = source.split(TEST_MODULE).next().unwrap_or(&source);
                let count = production.matches(".group_of(").count()
                    + production.matches(".group_of_kind(").count();
                if count > 0 {
                    let rel = path
                        .strip_prefix(root.parent().expect("src has a parent"))
                        .expect("under the crate root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    found.push((rel, count));
                }
            }
        }
        found.sort();

        let mut expected: Vec<(String, usize)> = GROUP_ACQUISITION_SITES
            .iter()
            .map(|(file, count, _)| ((*file).to_owned(), *count))
            .collect();
        expected.sort();

        assert_eq!(
            found, expected,
            "the set of production group-lock acquisition sites changed.\n\
             A new or moved site is not automatically wrong, but it has to be \
             accounted for: if it nests a second group, it needs a loom model \
             and an entry in the lock-ordering rule in AGENTS.md; if it does \
             not, add it to GROUP_ACQUISITION_SITES with the reason. See that \
             list's doc comment.",
        );
    }

    /// A stale token is rejected without the address ever being read.
    #[test]
    fn a_freed_token_never_resolves_again() {
        let reg = Registry::new();
        let group = GroupLock::new();
        let (token, _, _) = reg
            .register(HandleKind::Dbc, 0x1000, Arc::clone(&group), None)
            .expect("registered");
        reg.unregister(token, HandleKind::Dbc).expect("was live");
        assert!(
            reg.resolve_in_group(token, HandleKind::Dbc, &group)
                .is_none()
        );
        assert!(reg.group_of(token).is_none());
    }

    /// A slot handed back out after a free must not carry over its previous
    /// occupant's lock group, parent, or cancel token — otherwise a handle
    /// allocated into a reused slot could end up sharing a lock, or a cancel
    /// token, with a completely unrelated handle that used to live there.
    #[test]
    fn a_reused_slot_inherits_nothing_from_its_previous_occupant() {
        let reg = Registry::new();
        let (old_parent, _, _) = reg
            .register(HandleKind::Env, 0x9000, GroupLock::new(), None)
            .expect("registered");

        let old_group = GroupLock::new();
        let (first, _, _) = reg
            .register(
                HandleKind::Dbc,
                0x1000,
                Arc::clone(&old_group),
                Some(old_parent as usize),
            )
            .expect("registered");
        reg.set_cancel(first, StdArc::new(1u32));
        reg.unregister(first, HandleKind::Dbc).expect("was live");

        // The table's only free slot is the one just retired, so this reuses
        // it rather than appending a new one.
        let (second, _, _) = reg
            .register(HandleKind::Dbc, 0x2000, GroupLock::new(), None)
            .expect("registered");

        let stored_group = reg.group_of(second).expect("live");
        assert!(
            !Arc::ptr_eq(&stored_group, &old_group),
            "a reused slot must not keep its previous occupant's lock group"
        );
        assert!(
            reg.cancel_of(second).is_none(),
            "a reused slot must not keep its previous occupant's cancel token"
        );
        assert!(
            !reg.children_of(old_parent).contains(&second),
            "a reused slot must not keep its previous occupant's parent"
        );
    }

    /// `SQLCancel`'s whole spec behaviour rests on this returning `Some` when
    /// no other thread is inside the connection.
    #[test]
    fn try_lock_succeeds_when_the_group_is_free() {
        let group = GroupLock::new();
        assert!(
            group.try_lock().is_some(),
            "an uncontended group must be lockable without blocking"
        );
    }

    /// The other half of `SQLCancel`'s spec behaviour: `try_lock` must not
    /// succeed while another thread is inside the connection.
    #[test]
    fn try_lock_fails_while_another_thread_holds_the_lock() {
        let group = GroupLock::new();
        let (holding_tx, holding_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let holder = Arc::clone(&group);
        let handle = std::thread::spawn(move || {
            let _guard = holder.lock();
            holding_tx.send(()).expect("main thread still waiting");
            release_rx.recv().expect("main thread still running");
        });

        holding_rx
            .recv()
            .expect("worker thread panicked before locking");
        assert!(
            group.try_lock().is_none(),
            "try_lock must not succeed while another thread holds the lock"
        );
        release_tx
            .send(())
            .expect("worker thread still waiting to release");
        handle.join().expect("worker thread panicked");
    }
}

/// loom models of this module's lock discipline.
///
/// Run with `RUSTFLAGS="--cfg loom" cargo test --lib loom_tests`; see
/// AGENTS.md's "Concurrency: the lock discipline" section for the design
/// these check. The `loom_tests` filter is required, not cosmetic: every
/// other unit test in the crate also runs under `--cfg loom` once it is set,
/// and calls the process-wide registry outside a `loom::model`, which panics
/// (see [`registry`]'s doc comment) — the filter keeps this crate's ordinary
/// tests out of a build they were never meant to run under.
///
/// Every model builds its own [`Registry`] rather than calling [`registry`]:
/// that function panics outside an active `loom::model` closure, and cannot
/// be called safely from inside one either, because loom replays the same
/// closure many times to explore interleavings while a `static` only runs its
/// initializer once — see `registry`'s own doc comment. A model therefore
/// never drives `alloc_environment`, `alloc_connection`, `alloc_statement`,
/// the `free_*` functions in `handles::mod`, `HandleScope`, `panic_safe`, or
/// `sql_cancel` — all of them resolve that process-wide singleton internally.
/// What follows proves properties of [`Registry`] and [`GroupLock`], where the
/// lock discipline actually lives, not of the FFI entry points that sit on
/// top of them.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::atomic::{AtomicBool, Ordering};
    use loom::thread;

    /// A statement and its connection share a group: `group_of` returns the
    /// same lock for both tokens. One thread holds that lock via the
    /// statement's token and resolves both the statement and its parent
    /// connection through it; a second thread does the same via the
    /// connection's token. Neither thread ever takes a second lock — `resolve`
    /// only reads the registry's own read lock — so this model does not
    /// exercise a two-lock deadlock; it checks that sharing one group between
    /// a statement and its parent lets a single acquisition resolve both.
    #[test]
    fn one_acquisition_covers_a_statement_and_its_parent() {
        loom::model(|| {
            let reg = Arc::new(Registry::new());
            let group = GroupLock::new();
            let (conn, _, _) = reg
                .register(HandleKind::Dbc, 0x1000, Arc::clone(&group), None)
                .expect("registered");
            let (stmt, _, _) = reg
                .register(
                    HandleKind::Stmt,
                    0x2000,
                    Arc::clone(&group),
                    Some(conn as usize),
                )
                .expect("registered");

            let a = {
                let reg = Arc::clone(&reg);
                thread::spawn(move || {
                    let g = reg.group_of(stmt).expect("live");
                    let _guard = g.lock();
                    assert!(reg.resolve_in_group(stmt, HandleKind::Stmt, &g).is_some());
                    assert!(reg.resolve_in_group(conn, HandleKind::Dbc, &g).is_some());
                })
            };
            let b = {
                let reg = Arc::clone(&reg);
                thread::spawn(move || {
                    let g = reg.group_of(conn).expect("live");
                    let _guard = g.lock();
                    assert!(reg.resolve_in_group(conn, HandleKind::Dbc, &g).is_some());
                })
            };
            a.join().expect("thread a panicked");
            b.join().expect("thread b panicked");
        });
    }

    /// `SQLCancel` must reach the backend without ever waiting for the group,
    /// even when another thread holds it for the model's entire duration.
    /// This model's own (main) thread plays that other thread: it takes the
    /// group and holds the guard until after `canceller.join()` returns,
    /// which is what makes the hold last exactly as long as the cancel takes
    /// — a cancel that (incorrectly) took the blocking `lock` instead of
    /// `try_lock` would then deadlock against this thread's own guard, not
    /// merely race it, since nothing releases the group until the join this
    /// thread is blocked in returns. A cancel that waited for the lock would
    /// be a cancel that waits for the query it is cancelling.
    #[test]
    fn cancel_never_waits_for_the_group() {
        loom::model(|| {
            let reg = Arc::new(Registry::new());
            let group = GroupLock::new();
            let (stmt, _, _) = reg
                .register(HandleKind::Stmt, 0x1000, Arc::clone(&group), None)
                .expect("registered");
            let flag = StdArc::new(AtomicBool::new(false));
            reg.set_cancel(stmt, StdArc::clone(&flag) as StdArc<dyn Any + Send + Sync>);

            // Held until this function returns, i.e. past `canceller.join()`
            // below: Rust drops locals in reverse declaration order, so this
            // guard outlives the join that waits on the thread it must not
            // block.
            let _executor_guard = group.lock();

            let canceller = {
                let reg = Arc::clone(&reg);
                let g = Arc::clone(&group);
                thread::spawn(move || {
                    // The behaviour under test: try_lock, never the blocking
                    // lock. A cancel that took the latter here would block
                    // forever on `_executor_guard` above, which is not
                    // dropped until after this thread has already been
                    // joined.
                    let _guard = g.try_lock();
                    let token = reg.cancel_of(stmt).expect("token still set");
                    token
                        .downcast_ref::<AtomicBool>()
                        .expect("type")
                        .store(true, Ordering::SeqCst);
                })
            };

            canceller.join().expect("canceller panicked");
            assert!(flag.load(Ordering::SeqCst), "cancel must always land");
        });
    }

    /// The clone taken via [`Registry::cancel_of`] is what makes a cancel
    /// safe against a free on another thread — the SQLite
    /// close-during-interrupt hazard, as a model rather than a comment. The
    /// model races `cancel_of` directly against [`Registry::unregister`],
    /// with neither side holding any `GroupLock`.
    ///
    /// That is *not* how the real FFI path behaves, and saying so matters:
    /// `sql_free_handle` (`ffi::handle`) wraps the whole free in `panic_safe`,
    /// which locks the statement's group — the same group `sql_cancel`'s
    /// `try_lock` contends for — before calling into `free_statement`, and
    /// `SQLDisconnect` holds its connection's group the same way while
    /// freeing every statement on it. So this model is the *more* adversarial
    /// case: proving `cancel_of` is safe with no lock serialising it against
    /// `unregister` at all is a stronger property than the real path needs,
    /// not a gap in coverage. It is also the only version a loom model can
    /// exercise, since a model can reach `Registry`/`GroupLock` directly but
    /// not `panic_safe` or `sql_free_handle` (this module's top-level doc
    /// comment).
    ///
    /// This is the strongest model available for the ordering `sql_cancel`
    /// uses (clone the token, *then* attempt `try_lock`): every step in that
    /// sequence — `cancel_of`, `group_of_kind`, `try_lock` — is a bounds- and
    /// generation-checked lookup that returns an owned value or `None`, never
    /// a stale reference, so no interleaving of either call order with a
    /// concurrent `unregister` produces a state distinguishable from this one.
    /// What the ordering actually narrows is the window in which a
    /// sufficiently fast concurrent free makes `cancel_of` observe `None`
    /// instead of `Some` — a race in *outcome*, not in soundness, and both
    /// outcomes are safe. On the real path this window is not governed by
    /// `sql_cancel`'s own ordering at all: while a free is in flight the
    /// freeing thread holds the group for the free's entire duration, so
    /// `try_lock` already fails for that whole window regardless of whether
    /// `cancel_of` ran before or after it; reordering the two changes only
    /// which instant inside the free's critical section `cancel_of` might
    /// land on, not whether `sql_cancel` takes the busy branch. There is
    /// consequently no assertion that holds for this order and fails for its
    /// reverse; a swapped `sql_cancel` still only ever reads a live token or
    /// correctly gets nothing. Reviewing the two lines in `sql_cancel` is what
    /// continues to guard that ordering.
    ///
    /// Said honestly: this model cannot fail by construction, and that is the
    /// point rather than a weakness. `StdArc` gives an owned clone a
    /// type-level guarantee that its target outlives it; no interleaving loom
    /// explores can violate that any more than it could for any other `Arc`
    /// in safe Rust, so there is no assertion here that a bug could trip.
    /// What the model still earns its keep on is running `cancel_of` and
    /// `unregister` against each other on every interleaving without a panic,
    /// a downcast failure, or a use of the token that fails once its target
    /// is gone — i.e. exercising the concurrent registry access this crate's
    /// design leans on, not proving a property beyond what `Arc` already
    /// gives for free. This is the same reason Priority 1's declined ordering
    /// test was correct to decline: the guarantee lives in the type system,
    /// not in an interleaving a model could catch.
    #[test]
    fn a_cancel_token_survives_a_concurrent_free() {
        loom::model(|| {
            let reg = Arc::new(Registry::new());
            let (stmt, _, _) = reg
                .register(HandleKind::Stmt, 0x1000, GroupLock::new(), None)
                .expect("registered");
            let live = StdArc::new(AtomicBool::new(true));
            reg.set_cancel(stmt, StdArc::clone(&live) as StdArc<dyn Any + Send + Sync>);

            let freer = {
                let reg = Arc::clone(&reg);
                thread::spawn(move || {
                    reg.unregister(stmt, HandleKind::Stmt);
                })
            };
            let canceller = {
                let reg = Arc::clone(&reg);
                thread::spawn(move || {
                    // Read through the clone, the way `sql_cancel` actually
                    // uses it (`signal_cancel`, `ffi::cursor`), rather than
                    // only checking the refcount.
                    if let Some(token) = reg.cancel_of(stmt) {
                        assert!(
                            token
                                .downcast_ref::<AtomicBool>()
                                .expect("type")
                                .load(Ordering::SeqCst)
                        );
                    }
                })
            };

            freer.join().expect("freer panicked");
            canceller.join().expect("canceller panicked");
        });
    }

    /// A `SQLCancel` reading a statement's token while a new execution is
    /// replacing it must observe one whole token or the other, never a torn or
    /// absent one.
    ///
    /// This interleaving only became reachable when cancel tokens started being
    /// minted per execution rather than once per statement: `set_cancel` used
    /// to run at most once, so nothing could race a *replacement*. Now every
    /// statement-producing call writes, while `sql_cancel` reads with no group
    /// lock at all — the two are serialised only by the registry's own
    /// `RwLock`.
    ///
    /// *Which* of the two tokens the canceller gets is deliberately not
    /// asserted: both are correct. Getting the outgoing one means cancelling an
    /// execution that has already finished, which the spec defines as a no-op
    /// ("a call to SQLCancel when no processing is being done on the statement
    /// ... has is [sic] no effect at all"), and getting the incoming one means
    /// cancelling the run that is actually in flight. What the model earns is
    /// that neither thread panics, the downcast always succeeds, and the read
    /// never yields `None` for a slot that has held a token throughout.
    #[test]
    fn a_cancel_token_replaced_by_a_new_execution_is_never_torn() {
        loom::model(|| {
            let reg = Arc::new(Registry::new());
            let (stmt, _, _) = reg
                .register(HandleKind::Stmt, 0x1000, GroupLock::new(), None)
                .expect("registered");
            // The token the first execution minted.
            reg.set_cancel(stmt, StdArc::new(1u32) as StdArc<dyn Any + Send + Sync>);

            let executor = {
                let reg = Arc::clone(&reg);
                thread::spawn(move || {
                    // A second execution on the same statement mints its own.
                    reg.set_cancel(stmt, StdArc::new(2u32) as StdArc<dyn Any + Send + Sync>);
                })
            };
            let canceller = {
                let reg = Arc::clone(&reg);
                thread::spawn(move || {
                    let token = reg
                        .cancel_of(stmt)
                        .expect("a token has been set since before either thread started");
                    let seen = *token.downcast_ref::<u32>().expect("type");
                    assert!(
                        seen == 1 || seen == 2,
                        "observed a token that was never set"
                    );
                })
            };

            executor.join().expect("executor panicked");
            canceller.join().expect("canceller panicked");
        });
    }

    /// `SQLEndTran(SQL_HANDLE_ENV)` walking a connection that is freed
    /// underneath it must skip that connection, not report failure.
    ///
    /// The race has two exits and they are easy to conflate: the registry may
    /// already show the slot retired when the group is resolved, or the
    /// resolve may succeed and the *lock* then block on the freeing thread, so
    /// the staleness is only discovered on the subsequent handle lookup.
    ///
    /// Said honestly: this model carries no assertion, because neither exit
    /// can panic by construction (both are a plain `continue` on `None`) and
    /// neither thread can deadlock — `unregister` here, like `sql_cancel`'s
    /// cross-thread branch, takes no `GroupLock` (this module's own primitive,
    /// not the real `free_connection` path, which does hold one; see
    /// `a_cancel_token_survives_a_concurrent_free` above for why that's the
    /// more adversarial case anyway), so the walker's `group.lock()` never has
    /// anything to contend with. What reaching the end of every explored
    /// interleaving actually shows is that `children_of`, `group_of`,
    /// `resolve` and `unregister` compose without a panic or a hang under
    /// concurrent access — not, by itself, that both exits are reached or
    /// handled identically. This is the model the freed-connection fix has no
    /// *unit* test for, because the interleaving cannot be forced
    /// single-threaded: a deterministic reproduction re-exercises one exit
    /// only, never both, which is what a loom model earns here even without
    /// an assertion of its own.
    #[test]
    fn a_connection_freed_mid_end_tran_is_skipped_not_reported() {
        loom::model(|| {
            let reg = Arc::new(Registry::new());
            let env_group = GroupLock::new();
            let (env, _, _) = reg
                .register(HandleKind::Env, 0x1000, env_group, None)
                .expect("registered");
            let conn_group = GroupLock::new();
            let (conn, _, _) = reg
                .register(
                    HandleKind::Dbc,
                    0x2000,
                    Arc::clone(&conn_group),
                    Some(env as usize),
                )
                .expect("registered");

            let freer = {
                let reg = Arc::clone(&reg);
                thread::spawn(move || {
                    reg.unregister(conn, HandleKind::Dbc);
                })
            };

            // The walk: snapshot, then resolve each child's group, then look
            // it up.
            let walker = {
                let reg = Arc::clone(&reg);
                thread::spawn(move || {
                    for token in reg.children_of(env) {
                        let Some(group) = reg.group_of(token) else {
                            continue; // exit one: already retired
                        };
                        let _guard = group.lock();
                        if reg
                            .resolve_in_group(token, HandleKind::Dbc, &group)
                            .is_none()
                        {
                            continue; // exit two: stale after the lock
                        }
                    }
                })
            };

            freer.join().expect("freer panicked");
            walker.join().expect("walker panicked");
            // Reaching here on every interleaving is the assertion: neither
            // exit panics, blocks forever, or produces a handle the walk
            // would misreport.
        });
    }

    /// Two threads nesting an environment's group then a connection's group
    /// cannot deadlock against each other, **through the crate's own nesting
    /// path**: `HandleScope::with_child_group_in`, which is what
    /// `SQLEndTran(SQL_HANDLE_ENV)` reaches and the only place in the crate
    /// that holds two groups at once.
    ///
    /// This model used to lock two `GroupLock`s of its own in the right order,
    /// which proved the ordering rule is *safe to follow* and nothing about
    /// whether the crate follows it — a regression reversing the acquisition
    /// order in `with_child_group` would not have made it fail. It could not do
    /// better while that function reached the process-wide `registry()`, which
    /// panics outside an active `loom::model` and cannot be called from inside
    /// one either: a `static` runs its initializer once, while loom replays the
    /// closure many times. Taking the registry as a parameter is what closed
    /// the gap.
    #[test]
    fn env_before_connection_cannot_deadlock() {
        use crate::handles::scope::HandleScope;

        loom::model(|| {
            let reg = Arc::new(Registry::new());
            let env_group = GroupLock::new();
            let (env, _, _) = reg
                .register(HandleKind::Env, 0x1000, Arc::clone(&env_group), None)
                .expect("env registered");
            let conn_group = GroupLock::new();
            let (conn, _, _) = reg
                .register(
                    HandleKind::Dbc,
                    0x2000,
                    Arc::clone(&conn_group),
                    Some(env as usize),
                )
                .expect("conn registered");

            // Exactly what `SQLEndTran(SQL_HANDLE_ENV)` does: hold the
            // environment's group, then reach into one of its connections.
            // The addresses above are never dereferenced — this models lock
            // acquisition, not handle access — so `f` does nothing with the
            // child scope it is handed.
            let nest = move |reg: &Registry, group: &Arc<GroupLock>| {
                let guard = group.lock();
                let mut scope = HandleScope::new(Some(Arc::clone(group)), Some(&guard));
                scope
                    .with_child_group_in(reg, conn, |_child| {})
                    .expect("the connection is live");
            };

            let t = {
                let reg = Arc::clone(&reg);
                let e = Arc::clone(&env_group);
                thread::spawn(move || nest(&reg, &e))
            };
            nest(&reg, &env_group);
            t.join().expect("thread panicked");
        });
    }
}
