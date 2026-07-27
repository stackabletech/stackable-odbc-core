//! The live-handle table.
//!
//! A handle is an opaque token pairing a slot index with a generation counter,
//! never an address; see the module docs in `super` for why. This module owns
//! the table, the per-connection lock groups keyed off it, and the cancel
//! tokens `SQLCancel` reads without taking any group lock.

use std::any::Any;
use std::ffi::c_void;

use crate::sync::{Arc, Mutex, MutexGuard, RwLock};

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
/// allocations, and this is a lock *token*, not a container. That is also why
/// recovering from poisoning is uncontroversial here — there is no data inside
/// to have been left half-written by a panic.
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
    #[allow(
        dead_code,
        reason = "no caller until the ffi/*.rs migrations (tasks 5-11) acquire a handle's group before touching it"
    )]
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
            Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
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
    /// connection for a statement or descriptor, `None` for an environment.
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
    cancel: Option<Arc<dyn Any + Send + Sync>>,
}

/// The live-handle table.
///
/// Read on every FFI call and written only when a handle is allocated or
/// freed, so it is read-mostly by a wide margin.
///
/// A type rather than a bare static so that a loom model can construct its own
/// and drive this code directly. loom's primitives are not const-constructible,
/// so a global static would have put the real implementation out of loom's
/// reach and left the models replicating it instead of proving it.
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

    /// Every live handle whose parent is `token`, as an owned snapshot.
    ///
    /// Owned, not borrowed: a caller iterating this while another call frees a
    /// child is the shape that made `SQLEndTran` unsound when the list was a
    /// field of the handle.
    #[allow(
        dead_code,
        reason = "no caller until a later task derives EnvironmentHandle::connections and ConnectionHandle::statements from this instead of a handle field"
    )]
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
        reason = "no caller until Backend::CancelToken (task 13) gives statements something to store here"
    )]
    pub(crate) fn set_cancel(&self, token: *mut c_void, cancel: Arc<dyn Any + Send + Sync>) {
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
    #[allow(
        dead_code,
        reason = "no caller until SQLCancel (task 15) reads the token stored by set_cancel"
    )]
    pub(crate) fn cancel_of(&self, token: *mut c_void) -> Option<Arc<dyn Any + Send + Sync>> {
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
/// builds is — but a loom model must never call this function. loom's own
/// primitives register with the execution of whichever `loom::model` closure
/// constructs them, while a `static` runs its construction exactly once for
/// the life of the process. loom replays its closure many times to explore
/// interleavings, so a model reaching this instance would hand its second
/// replay a `Registry` still wired to the first replay's execution. A loom
/// model calls `Registry::new()` directly inside the closure instead, the way
/// this module's own tests do.
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

        let token: Arc<dyn Any + Send + Sync> = Arc::new(42u32);
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
}
