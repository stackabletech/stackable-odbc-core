//! Access to handle contents, gated on holding the owning connection's lock.
//!
//! A `HandleScope` is the only way to obtain `&mut` to a handle. The only way
//! to obtain one is through the three callers of the `pub(crate)`
//! `HandleScope::new` in this crate — [`panic_safe`], which builds the
//! outermost scope for an FFI call; [`HandleScope::with_child_group`], which
//! builds a nested scope for the one legitimate case of holding two groups at
//! once; and `sql_cancel`, which builds one only on the branch where its own
//! `try_lock` succeeded. All three lock the group immediately before
//! constructing the scope and tie its lifetime to that lock (see
//! [`HandleScope::new`]), which is what makes "the group lock is held" a fact
//! the compiler checks rather than a rule a comment states.
//!
//! [`panic_safe`]: crate::panic::panic_safe

use std::ffi::c_void;
use std::marker::PhantomData;

use crate::backend::Backend;
use crate::descriptor::DescriptorRole;
use crate::diagnostics::DiagnosticQueue;
use crate::errors::OdbcError;
use crate::handles::registry::{GroupLock, HandleKind, Registry, registry};
use crate::handles::{ConnectionHandle, EnvironmentHandle, HasKind, StatementHandle};
use crate::sync::{Arc, MutexGuard};

/// Proof that the caller holds one lock group, and the gateway to the handles
/// inside it.
///
/// `HandleScope::new` is `pub(crate)`, with exactly three callers in this
/// crate: [`panic_safe`], which builds the outermost scope for an FFI call;
/// [`Self::with_child_group`], which builds a nested scope for the one
/// legitimate case of holding two groups at once; and `sql_cancel`
/// (`ffi::cursor`), which builds one only on the branch where its own
/// `try_lock` succeeded — never on the branch where another thread holds the
/// group. All three lock the group immediately before constructing the scope
/// and pass a borrow of that lock as `new`'s `guard` parameter, which is what
/// ties the lifetime `'a` to it: a `HandleScope<'a>` cannot be constructed,
/// returned, or used once its originating guard is gone, so a live
/// `HandleScope` always corresponds to a held group lock — or, for a null
/// handle, to nothing needing one.
///
/// [`panic_safe`]: crate::panic::panic_safe
pub struct HandleScope<'a> {
    /// The group whose lock the caller holds, or `None` for a call that
    /// arrived with `SQL_NULL_HANDLE` and so has nothing to protect.
    group: Option<Arc<GroupLock>>,
    /// Ties `'a` to the guard borrowed in [`Self::new`], and makes the scope
    /// `!Send`.
    ///
    /// `*const ()` rather than `&'a ()` for the `!Send`: a `MutexGuard` is
    /// itself `!Send` because releasing a lock on a thread other than the one
    /// that took it is undefined for the underlying primitive, and a scope is
    /// only valid while that guard is held. Leaving the scope `Send` would let a
    /// scoped thread receive one whose guard is held elsewhere, and reach handle
    /// contents while claiming a lock it does not hold.
    ///
    /// Currently unreachable — every closure that receives a scope is in-crate
    /// and none spawns — so this closes the hole rather than fixing a live bug.
    /// It costs nothing: the lifetime is still tied, and `PhantomData<*const ()>`
    /// carries no variance the scope relies on.
    _guard: PhantomData<*const &'a ()>,
}

impl<'a> HandleScope<'a> {
    /// Construct a scope for a held group.
    ///
    /// `guard` is a borrow of the `MutexGuard` the caller is already holding
    /// for `group` (or `None`, for the null-handle case with nothing locked);
    /// its lifetime is what `'a` on the returned scope is unified with, so the
    /// borrow checker — not just a doc comment — refuses a `HandleScope` that
    /// outlives the lock it claims to hold. `guard`'s value is never read:
    /// this scope reaches handles through the registry, not through the
    /// guard, so the parameter exists purely to carry the lifetime.
    ///
    /// `pub(crate)` so that only this crate's three callers can claim to hold
    /// a lock.
    pub(crate) fn new(
        group: Option<Arc<GroupLock>>,
        guard: Option<&'a MutexGuard<'_, ()>>,
    ) -> Self {
        let _ = guard;
        Self {
            group,
            _guard: PhantomData,
        }
    }

