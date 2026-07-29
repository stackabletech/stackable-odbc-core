//! Statement execution: `SQLExecDirectW`, `SQLPrepareW`, `SQLExecute`.

use std::ffi::c_void;

use crate::backend::{Backend, StatementBackend};
use crate::errors::OdbcError;
use crate::handles::StatementHandle;
use crate::panic::panic_safe;
use crate::types::{SQL_NTS, SqlReturn, SqlState};
use crate::utf16::utf16_to_string;

/// Report this execution's one parameter set through the application's
/// `SQL_ATTR_PARAMS_PROCESSED_PTR` and `SQL_ATTR_PARAM_STATUS_PTR`.
///
/// # Safety
///
/// See [`crate::ffi::params::report_params_processed`].
unsafe fn report_param_set<B: Backend>(stmt: &StatementHandle<B>, succeeded: bool) {
    let status = if succeeded {
        crate::types::SQL_PARAM_SUCCESS
    } else {
        crate::types::SQL_PARAM_ERROR
    };
    // SAFETY: the caller's contract, forwarded.
    unsafe { crate::ffi::params::report_params_processed(stmt, status) };
}

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
/// - 07002: COUNT field incorrect — the row's first clause, "the number of parameters
///   specified in `SQLBindParameter` was less than the number of parameters in the SQL
///   statement", is **returned here**: a `?` marker with no binding is rejected rather than
///   padded with NULL (`ffi::params::collect_params`). The second clause, a binding whose
///   `ParameterValuePtr` is null with a non-`SQL_NULL_DATA`/`SQL_DATA_AT_EXEC` indicator, is
///   rejected by `SQLBindParameter` itself. Also propagated from backend.
/// - 07006: Restricted data type attribute violation — `SQLBindParameter` refuses a
///   `SQL_C_BINARY` parameter bound to a target core cannot convert it to, so it does not
///   reach execution (`crate::binary_convert`). Also propagated from backend.
/// - 07007: Restricted parameter value violation — propagated from backend.
/// - 07S01: Invalid use of default parameter — propagated from backend.
/// - 08S01: Communication link failure — propagated from backend.
/// - 21S01: Insert value list does not match column list — propagated from backend.
/// - 21S02: Degree of derived table does not match column list — propagated from backend.
/// - 22001: String data, right truncation — returned here when a parameter value does not
///   fit the SQL type the application declared: fractional digits dropped or whole digits
///   lost for an exact-numeric target, or a value longer than the declared `ColumnSize` for
///   a character or binary target (`crate::param_convert`, the "C to SQL: Character" table,
///   and its "C to SQL: Binary" counterpart for a `SQL_C_BINARY` value bound to a binary
///   type). Also propagated from backend.
/// - 22002: Indicator variable required but not supplied — propagated from backend.
/// - 22003: Numeric value out of range — returned here when character parameter data falls
///   outside the range of the declared approximate-numeric or `SQL_BIT` type
///   (`crate::param_convert`), or when a `SQL_C_BINARY` parameter's byte count is not
///   exactly the declared SQL type's width (`crate::binary_convert`, the "C to SQL: Binary"
///   table). Also propagated from backend.
/// - 22007: Invalid datetime format — returned here for character parameter data that is a
///   datetime literal with an out-of-range field (`crate::param_convert`). Also propagated
///   from backend.
/// - 22008: Datetime field overflow — returned here when character parameter data carries a
///   datetime component the declared type cannot hold: a non-zero time for `SQL_TYPE_DATE`,
///   or non-zero fractional seconds for `SQL_TYPE_TIME` (`crate::param_convert`). Also
///   propagated from backend.
/// - 22012: Division by zero — propagated from backend.
/// - 22015: Interval field overflow — propagated from backend.
/// - 22018: Invalid character value for cast specification — returned here when character
///   parameter data is not a valid literal of the SQL type declared for it at
///   `SQLBindParameter` (`crate::param_convert`). Also propagated from backend.
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
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
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
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure. statement_text is checked
    // for null before use, and is then valid for text_length UTF-16 code units (or
    // null-terminated if text_length == SQL_NTS/-3); caller upholds this per the
    // function's safety contract.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
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

            // Spec HY010: Connection must be open.
            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            let sql = utf16_to_string(statement_text, text_length)?;
            // Needed whether or not escapes are translated: `count_params` uses
            // the dialect's identifier delimiters to tell a `?` inside a
            // quoted identifier from a parameter marker.
            let dialect = B::escape_dialect(connection);
            // Spec: SQL_ATTR_NOSCAN=SQL_NOSCAN_ON disables escape-sequence
            // scanning; otherwise translate `{fn}`/`{d}`/`{t}`/`{ts}`/`{oj}`/
            // `{escape}` (and reject `{call}` with HYC00) before it reaches
            // the backend.
            let sql = if noscan {
                sql
            } else {
                crate::escape::translate_escapes(&sql, &dialect)?
            };

            // Check for data-at-execution parameters.
            let param_count = crate::ffi::params::count_params(&sql, &dialect);
            if param_count > 0 {
                // SAFETY: caller guarantees all bound buffer pointers remain valid.
                let (non_dae_values, dae_params) = crate::ffi::params::find_data_at_exec_params(
                    stmt.param_records(),
                    param_count,
                )?;

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

            // This execution's own token, replacing whatever the previous one
            // left behind (see `mint_cancel_token`). `SQLCancel` signals it
            // from another thread; the error paths below ask it, so that a
            // cancellation is reported as HY008 rather than as whatever
            // symptom the backend's client library happened to see.
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // `Backend::exec_direct` takes no parameters, so a parameterised
            // statement must go through prepare + execute, which binds them.
            // Routing it to exec_direct would send the literal `?` to the
            // backend and silently discard every bound value.
            let result = if param_count > 0 {
                // SAFETY: caller guarantees all bound buffer pointers remain valid.
                let params = crate::ffi::params::collect_params(stmt.param_records(), param_count)?;
                let mut prepared =
                    timer.check::<B, _, _>(B::prepare(connection, cancel, &sql), cancel)?;
                let executed = timer.check::<B, _, _>(
                    B::execute(connection, cancel, &mut prepared, &params),
                    cancel,
                );
                // Reported before the error is propagated, so a set that failed
                // is reported as SQL_PARAM_ERROR rather than left unwritten.
                // SAFETY: the application's parameter-status pointers remain
                // valid per the `SQLSetStmtAttr` contract.
                report_param_set(stmt, executed.is_ok());
                let outcome = executed?;
                // SAFETY: the application's bound output buffer pointers remain
                // valid per the caller contract (same guarantee collect_params relies on).
                // Already inside the enclosing `unsafe` context, like collect_params above.
                crate::ffi::params::write_output_params(
                    stmt.param_records(),
                    &outcome.output_params,
                )?;
                prepared
            } else {
                let executed =
                    timer.check::<B, _, _>(B::exec_direct(connection, cancel, &sql), cancel);
                // SAFETY: as above.
                report_param_set(stmt, executed.is_ok());
                executed?
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
///   `SQLPrepareW` reads no parameter values, so the character-to-SQL-type conversion that
///   raises this in `SQLExecute` and `SQLExecDirectW` does not run here.
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
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
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
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure. statement_text is checked
    // for null before use, and is then valid for text_length UTF-16 code units (or
    // null-terminated if text_length == SQL_NTS/-3); caller upholds this per the
    // function's safety contract.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
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

            // Resolved before translating, because the escape dialect is a
            // property of the connection.
            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // Needed whether or not escapes are translated: `count_params` uses
            // the dialect's identifier delimiters to tell a `?` inside a
            // quoted identifier from a parameter marker.
            let dialect = B::escape_dialect(connection);
            // Spec: SQL_ATTR_NOSCAN=SQL_NOSCAN_ON disables escape-sequence
            // scanning; otherwise translate before counting `?` markers:
            // escapes never contain them, so translation cannot move a
            // marker's position.
            let sql = if noscan {
                sql
            } else {
                crate::escape::translate_escapes(&sql, &dialect)?
            };
            let param_count = crate::ffi::params::count_params(&sql, &dialect);

            // This execution's own token, replacing whatever the previous one
            // left behind (see `mint_cancel_token`). `SQLCancel` signals it
            // from another thread; the error paths below ask it, so that a
            // cancellation is reported as HY008 rather than as whatever
            // symptom the backend's client library happened to see.
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

            // Ask backend to validate and prepare the statement.
            let prepared = timer.check::<B, _, _>(B::prepare(connection, cancel, &sql), cancel)?;

            // Prepared, not executed (S2/S3): no cursor is open, and a
            // re-prepare closes any cursor the previous execution left open.
            stmt.set_prepared_statement(crate::handles::StatementData::Backend(prepared));
            stmt.prepared_sql = Some(sql);
            stmt.param_count = Some(param_count);
            // Parameter bindings deliberately survive. SQLBindParameter's spec
            // names the only three things that unbind a parameter — another
            // SQLBindParameter, SQLFreeStmt(SQL_RESET_PARAMS), and
            // SQLSetDescField setting the APD's SQL_DESC_COUNT to 0 — and
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
/// - 07002: COUNT field incorrect — the row's first clause, "the number of parameters
///   specified in `SQLBindParameter` was less than the number of parameters in the SQL
///   statement", is **returned here**: a `?` marker with no binding is rejected rather than
///   padded with NULL (`ffi::params::collect_params`). The second clause, a binding whose
///   `ParameterValuePtr` is null with a non-`SQL_NULL_DATA`/`SQL_DATA_AT_EXEC` indicator, is
///   rejected by `SQLBindParameter` itself. Also propagated from backend.
/// - 07006: Restricted data type attribute violation — `SQLBindParameter` refuses a
///   `SQL_C_BINARY` parameter bound to a target core cannot convert it to, so it does not
///   reach execution (`crate::binary_convert`). Also propagated from backend.
/// - 07007: Restricted parameter value violation — propagated from backend.
/// - 07S01: Invalid use of default parameter — propagated from backend.
/// - 08S01: Communication link failure — propagated from backend.
/// - 21S02: Degree of derived table does not match column list — propagated from backend.
/// - 22001: String data, right truncation — returned here when a parameter value does not
///   fit the SQL type the application declared: fractional digits dropped or whole digits
///   lost for an exact-numeric target, or a value longer than the declared `ColumnSize` for
///   a character or binary target (`crate::param_convert`, the "C to SQL: Character" table,
///   and its "C to SQL: Binary" counterpart for a `SQL_C_BINARY` value bound to a binary
///   type). Also propagated from backend.
/// - 22002: Indicator variable required but not supplied — propagated from backend.
/// - 22003: Numeric value out of range — returned here when character parameter data falls
///   outside the range of the declared approximate-numeric or `SQL_BIT` type
///   (`crate::param_convert`), or when a `SQL_C_BINARY` parameter's byte count is not
///   exactly the declared SQL type's width (`crate::binary_convert`, the "C to SQL: Binary"
///   table). Also propagated from backend.
/// - 22007: Invalid datetime format — returned here for character parameter data that is a
///   datetime literal with an out-of-range field (`crate::param_convert`). Also propagated
///   from backend.
/// - 22008: Datetime field overflow — returned here when character parameter data carries a
///   datetime component the declared type cannot hold: a non-zero time for `SQL_TYPE_DATE`,
///   or non-zero fractional seconds for `SQL_TYPE_TIME` (`crate::param_convert`). Also
///   propagated from backend.
/// - 22012: Division by zero — propagated from backend.
/// - 22015: Interval field overflow — propagated from backend.
/// - 22018: Invalid character value for cast specification — returned here when character
///   parameter data is not a valid literal of the SQL type declared for it at
///   `SQLBindParameter` (`crate::param_convert`). Also propagated from backend.
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
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
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
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure. Bound parameter buffer
    // pointers in the APD are validated when they were registered via
    // SQLBindParameter; collect_params reads them under the caller's guarantee that
    // they remain valid.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
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
                crate::ffi::params::find_data_at_exec_params(stmt.param_records(), param_count)?;

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
            let params = crate::ffi::params::collect_params(stmt.param_records(), param_count)?;

            // Manual-commit mode: this call opens a transaction (or extends
            // the open one), which is what SQL_ATTR_TXN_ISOLATION's HY011 is
            // about. Recorded before the backend call, not after it succeeds:
            // a call that fails partway may still have opened one.
            conn.note_work_started();

            let Some(ref connection) = conn.connection else {
                return Err(OdbcError::general(
                    "Connection is not open",
                    SqlState::function_sequence_error(),
                ));
            };

            // This execution's own token, replacing whatever the previous one
            // left behind (see `mint_cancel_token`). `SQLCancel` signals it
            // from another thread; the error paths below ask it, so that a
            // cancellation is reported as HY008 rather than as whatever
            // symptom the backend's client library happened to see.
            let cancel_token = crate::handles::mint_cancel_token::<B>(statement_handle, connection);
            let cancel = crate::handles::cancel_as::<B>(&cancel_token)?;
            // Core-enforced deadline, if the backend asked core to own one.
            // Disarmed by `Drop` the moment this scope ends, so a fast call
            // leaves no thread behind.
            let timer =
                crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, &cancel_token);

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
                let prepared =
                    timer.check::<B, _, _>(B::prepare(connection, cancel, sql), cancel)?;
                stmt.set_prepared_statement(crate::handles::StatementData::Backend(prepared));
            }

            let stmt_data = stmt.statement.as_mut().ok_or_else(|| {
                OdbcError::general(
                    "No prepared statement (call SQLPrepare first)",
                    SqlState::function_sequence_error(),
                )
            })?;

            let executed = match stmt_data {
                crate::handles::StatementData::Backend(backend_stmt) => timer.check::<B, _, _>(
                    B::execute(connection, cancel, backend_stmt, &params),
                    cancel,
                ),
                crate::handles::StatementData::Synthetic(_) => {
                    return Err(OdbcError::general(
                        "Cannot re-execute a synthetic statement",
                        SqlState::general_error(),
                    ));
                }
            };
            // SAFETY: the application's parameter-status pointers remain valid
            // per the `SQLSetStmtAttr` contract.
            report_param_set(stmt, executed.is_ok());
            let outcome = executed?;

            // The `stmt_data` mutable borrow has ended; write any OUTPUT / INOUT
            // parameter values back into the bound buffers, the symmetric
            // counterpart of collecting the input params.
            // A cursor is open only if the execution produced columns; an
            // `UPDATE` leaves the statement in S4, not S5.
            stmt.cursor_open = stmt
                .statement
                .as_ref()
                .is_some_and(|s| s.column_count() > 0);
            // SAFETY: the application's bound output buffer pointers remain valid
            // per the caller contract (same guarantee collect_params relies on).
            // Already inside the enclosing `unsafe` context, like collect_params above.
            crate::ffi::params::write_output_params(stmt.param_records(), &outcome.output_params)?;

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLExecute -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::handle::sql_free_handle;
    use crate::handles::ConnectionHandle;
    use crate::handles::StatementHandle;
    use crate::test_utils::{
        MockBackend, MockBlockingBackend, MockCancelAwareBackend, MockConnection,
        MockCoreCancelsTimeoutBackend, MockRecordingBackend, alloc_env_conn_stmt, with_handle,
    };
    use odbc_sys::HandleType;

    /// As [`alloc_env_conn_stmt`], but generic over `B`. Needed for
    /// [`MockRecordingBackend`], which is not `MockBackend`.
    unsafe fn alloc_env_conn_stmt_for<B: Backend>() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = crate::ffi::handle::sql_alloc_handle::<B>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ =
                crate::ffi::handle::sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ =
                crate::ffi::handle::sql_alloc_handle::<B>(HandleType::Stmt as i16, conn, &mut stmt);
            (env, conn, stmt)
        }
    }

    /// As `cleanup`, but generic over `B`, for `alloc_env_conn_stmt_for`.
    ///
    /// Disconnects before freeing the connection, exactly as `cleanup` below
    /// does: `free_connection` refuses HY010 while `conn.connection.is_some()`
    /// (spec-correct — `SQLDisconnect` must run first), so a caller that sets
    /// `connection` directly (as the cancel-token tests do, via
    /// `with_handle`) and skips this leaks the connection box, and then the
    /// environment box behind it, since `free_environment` also correctly
    /// refuses while it still has a live child.
    unsafe fn cleanup_env_conn_stmt_for<B: Backend>(
        env: *mut c_void,
        conn: *mut c_void,
        stmt: *mut c_void,
    ) {
        unsafe {
            let _ = sql_free_handle::<B>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
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

            with_handle::<MockBackend, crate::handles::StatementHandle<MockBackend>, _>(
                stmt,
                |stmt_handle| {
                    assert_eq!(stmt_handle.prepared_sql.as_deref(), Some(sql));
                    assert_eq!(stmt_handle.param_count, Some(2));
                },
            );

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
            with_handle::<MockBackend, crate::handles::StatementHandle<MockBackend>, _>(
                stmt,
                |h| assert!(!h.noscan_enabled()),
            );

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

            with_handle::<MockBackend, crate::handles::StatementHandle<MockBackend>, _>(
                stmt,
                |h| assert!(h.noscan_enabled()),
            );

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
        // preparing a new SQL statement" — advice only a driver that keeps them
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

            with_handle::<MockBackend, crate::handles::StatementHandle<MockBackend>, _>(
                stmt,
                |stmt_handle| {
                    assert!(
                        stmt_handle.app_param_desc.records.contains_key(&1)
                            && stmt_handle.imp_param_desc.records.contains_key(&1),
                        "SQLPrepare unbound a parameter, which only SQLBindParameter, \
                         SQLFreeStmt(SQL_RESET_PARAMS) or SQLSetDescField may do"
                    );
                    // The new statement has no markers, so nothing reads the surviving
                    // binding: `collect_params` walks 1..=param_count.
                    assert_eq!(stmt_handle.param_count, Some(0));
                },
            );

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

            with_handle::<MockBackend, crate::handles::StatementHandle<MockBackend>, _>(
                stmt,
                |handle| {
                    assert!(
                        handle.statement.is_some(),
                        "the executed statement was dropped"
                    );
                    assert!(
                        !handle.cursor_open,
                        "an execution that produced no result set opened a cursor"
                    );
                },
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

            with_handle::<MockBackend, crate::handles::StatementHandle<MockBackend>, _>(
                stmt,
                |handle| {
                    assert!(handle.statement.is_some());
                    assert!(!handle.cursor_open, "SQLPrepare opened a cursor");
                },
            );

            cleanup(env, conn, stmt);
        }
    }

    /// `SQLFreeStmt(SQL_CLOSE)` discards the backend statement but keeps
    /// `prepared_sql`; per spec a subsequent `SQLExecute` re-executes the same
    /// prepared SQL rather than failing. This is the one path through
    /// `sql_execute` that reaches the `stmt.statement.is_none()` branch:
    /// `MockBackend::prepare` always succeeds, so `stmt.statement` is `Some`
    /// for the entire life of an ordinarily-used handle, and no other test
    /// exercises `B::prepare` being called a second time against the same
    /// statement.
    #[test]
    fn execute_after_close_reprepares_the_statement() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(connect_handle(conn), SqlReturn::SUCCESS);

            let sql = "SELECT 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            assert_eq!(
                sql_prepare_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS
            );
            assert_eq!(sql_execute::<MockBackend>(stmt), SqlReturn::SUCCESS);

            assert_eq!(
                crate::ffi::handle::sql_free_stmt::<MockBackend>(
                    stmt,
                    odbc_sys::FreeStmtOption::Close as u16,
                ),
                SqlReturn::SUCCESS
            );
            with_handle::<MockBackend, crate::handles::StatementHandle<MockBackend>, _>(
                stmt,
                |handle| {
                    assert!(
                        handle.statement.is_none(),
                        "SQL_CLOSE must discard the backend statement"
                    );
                    assert_eq!(
                        handle.prepared_sql.as_deref(),
                        Some(sql),
                        "SQL_CLOSE must not forget the prepared SQL"
                    );
                },
            );

            assert_eq!(
                sql_execute::<MockBackend>(stmt),
                SqlReturn::SUCCESS,
                "SQLExecute after SQL_CLOSE must re-prepare the statement, not fail"
            );
            with_handle::<MockBackend, crate::handles::StatementHandle<MockBackend>, _>(
                stmt,
                |handle| {
                    assert!(
                        handle.statement.is_some(),
                        "the re-prepare must leave a live backend statement"
                    );
                },
            );

            cleanup(env, conn, stmt);
        }
    }

    /// Spec, `SQL_ATTR_PARAMS_PROCESSED_PTR`: the driver returns the number of
    /// sets of parameters that have been processed, and
    /// `SQL_ATTR_PARAM_STATUS_PTR` holds one status per set.
    /// `SQL_ATTR_PARAMSET_SIZE` is pinned at 1, so an execution processes
    /// exactly one set and reports `SQL_PARAM_SUCCESS` for it — the
    /// parameter-side counterpart of what `SQLFetch` writes through
    /// `SQL_ATTR_ROWS_FETCHED_PTR`.
    #[test]
    fn an_execution_reports_its_parameter_set_as_processed() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let mut processed: usize = usize::MAX;
            let mut status: u16 = u16::MAX;
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockRecordingBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::ParamsProcessedPtr as i32,
                    std::ptr::from_mut(&mut processed).cast(),
                    0,
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockRecordingBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::ParamStatusPtr as i32,
                    std::ptr::from_mut(&mut status).cast(),
                    0,
                ),
                SqlReturn::SUCCESS
            );

            let wide: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                sql_exec_direct_w::<MockRecordingBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS
            );

            assert_eq!(
                processed, 1,
                "SQL_ATTR_PARAMS_PROCESSED_PTR was not written"
            );
            assert_eq!(
                status,
                crate::types::SQL_PARAM_SUCCESS,
                "SQL_ATTR_PARAM_STATUS_PTR was not written"
            );

            cleanup_env_conn_stmt_for::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// The other half: a set whose execution failed is reported as
    /// `SQL_PARAM_ERROR`, so an application reading the status array sees the
    /// failure rather than an untouched buffer.
    #[test]
    fn a_failed_execution_reports_its_parameter_set_as_errored() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockBackend>();
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |c| {
                c.connection = Some(MockConnection);
            });

            let mut processed: usize = usize::MAX;
            let mut status: u16 = u16::MAX;
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::ParamsProcessedPtr as i32,
                    std::ptr::from_mut(&mut processed).cast(),
                    0,
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::ParamStatusPtr as i32,
                    std::ptr::from_mut(&mut status).cast(),
                    0,
                ),
                SqlReturn::SUCCESS
            );

            let wide: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                sql_exec_direct_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::ERROR
            );

            assert_eq!(processed, 1);
            assert_eq!(status, crate::types::SQL_PARAM_ERROR);

            cleanup_env_conn_stmt_for::<MockBackend>(env, conn, stmt);
        }
    }

    /// A backend that cancels by query id needs the token *before* the work
    /// starts, so it can record what to cancel. Passing it to the
    /// statement-producing methods is what makes that possible; a token
    /// handed back afterwards would arrive after the window it exists for.
    #[test]
    fn a_statement_producing_call_receives_the_statements_cancel_token() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let sql = "SELECT 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            let ret =
                sql_exec_direct_w::<MockRecordingBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::SUCCESS);

            let token = crate::handles::registry::registry()
                .cancel_of(stmt)
                .expect("token stored");
            let token = token
                .downcast_ref::<crate::test_utils::MockCancelToken>()
                .expect("backend's own type");
            assert!(
                token
                    .saw_execution
                    .load(std::sync::atomic::Ordering::SeqCst),
                "exec_direct must receive the same token the statement carries"
            );

            cleanup_env_conn_stmt_for::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// Mint-per-execution, pinned at the FFI entry point rather than against
    /// the internal helper directly: a second `SQLExecDirectW` on the same
    /// handle gets its own token, so a `SQLCancel` aimed at the first
    /// execution cannot reach into the second.
    ///
    /// This test asserted the opposite until M4, on the reasoning that a stale
    /// token would leave `SQLCancel` signalling a finished execution while the
    /// real one ran uncancelled. The spec makes that outcome correct rather
    /// than a bug: "In ODBC 3.5, a call to SQLCancel when no processing is
    /// being done on the statement ... has is [sic] no effect at all."
    /// Cancelling a run that already finished is *required* to do nothing;
    /// killing the unrelated run that replaced it is not. Create-once also
    /// broke the spec's "After the statement has been canceled, the
    /// application can call SQLExecute or SQLExecDirect again", because the
    /// reused token stayed signalled forever.
    #[test]
    fn a_second_execution_on_the_same_statement_mints_a_fresh_token() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let sql = "SELECT 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();

            assert_eq!(
                sql_exec_direct_w::<MockRecordingBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS
            );
            let first = crate::handles::registry::registry()
                .cancel_of(stmt)
                .expect("token stored after first execution");

            assert_eq!(
                sql_exec_direct_w::<MockRecordingBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS
            );
            let second = crate::handles::registry::registry()
                .cancel_of(stmt)
                .expect("token still stored after second execution");

            assert!(
                !std::sync::Arc::ptr_eq(&first, &second),
                "each execution owns its own cancel token, so a cancel aimed at one execution \
                 cannot leak into the next — which is what makes a cancelled statement reusable"
            );

            cleanup_env_conn_stmt_for::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// The end-to-end shape this crate's concurrency work exists for: thread A
    /// is inside the backend holding the connection's group lock, thread B
    /// calls `SQLCancel`, and A returns `HY008`.
    ///
    /// Spec, `SQLCancel`: "In a multithread application, the application can
    /// cancel a function that is running on another thread. … If the original
    /// function is canceled, it returns SQL_ERROR and SQLSTATE HY008 (Operation
    /// canceled)."
    ///
    /// Every other `HY008` test simulates the interleaving with a flag on one
    /// thread, which pins the reclassification but not the lock behaviour. This
    /// one needs the real thing: `SQLCancel` must take its `try_lock`-failed
    /// branch, because thread A genuinely holds the group.
    #[test]
    fn a_cancel_from_another_thread_makes_the_running_call_return_hy008() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockBlockingBackend>();
            with_handle::<MockBlockingBackend, ConnectionHandle<MockBlockingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            // A raw pointer is not `Send`; the token is just an integer, so it
            // travels as one and is rebuilt on the other side.
            let stmt_addr = stmt as usize;
            let executor = std::thread::spawn(move || {
                let stmt = stmt_addr as *mut c_void;
                let wide: Vec<u16> = "SELECT 1".encode_utf16().collect();
                sql_exec_direct_w::<MockBlockingBackend>(stmt, wide.as_ptr(), wide.len() as i32)
            });

            MockBlockingBackend::wait_until_started();
            assert_eq!(
                crate::ffi::cursor::sql_cancel::<MockBlockingBackend>(stmt),
                SqlReturn::SUCCESS,
                "SQLCancel must not block on the group the executing thread holds",
            );

            let ret = executor.join().expect("executor thread did not panic");
            assert_eq!(ret, SqlReturn::ERROR);

            with_handle::<MockBlockingBackend, StatementHandle<MockBlockingBackend>, _>(
                stmt,
                |h| {
                    let rec = h.diagnostics.get(0).expect("record 1 exists");
                    assert_eq!(
                        rec.sqlstate.as_str(),
                        "HY008",
                        "the cancelled call owes HY008; a different state means the wait timed \
                         out and the backend failed for its own reasons"
                    );
                },
            );
            cleanup_env_conn_stmt_for::<MockBlockingBackend>(env, conn, stmt);
        }
    }

    /// Env + connection + statement for [`MockCancelAwareBackend`], with the
    /// connection populated directly. `MockCancelAwareBackend::connect`
    /// succeeds, but going through `SQLDriverConnectW` would leave the DSN
    /// machinery in the picture for tests that are about cancellation.
    unsafe fn alloc_cancel_aware_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockCancelAwareBackend>();
            with_handle::<MockCancelAwareBackend, ConnectionHandle<MockCancelAwareBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );
            (env, conn, stmt)
        }
    }

    /// The SQLSTATE of the statement handle's first diagnostic record.
    fn first_sqlstate(stmt: *mut c_void) -> String {
        let mut state = String::new();
        with_handle::<MockCancelAwareBackend, StatementHandle<MockCancelAwareBackend>, _>(
            stmt,
            |h| {
                state = h
                    .diagnostics
                    .get(0)
                    .expect("record 1 exists")
                    .sqlstate
                    .as_str()
                    .to_owned();
            },
        );
        state
    }

    /// Spec, `SQLExecDirect` `HY008`, second clause: the function "was called,
    /// and before it completed execution, **SQLCancel** or **SQLCancelHandle**
    /// was called on the *StatementHandle* from a different thread in a
    /// multithread application." The row carries no `(DM)` marker, so this is
    /// the driver's to return.
    #[test]
    fn a_cancelled_execution_reports_hy008_not_the_backends_own_state() {
        unsafe {
            let (env, conn, stmt) = alloc_cancel_aware_stmt();
            MockCancelAwareBackend::fail_next_execution();
            MockCancelAwareBackend::cancel_before_returning();

            let wide: Vec<u16> = "SELECT 1".encode_utf16().collect();
            let ret =
                sql_exec_direct_w::<MockCancelAwareBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate(stmt), "HY008");

            cleanup_env_conn_stmt_for::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// The other half: a failure that was *not* a cancellation keeps the
    /// backend's own SQLSTATE. Reclassifying unconditionally would relabel
    /// every backend error in the crate as `HY008`.
    #[test]
    fn an_uncancelled_failure_keeps_its_own_state() {
        unsafe {
            let (env, conn, stmt) = alloc_cancel_aware_stmt();
            MockCancelAwareBackend::fail_next_execution();

            let wide: Vec<u16> = "SELECT 1".encode_utf16().collect();
            let ret =
                sql_exec_direct_w::<MockCancelAwareBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::ERROR);
            assert_ne!(first_sqlstate(stmt), "HY008");

            cleanup_env_conn_stmt_for::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// `SQLPrepare` reaches `Backend::prepare`, which is cancellable on the
    /// same terms as an execution. Its `HY008` row carries no `(DM)` marker
    /// either.
    #[test]
    fn a_cancelled_prepare_reports_hy008() {
        unsafe {
            let (env, conn, stmt) = alloc_cancel_aware_stmt();
            MockCancelAwareBackend::fail_next_execution();
            MockCancelAwareBackend::cancel_before_returning();

            let wide: Vec<u16> = "SELECT 1".encode_utf16().collect();
            let ret =
                sql_prepare_w::<MockCancelAwareBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(first_sqlstate(stmt), "HY008");

            cleanup_env_conn_stmt_for::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// `SQLExecute`'s own `Backend::execute` call site, reached after a
    /// successful `SQLPrepare` so that the one-shot failure switch lands on
    /// the execution rather than on the prepare that precedes it.
    #[test]
    fn a_cancelled_execute_reports_hy008() {
        unsafe {
            let (env, conn, stmt) = alloc_cancel_aware_stmt();

            let wide: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                sql_prepare_w::<MockCancelAwareBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS,
                "precondition: prepare succeeds",
            );

            MockCancelAwareBackend::fail_next_execution();
            MockCancelAwareBackend::cancel_before_returning();

            assert_eq!(
                sql_execute::<MockCancelAwareBackend>(stmt),
                SqlReturn::ERROR
            );
            assert_eq!(first_sqlstate(stmt), "HY008");

            cleanup_env_conn_stmt_for::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLCancel`: "After the statement has been canceled, the
    /// application can call **SQLExecute** or **SQLExecDirect** again."
    ///
    /// With one token per statement, `Backend::cancel` marked a token the next
    /// execution reused, so every later call on that statement reported
    /// `HY008` forever. A token minted per execution cannot leak across that
    /// boundary. This is the end-to-end guard on `mint_cancel_token`, and it
    /// only bites once a call site reclassifies — before Task 4 the second
    /// execution succeeded whatever the token said.
    #[test]
    fn a_statement_is_reusable_after_being_cancelled() {
        unsafe {
            let (env, conn, stmt) = alloc_cancel_aware_stmt();
            let wide: Vec<u16> = "SELECT 1".encode_utf16().collect();

            assert_eq!(
                sql_exec_direct_w::<MockCancelAwareBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS,
            );

            // Signals the token the execution above minted.
            assert_eq!(
                crate::ffi::cursor::sql_cancel::<MockCancelAwareBackend>(stmt),
                SqlReturn::SUCCESS,
            );
            let _ = crate::ffi::cursor::sql_close_cursor::<MockCancelAwareBackend>(stmt);

            // A fresh execution mints a fresh token, so the stale signal cannot
            // reach it — even though this one is also told to fail, which is
            // what makes the assertion about the token and not about luck.
            MockCancelAwareBackend::fail_next_execution();
            assert_eq!(
                sql_exec_direct_w::<MockCancelAwareBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::ERROR,
            );
            assert_ne!(
                first_sqlstate(stmt),
                "HY008",
                "a cancel aimed at a finished execution must not reach the one that replaced it",
            );

            cleanup_env_conn_stmt_for::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// The link every other query-timeout test leaves untested: that core
    /// actually *arms* its timer at a statement-producing call site.
    ///
    /// `SQLSetStmtAttr` recording `core_query_timeout` and `QueryTimer`
    /// cancelling on expiry are covered separately, and both would keep passing
    /// if the `QueryTimer::arm` line were deleted from this function. Only an
    /// execution that overruns its deadline proves the two are joined up.
    ///
    /// Not run under Miri: it turns on a real one-second deadline and a
    /// spin-until-cancelled backend, so Miri's slowdown would stretch it
    /// unpredictably for no memory-safety gain.
    #[test]
    #[cfg_attr(miri, ignore = "wall-clock deadline; no unsafe to check")]
    fn an_execution_that_overruns_its_query_timeout_reports_hyt00() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockCoreCancelsTimeoutBackend>();

            // The backend delegates enforcement to core, so this records a
            // one-second deadline for the timer to act on.
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockCoreCancelsTimeoutBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::QueryTimeout as i32,
                    // An integer-valued attribute, not a pointer; the ODBC
                    // ABI passes it through a pointer-typed parameter.
                    std::ptr::without_provenance_mut::<c_void>(1),
                    0,
                ),
                SqlReturn::SUCCESS,
                "the mock delegates the deadline to core rather than refusing it",
            );

            // `MockAppliedConnection::exec_direct` spins on this text until its
            // token is cancelled, standing in for a runaway query.
            let wide: Vec<u16> = "BLOCK".encode_utf16().collect();
            let ret = sql_exec_direct_w::<MockCoreCancelsTimeoutBackend>(
                stmt,
                wide.as_ptr(),
                wide.len() as i32,
            );
            assert_eq!(
                ret,
                SqlReturn::ERROR,
                "a query cancelled by its deadline must not report success",
            );

            let state = with_handle::<
                MockCoreCancelsTimeoutBackend,
                StatementHandle<MockCoreCancelsTimeoutBackend>,
                _,
            >(stmt, |h| {
                h.diagnostics
                    .get(0)
                    .expect("a diagnostic record")
                    .sqlstate
                    .as_str()
                    .to_owned()
            });
            assert_eq!(
                state, "HYT00",
                "an expired deadline is a timeout, not the HY008 a SQLCancel would give",
            );

            cleanup_env_conn_stmt_for::<MockCoreCancelsTimeoutBackend>(env, conn, stmt);
        }
    }
}
