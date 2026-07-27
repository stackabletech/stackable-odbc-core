//! Access to handle contents, gated on holding the owning connection's lock.
//!
//! A `HandleScope` is the only way to obtain `&mut` to a handle, and the only
//! way to obtain a `HandleScope` is to be inside [`panic_safe_scoped`]. That is
//! what makes "the group lock is held" a fact the compiler checks rather than a
//! rule a comment states.
//!
//! [`panic_safe_scoped`]: crate::panic::panic_safe_scoped

use std::ffi::c_void;
use std::marker::PhantomData;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::handles::registry::{GroupLock, registry};
use crate::handles::{ConnectionHandle, EnvironmentHandle, HasKind, StatementHandle};
use crate::sync::Arc;

/// Proof that the caller holds one lock group, and the gateway to the handles
/// inside it.
///
/// The lifetime `'a` is the guard's: a `&mut` handle obtained from a scope
/// cannot outlive the lock that makes it sound. A `HandleScope` is
/// constructible only from [`Self::new`], which is `pub(crate)` and called
/// from exactly one place — [`panic_safe_scoped`] — so every scope in
/// existence corresponds to a held group lock (or, for a null handle, to
/// nothing needing one).
///
/// [`panic_safe_scoped`]: crate::panic::panic_safe_scoped
pub struct HandleScope<'a> {
    /// The group whose lock the caller holds, or `None` for a call that
    /// arrived with `SQL_NULL_HANDLE` and so has nothing to protect.
    group: Option<Arc<GroupLock>>,
    /// Ties `'a` to the guard living in the caller's frame.
    _guard: PhantomData<&'a ()>,
}

impl<'a> HandleScope<'a> {
    /// Construct a scope for a held group. `pub(crate)` so that
    /// `panic_safe_scoped` is the only place that can claim to hold a lock.
    pub(crate) fn new(group: Option<Arc<GroupLock>>) -> Self {
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
    /// Sound because the two are different [`HandleKind`]s and therefore
    /// different allocations: no aliasing is possible. They share one group, so
    /// this needs no second acquisition.
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
        // different `Box` allocations from different `alloc_*` calls: the two
        // addresses below can never be equal, so handing out both `&mut`s at
        // once cannot alias.
        debug_assert_ne!(stmt_addr as usize, conn_addr as usize);
        // SAFETY: both addresses came from `get`, which validated each token
        // against the registry and confirmed it belongs to the group this
        // scope holds. Distinct `HandleKind`s guarantee distinct allocations
        // (see the `debug_assert_ne!` above), so the two references cannot
        // alias.
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
    #[allow(
        dead_code,
        reason = "no caller until SQLEndTran(SQL_HANDLE_ENV) migrates (task 11) and needs to hold a child connection's group while inside the environment's"
    )]
    pub fn with_child_group<R>(
        &mut self,
        token: *mut c_void,
        f: impl FnOnce(&mut HandleScope<'_>) -> R,
    ) -> Result<R, OdbcError> {
        let group = registry().group_of(token).ok_or(OdbcError::InvalidHandle)?;
        let guard = group.lock();
        let mut child = HandleScope::new(Some(Arc::clone(&group)));
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
            // of tearing it down, so `stmt` is already gone by the time
            // `cleanup_env_conn_stmt` reaches it; `free_connection` would
            // otherwise refuse to free a still-connected connection (HY010),
            // leaking its registry slot.
            let _ = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            cleanup_env_conn_stmt(env, conn, stmt);
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
}