    /// True when `token` belongs to the group this scope holds.
    ///
    /// Only [`Self::with_child_group_in`] needs this as a separate question:
    /// [`Self::get`] and [`Self::diagnostics`] fold it into their single
    /// registry pass. See that method for why the registry is a parameter.
    fn holds_in(&self, reg: &Registry, token: *mut c_void) -> bool {
        match (&self.group, reg.group_of(token)) {
            (Some(held), Some(theirs)) => Arc::ptr_eq(held, &theirs),
            _ => false,
        }
    }

    /// Borrow a handle from the locked group.
    ///
    /// Returns [`OdbcError::InvalidHandle`] for a stale token, a token of the
    /// wrong kind, **or a token belonging to another group** — the last case
    /// being what stops a caller reaching a handle this scope does not protect.
    ///
    /// The returned lifetime is tied to `&mut self`, so two handles cannot be
    /// held at once. Use [`Self::stmt_with_parent`] when both a statement and
    /// its connection are needed.
    /// One registry pass answers all three questions — live, right kind, right
    /// group — because this is the hottest lookup in the crate: it is on every
    /// FFI entry point. It used to be two, a [`Self::holds`] and a
    /// `Registry::resolve`, each taking the lock and decoding the token
    /// separately, plus an `Arc` clone `holds` made only to compare and drop.
    pub fn get<T: HasKind>(&mut self, token: *mut c_void) -> Result<&mut T, OdbcError> {
        // `None` is the null-handle case, where nothing is locked and so no
        // handle is reachable.
        let held = self.group.as_ref().ok_or(OdbcError::InvalidHandle)?;
        let addr = registry()
            .resolve_in_group(token, T::KIND, held)
            .ok_or(OdbcError::InvalidHandle)?;
        // SAFETY: the registry produced `addr`, so it came from `Box::into_raw`
        // in an `alloc_*` function for a handle of exactly `T::KIND` and has not
        // been freed. The same lookup established that the slot's group is the
        // one this scope holds the lock for, so no other thread can hold a
        // reference to the same handle.
        Ok(unsafe { &mut *(addr as *mut T) })
    }

    /// Borrow a statement and its parent connection together.
    ///
    /// Sound because **neither handle is reachable from the other**: `conn` is
    /// an opaque token (`*mut c_void`), not a typed pointer the compiler could
    /// follow from `&mut StatementHandle` to reborrow the connection, and
    /// `ConnectionHandle` holds no field pointing at its statements at all —
    /// parentage lives in the registry (`Registry::children_of`), not in
    /// either handle struct. Different [`HandleKind`]s alone would not be
    /// enough to justify this: `StatementHandle` owns four
    /// `Box<DescriptorHandle>` fields that *are* reachable through its own
    /// `&mut`, so a hypothetical `stmt_with_desc` built the same way would
    /// alias under Stacked/Tree Borrows despite the two addresses differing
    /// and `debug_assert_ne!` seeing nothing wrong — this combinator only
    /// exists for the one pair that is actually mutually unreachable.
    ///
    /// They share one group, so this needs no second acquisition.
    ///
    /// [`HandleKind`]: crate::handles::registry::HandleKind
    pub fn stmt_with_parent<B: Backend>(
        &mut self,
        token: *mut c_void,
    ) -> Result<(&mut StatementHandle<B>, &mut ConnectionHandle<B>), OdbcError> {
        let stmt_addr = {
            let stmt: &mut StatementHandle<B> = self.get(token)?;
            std::ptr::from_mut(stmt)
        };
        // SAFETY: `get` validated the statement above, and `stmt_addr` was
        // produced from that validated reference, so reading its `conn` field
        // is just reading our own already-validated allocation.
        let conn_token = unsafe { (*stmt_addr).conn };
        let conn_addr = {
            let conn: &mut ConnectionHandle<B> = self.get(conn_token)?;
            std::ptr::from_mut(conn)
        };
        // A statement and a connection are different `HandleKind`s, hence
        // different `Box` allocations from different `alloc_*` calls, so these
        // addresses can never be equal. This only pins the weaker fact that
        // they are literally different allocations — the reason the two
        // references cannot alias is that neither handle is reachable from the
        // other (see the doc comment above), which distinct addresses alone
        // would not establish.
        debug_assert_ne!(stmt_addr as usize, conn_addr as usize);
        // SAFETY: both addresses came from `get`, which validated each token
        // against the registry and confirmed it belongs to the group this
        // scope holds, so neither is stale or foreign. The second `get` call
        // takes `&mut self`, but `self` is just `{ group, _guard: PhantomData }`
        // — it holds no pointer into either handle's memory — so reborrowing
        // it to make that call touches nothing `stmt_addr` points at.
        // `stmt_addr` itself was produced by casting `addr as *mut T` inside
        // the *first* `get` call, never derived from `self`, so it carries no
        // provenance tying it to `self` for a later reborrow of `self` to
        // invalidate. Combined with neither handle being reachable from the
        // other, the two `&mut`s below cannot alias.
        Ok(unsafe { (&mut *stmt_addr, &mut *conn_addr) })
    }

