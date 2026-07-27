//! `panic_safe` wraps every FFI entry point to catch panics and turn them into
//! `SQL_ERROR` instead of unwinding across the C ABI.

use std::panic::AssertUnwindSafe;

use crate::errors::OdbcError;
use crate::handles::registry::registry;
use crate::handles::scope::HandleScope;
use crate::handles::try_get_diagnostic_queue;
use crate::types::SqlReturn;

/// Execute `f` at the FFI boundary, catching both `Err` returns and panics.
///
/// On success the returned `SqlReturn` is passed through. On an `Err`, the
/// error is pushed onto the handle's diagnostic queue (if the handle is valid)
/// and `err.sql_return()` is returned. On a panic, an `OdbcError::Panic`
/// diagnostic is pushed and `SqlReturn::ERROR` is returned.
///
/// [`OdbcError::InvalidHandle`] is the exception: the spec states that when a
/// function returns `SQL_INVALID_HANDLE` no diagnostic record is posted, since
/// there is no valid handle to post it against and the application must not
/// call `SQLGetDiagRec` afterwards. Pushing one would leave a stale record that
/// the next call on that handle reports.
/// # Safety
///
/// `handle` must be either null or a valid pointer to an ODBC handle previously
/// allocated by one of the `alloc_*` functions in `handles`.
#[allow(
    dead_code,
    reason = "superseded by panic_safe_scoped; every ffi/ call site has migrated as of task 11, \
              leaving only this module's own tests as callers, pending removal in a follow-up task"
)]
pub unsafe fn panic_safe<B, F>(handle: *mut std::ffi::c_void, f: F) -> SqlReturn
where
    B: crate::backend::Backend,
    F: FnOnce() -> Result<SqlReturn, OdbcError>,
{
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(ret)) => ret,
        Ok(Err(err)) => {
            if !matches!(err, OdbcError::InvalidHandle)
                && let Ok(queue) = unsafe { try_get_diagnostic_queue::<B>(handle) }
            {
                queue.push(&err);
            }
            err.sql_return()
        }
        Err(_panic) => {
            let err = OdbcError::Panic {
                message: "internal driver panic".into(),
            };
            if let Ok(queue) = unsafe { try_get_diagnostic_queue::<B>(handle) } {
                queue.push(&err);
            }
            SqlReturn::ERROR
        }
    }
}

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
pub unsafe fn panic_safe_scoped<B, F>(handle: *mut std::ffi::c_void, f: F) -> SqlReturn
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockBackend;

    #[test]
    fn success_passes_through() {
        let result = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), || Ok(SqlReturn::SUCCESS))
        };
        assert_eq!(result, SqlReturn::SUCCESS);
    }

    #[test]
    fn error_returns_sql_error() {
        let result = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), || Err(OdbcError::NotConnected))
        };
        assert_eq!(result, SqlReturn::ERROR);
    }

    #[test]
    fn panic_is_caught() {
        let result = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), || {
                panic!("test panic");
            })
        };
        assert_eq!(result, SqlReturn::ERROR);
    }

    #[test]
    fn invalid_handle_error_returns_invalid_handle() {
        let result = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), || Err(OdbcError::InvalidHandle))
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

            let ret = panic_safe::<MockBackend, _>(env, || Err(OdbcError::NotConnected));
            assert_eq!(ret, SqlReturn::ERROR);

            // Verify diagnostic was pushed to the handle's queue.
            let queue = try_get_diagnostic_queue::<MockBackend>(env).unwrap();
            assert_eq!(queue.len(), 1);
            let record = queue.get(0).unwrap();
            assert_eq!(
                record.sqlstate.as_str(),
                crate::types::sql_state::CONNECTION_NOT_OPEN
            );

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

            let ret = panic_safe::<MockBackend, _>(env, || Err(OdbcError::InvalidHandle));
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);

            let queue = try_get_diagnostic_queue::<MockBackend>(env).unwrap();
            assert_eq!(queue.len(), 0);

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

            let ret = panic_safe::<MockBackend, _>(env, || {
                panic!("test panic for diagnostic");
            });
            assert_eq!(ret, SqlReturn::ERROR);

            // Verify panic diagnostic was pushed to the handle's queue.
            let queue = try_get_diagnostic_queue::<MockBackend>(env).unwrap();
            assert_eq!(queue.len(), 1);
            let record = queue.get(0).unwrap();
            assert_eq!(
                record.sqlstate.as_str(),
                crate::types::sql_state::GENERAL_ERROR
            );
            assert!(record.message.contains("panic"));

            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }
}
