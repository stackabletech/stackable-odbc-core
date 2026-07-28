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
/// Replaces the old magic tags. The kind lives in the registry rather than in
/// the allocation, so checking it never touches the caller's value.
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
    #[allow(
        dead_code,
        reason = "read by lock()/try_lock(), which have no caller until the ffi/*.rs migrations (tasks 5-11) and SQLCancel (task 15) take the group lock this type guards"
    )]
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
    #[allow(
        dead_code,
        reason = "no caller until SQLCancel (task 15) stops taking the group lock and uses this instead"
    )]
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
/// primitives are also not const-constructible, so a global static could not
/// have been declared the way the pre-Task-2 `REGISTRY` was; a type gives
/// loom code a `Registry` it builds for itself instead.
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

    /// Resolve a token to the address of a live handle of the expected kind.
    ///
    /// Returns `None` for a token that was never issued, was issued for a
    /// different kind, or has been freed — **without dereferencing `token`**.
    pub(crate) fn resolve(&self, token: *mut c_void, expected: HandleKind) -> Option<usize> {
        if token.is_null() {
            return None;
        }
        let (index, generation) = decode_token(token);
        let slots = self.read();
        let slot = slots.get(index)?;
        if slot.generation != generation || slot.kind != Some(expected) {
            return None;
        }
        Some(slot.addr)
    }

    /// Resolve a token to `(kind, address)` without knowing its kind in
    /// advance.
    ///
    /// Used by the diagnostic-queue lookup, which accepts any handle type.
    pub(crate) fn resolve_any(&self, token: *mut c_void) -> Option<(HandleKind, usize)> {
        if token.is_null() {
            return None;
        }
        let (index, generation) = decode_token(token);
        let slots = self.read();
        let slot = slots.get(index)?;
        if slot.generation != generation {
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
    #[allow(
        dead_code,
        reason = "no production caller until task 14 wires handles::resolve_cancel_token into the nine statement-producing call sites; exercised today only by resolve_cancel_token's own unit test"
    )]
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

    /// A stale token is rejected without the address ever being read.
    #[test]
    fn a_freed_token_never_resolves_again() {
        let reg = Registry::new();
        let (token, _, _) = reg
            .register(HandleKind::Dbc, 0x1000, GroupLock::new(), None)
            .expect("registered");
        reg.unregister(token, HandleKind::Dbc).expect("was live");
        assert!(reg.resolve(token, HandleKind::Dbc).is_none());
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