    /// Run `f` while additionally holding `token`'s group.
    ///
    /// The crate's one lock-ordering rule is **environment before
    /// connection**, and `SQLEndTran(SQL_HANDLE_ENV)` is its only site: it
    /// holds the environment's group while walking that environment's
    /// connections. Do not call this in the other direction.
    ///
    /// Closure-shaped so the child's guard lives in this function's frame; a
    /// version returning a `HandleScope` would have to own its guard alongside
    /// the `Arc` it borrows from, which is not expressible without `unsafe`.
    ///
    /// `token` must name a group *different* from the one this scope already
    /// holds: `crate::sync::Mutex` is not reentrant, so locking the same group
    /// twice deadlocks the calling thread forever, with no diagnostic and no
    /// `SqlReturn`. A token from the already-held group (e.g. the scope's own
    /// token, or a statement belonging to a connection whose group this scope
    /// holds) is treated as a no-op instead: re-entering a group one already
    /// holds needs no second acquisition, so `f` runs directly against this
    /// scope and a [`tracing::warn!`] records the deviation. This runs
    /// identically in every build profile — a debug-only guard (e.g.
    /// `debug_assert!`) would leave the one branch that actually prevents the
    /// deadlock uncovered by every test this crate runs in debug, which is
    /// all of them.
    pub fn with_child_group<R>(
        &mut self,
        token: *mut c_void,
        f: impl FnOnce(&mut HandleScope<'_>) -> R,
    ) -> Result<R, OdbcError> {
        // Re-entering a group this scope already holds is a no-op, not a
        // nested acquisition: the lock is not reentrant, so taking it again
        // would hang the application thread with no diagnostic and no
        // SqlReturn. The one legitimate nesting is environment-then-
        // connection, where the groups differ.
        self.with_child_group_in(registry(), token, f)
    }

    /// The body of [`Self::with_child_group`], against an explicit registry.
    ///
    /// Split out so the loom model can drive the crate's **real** nesting path
    /// rather than a hand-written imitation of it. `registry()` panics outside
    /// an active `loom::model` and cannot be called from inside one either (a
    /// `static` runs its initializer once, while loom replays the closure many
    /// times), so a model restricted to `with_child_group` could only lock two
    /// `GroupLock`s of its own in the right order — which proves the ordering
    /// rule is safe to follow, not that this function follows it. Taking the
    /// registry as a parameter is what closes that gap: a regression reversing
    /// the acquisition order here now fails
    /// `env_before_connection_cannot_deadlock`.
    pub(crate) fn with_child_group_in<R>(
        &mut self,
        reg: &Registry,
        token: *mut c_void,
        f: impl FnOnce(&mut HandleScope<'_>) -> R,
    ) -> Result<R, OdbcError> {
        if self.holds_in(reg, token) {
            tracing::warn!(
                "with_child_group: token is already in the held group; not re-acquiring"
            );
            return Ok(f(self));
        }
        let group = reg.group_of(token).ok_or(OdbcError::InvalidHandle)?;
        let guard = group.lock();
        let mut child = HandleScope::new(Some(Arc::clone(&group)), Some(&guard));
        let result = f(&mut child);
        drop(guard);
        Ok(result)
    }

    /// Push a diagnostic onto whichever handle `token` names.
    ///
    /// Used by `panic_safe` on the error path. Silently does nothing for
    /// a token outside the held group or of a kind that carries no queue
    /// (descriptors), because there is no better handle to report against.
    /// Expressed through [`Self::diagnostics`] because that method already
    /// answers the same question — which queue, if any, does this token name —
    /// and answers it in one registry pass. Spelling the dispatch out a second
    /// time here cost seven lookups on a path that runs on **every** error:
    /// a `holds`, then up to three `get`s each doing a `holds` of its own.
    pub fn push_diagnostic<B: Backend>(&mut self, token: *mut c_void, err: &OdbcError) {
        if let Some(queue) = self.diagnostics::<B>(token) {
            queue.push(err);
        }
    }

    /// Borrow whichever handle `token` names, for its diagnostic queue only.
    ///
    /// `SQLGetDiagRecW`/`SQLGetDiagFieldW` are the spec's own exception to
    /// clearing a handle's diagnostics at the start of a call: they read the
    /// queue and must leave it untouched, so this hands back a plain
    /// `&mut DiagnosticQueue` rather than the whole handle, and — like
    /// [`Self::get`] and [`Self::push_diagnostic`] — refuses a token outside
    /// the held group. A descriptor is dispatched to
    /// [`Self::descriptor_diagnostics`], which cannot use the cast below.
    pub fn diagnostics<B: Backend>(&mut self, token: *mut c_void) -> Option<&mut DiagnosticQueue> {
        let (kind, addr) = {
            let held = self.group.as_ref()?;
            let (kind, addr, _parent) = registry().resolve_any_in_group(token, held)?;
            (kind, addr)
        };
        // SAFETY: the lookup confirmed this scope owns the lock guarding
        // `token`'s group, and the registry produced `addr` for exactly `kind`,
        // so the cast below matches what the corresponding `alloc_*` function
        // allocated (same reasoning as [`Self::get`]).
        match kind {
            HandleKind::Env => {
                Some(unsafe { &mut (*(addr as *mut EnvironmentHandle<B>)).diagnostics })
            }
            HandleKind::Dbc => {
                Some(unsafe { &mut (*(addr as *mut ConnectionHandle<B>)).diagnostics })
            }
            HandleKind::Stmt => {
                Some(unsafe { &mut (*(addr as *mut StatementHandle<B>)).diagnostics })
            }
            HandleKind::Desc => self.descriptor_diagnostics::<B>(token),
        }
    }

    /// Resolve a descriptor token to the statement that owns it and to which of
    /// the four it is.
    ///
    /// This is how every descriptor is reached. Every other handle kind is
    /// reached by casting the address the registry stored; a descriptor **must
    /// not** be, and that is why [`Descriptor`] deliberately has no `HasKind`
    /// impl. `HandleKind::Desc` is one kind covering four roles, so
    /// [`Self::get`], which dispatches on the kind alone, would resolve any one
    /// of a statement's four descriptors as any other and pass every check the
    /// registry can make. Without the impl, that call does not compile.
    ///
    /// Asking the owning statement answers the question a cast could only
    /// assume. `Slot::parent` records it at `alloc_statement` time, and
    /// comparing the token against the statement's four fields identifies the
    /// role exactly.
    ///
    /// Returning the *statement* rather than the descriptor is what makes the
    /// IRD-as-view workable, and it is also the only form the borrow rule in
    /// [`Self::stmt_with_parent`]'s comment permits. The caller reaches
    /// whatever it needs off this single `&mut`: a record map for the ARD, APD
    /// or IPD, or `stmt.statement`'s column metadata for the IRD. A
    /// `stmt_with_desc` combinator handing back both at once would alias under
    /// Stacked Borrows, because the four `Box<Descriptor>` fields *are*
    /// reachable through the statement's `&mut`.
    ///
    /// [`OdbcError::InvalidHandle`] for a token that is stale, outside the held
    /// group, not a descriptor, or somehow parentless — the same answer,
    /// because none of them names a descriptor this scope may reach.
    ///
    /// [`Descriptor`]: crate::handles::Descriptor
    pub fn descriptor_owner<B: Backend>(
        &mut self,
        token: *mut c_void,
    ) -> Result<(&mut StatementHandle<B>, DescriptorRole), OdbcError> {
        let parent = {
            let held = self.group.as_ref().ok_or(OdbcError::InvalidHandle)?;
            let (kind, _addr, parent) = registry()
                .resolve_any_in_group(token, held)
                .ok_or(OdbcError::InvalidHandle)?;
            if kind != HandleKind::Desc {
                return Err(OdbcError::InvalidHandle);
            }
            parent.ok_or(OdbcError::InvalidHandle)?
        };
        // A descriptor shares its statement's group, so this resolves under the
        // lock already held.
        let stmt: &mut StatementHandle<B> = self.get(parent)?;
        let role = if stmt.app_row_desc.token() == token {
            DescriptorRole::Ard
        } else if stmt.app_param_desc.token() == token {
            DescriptorRole::Apd
        } else if stmt.imp_row_desc.token() == token {
            DescriptorRole::Ird
        } else if stmt.imp_param_desc.token() == token {
            DescriptorRole::Ipd
        } else {
            return Err(OdbcError::InvalidHandle);
        };
        Ok((stmt, role))
    }

    /// Borrow a descriptor's diagnostic queue, through the statement that owns
    /// it.
    ///
    /// `SQLGetDescField`, `SQLSetDescField` and `SQLSetDescRec` all say their
    /// SQLSTATE "can be obtained by calling **SQLGetDiagRec** with a
    /// *HandleType* of SQL_HANDLE_DESC", so each descriptor carries a queue of
    /// its own. Which one is [`Self::descriptor_owner`]'s answer; this only
    /// picks the field.
    pub fn descriptor_diagnostics<B: Backend>(
        &mut self,
        token: *mut c_void,
    ) -> Option<&mut DiagnosticQueue> {
        let (owner, role) = self.descriptor_owner::<B>(token).ok()?;
        Some(match role {
            DescriptorRole::Ard => &mut owner.app_row_desc.diagnostics,
            DescriptorRole::Apd => &mut owner.app_param_desc.diagnostics,
            DescriptorRole::Ird => &mut owner.imp_row_desc.diagnostics,
            DescriptorRole::Ipd => &mut owner.imp_param_desc.diagnostics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panic::panic_safe;
    use crate::test_utils::{MockBackend, alloc_env_conn_stmt, cleanup_env_conn_stmt, with_handle};
    use crate::types::SqlReturn;

    /// The check that makes the whole scheme real: a token from a *different*
    /// group must be refused, because the scope does not hold that group's
    /// lock. Without this, `scope.get` would validate a token's kind and
    /// liveness but not its group, reaching a handle no lock protects it
    /// from — the exact unguarded access this type exists to close off.
    #[test]
    fn a_token_outside_the_locked_group_is_refused() {
        unsafe {
            let (env_a, conn_a, stmt_a) = alloc_env_conn_stmt();
            let (env_b, conn_b, stmt_b) = alloc_env_conn_stmt();

            let ret = panic_safe::<MockBackend, _>(conn_a, |scope| {
                // Its own group: fine.
                scope.get::<ConnectionHandle<MockBackend>>(conn_a)?;
                // Another connection's group, whose lock this scope does not
                // hold.
                let other = scope.get::<ConnectionHandle<MockBackend>>(conn_b);
                assert!(
                    matches!(other, Err(OdbcError::InvalidHandle)),
                    "a handle from an unlocked group must not be reachable"
                );
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);

            cleanup_env_conn_stmt(env_a, conn_a, stmt_a);
            cleanup_env_conn_stmt(env_b, conn_b, stmt_b);
        }
    }

    /// Each of a statement's four descriptors resolves to that statement and to
    /// its own role. Getting the role from the token is what `HY091` and the
    /// IRD's read-only rule are decided from, so a token resolving as the wrong
    /// role would answer the wrong SQLSTATE for every field.
    #[test]
    fn each_descriptor_token_resolves_to_its_statement_and_role() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let tokens =
                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    [
                        (handle.app_row_desc.token(), DescriptorRole::Ard),
                        (handle.app_param_desc.token(), DescriptorRole::Apd),
                        (handle.imp_row_desc.token(), DescriptorRole::Ird),
                        (handle.imp_param_desc.token(), DescriptorRole::Ipd),
                    ]
                });

            for (token, expected_role) in tokens {
                let ret = panic_safe::<MockBackend, _>(token, |scope| {
                    let (owner, role) = scope.descriptor_owner::<MockBackend>(token)?;
                    assert_eq!(role, expected_role);
                    // Every descriptor of one statement resolves to the same
                    // owner, whichever token was used.
                    assert_eq!(owner.app_row_desc.token(), tokens[0].0);
                    Ok(SqlReturn::SUCCESS)
                });
                assert_eq!(ret, SqlReturn::SUCCESS, "{expected_role:?} did not resolve");
            }

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A statement token is live and in the same group, so the group check
    /// alone would admit it. It is not a descriptor.
    #[test]
    fn descriptor_owner_refuses_a_non_descriptor_token() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let ret = panic_safe::<MockBackend, _>(stmt, |scope| {
                let result = scope.descriptor_owner::<MockBackend>(stmt);
                assert!(
                    matches!(result, Err(OdbcError::InvalidHandle)),
                    "a statement token resolved as a descriptor"
                );
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A statement and its parent connection share a group, so both are
    /// reachable from one acquisition — the seven `ffi/metadata.rs` sites.
    #[test]
    fn a_statement_and_its_parent_come_from_one_acquisition() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // `alloc_env_conn_stmt` deliberately leaves the connection
            // unconnected (`ffi/execute.rs`'s
            // `exec_direct_not_connected_returns_error` relies on exactly
            // that), so connect it here to exercise `c.connection.is_some()`
            // below.
            let input = "Host=localhost;Port=8080;Database=test;User=me";
            let wide: Vec<u16> = input.encode_utf16().collect();
            let connect_ret = crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(connect_ret, SqlReturn::SUCCESS);

            let ret = panic_safe::<MockBackend, _>(stmt, |scope| {
                let (s, c) = scope.stmt_with_parent::<MockBackend>(stmt)?;
                assert!(s.statement.is_none());
                assert!(c.connection.is_some());
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);

            // `SQLDisconnect` frees every statement on the connection as part
            // of tearing it down, so `stmt` is already gone (a stale token, not
            // a double-free — `unregister` returns `None` before any
            // `Box::from_raw`) by the time `cleanup_env_conn_stmt` reaches it;
            // pass null in its place rather than the now-stale `stmt` value, so
            // this call site does not contradict `cleanup_env_conn_stmt`'s
            // documented "must be live" precondition. `free_connection` would
            // otherwise refuse to free a still-connected connection (HY010),
            // leaking its registry slot, so assert the disconnect actually
            // succeeded rather than silently leaking on a future regression.
            let disconnect_ret = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            assert_eq!(disconnect_ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, std::ptr::null_mut());
        }
    }

    /// `SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, ..)` has no handle to
    /// lock, and must still run.
    #[test]
    fn a_null_handle_yields_a_scope_holding_no_lock() {
        let ret = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), |_scope| Ok(SqlReturn::SUCCESS))
        };
        assert_eq!(ret, SqlReturn::SUCCESS);
    }

    /// The other half of the null-handle case: a scope holding no group must
    /// still refuse a token from a real, live group — `holds` must treat "no
    /// group held" as "nothing is reachable," not as "anything goes."
    #[test]
    fn a_null_handle_scope_still_refuses_a_live_token() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe::<MockBackend, _>(std::ptr::null_mut(), |scope| {
                let result = scope.get::<ConnectionHandle<MockBackend>>(conn);
                assert!(
                    matches!(result, Err(OdbcError::InvalidHandle)),
                    "a scope holding no group must not reach any live handle"
                );
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A panic inside the closure releases the group rather than wedging every
    /// later call on that connection.
    #[test]
    fn a_panic_releases_the_group() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe::<MockBackend, _>(conn, |_scope| {
                panic!("test panic");
            });
            assert_eq!(ret, SqlReturn::ERROR);

            // The group is free: a second call on the same connection works.
            let ret = panic_safe::<MockBackend, _>(conn, |scope| {
                scope.get::<ConnectionHandle<MockBackend>>(conn)?;
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `with_child_group`'s primary use (`SQLEndTran(SQL_HANDLE_ENV)`): nest a
    /// second, distinct group's lock inside a scope that already holds a
    /// different one, and reach the child group's own handle through the
    /// nested scope. This is the interface's only correctness test; the test
    /// below covers the deadlock guard.
    #[test]
    fn with_child_group_locks_a_distinct_group_and_reaches_its_handle() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe::<MockBackend, _>(env, |scope| {
                let reached = scope.with_child_group(conn, |child| {
                    child.get::<ConnectionHandle<MockBackend>>(conn).is_ok()
                })?;
                assert!(
                    reached,
                    "the nested scope must be able to reach the child group's own handle"
                );
                // The outer scope's own group is still held and usable
                // afterward — with_child_group must not have released it.
                scope.get::<EnvironmentHandle<MockBackend>>(env)?;
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `crate::sync::Mutex` is not reentrant, so passing a token from the
    /// group this scope already holds would deadlock the calling thread
    /// forever, with no diagnostic and no `SqlReturn` — exactly the hazard
    /// `with_child_group`'s guard exists to close, and identically in every
    /// build profile: removing the early return here hangs this test rather
    /// than failing it, which is exactly why the guard runs unconditionally
    /// instead of only under `debug_assertions`.
    #[test]
    fn with_child_group_on_the_scope_s_own_group_is_a_no_op_not_a_deadlock() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe::<MockBackend, _>(env, |scope| {
                // `env` belongs to the group `scope` already holds.
                let ran = scope.with_child_group(env, |_| true)?;
                assert!(
                    ran,
                    "with_child_group must still run f, against this scope, rather than \
                     silently dropping the call"
                );
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "re-entering the already-held group must be a no-op, not an error or a hang"
            );
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `HandleScope` must not be `Send`.
    ///
    /// It is only valid while the group's `MutexGuard` is held, and a guard is
    /// itself `!Send` because releasing a lock from a thread other than the one
    /// that took it is undefined for the underlying primitive. A `Send` scope
    /// could be handed to a scoped thread, which would then reach handle
    /// contents while claiming a lock held on another thread.
    ///
    /// This is a compile-time assertion, not a runtime one: it fails to build if
    /// `_guard` ever goes back to a `PhantomData` that carries `Send`.
    const _: () = {
        trait AmbiguousIfSend<A> {
            fn some_item() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

        // The type parameter must be an inference variable: that is what makes
        // both impls candidates. Resolution succeeds only because exactly one
        // applies, i.e. `HandleScope` is not `Send`. If it became `Send` the
        // second impl would apply too and this fails to compile as ambiguous.
        // Naming `AmbiguousIfSend<()>` explicitly here would pick that impl
        // directly and assert nothing.
        let _ = <HandleScope<'static> as AmbiguousIfSend<_>>::some_item;
    };
}
