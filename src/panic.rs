//! `panic_safe` wraps every FFI entry point to catch panics and turn them into
//! `SQL_ERROR` instead of unwinding across the C ABI.

use std::panic::AssertUnwindSafe;

use crate::errors::OdbcError;
use crate::handles::registry::registry;
use crate::handles::scope::HandleScope;
use crate::types::SqlReturn;

/// Execute `f` at the FFI boundary, holding `handle`'s lock group, catching
/// both `Err` returns and panics.
///
/// The group's `Arc` and its guard live in *this* frame, and the
/// [`HandleScope`] handed to `f` merely borrows them. That is what keeps the
/// scope free of a self-referential owned guard, and so free of `unsafe`.
///
/// A null `handle` is not an error: `SQLAllocHandle(SQL_HANDLE_ENV,
/// SQL_NULL_HANDLE, ..)` legitimately arrives with nothing to lock, and gets a
/// scope holding no group.
///
/// On an `Err`, the error is pushed onto the handle's diagnostic queue through
/// the scope this function still holds — no second acquisition. On a panic,
/// `catch_unwind` catches inside *this function's own frame*, so the unwind
/// never reaches `_guard`: it is untouched by the panic and stays held while
/// an [`OdbcError::Panic`] diagnostic is pushed through that same scope, then
/// releases normally when this function returns and `_guard` drops. That is
/// what makes the diagnostic push on this path sound rather than an unguarded
/// mutation of the handle.
///
/// [`OdbcError::InvalidHandle`] is the exception: the spec posts no diagnostic
/// record alongside `SQL_INVALID_HANDLE`, since there is no valid handle to
/// post it against.
///
/// # Safety
///
/// `handle` must be either null or a token previously issued by one of the
/// `alloc_*` functions in [`crate::handles`].
pub unsafe fn panic_safe<B, F>(handle: *mut std::ffi::c_void, f: F) -> SqlReturn
where
    B: crate::backend::Backend,
    F: FnOnce(&mut HandleScope<'_>) -> Result<SqlReturn, OdbcError>,
{
    let group = registry().group_of(handle);
    let _guard = group.as_ref().map(|g| g.lock());
    let mut scope = HandleScope::new(group.clone(), _guard.as_ref());

    match std::panic::catch_unwind(AssertUnwindSafe(|| f(&mut scope))) {
        Ok(Ok(ret)) => ret,
        Ok(Err(err)) => {
            if !matches!(err, OdbcError::InvalidHandle) {
                scope.push_diagnostic::<B>(handle, &err);
            }
            err.sql_return()
        }
        Err(_panic) => {
            let err = OdbcError::Panic {
                message: "internal driver panic".into(),
            };
            scope.push_diagnostic::<B>(handle, &err);
            SqlReturn::ERROR
        }
    }
}

/// Catch panics at the FFI boundary without acquiring any lock.
///
/// [`panic_safe`] cannot serve `sql_cancel`: it acquires the target handle's
/// group lock unconditionally, and the whole point of `sql_cancel`'s
/// `try_lock` bifurcation is to keep running when that lock is held by
/// another thread. This sibling exists so the no-unwinding-across-the-C-ABI
/// guarantee stays a single implementation rather than growing a second copy
/// with its own `catch_unwind`.
///
/// Two callers, and both are here because they have no handle to work through:
///
/// - `sql_cancel`, whose cross-thread path must not touch the diagnostic state
///   [`panic_safe`] clears and pushes under a held group lock, per the spec's
///   own carve-out for cancelling a function running on another thread.
/// - `config_dsn_w`, the `ConfigDSNW` installer entry point, which takes no
///   ODBC handle at all — its arguments are a window handle, a request code and
///   two strings. There is no token to lock a group by and no diagnostic queue
///   to push to, so [`panic_safe`] is not merely unnecessary there but
///   inapplicable. It is still an `extern "system"` boundary, and an unwind
///   across it lands in the ODBC Administrator.
///
/// Generic over the return type for that second caller: `ConfigDSN` returns a
/// `BOOL`, not a `SqlReturn`. `on_panic` is a closure rather than a value so a
/// caller with its own error channel can use it — `config_dsn_w` posts an
/// installer error there, which is the only way its FALSE says anything.
///
/// Unlike `panic_safe`, a caught panic here pushes **no** diagnostic record —
/// it returns a bare `SQL_ERROR` and nothing else. `panic_safe` can push
/// [`OdbcError::Panic`] because it always holds a scope to push it through;
/// this function holds no lock at all on `sql_cancel`'s cross-thread branch,
/// so there is nothing sound to push through, and on the idle branch the
/// scope (and the lock it depends on) has already gone out of scope by the
/// time `catch_unwind` returns control here. A panicking `cancel` is
/// therefore reported to the application only as `SQL_ERROR`, with no
/// SQLSTATE a later `SQLGetDiagRec` could read back.
pub(crate) fn panic_safe_unlocked<T, F, P>(f: F, on_panic: P) -> T
where
    F: FnOnce() -> T,
    P: FnOnce() -> T,
{
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(ret) => ret,
        Err(_panic) => on_panic(),
    }
}

