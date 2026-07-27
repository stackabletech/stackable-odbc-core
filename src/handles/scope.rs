//! Access to handle contents, gated on holding the owning connection's lock.
//!
//! A `HandleScope` is the only way to obtain `&mut` to a handle. The only way
//! to obtain one is through the two callers of the `pub(crate)`
//! `HandleScope::new` in this crate — [`panic_safe_scoped`], which builds the
//! outermost scope for an FFI call, and [`HandleScope::with_child_group`],
//! which builds a nested scope for the one legitimate case of holding two
//! groups at once. Both lock the group immediately before constructing the
//! scope and tie its lifetime to that lock (see [`HandleScope::new`]), which
//! is what makes "the group lock is held" a fact the compiler checks rather
//! than a rule a comment states.
//!
//! [`panic_safe_scoped`]: crate::panic::panic_safe_scoped

use std::ffi::c_void;
use std::marker::PhantomData;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::handles::registry::{GroupLock, registry};
use crate::handles::{ConnectionHandle, EnvironmentHandle, HasKind, StatementHandle};
use crate::sync::{Arc, MutexGuard};

/// Proof that the caller holds one lock group, and the gateway to the handles
/// inside it.
///
/// `HandleScope::new` is `pub(crate)`, with exactly two callers in this crate:
/// [`panic_safe_scoped`], which builds the outermost scope for an FFI call,
/// and [`Self::with_child_group`], which builds a nested scope for the one
/// legitimate case of holding two groups at once. Both lock the group
/// immediately before constructing the scope and pass a borrow of that lock
/// as `new`'s `guard` parameter, which is what ties the lifetime `'a` to it:
/// a `HandleScope<'a>` cannot be constructed, returned, or used once its
/// originating guard is gone, so a live `HandleScope` always corresponds to a
/// held group lock — or, for a null handle, to nothing needing one.
///
/// [`panic_safe_scoped`]: crate::panic::panic_safe_scoped
pub struct HandleScope<'a> {
    /// The group whose lock the caller holds, or `None` for a call that
    /// arrived with `SQL_NULL_HANDLE` and so has nothing to protect.
    group: Option<Arc<GroupLock>>,
    /// Ties `'a` to the guard borrowed in [`Self::new`].
    _guard: PhantomData<&'a ()>,
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
    /// `pub(crate)` so that only this module's two callers can claim to hold
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
    fn holds(&self, token: *mut c_void) -> bool {
        match (&self.group, registry().group_of(token)) {
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
    pub fn get<T: HasKind>(&mut self, token: *mut c_void) -> Result<&mut T, OdbcError> {
        if !self.holds(token) {
            return Err(OdbcError::InvalidHandle);
        }
        let addr = registry()
            .resolve(token, T::KIND)
            .ok_or(OdbcError::InvalidHandle)?;
        // SAFETY: the registry produced `addr`, so it came from `Box::into_raw`
        // in an `alloc_*` function for a handle of exactly `T::KIND` and has not
        // been freed. `holds` established that this scope owns the lock guarding
        // it, so no other thread can hold a reference to the same handle.
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
    #[allow(
        dead_code,
        reason = "no caller until the ffi/metadata.rs migration (tasks 8-9) needs a statement and its parent connection from one acquisition"
    )]
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
        // they are literally different allocations -- the reason the two
        // references cannot alias is that neither handle is reachable from the
        // other (see the doc comment above), which distinct addresses alone
        // would not establish.
        debug_assert_ne!(stmt_addr as usize, conn_addr as usize);
        // SAFETY: both addresses came from `get`, which validated each token
        // against the registry and confirmed it belongs to the group this
        // scope holds, so neither is stale or foreign. The second `get` call
        // takes `&mut self`, but `self` is just `{ group, _guard: PhantomData }`
        // -- it holds no pointer into either handle's memory -- so reborrowing
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
    /// holds) is caught two ways: a `debug_assert!` panics immediately in a
    /// debug build — which `panic_safe_scoped`'s `catch_unwind` turns into a
    /// normal `SQL_ERROR`, never a hang — and, since re-entering a group one
    /// already holds is logically a no-op, an early return runs `f` directly
    /// against this scope instead of relocking. The early return is what
    /// actually removes the deadlock once `debug_assertions` are compiled out;
    /// the assertion exists only to make the mistake loud during development
    /// instead of silently degrading to a no-op.
    #[allow(
        dead_code,
        reason = "no caller until SQLEndTran(SQL_HANDLE_ENV) migrates (task 11) and needs to hold a child connection's group while inside the environment's"
    )]
    pub fn with_child_group<R>(
        &mut self,
        token: *mut c_void,
        f: impl FnOnce(&mut HandleScope<'_>) -> R,
    ) -> Result<R, OdbcError> {
        let already_held = self.holds(token);
        debug_assert!(
            !already_held,
            "with_child_group called with a token from the group this scope already holds; \
             crate::sync::Mutex is not reentrant, so this would deadlock in a release build"
        );
        if already_held {
            return Ok(f(self));
        }
        let group = registry().group_of(token).ok_or(OdbcError::InvalidHandle)?;
        let guard = group.lock();
        let mut child = HandleScope::new(Some(Arc::clone(&group)), Some(&guard));
        let result = f(&mut child);
        drop(guard);
        Ok(result)
    }

    /// Push a diagnostic onto whichever handle `token` names.
    ///
    /// Used by `panic_safe_scoped` on the error path. Silently does nothing for
    /// a token outside the held group or of a kind that carries no queue
    /// (descriptors), because there is no better handle to report against.
    pub fn push_diagnostic<B: Backend>(&mut self, token: *mut c_void, err: &OdbcError) {
        if !self.holds(token) {
            return;
        }
        if let Ok(env) = self.get::<EnvironmentHandle<B>>(token) {
            env.diagnostics.push(err);
        } else if let Ok(conn) = self.get::<ConnectionHandle<B>>(token) {
            conn.diagnostics.push(err);
        } else if let Ok(stmt) = self.get::<StatementHandle<B>>(token) {
            stmt.diagnostics.push(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panic::panic_safe_scoped;
    use crate::test_utils::{MockBackend, alloc_env_conn_stmt, cleanup_env_conn_stmt};
    use crate::types::SqlReturn;

    /// The check that makes the whole scheme real: a token from a *different*
    /// group must be refused, because the scope does not hold that group's
    /// lock. Without this, `scope.get` is `as_handle_ref` with extra syntax.
    #[test]
    fn a_token_outside_the_locked_group_is_refused() {
        unsafe {
            let (env_a, conn_a, stmt_a) = alloc_env_conn_stmt();
            let (env_b, conn_b, stmt_b) = alloc_env_conn_stmt();

            let ret = panic_safe_scoped::<MockBackend, _>(conn_a, |scope| {
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

            let ret = panic_safe_scoped::<MockBackend, _>(stmt, |scope| {
                let (s, c) = scope.stmt_with_parent::<MockBackend>(stmt)?;
                assert!(s.statement.is_none());
                assert!(c.connection.is_some());
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);

            // `SQLDisconnect` frees every statement on the connection as part
            // of tearing it down, so `stmt` is already gone (a stale token, not
            // a double-free -- `unregister` returns `None` before any
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
            panic_safe_scoped::<MockBackend, _>(std::ptr::null_mut(), |_scope| {
                Ok(SqlReturn::SUCCESS)
            })
        };
        assert_eq!(ret, SqlReturn::SUCCESS);
    }

    /// The other half of the null-handle case: a scope holding no group must
    /// still refuse a token from a real, live group -- `holds` must treat "no
    /// group held" as "nothing is reachable," not as "anything goes."
    #[test]
    fn a_null_handle_scope_still_refuses_a_live_token() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe_scoped::<MockBackend, _>(std::ptr::null_mut(), |scope| {
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
            let ret = panic_safe_scoped::<MockBackend, _>(conn, |_scope| {
                panic!("test panic");
            });
            assert_eq!(ret, SqlReturn::ERROR);

            // The group is free: a second call on the same connection works.
            let ret = panic_safe_scoped::<MockBackend, _>(conn, |scope| {
                scope.get::<ConnectionHandle<MockBackend>>(conn)?;
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `with_child_group`'s primary use (Task 11's `SQLEndTran(SQL_HANDLE_ENV)`):
    /// nest a second, distinct group's lock inside a scope that already holds
    /// a different one, and reach the child group's own handle through the
    /// nested scope. This is the interface's only correctness test; the two
    /// tests below cover the deadlock guard.
    #[test]
    fn with_child_group_locks_a_distinct_group_and_reaches_its_handle() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe_scoped::<MockBackend, _>(env, |scope| {
                let reached = scope.with_child_group(conn, |child| {
                    child.get::<ConnectionHandle<MockBackend>>(conn).is_ok()
                })?;
                assert!(
                    reached,
                    "the nested scope must be able to reach the child group's own handle"
                );
                // The outer scope's own group is still held and usable
                // afterward -- with_child_group must not have released it.
                scope.get::<EnvironmentHandle<MockBackend>>(env)?;
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `crate::sync::Mutex` is not reentrant, so passing a token from the
    /// group this scope already holds would deadlock the calling thread
    /// forever, with no diagnostic and no `SqlReturn` -- exactly the hazard
    /// `with_child_group`'s guard exists to close. In a debug build the
    /// `debug_assert!` fires first; `panic_safe_scoped`'s `catch_unwind`
    /// catches it and reports `SQL_ERROR`, never a hang. This test is
    /// therefore debug-build-only: with `debug_assertions` off, the
    /// `if self.holds(token)` early return takes over instead and this same
    /// call returns `SqlReturn::SUCCESS` -- not exercised here, since every
    /// verification command for this crate runs a debug build.
    #[cfg(debug_assertions)]
    #[test]
    fn with_child_group_on_the_scope_s_own_group_is_caught_not_deadlocked() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe_scoped::<MockBackend, _>(env, |scope| {
                // `env` belongs to the group `scope` already holds.
                let _ = scope.with_child_group(env, |_| ());
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(
                ret,
                SqlReturn::ERROR,
                "the debug_assert! must be caught by panic_safe_scoped's catch_unwind, not left \
                 to unwind into a hang or a process abort"
            );
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }
}
