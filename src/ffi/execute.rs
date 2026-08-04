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

/// Whether the execution that just completed was a searched DML statement that
/// affected no rows, the spec's `SQL_NO_DATA` case.
///
/// `SQLExecDirect`'s Comments, repeated verbatim on `SQLExecute`: "If
/// SQLExecDirect executes a searched update, insert, or delete statement that
/// doesn't affect any rows at the data source, the call to SQLExecDirect
/// returns SQL_NO_DATA." Appendix B's footnote `[nf]` corroborates it from the
/// other side, carving these functions out of its own definition of
/// `SQL_NO_DATA` precisely because they return it here.
///
/// Two facts decide it, and both are already on the statement:
///
/// - **`column_count() == 0`** separates DML from a query. A `SELECT` matching
///   nothing still declares its columns, so it is `SQL_SUCCESS` with an empty
///   result set. Reporting `SQL_NO_DATA` for it would send an application down
///   its end-of-cursor path before it fetched anything.
/// - **`row_count() == Some(0)`** is the backend saying it counted, and counted
///   zero. `None` means "not applicable to this statement" and
///   `Some(SQL_NO_TOTAL)` means "the driver cannot determine the count"; neither
///   asserts that nothing was affected, so both keep `SQL_SUCCESS`. A backend
///   that reports no row counts therefore never reaches `SQL_NO_DATA`.
///   Under-reporting leaves the application on the path it already takes, while
///   over-reporting would make a successful `CREATE TABLE` look like a miss.
fn zero_row_searched_dml<B: Backend>(stmt: &StatementHandle<B>) -> bool {
    stmt.statement
        .as_ref()
        .is_some_and(|s| s.column_count() == 0 && s.row_count() == Some(0))
}

