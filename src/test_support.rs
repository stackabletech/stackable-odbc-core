//! Hooks a driver's test suite needs and a driver's production build must not
//! have. Behind the default-off `test-support` feature, like [`conformance`].
//!
//! [`conformance`]: crate::conformance
//!
//! # Why this exists
//!
//! Most of core's interesting behaviour is on the *connected* path: the
//! `SQLGetInfoW` fallback chain, the catalog functions, `SQLGetTypeInfo`. A
//! driver's test suite wants to exercise that against its own `Backend` without
//! standing up a database, which means putting a connection object into a
//! connection handle without going through `SQLDriverConnectW`.
//!
//! The `handles` module is `pub(crate)`, so there is no way to do that from
//! outside, because a `SQLHANDLE` is an opaque token and the handle layout must
//! stay changeable. These functions are the supported
//! alternative: they take the same token an application holds, validate it the
//! same way every FFI entry point does, and never expose the handle type.

use std::ffi::c_void;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::handles::ConnectionHandle;
use crate::types::SqlReturn;

/// Puts `connection` into an allocated connection handle, as though
/// `SQLDriverConnectW` had succeeded.
///
/// The backend's `connect` is not called, so the connection may be one that
/// never talked to a data source, which is what lets a driver test core's
/// connected-path behaviour offline.
///
/// An already-attached connection is dropped and replaced rather than reported
/// as `08002`, so one handle can be re-armed across cases.
///
/// # Teardown
///
/// `SQLFreeHandle` refuses a connection handle that still holds a connection
/// (spec `HY010`), so a test must call one of two things. `SQLDisconnect`
/// invokes `Backend::disconnect`, which may not be meaningful for a connection
/// that was never opened. [`detach_connection`] takes the connection back out
/// without involving the backend at all, and is usually what an offline test
/// wants.
///
/// Holds `connection_handle`'s group lock for the duration, exactly as an FFI
/// entry point would, and catches a panic instead of letting it unwind out of
/// this crate into the driver's test binary.
///
/// # Safety
///
/// `connection_handle` must be a connection handle returned by
/// `SQLAllocHandle`, still live and not yet freed.
pub unsafe fn attach_connection<B: Backend>(
    connection_handle: *mut c_void,
    connection: B::Connection,
) -> Result<(), OdbcError> {
    let mut connection = Some(connection);
    let mut error = None;
    let ret = unsafe {
        crate::panic::panic_safe::<B, _>(connection_handle, |scope| {
            match scope.get::<ConnectionHandle<B>>(connection_handle) {
                Ok(handle) => handle.connection = connection.take(),
                Err(err) => error = Some(err),
            }
            Ok(SqlReturn::SUCCESS)
        })
    };
    match error {
        Some(err) => Err(err),
        // The closure above never returns anything but `Ok(SqlReturn::SUCCESS)`,
        // so a non-`SUCCESS` result with no `error` set means `panic_safe`
        // caught a panic instead of running the closure to completion. The
        // panic's unwind never reaches this caller, which is the entire
        // point of `panic_safe`, so this return value is the only signal
        // left that the attach did not happen; discarding it here would
        // silently report success while `connection` (and any previously
        // attached connection this call replaced) is dropped.
        None if ret != SqlReturn::SUCCESS => Err(OdbcError::Panic {
            message: "panic while attaching a connection".into(),
        }),
        None => Ok(()),
    }
}

/// Takes the connection back out of a connection handle, without calling
/// `Backend::disconnect`.
///
/// Returns `None` when the handle held no connection. After this the handle can
/// be freed with `SQLFreeHandle`, which otherwise reports `HY010`.
///
/// Holds `connection_handle`'s group lock for the duration, exactly as an FFI
/// entry point would, and catches a panic instead of letting it unwind out of
/// this crate into the driver's test binary.
///
/// # Safety
///
/// As [`attach_connection`].
pub unsafe fn detach_connection<B: Backend>(
    connection_handle: *mut c_void,
) -> Result<Option<B::Connection>, OdbcError> {
    let mut taken = None;
    let mut error = None;
    let ret = unsafe {
        crate::panic::panic_safe::<B, _>(connection_handle, |scope| {
            match scope.get::<ConnectionHandle<B>>(connection_handle) {
                Ok(handle) => taken = handle.connection.take(),
                Err(err) => error = Some(err),
            }
            Ok(SqlReturn::SUCCESS)
        })
    };
    match error {
        Some(err) => Err(err),
        // Same reasoning as `attach_connection`: a non-`SUCCESS` result with
        // no `error` set is the only remaining evidence of a caught panic,
        // and discarding it would report `Ok(None)`, meaning "nothing was
        // attached", when the true outcome is unknown.
        None if ret != SqlReturn::SUCCESS => Err(OdbcError::Panic {
            message: "panic while detaching a connection".into(),
        }),
        None => Ok(taken),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::{MockBackend, MockConnection};
    use crate::types::{HandleType, SqlReturn};

    unsafe fn alloc_env_and_conn() -> (*mut c_void, *mut c_void) {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn);
            (env, conn)
        }
    }

    #[test]
    fn an_attached_connection_puts_get_info_on_the_connected_path() {
        // The reason the hook exists: without a connection, `SQLGetInfoW` takes
        // the pre-connect branch and never reaches `Backend::get_info`.
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            attach_connection::<MockBackend>(conn, MockConnection).expect("attach");

            let mut buf = [0u16; 32];
            let mut len: i16 = -1;
            let ret = crate::ffi::info::sql_get_info_w::<MockBackend>(
                conn,
                crate::types::InfoType::DriverOdbcVer as u16,
                buf.as_mut_ptr() as *mut c_void,
                64,
                &mut len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let taken = detach_connection::<MockBackend>(conn).expect("detach");
            assert!(
                taken.is_some(),
                "the attached connection must come back out"
            );

            assert_eq!(
                sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn),
                SqlReturn::SUCCESS,
                "a detached handle must be freeable"
            );
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn freeing_a_still_attached_handle_is_refused() {
        // Spec HY010. This is why `detach_connection` exists: a test that
        // attaches and then frees would otherwise leak, and Miri's leak
        // reporting would fail the driver's own suite.
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            attach_connection::<MockBackend>(conn, MockConnection).expect("attach");

            assert_eq!(
                sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn),
                SqlReturn::ERROR,
                "a connected handle must not be freeable"
            );

            let _ = detach_connection::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn detach_reports_none_when_nothing_was_attached() {
        unsafe {
            let (env, conn) = alloc_env_and_conn();
            assert!(
                detach_connection::<MockBackend>(conn)
                    .expect("detach")
                    .is_none()
            );
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn an_invalid_handle_is_rejected_rather_than_dereferenced() {
        unsafe {
            assert!(
                attach_connection::<MockBackend>(std::ptr::null_mut(), MockConnection).is_err()
            );
            assert!(detach_connection::<MockBackend>(std::ptr::dangling_mut()).is_err());
        }
    }
}