/// Catch a panic escaping `f`, converting it into the same
/// [`OdbcError::Panic`] shape a non-panicking failure already returns.
///
/// `SQLCopyDesc`'s phase one (`ffi::desc::sql_copy_desc`) calls this from
/// inside `HandleScope::with_group` on the *source* descriptor's group,
/// before phase two's [`panic_safe`] ever runs on the *target*. `with_group`
/// carries no `catch_unwind` of its own — it is a plain lock-then-call, not an
/// FFI-boundary guard — so a panic reaching `describe_col` through
/// `snapshot_ird` (driver-author code, the same surface every other
/// `Backend` call runs under a guard for) would otherwise unwind straight
/// through it, past `sql_copy_desc`, and across the `extern "system"`
/// boundary `forward_ffi!` generates for it.
///
/// Unlike [`panic_safe_unlocked`], this is not itself an FFI-boundary guard:
/// it exists so phase one's panic can be turned into the *same* `Err` shape
/// phase one already returns for a non-panicking failure (`HY007`, an
/// unpopulated IRD), rather than becoming a second, cruder failure mode. Both
/// flow through phase two's `panic_safe` unchanged, via the `?` on the
/// snapshot [`sql_copy_desc`] already had — so the panic is posted as `HY000`
/// to the *target*'s diagnostic queue, exactly where the spec says this
/// call's diagnostics belong (and where `HY007` was already posted). There is
/// no handle to push a diagnostic through at the point this function runs —
/// the same reason [`panic_safe_unlocked`] posts none on its own two call
/// sites — so it returns a plain `Result` for its caller to route onward
/// instead of trying.
///
/// [`panic_safe`]: crate::panic::panic_safe
pub(crate) fn catch_panic_as_error<T>(
    f: impl FnOnce() -> Result<T, OdbcError>,
) -> Result<T, OdbcError> {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_panic| {
        Err(OdbcError::Panic {
            message: "internal driver panic".into(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handles::EnvironmentHandle;
    use crate::test_utils::{MockBackend, with_handle};

    #[test]
    fn success_passes_through() {
        let result = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), |_scope| Ok(SqlReturn::SUCCESS))
        };
        assert_eq!(result, SqlReturn::SUCCESS);
    }

    #[test]
    fn error_returns_sql_error() {
        let result = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), |_scope| {
                Err(OdbcError::NotConnected)
            })
        };
        assert_eq!(result, SqlReturn::ERROR);
    }

    #[test]
    fn panic_is_caught() {
        let result = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), |_scope| {
                panic!("test panic");
            })
        };
        assert_eq!(result, SqlReturn::ERROR);
    }

    #[test]
    fn invalid_handle_error_returns_invalid_handle() {
        let result = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), |_scope| {
                Err(OdbcError::InvalidHandle)
            })
        };
        assert_eq!(result, SqlReturn::INVALID_HANDLE);
    }

    #[test]
    fn error_pushes_diagnostic_to_handle() {
        use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
        use odbc_sys::HandleType;

        unsafe {
            let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            let ret = panic_safe::<MockBackend, _>(env, |_scope| Err(OdbcError::NotConnected));
            assert_eq!(ret, SqlReturn::ERROR);

            // Verify diagnostic was pushed to the handle's queue.
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                assert_eq!(handle.diagnostics.len(), 1);
                let record = handle.diagnostics.get(0).unwrap();
                assert_eq!(
                    record.sqlstate.as_str(),
                    crate::types::sql_state::CONNECTION_NOT_OPEN
                );
            });

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    /// The spec posts no diagnostic record alongside `SQL_INVALID_HANDLE`, so
    /// the queue must stay empty; otherwise the record lingers and is reported
    /// by whatever call comes next on that handle.
    #[test]
    fn invalid_handle_pushes_no_diagnostic() {
        use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
        use odbc_sys::HandleType;

        unsafe {
            let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            let ret = panic_safe::<MockBackend, _>(env, |_scope| Err(OdbcError::InvalidHandle));
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);

            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                assert_eq!(handle.diagnostics.len(), 0);
            });

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn panic_pushes_diagnostic_to_handle() {
        use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
        use odbc_sys::HandleType;

        unsafe {
            let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );

            let ret = panic_safe::<MockBackend, _>(env, |_scope| {
                panic!("test panic for diagnostic");
            });
            assert_eq!(ret, SqlReturn::ERROR);

            // Verify panic diagnostic was pushed to the handle's queue.
            with_handle::<MockBackend, EnvironmentHandle<MockBackend>, _>(env, |handle| {
                assert_eq!(handle.diagnostics.len(), 1);
                let record = handle.diagnostics.get(0).unwrap();
                assert_eq!(
                    record.sqlstate.as_str(),
                    crate::types::sql_state::GENERAL_ERROR
                );
                assert!(record.message.contains("panic"));
            });

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }
}