/// Generic implementation of SQLExecDirectW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlexecdirect-function>
///
/// Parses the UTF-16 SQL statement, calls `B::exec_direct`, and stores the
/// resulting statement in the handle. A statement carrying parameter markers
/// goes through `B::prepare` plus `B::execute` instead, because
/// `Backend::exec_direct` takes no parameters.
///
/// On that path each bound buffer is read at its bound address plus
/// `SQL_ATTR_PARAM_BIND_OFFSET_PTR`, dereferenced once per execution. That is
/// the parameter-side counterpart of what `SQLFetch` does with
/// `SQL_ATTR_ROW_BIND_OFFSET_PTR`, and identical to [`sql_execute`]'s handling.
/// See `SQLBindParameter`'s "Rebinding with Offsets", and
/// `descriptor::BindOffset` for why a null pointer is left unshifted.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (`SQLHSTMT`).
/// - `statement_text`: Pointer to the UTF-16 SQL statement text.
/// - `text_length`: Length of `statement_text` in characters, or `SQL_NTS` (-3) if
///   null-terminated.
///
/// # Return values
///
/// `SQL_NO_DATA` is returned for a searched update, insert or delete that affected no rows.
/// The Comments say so directly, and Appendix B's `[nf]` footnote carves this function out
/// of its own `SQL_NO_DATA` definition for that reason. Core decides it from
/// `StatementBackend::column_count` and `StatementBackend::row_count`; see
/// `zero_row_searched_dml` for why a backend that reports no row count keeps
/// `SQL_SUCCESS`.
///
/// # Spec compliance
///
/// - 01000: General warning. Propagated from backend.
/// - 01001: Cursor operation conflict. Propagated from backend.
/// - 01003: NULL value eliminated in set function. Propagated from backend.
/// - 01004: String data, right truncated. Propagated from backend.
/// - 01006: Privilege not revoked. Propagated from backend.
/// - 01007: Privilege not granted. Propagated from backend.
/// - 01S02: Option value changed. Propagated from backend.
/// - 01S07: Fractional truncation. Propagated from backend.
/// - 07002: COUNT field incorrect. The row's first clause, "the number of parameters
///   specified in `SQLBindParameter` was less than the number of parameters in the SQL
///   statement", is **returned here**: a `?` marker with no binding is rejected rather than
///   padded with NULL (`ffi::params::collect_params`). The second clause, a binding whose
///   `ParameterValuePtr` is null with a non-`SQL_NULL_DATA`/`SQL_DATA_AT_EXEC` indicator, is
///   rejected by `SQLBindParameter` itself. Also propagated from backend.
/// - 07006: Restricted data type attribute violation. **Returned by this driver**, for a
///   binding whose C type core cannot read a parameter out of: the thirteen
///   `SQL_C_INTERVAL_*` types and the `SQL_ARD_TYPE` / `SQL_APD_TYPE` sentinels
///   (`ffi::params::read_param_value`'s terminal arm). `SQLBindParameter` refuses those,
///   along with a `SQL_C_BINARY` or numeric parameter bound to a target its conversion
///   table does not define, so a binding made *through it* cannot reach execution carrying
///   one; a binding assembled through `SQLSetDescField` never passes that gate and is
///   caught here instead. Also propagated from backend.
/// - 07007: Restricted parameter value violation. Propagated from backend.
/// - 07S01: Invalid use of default parameter. Propagated from backend.
/// - 08S01: Communication link failure. Propagated from backend.
/// - 21S01: Insert value list does not match column list. Propagated from backend.
/// - 21S02: Degree of derived table does not match column list. Propagated from backend.
/// - 22001: String data, right truncation. Returned here when a parameter value does not
///   fit the SQL type the application declared: fractional digits dropped or whole digits
///   lost for an exact-numeric target, or a value longer than the declared `ColumnSize` for
///   a character or binary target (`crate::param_convert`, the "C to SQL: Character" table,
///   and its "C to SQL: Binary" counterpart for a `SQL_C_BINARY` value bound to a binary
///   type). Also propagated from backend.
/// - 22002: Indicator variable required but not supplied. Propagated from backend.
/// - 22003: Numeric value out of range. Returned here when character parameter data falls
///   outside the range of the declared approximate-numeric or `SQL_BIT` type
///   (`crate::param_convert`), or when a `SQL_C_BINARY` parameter's byte count is not
///   exactly the declared SQL type's width (`crate::binary_convert`, the "C to SQL: Binary"
///   table). Also propagated from backend.
/// - 22007: Invalid datetime format. Returned here for character parameter data that is a
///   datetime literal with an out-of-range field (`crate::param_convert`). Also propagated
///   from backend.
/// - 22008: Datetime field overflow. Returned here when character parameter data carries a
///   datetime component the declared type cannot hold: a non-zero time for `SQL_TYPE_DATE`,
///   or non-zero fractional seconds for `SQL_TYPE_TIME` (`crate::param_convert`). Also
///   propagated from backend.
/// - 22012: Division by zero. Propagated from backend.
/// - 22015: Interval field overflow. Propagated from backend.
/// - 22018: Invalid character value for cast specification. Returned here when character
///   parameter data is not a valid literal of the SQL type declared for it at
///   `SQLBindParameter` (`crate::param_convert`). Also propagated from backend.
/// - 22019: Invalid escape character. Propagated from backend.
/// - 22025: Invalid escape sequence. Propagated from backend.
/// - 23000: Integrity constraint violation. Propagated from backend.
/// - 24000: Invalid cursor state. **Returned here** when a cursor is already open on the
///   statement. The row carries no `(DM)` marker. It splits its first condition in prose
///   instead, giving it to the Driver Manager while `SQLFetch` has not yet returned
///   `SQL_NO_DATA` and to the driver once it has; its remaining conditions are unattributed
///   and so the driver's. Also propagated from the backend for a positioned update or delete
///   on an improperly positioned cursor.
/// - 34000: Invalid cursor name. Propagated from backend.
/// - 3D000: Invalid catalog name. Propagated from backend.
/// - 3F000: Invalid schema name. Propagated from backend.
/// - 40001: Serialization failure. Propagated from backend.
/// - 40003: Statement completion unknown. Propagated from backend.
/// - 42000: Syntax error or access violation. Propagated from backend; also returned here
///   (checked before the backend is called) for an unterminated (malformed) ODBC escape
///   sequence when NOSCAN is off (`crate::escape::translate_escapes`).
/// - 42S01: Base table or view already exists. Propagated from backend.
/// - 42S02: Base table or view not found. Propagated from backend.
/// - 42S11: Index already exists. Propagated from backend.
/// - 42S12: Index not found. Propagated from backend.
/// - 42S21: Column already exists. Propagated from backend.
/// - 42S22: Column not found. Propagated from backend.
/// - 44000: WITH CHECK OPTION violation. Propagated from backend.
/// - HY000: General error. Propagated from backend.
/// - HY001: Memory allocation error. Propagated from backend.
/// - HY008: Operation canceled. The row's first clause (asynchronous processing, then the
///   function called again) is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. The spec annotates the "`StatementText` was a null
///   pointer" clause `(DM)`, and it is the row's only clause; it is guarded defensively here.
///   Core is also linked directly, by its own tests and by an embedder with no Driver Manager
///   in front of it, and a null pointer that reaches `utf16_to_string` is a soundness question
///   rather than a spec question.
/// - HY010: Function sequence error (DM cases for async/NEED_DATA: driver-manager-handled; not
///   returned here); fails if the connection is not open (checked here).
/// - HY013: Memory management error. Propagated from backend.
/// - HY090: Invalid string or buffer length. Only the first sentence, `TextLength <= 0` and
///   `!= SQL_NTS`, is `(DM)`-marked; it is guarded defensively here. The three sentences that
///   follow it are not marked and are the driver's: each describes a parameter length value
///   set with `SQLBindParameter` that the row rules out. The sentence naming a non-null
///   parameter value "and the parameter length value was less than 0, but not equal to
///   `SQL_NTS`, `SQL_NULL_DATA`, `SQL_DATA_AT_EXEC`, `SQL_DEFAULT_PARAM`, or less than or equal
///   to `SQL_LEN_DATA_AT_EXEC_OFFSET`" is **returned by this driver**, from
///   `ffi::params::read_param_value`. The remaining sentences are propagated from the backend.
///
///   **Also returned here**, for a condition none of those four sentences states: an
///   `SQL_NTS` argument whose null terminator is not within `MAX_NTS_SCAN` (1 048 576) units,
///   which is a length the driver cannot determine. Exactly two arguments of this call can
///   reach it: `StatementText` itself, and a `SQL_C_CHAR` or `SQL_C_WCHAR` parameter bound
///   with an `SQL_NTS` (or absent) length indicator. Answering with the cap-length prefix
///   instead would execute a statement the application never wrote, so the call fails rather
///   than truncating. An explicitly measured `TextLength` is not limited by this, at any
///   size. See `nts_input_longer_than_the_scan_cap_is_hy090_not_a_truncated_statement` and
///   `an_explicitly_measured_statement_past_the_scan_cap_still_executes`.
/// - HY105: Invalid parameter type. Propagated from backend.
/// - HY109: Invalid cursor position. Propagated from backend.
/// - HY117: Connection suspended (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented. Propagated from backend; also returned here
///   (checked before the backend is called) for a `{call ...}`/`{?= call ...}` stored-procedure
///   escape, which this driver does not support, when NOSCAN is off.
/// - HYT00: Timeout expired. **Returned by this driver**, not merely propagated. A backend
///   whose `Backend::set_query_timeout` answered `QueryTimeout::CoreCancels` gets core's own
///   timer (`crate::query_timer`), armed here, and `QueryTimer::reclassify` replaces the
///   failing call's SQLSTATE with `HYT00` when the deadline fired. That pass runs after the
///   cancel pass, so "my deadline passed" wins over "another thread cancelled me". A backend
///   enforcing its own timeout has its `HYT00` propagated unchanged.
/// - HYT01: Connection timeout expired. Propagated from backend.
/// - IM001: Driver does not support this function (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported, and not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported, and not DM-annotated in the spec).
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
            let (stmt, conn, records) = scope.stmt_with_parent_and_params::<B>(statement_handle)?;
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

            // Whether a parameter conversion raised a warning, which makes
            // this call SQL_SUCCESS_WITH_INFO rather than SQL_SUCCESS. The
            // "C to SQL: Numeric" table's footnote [b] is the only source:
            // fractional truncation sends the value and tells the application.
            let mut converted_with_info = false;

            // Check for data-at-execution parameters.
            let param_count = crate::ffi::params::count_params(&sql, &dialect);
            if param_count > 0 {
                // SAFETY: caller guarantees all bound buffer pointers remain valid.
                let (non_dae_values, dae_params, warnings) =
                    crate::ffi::params::find_data_at_exec_params(records, param_count)?;

                if !dae_params.is_empty() {
                    // The warnings travel with the state rather than being
                    // posted here: `SQL_NEED_DATA` is not a completion and the
                    // diagnostic belongs with the call that sends the value.
                    // See `DataAtExecState::warnings`.
                    stmt.data_at_exec = Some(crate::handles::DataAtExecState {
                        pending_params: dae_params.into(),
                        current_param: None,
                        buffer: Vec::new(),
                        put_state: crate::handles::PutDataState::NotCalled,
                        collected_values: non_dae_values,
                        sql: sql.clone(),
                        warnings,
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
                let (params, warnings) = crate::ffi::params::collect_params(records, param_count)?;
                for warning in &warnings {
                    stmt.diagnostics.push(warning);
                }
                converted_with_info |= !warnings.is_empty();
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
                crate::ffi::params::write_output_params(records, &outcome.output_params)?;
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

            // A searched DML that affected nothing is SQL_NO_DATA, and it wins
            // over SQL_SUCCESS_WITH_INFO: it is a completion the application has
            // to branch on, while the parameter-conversion warning behind
            // `converted_with_info` is already in the diagnostic queue and stays
            // readable through SQLGetDiagRec either way.
            if zero_row_searched_dml(stmt) {
                return Ok(SqlReturn::NO_DATA);
            }

            Ok(if converted_with_info {
                SqlReturn::SUCCESS_WITH_INFO
            } else {
                SqlReturn::SUCCESS
            })
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
/// - 01000: General warning. Propagated from backend.
/// - 01S02: Option value changed. Propagated from backend.
/// - 08S01: Communication link failure. Propagated from backend.
/// - 21S01: Insert value list does not match column list. Propagated from backend.
/// - 21S02: Degree of derived table does not match column list. Propagated from backend.
/// - 22018: Invalid character value for cast specification. Propagated from backend.
///   `SQLPrepareW` reads no parameter values, so the character-to-SQL-type conversion that
///   raises this in `SQLExecute` and `SQLExecDirectW` does not run here.
/// - 22019: Invalid escape character. Propagated from backend.
/// - 22025: Invalid escape sequence. Propagated from backend.
/// - 24000: Invalid cursor state. The row's first sentence, a cursor open on the statement
///   where "`SQLFetch` or `SQLFetchScroll` had been called", is `(DM)`-marked and not returned
///   here. The second is unmarked and is the driver's: a cursor open on the statement but
///   where `SQLFetch` or `SQLFetchScroll` had not been called. Core does not return it either,
///   because re-preparing is allowed and simply replaces the current state.
/// - 34000: Invalid cursor name. Propagated from backend.
/// - 3D000: Invalid catalog name. Propagated from backend.
/// - 3F000: Invalid schema name. Propagated from backend.
/// - 42000: Syntax error or access violation. Propagated from backend; also returned here
///   (checked before the backend is called) for an unterminated (malformed) ODBC escape
///   sequence when NOSCAN is off (`crate::escape::translate_escapes`).
/// - 42S01: Base table or view already exists. Propagated from backend.
/// - 42S02: Base table or view not found. Propagated from backend.
/// - 42S11: Index already exists. Propagated from backend.
/// - 42S12: Index not found. Propagated from backend.
/// - 42S21: Column already exists. Propagated from backend.
/// - 42S22: Column not found. Propagated from backend.
/// - HY000: General error. Propagated from backend.
/// - HY001: Memory allocation error. Propagated from backend.
/// - HY008: Operation canceled. The row's first clause (asynchronous processing, then the
///   function called again) is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009: Invalid use of null pointer. The spec annotates the "`StatementText` was a null
///   pointer" clause `(DM)`, and it is the row's only clause; it is guarded defensively here.
///   Core is also linked directly, by its own tests and by an embedder with no Driver Manager
///   in front of it, and a null pointer that reaches `utf16_to_string` is a soundness question
///   rather than a spec question.
/// - HY010: Function sequence error (DM cases for async/NEED_DATA: driver-manager-handled; not
///   returned here); fails if the connection is not open (checked here).
/// - HY013: Memory management error. Propagated from backend.
/// - HY090: Invalid string or buffer length (DM case `TextLength <= 0 and != SQL_NTS`:
///   driver-manager-handled); fails if `TextLength < 0` and `!= SQL_NTS` (checked here).
///   **Also returned here**, for the condition the row does not state: a `StatementText`
///   passed as `SQL_NTS` whose null terminator is not within `MAX_NTS_SCAN` (1 048 576) units.
///   `StatementText` is this function's only `SQL_NTS` argument, so it is the whole set,
///   because parameters are bound but not read until `SQLExecute`. An explicitly measured
///   `TextLength` is not limited, at any size. See
///   `prepare_refuses_an_nts_statement_with_no_terminator_within_the_scan_cap`.
/// - HY117: Connection suspended (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented. Propagated from backend; also returned here
///   (checked before the backend is called) for a `{call ...}`/`{?= call ...}` stored-procedure
///   escape, which this driver does not support, when NOSCAN is off.
/// - HYT00: Timeout expired. **Returned by this driver**, not merely propagated. A backend
///   whose `Backend::set_query_timeout` answered `QueryTimeout::CoreCancels` gets core's own
///   timer (`crate::query_timer`), armed here, and `QueryTimer::reclassify` replaces the
///   failing call's SQLSTATE with `HYT00` when the deadline fired. That pass runs after the
///   cancel pass, so "my deadline passed" wins over "another thread cancelled me". A backend
///   enforcing its own timeout has its `HYT00` propagated unchanged.
/// - HYT01: Connection timeout expired. Propagated from backend.
/// - IM001: Driver does not support this function (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported, and not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported, and not DM-annotated in the spec).
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
            // Parameter bindings survive. SQLBindParameter's spec names the
            // only three things that unbind a parameter (another
            // SQLBindParameter, SQLFreeStmt(SQL_RESET_PARAMS), and
            // SQLSetDescField setting the APD's SQL_DESC_COUNT to 0), and
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
/// Each buffer is read at its bound address plus
/// `SQL_ATTR_PARAM_BIND_OFFSET_PTR`, dereferenced once per execution. That is
/// the parameter-side counterpart of what `SQLFetch` does with
/// `SQL_ATTR_ROW_BIND_OFFSET_PTR`. See `SQLBindParameter`'s "Rebinding with
/// Offsets", and `descriptor::BindOffset` for why a null pointer is left
/// unshifted.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (`SQLHSTMT`).
///
/// # Return values
///
/// `SQL_NO_DATA` is returned for a searched update, insert or delete that affected no rows.
/// The Comments say so directly, and Appendix B's `[nf]` footnote carves this function out
/// of its own `SQL_NO_DATA` definition for that reason. Core decides it from
/// `StatementBackend::column_count` and `StatementBackend::row_count`; see
/// `zero_row_searched_dml` for why a backend that reports no row count keeps
/// `SQL_SUCCESS`.
///
/// # Spec compliance
///
/// - 01000: General warning. Propagated from backend.
/// - 01001: Cursor operation conflict. Propagated from backend.
/// - 01003: NULL value eliminated in set function. Propagated from backend.
/// - 01004: String data, right truncated. Propagated from backend.
/// - 01006: Privilege not revoked. Propagated from backend.
/// - 01007: Privilege not granted. Propagated from backend.
/// - 01S02: Option value changed. Propagated from backend.
/// - 01S07: Fractional truncation. Propagated from backend.
/// - 07002: COUNT field incorrect. The row's first clause, "the number of parameters
///   specified in `SQLBindParameter` was less than the number of parameters in the SQL
///   statement", is **returned here**: a `?` marker with no binding is rejected rather than
///   padded with NULL (`ffi::params::collect_params`). The second clause, a binding whose
///   `ParameterValuePtr` is null with a non-`SQL_NULL_DATA`/`SQL_DATA_AT_EXEC` indicator, is
///   rejected by `SQLBindParameter` itself. Also propagated from backend.
/// - 07006: Restricted data type attribute violation. **Returned by this driver**, for a
///   binding whose C type core cannot read a parameter out of: the thirteen
///   `SQL_C_INTERVAL_*` types and the `SQL_ARD_TYPE` / `SQL_APD_TYPE` sentinels
///   (`ffi::params::read_param_value`'s terminal arm). `SQLBindParameter` refuses those,
///   along with a `SQL_C_BINARY` or numeric parameter bound to a target its conversion
///   table does not define, so a binding made *through it* cannot reach execution carrying
///   one; a binding assembled through `SQLSetDescField` never passes that gate and is
///   caught here instead. Also propagated from backend.
/// - 07007: Restricted parameter value violation. Propagated from backend.
/// - 07S01: Invalid use of default parameter. Propagated from backend.
/// - 08S01: Communication link failure. Propagated from backend.
/// - 21S02: Degree of derived table does not match column list. Propagated from backend.
/// - 22001: String data, right truncation. Returned here when a parameter value does not
///   fit the SQL type the application declared: fractional digits dropped or whole digits
///   lost for an exact-numeric target, or a value longer than the declared `ColumnSize` for
///   a character or binary target (`crate::param_convert`, the "C to SQL: Character" table,
///   and its "C to SQL: Binary" counterpart for a `SQL_C_BINARY` value bound to a binary
///   type). Also propagated from backend.
/// - 22002: Indicator variable required but not supplied. Propagated from backend.
/// - 22003: Numeric value out of range. Returned here when character parameter data falls
///   outside the range of the declared approximate-numeric or `SQL_BIT` type
///   (`crate::param_convert`), or when a `SQL_C_BINARY` parameter's byte count is not
///   exactly the declared SQL type's width (`crate::binary_convert`, the "C to SQL: Binary"
///   table). Also propagated from backend.
/// - 22007: Invalid datetime format. Returned here for character parameter data that is a
///   datetime literal with an out-of-range field (`crate::param_convert`). Also propagated
///   from backend.
/// - 22008: Datetime field overflow. Returned here when character parameter data carries a
///   datetime component the declared type cannot hold: a non-zero time for `SQL_TYPE_DATE`,
///   or non-zero fractional seconds for `SQL_TYPE_TIME` (`crate::param_convert`). Also
///   propagated from backend.
/// - 22012: Division by zero. Propagated from backend.
/// - 22015: Interval field overflow. Propagated from backend.
/// - 22018: Invalid character value for cast specification. Returned here when character
///   parameter data is not a valid literal of the SQL type declared for it at
///   `SQLBindParameter` (`crate::param_convert`). Also propagated from backend.
/// - 22019: Invalid escape character. Propagated from backend.
/// - 22025: Invalid escape sequence. Propagated from backend.
/// - 23000: Integrity constraint violation. Propagated from backend.
/// - 24000: Invalid cursor state. **Returned here** when a cursor is already open on the
///   statement, which the Comments require the application to close first: "to execute a
///   SELECT statement more than once, the application must call SQLCloseCursor before
///   reexecuting". The row carries no `(DM)` marker. It splits its first condition in prose
///   instead, giving it to the Driver Manager while `SQLFetch` has not yet returned
///   `SQL_NO_DATA` and to the driver once it has; its remaining conditions are unattributed
///   and so the driver's. Also propagated from the backend for a positioned update or delete
///   on an improperly positioned cursor.
/// - 40001: Serialization failure. Propagated from backend.
/// - 40003: Statement completion unknown. Propagated from backend.
/// - 42000: Syntax error or access violation. Propagated from backend.
/// - 44000: WITH CHECK OPTION violation. Propagated from backend.
/// - HY000: General error. Propagated from backend.
/// - HY001: Memory allocation error. Propagated from backend.
/// - HY008: Operation canceled. The row's first clause (asynchronous processing, then the
///   function called again) is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY010: Function sequence error. Every clause of this row is `(DM)`, including
///   "the `StatementHandle` was not prepared", which this row carries and its two siblings do
///   not. The check is guarded defensively here: without it a backend would be asked to
///   execute a statement it was never given, which is an internal invariant violation rather
///   than a spec check the Driver Manager can be relied on to make. The connection-not-open
///   check is guarded here for the same reason.
/// - HY013: Memory management error. Propagated from backend.
/// - HY090: Invalid string or buffer length. Propagated from backend (parameter buffer length
///   validation). **Also returned here** for a bound parameter whose length indicator is
///   negative and names none of `SQL_NTS`, `SQL_NULL_DATA`, `SQL_DEFAULT_PARAM`,
///   `SQL_DATA_AT_EXEC` or `SQL_LEN_DATA_AT_EXEC(n)`, which is the row's own third sentence.
///   Folding such a value into `SQL_NTS` would bind the whole null-terminated string and
///   answer `SUCCESS` for a parameter length value the row rules out. **And returned here**,
///   for a bound `SQL_C_CHAR` or `SQL_C_WCHAR`
///   parameter whose `SQL_NTS` (or absent) length indicator sends core scanning and whose
///   null terminator is not within `MAX_NTS_SCAN` (1 048 576) units. Those two C types are the
///   complete set: every other bound type has a fixed width or an explicit indicator, and
///   this function takes no string argument of its own. See
///   `read_param_value_refuses_a_wchar_nts_buffer_that_runs_to_the_scan_cap` and
///   `read_param_value_refuses_a_char_nts_buffer_that_runs_to_the_scan_cap`.
/// - HY105: Invalid parameter type. Propagated from backend.
/// - HY109: Invalid cursor position. Propagated from backend.
/// - HY117: Connection suspended (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented. Propagated from backend.
/// - HYT00: Timeout expired. **Returned by this driver**, not merely propagated. A backend
///   whose `Backend::set_query_timeout` answered `QueryTimeout::CoreCancels` gets core's own
///   timer (`crate::query_timer`), armed here, and `QueryTimer::reclassify` replaces the
///   failing call's SQLSTATE with `HYT00` when the deadline fired. That pass runs after the
///   cancel pass, so "my deadline passed" wins over "another thread cancelled me". A backend
///   enforcing its own timeout has its `HYT00` propagated unchanged.
/// - HYT01: Connection timeout expired. Propagated from backend.
/// - IM001: Driver does not support this function (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported, and not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported, and not DM-annotated in the spec).
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
            let (stmt, conn, records) = scope.stmt_with_parent_and_params::<B>(statement_handle)?;
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

            // Spec 24000: "A cursor was open on the StatementHandle." The
            // Comments say it plainly: "to execute a SELECT statement more than
            // once, the application must call SQLCloseCursor before
            // reexecuting". The row's driver clause covers the case where
            // SQLFetch has already returned SQL_NO_DATA. `SQLExecDirectW`
            // carries the same check.
            //
            // After the "no SQL has been prepared" HY010 above, not before:
            // Appendix B's cursor-states table for this function reads
            // `24000 [p]` beside `HY010 [np]`, so an unprepared statement is
            // HY010 whatever its cursor state.
            //
            // Reads `cursor_open`, which is core-owned: the backend is never
            // told a cursor is open, so this state is returned here rather than
            // propagated from the backend.
            if stmt.cursor_open {
                return Err(OdbcError::general(
                    "A cursor is already open on this statement; call SQLCloseCursor first",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // Whether a parameter conversion raised a warning, which makes this
            // call SQL_SUCCESS_WITH_INFO rather than SQL_SUCCESS. See the twin
            // of this flag in `sql_exec_direct_w`.
            let mut converted_with_info = false;

            // Check for data-at-execution parameters.
            // SAFETY: caller guarantees all bound buffer pointers remain valid.
            let (non_dae_values, dae_params, warnings) =
                crate::ffi::params::find_data_at_exec_params(records, param_count)?;

            if !dae_params.is_empty() {
                // Carried, not posted; see `DataAtExecState::warnings` and the
                // twin of this branch in `sql_exec_direct_w`.
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
                    put_state: crate::handles::PutDataState::NotCalled,
                    collected_values: non_dae_values,
                    sql,
                    warnings,
                });
                return Ok(SqlReturn::NEED_DATA);
            }

            // No DAE params: collect all values normally and execute immediately.
            // SAFETY: caller guarantees all bound buffer pointers remain valid.
            let (params, warnings) = crate::ffi::params::collect_params(records, param_count)?;
            for warning in &warnings {
                stmt.diagnostics.push(warning);
            }
            converted_with_info |= !warnings.is_empty();

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
            stmt.note_executed();
            // SAFETY: the application's bound output buffer pointers remain valid
            // per the caller contract (same guarantee collect_params relies on).
            // Already inside the enclosing `unsafe` context, like collect_params above.
            crate::ffi::params::write_output_params(records, &outcome.output_params)?;

            // As in `sql_exec_direct_w`: this page carries the same Comments
            // sentence, so it answers the same way.
            if zero_row_searched_dml(stmt) {
                return Ok(SqlReturn::NO_DATA);
            }

            Ok(if converted_with_info {
                SqlReturn::SUCCESS_WITH_INFO
            } else {
                SqlReturn::SUCCESS
            })
        })
    };
    tracing::debug!("SQLExecute -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::DescriptorRole;
    use crate::handles::ConnectionHandle;
    use crate::handles::StatementHandle;
    use crate::test_utils::{
        MockBackend, MockBlockingBackend, MockCancelAwareBackend, MockConnection,
        MockCoreCancelsTimeoutBackend, MockLongDataBackend, MockRecordingBackend,
        MockRowCountBackend, alloc_env_conn_stmt, alloc_env_conn_stmt_for,
        cleanup_connected_env_conn_stmt, connect_handle, with_descriptor, with_handle,
    };
    use odbc_sys::{CDataType, HandleType, ParamType, SqlDataType};

    #[test]
    fn exec_direct_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Not connected, so this must fail with HY010.
            let sql = "SELECT 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            let ret = sql_exec_direct_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_diag_state::<MockBackend>(stmt).as_deref(),
                Some("HY010"),
                "no open connection is a function sequence error",
            );
            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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
            assert_eq!(
                first_diag_state::<MockBackend>(stmt).as_deref(),
                Some("HY009"),
                "a null StatementText is an invalid use of a null pointer",
            );
            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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
            assert_eq!(
                first_diag_state::<MockBackend>(stmt).as_deref(),
                Some("HY090"),
                "a TextLength that is neither SQL_NTS nor non-negative is HY090",
            );
            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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
            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn prepare_not_connected_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Not connected, so this must fail.
            let sql = "SELECT ?";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            let ret = sql_prepare_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::ERROR);
            assert_eq!(
                first_diag_state::<MockBackend>(stmt).as_deref(),
                Some("HY010"),
                "no open connection is a function sequence error",
            );
            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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
            assert_eq!(
                first_diag_state::<MockBackend>(stmt).as_deref(),
                Some("HY009"),
                "a null StatementText is an invalid use of a null pointer",
            );
            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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
            assert_eq!(
                first_diag_state::<MockBackend>(stmt).as_deref(),
                Some("HY010"),
                "an unprepared statement is a function sequence error",
            );
            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
        }
    }

    #[test]
    fn prepare_keeps_previous_param_bindings() {
        // SQLBindParameter's spec names the only three things that unbind a
        // parameter: another SQLBindParameter, SQLFreeStmt(SQL_RESET_PARAMS),
        // and SQLSetDescField setting the APD's SQL_DESC_COUNT to 0. SQLPrepare
        // is not among them, and SQLPrepare's own Comments tell the application
        // to "unbind all parameters that applied to an old SQL statement before
        // preparing a new SQL statement", which is advice only a driver that
        // keeps them could need.
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

            for role in [DescriptorRole::Apd, DescriptorRole::Ipd] {
                with_descriptor::<MockBackend, _>(stmt, role, |desc| {
                    assert!(
                        desc.records.contains_key(&1),
                        "SQLPrepare unbound a parameter ({role:?}), which only \
                         SQLBindParameter, SQLFreeStmt(SQL_RESET_PARAMS) or \
                         SQLSetDescField may do"
                    );
                });
            }
            with_handle::<MockBackend, crate::handles::StatementHandle<MockBackend>, _>(
                stmt,
                |stmt_handle| {
                    // The new statement has no markers, so nothing reads the surviving
                    // binding: `collect_params` walks 1..=param_count.
                    assert_eq!(stmt_handle.param_count, Some(0));
                },
            );

            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
        }
    }

    /// Read record 1 of a statement's diagnostic queue as a SQLSTATE, or `None`
    /// if the queue is empty.
    unsafe fn first_diag_state<B: Backend>(stmt: *mut c_void) -> Option<String> {
        let mut state = [0u16; 6];
        let mut native_err: i32 = 0;
        let mut msg_buf = [0u16; 512];
        let mut msg_len: i16 = 0;
        // SAFETY: every buffer is a live local of the declared size.
        let ret = unsafe {
            crate::ffi::diag::sql_get_diag_rec_w::<B>(
                HandleType::Stmt as i16,
                stmt,
                1,
                state.as_mut_ptr(),
                &mut native_err,
                msg_buf.as_mut_ptr(),
                512,
                &mut msg_len,
            )
        };
        if ret == SqlReturn::NO_DATA {
            return None;
        }
        Some(String::from_utf16_lossy(&state[..5]))
    }

    /// Bind `3.7` as a `SQL_C_DOUBLE` to a `SQL_INTEGER` parameter.
    ///
    /// # Safety
    ///
    /// `value` must outlive the execution that reads it.
    unsafe fn bind_truncating_double<B: Backend>(stmt: *mut c_void, value: &mut f64) {
        // SAFETY: `value` is a live local the caller keeps alive.
        let ret = unsafe {
            crate::ffi::params::sql_bind_parameter::<B>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::Double as i16,
                SqlDataType::INTEGER.0,
                0,
                0,
                std::ptr::from_mut(value).cast(),
                8,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "binding the parameter failed");
    }

    /// Footnote [b] of the *C to SQL: Numeric* table, end to end: binding 3.7
    /// to an `SQL_INTEGER` parameter sends 3 and tells the application it lost
    /// the fraction.
    #[test]
    fn exec_direct_reports_a_truncated_parameter_as_success_with_info() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let mut value: f64 = 3.7;
            bind_truncating_double::<MockRecordingBackend>(stmt, &mut value);

            let wide: Vec<u16> = "SELECT ?".encode_utf16().collect();
            assert_eq!(
                sql_exec_direct_w::<MockRecordingBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS_WITH_INFO
            );
            assert_eq!(
                first_diag_state::<MockRecordingBackend>(stmt).as_deref(),
                Some("01S07")
            );

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// The same through prepare + execute, which assembles its return value
    /// separately and so could regress on its own.
    #[test]
    fn execute_reports_a_truncated_parameter_as_success_with_info() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let wide: Vec<u16> = "SELECT ?".encode_utf16().collect();
            assert_eq!(
                sql_prepare_w::<MockRecordingBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS
            );

            let mut value: f64 = 3.7;
            bind_truncating_double::<MockRecordingBackend>(stmt, &mut value);

            assert_eq!(
                sql_execute::<MockRecordingBackend>(stmt),
                SqlReturn::SUCCESS_WITH_INFO
            );
            assert_eq!(
                first_diag_state::<MockRecordingBackend>(stmt).as_deref(),
                Some("01S07")
            );

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// A parameter that converts cleanly leaves the call at plain
    /// `SQL_SUCCESS`, because the flag must not latch on.
    #[test]
    fn an_untruncated_parameter_leaves_the_return_at_success() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let mut value: f64 = 4.0;
            bind_truncating_double::<MockRecordingBackend>(stmt, &mut value);

            let wide: Vec<u16> = "SELECT ?".encode_utf16().collect();
            assert_eq!(
                sql_exec_direct_w::<MockRecordingBackend>(stmt, wide.as_ptr(), wide.len() as i32),
                SqlReturn::SUCCESS
            );
            assert_eq!(first_diag_state::<MockRecordingBackend>(stmt), None);

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQL_ATTR_PARAMS_PROCESSED_PTR`: the driver returns the number of
    /// sets of parameters that have been processed, and
    /// `SQL_ATTR_PARAM_STATUS_PTR` holds one status per set.
    /// `SQL_ATTR_PARAMSET_SIZE` is pinned at 1, so an execution processes
    /// exactly one set and reports `SQL_PARAM_SUCCESS` for it, the
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

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
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
            // Through the production accessor, not a hand-written downcast:
            // the registry stores a `CancelState` wrapper, and a test that
            // spelled the stored type itself would keep passing while
            // `cancel_as`, what every call site actually uses, broke.
            let token = crate::handles::cancel_as::<MockRecordingBackend>(&token)
                .expect("backend's own type");
            assert!(
                token
                    .saw_execution
                    .load(std::sync::atomic::Ordering::SeqCst),
                "exec_direct must receive the same token the statement carries"
            );

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// Mint-per-execution, pinned at the FFI entry point rather than against
    /// the internal helper directly: a second `SQLExecDirectW` on the same
    /// handle gets its own token, so a `SQLCancel` aimed at the first
    /// execution cannot reach into the second.
    ///
    /// This test asserted the opposite, on the reasoning that a stale
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
                 cannot leak into the next, which is what makes a cancelled statement reusable"
            );

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// A `SQL_NTS` statement with no terminator inside `MAX_NTS_SCAN` is
    /// `HY090`, and the backend is never called.
    ///
    /// This is the silent-truncation case on the *input* side. Handing back the
    /// cap-length prefix and executing it would run a **different statement than
    /// the application wrote** whenever the prefix is syntactically complete:
    /// `DELETE FROM t WHERE id = 1` in place of
    /// `DELETE FROM t WHERE id = 1 AND …`.
    ///
    /// "The backend was never called" is asserted through the cancel token:
    /// `mint_cancel_token` runs after the text is decoded and its result is the
    /// token `MockRecordingBackend::exec_direct` records itself against, so an
    /// absent token is proof no statement-producing call happened. The cursor
    /// and statement checks are the same fact read off the handle.
    #[test]
    fn nts_input_longer_than_the_scan_cap_is_hy090_not_a_truncated_statement() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            // No terminator anywhere, and the allocation stops at the cap so a
            // scan that runs past it is a heap overflow rather than a longer
            // prefix.
            let wide = vec![b'a' as u16; crate::utf16::MAX_NTS_SCAN];
            assert_eq!(
                sql_exec_direct_w::<MockRecordingBackend>(stmt, wide.as_ptr(), SQL_NTS),
                SqlReturn::ERROR
            );
            assert_eq!(
                first_diag_state::<MockRecordingBackend>(stmt).as_deref(),
                Some(crate::types::sql_state::INVALID_STRING_OR_BUFFER_LENGTH)
            );
            assert!(
                crate::handles::registry::registry()
                    .cancel_of(stmt)
                    .is_none(),
                "no cancel token was minted, so no statement-producing backend call ran"
            );
            with_handle::<MockRecordingBackend, StatementHandle<MockRecordingBackend>, _>(
                stmt,
                |s| {
                    assert!(!s.cursor_open, "no cursor may be opened by a refused call");
                    assert!(s.statement.is_none(), "no statement may be left behind");
                },
            );

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// The accepting side of the same boundary, so the check above cannot be
    /// satisfied by refusing every long statement: a terminator in the last
    /// position the scan may read still executes, in full.
    #[test]
    fn an_nts_statement_terminated_at_the_last_scannable_position_still_executes() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let mut wide = vec![b'a' as u16; crate::utf16::MAX_NTS_SCAN];
            wide[crate::utf16::MAX_NTS_SCAN - 1] = 0;
            assert_eq!(
                sql_exec_direct_w::<MockRecordingBackend>(stmt, wide.as_ptr(), SQL_NTS),
                SqlReturn::SUCCESS
            );

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// A 100 000-character `SQL_NTS` statement executes.
    ///
    /// The length is the point: it is above `i16::MAX` (32 767) and far below
    /// `MAX_NTS_SCAN`. Machine-generated SQL (a batched `INSERT … VALUES`, an
    /// `IN` list built from a key set) passes that mark routinely, and unixODBC
    /// forwards `SQL_NTS` unchanged from a Unicode application to a Unicode
    /// driver, so such a statement reaches this function still needing a scan.
    /// A cap of `i16::MAX` would answer `HY090` for it.
    ///
    /// What the driver survey on `MAX_NTS_SCAN` establishes is narrower than
    /// "the other drivers run this statement", which depends on the data source
    /// and not on the driver: it is that **none of them refuses it for length**.
    /// Their `SQL_NTS` scans are unbounded, so no length threshold exists at
    /// which they stop. MySQL Connector/ODBC is the one that has an `HY090`
    /// length limit at all (`GET_NAME_LEN`, 192 bytes), and it does not bear on
    /// this: it is a post-hoc MySQL identifier check applied to catalog-function
    /// name arguments, never to SQL text.
    ///
    /// A literal rather than a fraction of `MAX_NTS_SCAN`: a test written
    /// against the constant passes at every value of it, including the one this
    /// test exists to rule out.
    #[cfg_attr(
        miri,
        ignore = "asserts an absolute 100 000-character input is accepted, which the \
                  Miri-only MAX_NTS_SCAN of 4096 refuses"
    )]
    #[test]
    fn an_nts_statement_of_a_hundred_thousand_characters_executes() {
        const STATEMENT_UNITS: usize = 100_000;

        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let mut wide = vec![b'a' as u16; STATEMENT_UNITS + 1];
            wide[STATEMENT_UNITS] = 0;
            assert_eq!(
                sql_exec_direct_w::<MockRecordingBackend>(stmt, wide.as_ptr(), SQL_NTS),
                SqlReturn::SUCCESS,
                "a {STATEMENT_UNITS}-character SQL_NTS statement is ordinary generated SQL, \
                 not a length the driver may refuse",
            );

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// The cap bounds a scan, never a declared length. A statement one unit past
    /// the cap, passed with its real `TextLength`, is unaffected, which is the
    /// property that keeps generated multi-row `INSERT` statements working
    /// however long they get.
    ///
    /// One unit past rather than some larger multiple: that is the boundary the
    /// claim is about, and every further unit only lengthens a scan the
    /// neighbouring tests already pay for at the cap itself.
    #[test]
    fn an_explicitly_measured_statement_past_the_scan_cap_still_executes() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let wide = vec![b'a' as u16; crate::utf16::MAX_NTS_SCAN + 1];
            assert_eq!(
                sql_exec_direct_w::<MockRecordingBackend>(
                    stmt,
                    wide.as_ptr(),
                    i32::try_from(wide.len()).expect("fits in i32"),
                ),
                SqlReturn::SUCCESS
            );

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
        }
    }

    /// `SQLPrepare` routes its `StatementText` through the same helper, so it
    /// inherits the same refusal.
    #[test]
    fn prepare_refuses_an_nts_statement_with_no_terminator_within_the_scan_cap() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt_for::<MockRecordingBackend>();
            with_handle::<MockRecordingBackend, ConnectionHandle<MockRecordingBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );

            let wide = vec![b'a' as u16; crate::utf16::MAX_NTS_SCAN];
            assert_eq!(
                sql_prepare_w::<MockRecordingBackend>(stmt, wide.as_ptr(), SQL_NTS),
                SqlReturn::ERROR
            );
            assert_eq!(
                first_diag_state::<MockRecordingBackend>(stmt).as_deref(),
                Some(crate::types::sql_state::INVALID_STRING_OR_BUFFER_LENGTH)
            );
            with_handle::<MockRecordingBackend, StatementHandle<MockRecordingBackend>, _>(
                stmt,
                |s| assert!(s.statement.is_none(), "nothing may be prepared"),
            );

            cleanup_connected_env_conn_stmt::<MockRecordingBackend>(env, conn, stmt);
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
            cleanup_connected_env_conn_stmt::<MockBlockingBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockCancelAwareBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockCancelAwareBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockCancelAwareBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// Spec, `SQLCancel`: "After the statement has been canceled, the
    /// application can call **SQLExecute** or **SQLExecDirect** again."
    ///
    /// With one token per statement, `Backend::cancel` marked a token the next
    /// execution reused, so every later call on that statement reported
    /// `HY008` forever. A token minted per execution cannot leak across that
    /// boundary. This is the end-to-end guard on `mint_cancel_token`, and it
    /// only bites once a call site reclassifies: before any of them did, the
    /// second execution succeeded whatever the token said.
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
            // reach it, even though this one is also told to fail, which is
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

            cleanup_connected_env_conn_stmt::<MockCancelAwareBackend>(env, conn, stmt);
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

            cleanup_connected_env_conn_stmt::<MockCoreCancelsTimeoutBackend>(env, conn, stmt);
        }
    }

    /// Both spec pages state it in the same words: "If SQLExecDirect executes a
    /// searched update, insert, or delete statement that doesn't affect any rows
    /// at the data source, the call to SQLExecDirect returns SQL_NO_DATA."
    /// Appendix B's footnote [nf] corroborates from the other side, carving
    /// exactly these three functions out of its own definition of SQL_NO_DATA.
    #[test]
    fn exec_direct_reports_no_data_for_a_zero_row_searched_dml() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockRowCountBackend>();

            let sql: Vec<u16> = "DELETE FROM t WHERE 1 = 0".encode_utf16().collect();
            assert_eq!(
                sql_exec_direct_w::<MockRowCountBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::NO_DATA,
                "no columns and a counted zero rows is the spec's SQL_NO_DATA case",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockRowCountBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The distinction the discriminator exists to draw. A `SELECT` matching
    /// nothing is `SQL_SUCCESS` with an empty result set, and reporting
    /// `SQL_NO_DATA` for it would send every application down its
    /// end-of-cursor path before it had fetched anything.
    #[test]
    fn exec_direct_reports_success_for_a_select_with_no_rows() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockRowCountBackend>();

            let sql: Vec<u16> = "SELECT a FROM t WHERE 1 = 0".encode_utf16().collect();
            assert_eq!(
                sql_exec_direct_w::<MockRowCountBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::SUCCESS,
                "an empty result set is not the same as no result",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockRowCountBackend>(
                env, conn, stmt,
            );
        }
    }

    /// `row_count() == None` means "not applicable to this statement" and
    /// `Some(SQL_NO_TOTAL)` means "the backend could not work it out". Neither
    /// asserts that nothing was affected, so both keep SQL_SUCCESS. A backend
    /// that reports no counts must not have every DDL statement reported as a
    /// miss.
    #[test]
    fn exec_direct_reports_success_when_the_backend_gives_no_row_count() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockRowCountBackend>();

            let sql: Vec<u16> = "CREATE TABLE unknown_count (a INT)"
                .encode_utf16()
                .collect();
            assert_eq!(
                sql_exec_direct_w::<MockRowCountBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::SUCCESS,
                "an absent row count is not a count of zero",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockRowCountBackend>(
                env, conn, stmt,
            );
        }
    }

    /// A searched DML that did affect rows is an ordinary success.
    #[test]
    fn exec_direct_reports_success_for_a_dml_that_affected_rows() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockRowCountBackend>();

            let sql: Vec<u16> = "DELETE FROM many WHERE a > 0".encode_utf16().collect();
            assert_eq!(
                sql_exec_direct_w::<MockRowCountBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::SUCCESS
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockRowCountBackend>(
                env, conn, stmt,
            );
        }
    }

    /// `SQLExecute`'s page carries the same sentence as `SQLExecDirect`'s, so
    /// the prepared path answers the same way. Covered separately because the
    /// two functions have separate success paths.
    #[test]
    fn execute_reports_no_data_for_a_zero_row_searched_dml() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockRowCountBackend>();

            let sql: Vec<u16> = "UPDATE t SET a = 1 WHERE 1 = 0".encode_utf16().collect();
            assert_eq!(
                sql_prepare_w::<MockRowCountBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                sql_execute::<MockRowCountBackend>(stmt),
                SqlReturn::NO_DATA,
                "SQLExecute carries the same Comments sentence as SQLExecDirect",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockRowCountBackend>(
                env, conn, stmt,
            );
        }
    }

    /// `SQLExecute`'s half of the SELECT distinction.
    #[test]
    fn execute_reports_success_for_a_select_with_no_rows() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockRowCountBackend>();

            let sql: Vec<u16> = "SELECT a FROM t WHERE 1 = 0".encode_utf16().collect();
            assert_eq!(
                sql_prepare_w::<MockRowCountBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(sql_execute::<MockRowCountBackend>(stmt), SqlReturn::SUCCESS);

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockRowCountBackend>(
                env, conn, stmt,
            );
        }
    }

    /// Spec Comments: "To execute a SELECT statement more than once, the
    /// application must call SQLCloseCursor before reexecuting." The `24000`
    /// row gives the driver the clause where `SQLFetch` has returned
    /// `SQL_NO_DATA`, and Appendix B's cursor-states table for this function
    /// reads `24000 [p]` in every column, with `[p]` meaning prepared, which is
    /// the only way `SQLExecute` gets this far at all.
    ///
    /// `MockLongDataBackend` reports three columns, so the first execution
    /// really opens a cursor. `MockBackend` reports none and could not reach
    /// this branch.
    #[test]
    fn execute_is_refused_while_a_cursor_is_open() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockLongDataBackend>();

            let sql: Vec<u16> = "SELECT a, b, c FROM t".encode_utf16().collect();
            assert_eq!(
                sql_prepare_w::<MockLongDataBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("the fixed test SQL is short"),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(sql_execute::<MockLongDataBackend>(stmt), SqlReturn::SUCCESS);
            with_handle::<MockLongDataBackend, StatementHandle<MockLongDataBackend>, _>(
                stmt,
                |handle| assert!(handle.cursor_open, "precondition: a cursor is open"),
            );

            assert_eq!(
                sql_execute::<MockLongDataBackend>(stmt),
                SqlReturn::ERROR,
                "re-executing over an open cursor must be refused",
            );
            assert_eq!(
                first_diag_state::<MockLongDataBackend>(stmt).as_deref(),
                Some("24000"),
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockLongDataBackend>(
                env, conn, stmt,
            );
        }
    }

    /// The other side of Appendix B's cursor-states row: `HY010 [np]` when the
    /// statement was never prepared. `sql_execute`'s existing "no SQL has been
    /// prepared" check already answers that, and the new `24000` must come
    /// after it rather than in front.
    #[test]
    fn execute_without_prepare_is_hy010_not_24000() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockBackend>();

            assert_eq!(sql_execute::<MockBackend>(stmt), SqlReturn::ERROR);
            assert_eq!(
                first_diag_state::<MockBackend>(stmt).as_deref(),
                Some("HY010"),
                "an unprepared statement is HY010, whatever its cursor state",
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockBackend>(env, conn, stmt);
        }
    }
}
