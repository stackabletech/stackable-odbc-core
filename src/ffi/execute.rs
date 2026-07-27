//! Statement execution: `SQLExecDirectW`, `SQLPrepareW`, `SQLExecute`.

use std::ffi::c_void;

use crate::backend::{Backend, StatementBackend};
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::{ConnectionHandle, StatementHandle, as_handle_ref};
use crate::panic::panic_safe;
use crate::types::{SQL_NTS, SqlReturn, SqlState};
use crate::utf16::utf16_to_string;

/// Generic implementation of SQLExecDirectW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlexecdirect-function>
///
/// Parses the UTF-16 SQL statement, calls `B::exec_direct`, and stores the
/// resulting statement in the handle.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (`SQLHSTMT`).
/// - `statement_text`: Pointer to the UTF-16 SQL statement text.
/// - `text_length`: Length of `statement_text` in characters, or `SQL_NTS` (-3) if
///   null-terminated.
///
/// # Spec compliance
///
/// - 01000: General warning — propagated from backend.
/// - 01001: Cursor operation conflict — propagated from backend.
/// - 01003: NULL value eliminated in set function — propagated from backend.
/// - 01004: String data, right truncated — propagated from backend.
/// - 01006: Privilege not revoked — propagated from backend.
/// - 01007: Privilege not granted — propagated from backend.
/// - 01S02: Option value changed — propagated from backend.
/// - 01S07: Fractional truncation — propagated from backend.
/// - 07002: COUNT field incorrect — propagated from backend.
/// - 07006: Restricted data type attribute violation — propagated from backend.
/// - 07007: Restricted parameter value violation — propagated from backend.
/// - 07S01: Invalid use of default parameter — propagated from backend.
/// - 08S01: Communication link failure — propagated from backend.
/// - 21S01: Insert value list does not match column list — propagated from backend.
/// - 21S02: Degree of derived table does not match column list — propagated from backend.
/// - 22001: String data, right truncation — propagated from backend.
/// - 22002: Indicator variable required but not supplied — propagated from backend.
/// - 22003: Numeric value out of range — propagated from backend.
/// - 22007: Invalid datetime format — propagated from backend.
/// - 22008: Datetime field overflow — propagated from backend.
/// - 22012: Division by zero — propagated from backend.
/// - 22015: Interval field overflow — propagated from backend.
/// - 22018: Invalid character value for cast specification — propagated from backend.
/// - 22019: Invalid escape character — propagated from backend.
/// - 22025: Invalid escape sequence — propagated from backend.
/// - 23000: Integrity constraint violation — propagated from backend.
/// - 24000: Invalid cursor state — fails if a cursor is already open on this statement (checked
///   here); also returned by backend for positioned-update/delete on improperly positioned cursor.
/// - 34000: Invalid cursor name — propagated from backend.
/// - 3D000: Invalid catalog name — propagated from backend.
/// - 3F000: Invalid schema name — propagated from backend.
/// - 40001: Serialization failure — propagated from backend.
/// - 40003: Statement completion unknown — propagated from backend.
/// - 42000: Syntax error or access violation — propagated from backend; also returned here
///   (checked before the backend is called) for an unterminated (malformed) ODBC escape
///   sequence when NOSCAN is off (`crate::escape::translate_escapes`).
/// - 42S01: Base table or view already exists — propagated from backend.
/// - 42S02: Base table or view not found — propagated from backend.
/// - 42S11: Index already exists — propagated from backend.
/// - 42S12: Index not found — propagated from backend.
/// - 42S21: Column already exists — propagated from backend.
/// - 42S22: Column not found — propagated from backend.
/// - 44000: WITH CHECK OPTION violation — propagated from backend.
/// - HY000: General error — propagated from backend.
/// - HY001: Memory allocation error — propagated from backend.
/// - HY008: Operation canceled — (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer — fails if `StatementText` is null (checked here).
/// - HY010: Function sequence error — (DM cases for async/NEED_DATA: driver-manager-handled; not
///   returned here); fails if the connection is not open (checked here).
/// - HY013: Memory management error — propagated from backend.
/// - HY090: Invalid string or buffer length — (DM case for `TextLength <= 0 and != SQL_NTS`:
///   driver-manager-handled); fails if `TextLength < 0` and `!= SQL_NTS` (checked here);
///   parameter-buffer-length cases propagated from backend.
/// - HY105: Invalid parameter type — propagated from backend.
/// - HY109: Invalid cursor position — propagated from backend.
/// - HY117: Connection suspended — (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented — propagated from backend; also returned here
///   (checked before the backend is called) for a `{call ...}`/`{?= call ...}` stored-procedure
///   escape, which this driver does not support, when NOSCAN is off.
/// - HYT00: Timeout expired — propagated from backend.
/// - HYT01: Connection timeout expired — propagated from backend.
/// - IM001: Driver does not support this function — (driver-manager-handled; not returned here).
/// - IM017: Polling disabled in async notification mode — (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called — (driver-manager-handled; not returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `statement_text` must be a valid UTF-16 string of the given length
/// (or null-terminated if `text_length` is `SQL_NTS`).
pub unsafe fn sql_exec_direct_w<B: Backend>(
    statement_handle: *mut c_void,
    statement_text: *const u16,
    text_length: i32,
) -> SqlReturn {
    tracing::debug!(
        "SQLExecDirectW(stmt={:?}, text_len={})",
        statement_handle,
        text_length
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; tag validated inside
    // panic_safe/as_handle_ref before any dereference occurs. statement_text is checked for
    // null before use, and is then valid for text_length UTF-16 code units (or null-terminated
    // if text_length == SQL_NTS/-3); caller upholds this per the function's safety contract.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();
            let noscan = stmt.noscan_enabled();

            // Spec HY009: StatementText must not be null.
            if statement_text.is_null() {
                return Err(OdbcError::general(
                    "StatementText is null",
                    SqlState::invalid_use_of_null_pointer(),
                ));
            }

            // Spec HY090: TextLength must be >= 0 or SQL_NTS.
            if text_length < 0 && text_length != SQL_NTS {
                return Err(OdbcError::general(
                    format!("Invalid text length: {text_length}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            // Spec 24000: Cursor already open. A prepared-but-unexecuted
            // statement (S2) has a `statement` but no cursor, so this reads
            // `cursor_open` rather than `statement.is_some()`.
            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Get parent connection via tag-validated traversal.
            let conn = as_handle_ref::<ConnectionHandle<B>>(stmt.conn)?;

            // Spec HY010: Connection must be open.
            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let sql = utf16_to_string(statement_text, text_length)?;
            // Spec: SQL_ATTR_NOSCAN=SQL_NOSCAN_ON disables escape-sequence
            // scanning; otherwise translate `{fn}`/`{d}`/`{t}`/`{ts}`/`{oj}`/
            // `{escape}` (and reject `{call}` with HYC00) before it reaches
            // the backend.
            let sql = if noscan {
                sql
            } else {
                crate::escape::translate_escapes(&sql, &B::escape_dialect())?
            };

            // Check for data-at-execution parameters.
            let param_count = crate::ffi::params::count_params(&sql);
            if param_count > 0 {
                // SAFETY: caller guarantees all bound buffer pointers remain valid.
                let (non_dae_values, dae_params) =
                    crate::ffi::params::find_data_at_exec_params(&stmt.param_bindings, param_count);

                if !dae_params.is_empty() {
                    stmt.data_at_exec = Some(crate::handles::DataAtExecState {
                        pending_params: dae_params.into(),
                        current_param: None,
                        buffer: Vec::new(),
                        collected_values: non_dae_values,
                        sql: sql.clone(),
                    });
                    stmt.param_count = Some(param_count);
                    return Ok(SqlReturn::NEED_DATA);
                }
            }

            // `Backend::exec_direct` takes no parameters, so a parameterised
            // statement must go through prepare + execute, which binds them.
            // Routing it to exec_direct would send the literal `?` to the
            // backend and silently discard every bound value.
            let result = if param_count > 0 {
                // SAFETY: caller guarantees all bound buffer pointers remain valid.
                let params = crate::ffi::params::collect_params(&stmt.param_bindings, param_count)?;
                let mut prepared = B::prepare(connection, &sql).into_odbc()?;
                let outcome = B::execute(connection, &mut prepared, &params).into_odbc()?;
                // SAFETY: the application's bound output buffer pointers remain
                // valid per the caller contract (same guarantee collect_params relies on).
                // Already inside the enclosing `unsafe` context, like collect_params above.
                crate::ffi::params::write_output_params(
                    &stmt.param_bindings,
                    &outcome.output_params,
                )?;
                prepared
            } else {
                B::exec_direct(connection, &sql).into_odbc()?
            };
            // Opens a cursor only if the statement actually returned columns.
            stmt.set_result_set(crate::handles::StatementData::Backend(result));

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLExecDirectW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLPrepareW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlprepare-function>
///
/// Validates and stores the SQL text, counts `?` parameter markers, and calls
/// `B::prepare` to let the backend validate the statement. Column metadata is
/// not available until `SQLExecute` is called.
///
/// Calling `SQLPrepareW` again on a statement clears the previous prepared
/// state and open cursor. Parameter bindings survive: per `SQLBindParameter`,
/// only another `SQLBindParameter`, `SQLFreeStmt(SQL_RESET_PARAMS)` or
/// `SQLSetDescField` setting the APD's `SQL_DESC_COUNT` to 0 unbinds one.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (`SQLHSTMT`).
/// - `statement_text`: Pointer to the UTF-16 SQL statement text.
/// - `text_length`: Length of `statement_text` in characters, or `SQL_NTS` (-3) if
///   null-terminated.
///
/// # Spec compliance
///
/// - 01000: General warning — propagated from backend.
/// - 01S02: Option value changed — propagated from backend.
/// - 08S01: Communication link failure — propagated from backend.
/// - 21S01: Insert value list does not match column list — propagated from backend.
/// - 21S02: Degree of derived table does not match column list — propagated from backend.
/// - 22018: Invalid character value for cast specification — propagated from backend.
/// - 22019: Invalid escape character — propagated from backend.
/// - 22025: Invalid escape sequence — propagated from backend.
/// - 24000: Invalid cursor state — (DM case for open cursor with fetched rows:
///   driver-manager-handled); driver case for open cursor without fetch is not checked here
///   because re-prepare is allowed and simply replaces the current state.
/// - 34000: Invalid cursor name — propagated from backend.
/// - 3D000: Invalid catalog name — propagated from backend.
/// - 3F000: Invalid schema name — propagated from backend.
/// - 42000: Syntax error or access violation — propagated from backend; also returned here
///   (checked before the backend is called) for an unterminated (malformed) ODBC escape
///   sequence when NOSCAN is off (`crate::escape::translate_escapes`).
/// - 42S01: Base table or view already exists — propagated from backend.
/// - 42S02: Base table or view not found — propagated from backend.
/// - 42S11: Index already exists — propagated from backend.
/// - 42S12: Index not found — propagated from backend.
/// - 42S21: Column already exists — propagated from backend.
/// - 42S22: Column not found — propagated from backend.
/// - HY000: General error — propagated from backend.
/// - HY001: Memory allocation error — propagated from backend.
/// - HY008: Operation canceled — (driver-manager-handled; not returned here).
/// - HY009: Invalid use of null pointer — fails if `StatementText` is null (checked here).
/// - HY010: Function sequence error — (DM cases for async/NEED_DATA: driver-manager-handled; not
///   returned here); fails if the connection is not open (checked here).
/// - HY013: Memory management error — propagated from backend.
/// - HY090: Invalid string or buffer length — (DM case `TextLength <= 0 and != SQL_NTS`:
///   driver-manager-handled); fails if `TextLength < 0` and `!= SQL_NTS` (checked here).
/// - HY117: Connection suspended — (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented — propagated from backend; also returned here
///   (checked before the backend is called) for a `{call ...}`/`{?= call ...}` stored-procedure
///   escape, which this driver does not support, when NOSCAN is off.
/// - HYT00: Timeout expired — propagated from backend.
/// - HYT01: Connection timeout expired — propagated from backend.
/// - IM001: Driver does not support this function — (driver-manager-handled; not returned here).
/// - IM017: Polling disabled in async notification mode — (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called — (driver-manager-handled; not returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_prepare_w<B: Backend>(
    statement_handle: *mut c_void,
    statement_text: *const u16,
    text_length: i32,
) -> SqlReturn {
    tracing::debug!(
        "SQLPrepareW(stmt={:?}, text_len={})",
        statement_handle,
        text_length
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; tag validated inside
    // panic_safe/as_handle_ref before any dereference occurs. statement_text is checked for
    // null before use, and is then valid for text_length UTF-16 code units (or null-terminated
    // if text_length == SQL_NTS/-3); caller upholds this per the function's safety contract.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();
            let noscan = stmt.noscan_enabled();

            if statement_text.is_null() {
                return Err(OdbcError::general(
                    "StatementText is null",
                    SqlState::invalid_use_of_null_pointer(),
                ));
            }

            if text_length < 0 && text_length != -3 {
                return Err(OdbcError::general(
                    format!("Invalid text length: {text_length}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            let sql = utf16_to_string(statement_text, text_length)?;
            // Spec: SQL_ATTR_NOSCAN=SQL_NOSCAN_ON disables escape-sequence
            // scanning; otherwise translate before counting `?` markers:
            // escapes never contain them, so translation cannot move a
            // marker's position.
            let sql = if noscan {
                sql
            } else {
                crate::escape::translate_escapes(&sql, &B::escape_dialect())?
            };
            let param_count = crate::ffi::params::count_params(&sql);

            // Get parent connection.
            let conn_ptr = stmt.conn;
            let conn = as_handle_ref::<ConnectionHandle<B>>(conn_ptr)?;
            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // Ask backend to validate and prepare the statement.
            let prepared = B::prepare(connection, &sql).into_odbc()?;

            // Re-acquire stmt after conn borrow ends, then store state.
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            // Prepared, not executed (S2/S3): no cursor is open, and a
            // re-prepare closes any cursor the previous execution left open.
            stmt.set_prepared_statement(crate::handles::StatementData::Backend(prepared));
            stmt.prepared_sql = Some(sql);
            stmt.param_count = Some(param_count);
            // Parameter bindings deliberately survive. SQLBindParameter's spec
            // names the only three things that unbind a parameter -- another
            // SQLBindParameter, SQLFreeStmt(SQL_RESET_PARAMS), and
            // SQLSetDescField setting the APD's SQL_DESC_COUNT to 0 -- and
            // SQLPrepare is not among them. SQLPrepare's own Comments confirm
            // it from the other side: "an application should unbind all
            // parameters that applied to an old SQL statement before preparing
            // a new SQL statement on the same statement", which is advice only
            // a driver that keeps them could need.
            //
            // Bindings above `param_count` are simply not read: `collect_params`
            // walks 1..=param_count. That is the "old parameter information
            // being applied to the new statement" the spec warns about, and it
            // is the application's to avoid.

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLPrepareW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLExecute.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlexecute-function>
///
/// Collects the current values from all bound parameter buffers (in order
/// `1..=param_count`), then calls `B::execute` with those values. The backend
/// modifies the statement in-place to hold the resulting cursor or DML count.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (`SQLHSTMT`).
///
/// # Spec compliance
///
/// - 01000: General warning — propagated from backend.
/// - 01001: Cursor operation conflict — propagated from backend.
/// - 01003: NULL value eliminated in set function — propagated from backend.
/// - 01004: String data, right truncated — propagated from backend.
/// - 01006: Privilege not revoked — propagated from backend.
/// - 01007: Privilege not granted — propagated from backend.
/// - 01S02: Option value changed — propagated from backend.
/// - 01S07: Fractional truncation — propagated from backend.
/// - 07002: COUNT field incorrect — propagated from backend.
/// - 07006: Restricted data type attribute violation — propagated from backend.
/// - 07007: Restricted parameter value violation — propagated from backend.
/// - 07S01: Invalid use of default parameter — propagated from backend.
/// - 08S01: Communication link failure — propagated from backend.
/// - 21S02: Degree of derived table does not match column list — propagated from backend.
/// - 22001: String data, right truncation — propagated from backend.
/// - 22002: Indicator variable required but not supplied — propagated from backend.
/// - 22003: Numeric value out of range — propagated from backend.
/// - 22007: Invalid datetime format — propagated from backend.
/// - 22008: Datetime field overflow — propagated from backend.
/// - 22012: Division by zero — propagated from backend.
/// - 22015: Interval field overflow — propagated from backend.
/// - 22018: Invalid character value for cast specification — propagated from backend.
/// - 22019: Invalid escape character — propagated from backend.
/// - 22025: Invalid escape sequence — propagated from backend.
/// - 23000: Integrity constraint violation — propagated from backend.
/// - 24000: Invalid cursor state — propagated from backend (cursor open or positioned-update/delete
///   on improperly positioned cursor).
/// - 40001: Serialization failure — propagated from backend.
/// - 40003: Statement completion unknown — propagated from backend.
/// - 42000: Syntax error or access violation — propagated from backend.
/// - 44000: WITH CHECK OPTION violation — propagated from backend.
/// - HY000: General error — propagated from backend.
/// - HY001: Memory allocation error — propagated from backend.
/// - HY008: Operation canceled — (driver-manager-handled; not returned here).
/// - HY010: Function sequence error — (DM cases for async/NEED_DATA: driver-manager-handled; not
///   returned here); fails if no SQL has been prepared (checked here); fails if
///   the connection is not open (checked here).
/// - HY013: Memory management error — propagated from backend.
/// - HY090: Invalid string or buffer length — propagated from backend (parameter buffer length
///   validation).
/// - HY105: Invalid parameter type — propagated from backend.
/// - HY109: Invalid cursor position — propagated from backend.
/// - HY117: Connection suspended — (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented — propagated from backend.
/// - HYT00: Timeout expired — propagated from backend.
/// - HYT01: Connection timeout expired — propagated from backend.
/// - IM001: Driver does not support this function — (driver-manager-handled; not returned here).
/// - IM017: Polling disabled in async notification mode — (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called — (driver-manager-handled; not returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_execute<B: Backend>(statement_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLExecute(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; tag validated inside
    // panic_safe/as_handle_ref before any dereference occurs. Bound parameter buffer pointers
    // in stmt.param_bindings are validated when they were registered via SQLBindParameter;
    // collect_params reads them under the caller's guarantee that they remain valid.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, || {
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            let param_count = match stmt.param_count {
                Some(n) => n,
                None => {
                    return Err(OdbcError::general(
                        "No SQL has been prepared (call SQLPrepare first)",
                        SqlState::function_sequence_error(),
                    ));
                }
            };

            // Check for data-at-execution parameters.
            // SAFETY: caller guarantees all bound buffer pointers remain valid.
            let (non_dae_values, dae_params) =
                crate::ffi::params::find_data_at_exec_params(&stmt.param_bindings, param_count);

            if !dae_params.is_empty() {
                // Store DAE state and return SQL_NEED_DATA.
                let sql = stmt.prepared_sql.clone().ok_or_else(|| {
                    OdbcError::general(
                        "No prepared statement (call SQLPrepare first)",
                        SqlState::function_sequence_error(),
                    )
                })?;
                stmt.data_at_exec = Some(crate::handles::DataAtExecState {
                    pending_params: dae_params.into(),
                    current_param: None,
                    buffer: Vec::new(),
                    collected_values: non_dae_values,
                    sql,
                });
                return Ok(SqlReturn::NEED_DATA);
            }

            // No DAE params: collect all values normally and execute immediately.
            // SAFETY: caller guarantees all bound buffer pointers remain valid.
            let params = crate::ffi::params::collect_params(&stmt.param_bindings, param_count)?;

            let conn_ptr = stmt.conn;
            let conn = as_handle_ref::<ConnectionHandle<B>>(conn_ptr)?;
            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // Re-acquire stmt (different allocation from conn, no aliasing).
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;

            // `SQLFreeStmt(SQL_CLOSE)` clears `stmt.statement` (discards the result set)
            // but the prepared SQL survives in `stmt.prepared_sql`. Per ODBC spec, calling
            // `SQLExecute` after `SQL_CLOSE` re-executes the same prepared statement.
            if stmt.statement.is_none() {
                let sql = stmt.prepared_sql.as_deref().ok_or_else(|| {
                    OdbcError::general(
                        "No prepared statement (call SQLPrepare first)",
                        SqlState::function_sequence_error(),
                    )
                })?;
                let prepared = B::prepare(connection, sql).into_odbc()?;
                let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
                stmt.set_prepared_statement(crate::handles::StatementData::Backend(prepared));
            }

            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            let stmt_data = stmt.statement.as_mut().ok_or_else(|| {
                OdbcError::general(
                    "No prepared statement (call SQLPrepare first)",
                    SqlState::function_sequence_error(),
                )
            })?;

            let outcome = match stmt_data {
                crate::handles::StatementData::Backend(backend_stmt) => {
                    B::execute(connection, backend_stmt, &params).into_odbc()?
                }
                crate::handles::StatementData::Synthetic(_) => {
                    return Err(OdbcError::general(
                        "Cannot re-execute a synthetic statement",
                        SqlState::general_error(),
                    ));
                }
            };

            // The `stmt_data` mutable borrow has ended; re-acquire the statement
            // to write any OUTPUT / INOUT parameter values back into the bound
            // buffers, the symmetric counterpart of collecting the input params.
            let stmt = as_handle_ref::<StatementHandle<B>>(statement_handle)?;
            // A cursor is open only if the execution produced columns; an
            // `UPDATE` leaves the statement in S4, not S5.
            stmt.cursor_open = stmt
                .statement
                .as_ref()
                .is_some_and(|s| s.column_count() > 0);
            // SAFETY: the application's bound output buffer pointers remain valid
            // per the caller contract (same guarantee collect_params relies on).
            // Already inside the enclosing `unsafe` context, like collect_params above.
            crate::ffi::params::write_output_params(&stmt.param_bindings, &outcome.output_params)?;

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLExecute -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::MockBackend;
    use odbc_sys::HandleType;

    /// Helper: allocate env + connection + statement handles.
    unsafe fn alloc_env_conn_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let _ = unsafe {
            sql_alloc_handle::<MockBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env)
        };
        let mut conn: *mut c_void = std::ptr::null_mut();
        let _ = unsafe { sql_alloc_handle::<MockBackend>(HandleType::Dbc as i16, env, &mut conn) };
        let mut stmt: *mut c_void = std::ptr::null_mut();
        let _ =
            unsafe { sql_alloc_handle::<MockBackend>(HandleType::Stmt as i16, conn, &mut stmt) };
        (env, conn, stmt)
    }

    /// Helper: connect a handle using a valid connection string.
    unsafe fn connect_handle(conn: *mut c_void) -> SqlReturn {
        let input = "Host=localhost;Port=8080;Database=test;User=me";
        let wide: Vec<u16> = input.encode_utf16().collect();
        unsafe {
            crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            )
        }
    }

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn exec_direct_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Not connected — should fail with HY010.
            let sql = "SELECT 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            let ret = sql_exec_direct_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn exec_direct_null_text_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ret = sql_exec_direct_w::<MockBackend>(stmt, std::ptr::null(), 0);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn exec_direct_invalid_length_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let sql = "SELECT 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            // -5 is not SQL_NTS (-3) and not >= 0
            let ret = sql_exec_direct_w::<MockBackend>(stmt, wide.as_ptr(), -5);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn exec_direct_mock_backend_returns_error() {
        // MockBackend::exec_direct returns Err(MockError), so this should fail.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let sql = "SELECT 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            let ret = sql_exec_direct_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            // MockBackend::exec_direct returns Err, so we get ERROR
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn prepare_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Not connected — should fail.
            let sql = "SELECT ?";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            let ret = sql_prepare_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn prepare_null_text_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);
            let ret = sql_prepare_w::<MockBackend>(stmt, std::ptr::null(), 0);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn prepare_stores_sql_and_param_count() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let sql = "SELECT * FROM t WHERE id = ? AND name = ?";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            let ret = sql_prepare_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let stmt_handle =
                as_handle_ref::<crate::handles::StatementHandle<MockBackend>>(stmt).unwrap();
            assert_eq!(stmt_handle.prepared_sql.as_deref(), Some(sql));
            assert_eq!(stmt_handle.param_count, Some(2));

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn execute_without_prepare_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);
            // No SQLPrepare called
            let ret = sql_execute::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn prepare_then_execute_succeeds() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let sql = "SELECT 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            let ret = sql_prepare_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ret = sql_execute::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS);

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn noscan_attr_round_trips_and_gates() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let h = as_handle_ref::<crate::handles::StatementHandle<MockBackend>>(stmt).unwrap();
            assert!(!h.noscan_enabled());

            // SQL_ATTR_NOSCAN = SQL_NOSCAN_ON (1); SQLSetStmtAttrW takes integer-valued
            // attributes through the pointer parameter itself (ODBC convention), so this
            // encodes the value rather than pointing at real memory, not an actual
            // dangling pointer, so the clippy suggestion doesn't apply here.
            #[allow(clippy::manual_dangling_ptr)]
            let noscan_on = 1usize as *mut c_void;
            let ret = crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                odbc_sys::StatementAttribute::NoScan as i32,
                noscan_on,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let h = as_handle_ref::<crate::handles::StatementHandle<MockBackend>>(stmt).unwrap();
            assert!(h.noscan_enabled());

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn prepare_keeps_previous_param_bindings() {
        // SQLBindParameter's spec names the only three things that unbind a
        // parameter: another SQLBindParameter, SQLFreeStmt(SQL_RESET_PARAMS),
        // and SQLSetDescField setting the APD's SQL_DESC_COUNT to 0. SQLPrepare
        // is not among them, and SQLPrepare's own Comments tell the application
        // to "unbind all parameters that applied to an old SQL statement before
        // preparing a new SQL statement" -- advice only a driver that keeps them
        // could need.
        //
        // Clearing them here would break the ordinary
        // bind -> prepare -> execute order silently: `collect_params`
        // substitutes ColumnValue::Null for every unbound slot, so the
        // statement would run with all-NULL parameters and return the wrong
        // rows with SQL_SUCCESS and no diagnostic.
        unsafe {
            use crate::ffi::params::sql_bind_parameter;
            use crate::types::{CDataType, ParamType, SqlDataType};

            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let sql1 = "SELECT ? + 1";
            let wide: Vec<u16> = sql1.encode_utf16().collect();
            let _ret = sql_prepare_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32);

            let mut v: i32 = 5;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SqlDataType::INTEGER.0,
                10,
                0,
                &mut v as *mut i32 as *mut c_void,
                std::mem::size_of::<i32>() as isize,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let sql2 = "SELECT 1";
            let wide2: Vec<u16> = sql2.encode_utf16().collect();
            let _ret = sql_prepare_w::<MockBackend>(stmt, wide2.as_ptr(), wide2.len() as i32);

            let stmt_handle =
                as_handle_ref::<crate::handles::StatementHandle<MockBackend>>(stmt).unwrap();
            assert!(
                stmt_handle.param_bindings.contains_key(&1),
                "SQLPrepare unbound a parameter, which only SQLBindParameter, \
                 SQLFreeStmt(SQL_RESET_PARAMS) or SQLSetDescField may do"
            );
            // The new statement has no markers, so nothing reads the surviving
            // binding: `collect_params` walks 1..=param_count.
            assert_eq!(stmt_handle.param_count, Some(0));

            cleanup(env, conn, stmt);
        }
    }
    #[test]
    fn execute_without_a_result_set_leaves_the_cursor_closed() {
        // `MockStatement` reports zero columns, so this stands for an UPDATE:
        // ODBC state S4 (executed, no cursor), not S5. The statement survives
        // for SQLRowCount, but no cursor is open, so SQLCloseCursor must say so
        // with 24000 rather than reporting success.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let sql = "UPDATE t SET a = 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            assert_eq!(
                sql_prepare_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS
            );
            assert_eq!(sql_execute::<MockBackend>(stmt), SqlReturn::SUCCESS);

            let handle =
                as_handle_ref::<crate::handles::StatementHandle<MockBackend>>(stmt).expect("valid");
            assert!(
                handle.statement.is_some(),
                "the executed statement was dropped"
            );
            assert!(
                !handle.cursor_open,
                "an execution that produced no result set opened a cursor"
            );

            assert_eq!(
                crate::ffi::cursor::sql_close_cursor::<MockBackend>(stmt),
                SqlReturn::ERROR,
                "SQLCloseCursor reported success with no cursor open"
            );

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn prepare_does_not_open_a_cursor() {
        // SQLPrepare stores a backend statement (state S2) but opens no cursor,
        // so SQLExecDirect on the same handle must not be refused with 24000.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let sql = "SELECT 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            assert_eq!(
                sql_prepare_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS
            );

            let handle =
                as_handle_ref::<crate::handles::StatementHandle<MockBackend>>(stmt).expect("valid");
            assert!(handle.statement.is_some());
            assert!(!handle.cursor_open, "SQLPrepare opened a cursor");

            cleanup(env, conn, stmt);
        }
    }
}
