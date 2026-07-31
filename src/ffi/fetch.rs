//! Row fetching and column retrieval: `SQLFetch`, `SQLFetchScroll`, `SQLGetData`.

use std::ffi::c_void;

use odbc_sys::FetchOrientation;

use crate::backend::{Backend, StatementBackend};
use crate::cancel::reclassify_cancelled_opt;
use crate::column_value::{write_column_value, write_column_value_at};
use crate::descriptor::{DescriptorRole, c_type_of};
use crate::errors::OdbcError;
use crate::handles::{Descriptor, GetDataCursor, StatementHandle};
use crate::panic::panic_safe;
use crate::types::{ColumnValue, FetchResult, SqlReturn, SqlState, fetch_orientation_from_raw};

/// One bound column, as `SQLFetch` needs it: the column number, its raw
/// `SQL_DESC_CONCISE_TYPE`, and the three application addresses before the
/// row-bind offset is applied.
///
/// The concise type stays **raw**, not parsed to a `CDataType`: reading it as a C
/// type can fail, and the spec orders that failure *after* this call's `HY010`
/// and `24000` checks. Collecting is not checking.
type Binding = (u16, i16, *mut c_void, isize, *mut isize);

/// Every binding of an ARD that this fetch must serve, in no particular order.
///
/// Collected before the statement is borrowed, because the descriptor and the
/// statement are separate allocations and the column reads below need `&mut` on
/// the statement. A bind cannot happen in between: this call holds the
/// connection's group throughout.
///
/// A record with a null `SQL_DESC_DATA_PTR` and a live `SQL_DESC_INDICATOR_PTR`
/// **is** included. It is not a data binding — [`DescriptorRecord::is_bound`]
/// says so, and `write_column_value` declines to write through the null target —
/// but the spec makes it a legal state of `SQLBindCol` ("An application can
/// unbind the data buffer for a column but still have a length/indicator buffer
/// bound for the column"), and the length is the only thing such an application
/// asked for.
///
/// The mature drivers split on this, so the evidence rather than the
/// conclusion: MySQL Connector/ODBC's `SQLBindCol` unbinds only when both
/// pointers are null and otherwise keeps the record with a null data pointer,
/// while psqlODBC's `PGAPI_BindCol` clears the whole binding on a null
/// `rgbValue` regardless of `pcbValue`. Core follows the spec sentence, which is
/// unconditional.
///
/// A record carrying *neither* pointer is skipped. `SQLSetDescField` can create
/// one by setting any single field, so presence in the map has never meant a
/// binding.
fn collect_bindings(ard: &Descriptor) -> Vec<Binding> {
    ard.records
        .iter()
        .filter(|(_, record)| record.is_bound() || !record.indicator_ptr.is_null())
        .map(|(&col, r)| {
            (
                col,
                r.concise_type,
                r.data_ptr,
                r.octet_length,
                r.indicator_ptr,
            )
        })
        .collect()
}

/// `SQL_ROW_SUCCESS` — the row-status value for a row fetched without warning.
const SQL_ROW_SUCCESS: u16 = 0;
/// `SQL_ROW_SUCCESS_WITH_INFO` — fetched, but a diagnostic was raised for it.
const SQL_ROW_SUCCESS_WITH_INFO: u16 = 6;

/// The current value of `SQL_ATTR_ROW_BIND_OFFSET_PTR`, in bytes, or `0`.
///
/// The attribute holds a *pointer to* an `SQLULEN`, not the offset itself, so
/// the application can move the whole binding set between fetches by writing
/// through that one pointer.
///
/// # Safety
///
/// The stored attribute must be null or a pointer to a valid `usize`, which is
/// the application's undertaking when it sets the attribute.
unsafe fn row_bind_offset(ard: &Descriptor) -> usize {
    // On the ARD's own header, not `stmt.attrs`: `SQL_ATTR_ROW_BIND_OFFSET_PTR`
    // *is* `SQL_DESC_BIND_OFFSET_PTR` — see `HeaderOwner`.
    let raw = ard
        .attrs
        .get(&(odbc_sys::Desc::BindOffsetPtr as u16))
        .copied()
        .unwrap_or(0);
    if raw == 0 {
        return 0;
    }
    // SAFETY: non-zero means the application set it to a pointer it promised is
    // a valid SQLULEN. `read_unaligned` because ODBC applications place these in
    // packed structures.
    unsafe { std::ptr::read_unaligned(raw as *const usize) }
}

/// Applies `SQL_ATTR_ROW_BIND_OFFSET_PTR` to a bound pointer, leaving a null
/// pointer null.
///
/// The offset shifts a *buffer* the application bound; a null
/// `SQL_DESC_DATA_PTR` or `SQL_DESC_INDICATOR_PTR` means there is no buffer,
/// so there is nothing to shift. `ptr.wrapping_byte_add(off)` on a null `ptr`
/// would otherwise turn "no buffer" into a non-null address built from the
/// offset alone — not a real allocation — and every caller downstream
/// (`SQLFetch`'s 22002 check, `write_column_value`) treats non-null as "there
/// is a buffer here."
fn offset_non_null(ptr: *mut c_void, off: usize) -> *mut c_void {
    if ptr.is_null() {
        ptr
    } else {
        ptr.wrapping_byte_add(off)
    }
}

/// Where a fetch reports its row count and row status.
///
/// `SQLFetch` and `SQLFetchScroll` report through the `SQL_ATTR_ROWS_FETCHED_PTR`
/// and `SQL_ATTR_ROW_STATUS_PTR` statement attributes. `SQLExtendedFetch` reports
/// through its own `RowCountPtr` and `RowStatusArray` arguments instead, and the
/// spec is explicit that the two are not the same storage: that buffer "is used
/// only by **SQLExtendedFetch**. It is not used by **SQLFetch** or
/// **SQLFetchScroll**", and "the address of this array is not stored in the
/// SQL_DESC_STATUS_ARRAY_PTR field in the IRD".
///
/// Modelled as a destination rather than a flag so the two are mutually
/// exclusive by construction: writing to both is the bug this type exists to
/// make unrepresentable.
#[derive(Clone, Copy)]
enum RowReport {
    /// `SQLFetch` / `SQLFetchScroll`: the statement attributes.
    Attributes,
    /// `SQLExtendedFetch`: the function's own arguments. Either may be null, in
    /// which case nothing is written through it.
    Arguments {
        row_count: *mut usize,
        row_status: *mut u16,
    },
}

/// Write `count` and `status` to wherever `report` says they go, when the
/// application supplied a destination.
///
/// Only element 0 of a row-status array is written because
/// `SQL_ATTR_ROW_ARRAY_SIZE` and `SQL_ROWSET_SIZE` are both pinned at 1
/// (`ffi/stmt_attr.rs` substitutes anything else back with `01S02`), so the
/// rowset this driver produces has exactly one row and the application's array
/// is required to be at least that long.
///
/// # Safety
///
/// For [`RowReport::Attributes`], each stored attribute must be null or a
/// pointer to a valid, writable `usize` / `u16` respectively — the application's
/// undertaking when it set them. For [`RowReport::Arguments`], the same
/// undertaking applies to the two arguments, per the `SQLExtendedFetch` contract.
unsafe fn report_rows_fetched<B: Backend>(
    stmt: &StatementHandle<B>,
    report: RowReport,
    count: usize,
    status: u16,
) {
    unsafe { report_rows_fetched_only(stmt, report, count) };

    let target: *mut u16 = match report {
        RowReport::Attributes => stmt
            .attrs
            .get(&(odbc_sys::StatementAttribute::RowStatusPtr as i32))
            .copied()
            .unwrap_or(0) as *mut u16,
        RowReport::Arguments { row_status, .. } => row_status,
    };
    if !target.is_null() {
        // SAFETY: non-null means the application supplied a row-status array of
        // at least one element. Unaligned because the array may sit at any
        // offset in a packed buffer.
        unsafe { std::ptr::write_unaligned(target, status) };
    }
}

/// The rows-fetched half of [`report_rows_fetched`], for the `SQL_NO_DATA` path
/// where there is no row and therefore no status to report.
///
/// # Safety
///
/// See [`report_rows_fetched`].
unsafe fn report_rows_fetched_only<B: Backend>(
    stmt: &StatementHandle<B>,
    report: RowReport,
    count: usize,
) {
    let target: *mut usize = match report {
        RowReport::Attributes => stmt
            .attrs
            .get(&(odbc_sys::StatementAttribute::RowsFetchedPtr as i32))
            .copied()
            .unwrap_or(0) as *mut usize,
        RowReport::Arguments { row_count, .. } => row_count,
    };
    if !target.is_null() {
        // SAFETY: non-null means the application supplied a writable SQLULEN.
        unsafe { std::ptr::write_unaligned(target, count) };
    }
}

/// Generic implementation of SQLFetch.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfetch-function>
///
/// Advances the cursor to the next row. Returns `SQL_SUCCESS` if a row is
/// available, `SQL_NO_DATA` when the result set is exhausted.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (input).
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; driver-specific informational messages are not
///   generated.
/// - 01004 (string data right truncated): pushed as a diagnostic, and `SQL_SUCCESS_WITH_INFO`
///   returned, when `write_column_value` reports that character or binary data was truncated
///   to fit a bound buffer.
/// - 01S01 (error in row): not returned; only single-row rowsets are processed.
/// - 01S07 (fractional truncation): raised by `write_column_value` as
///   `OdbcError::FractionalTruncation` when numeric fractional data is truncated, or when a
///   non-zero `ColumnValue::Time` fraction is dropped writing to `SQL_C_TYPE_TIME`; the
///   diagnostic is pushed by `panic_safe`.
/// - 07006 (restricted data type attribute violation): returned via `write_column_value` when
///   a column value cannot be converted to the bound C type.
/// - 07009 (invalid descriptor index): not returned here, and the row carries no `(DM)`
///   marker, so this is the driver's answer to give. Neither of its two clauses can arise:
///   a column-0 binding cannot exist to be fetched into, because `sql_bind_col` refuses
///   column 0 with `HYC00` before a binding is stored (bookmarks are unsupported — the
///   `Backend` trait has no concept of stable row identifiers), and the other clause
///   describes an ODBC 2.x driver, which core is not.
/// - 08S01 (communication link failure): propagated from the backend fetch.
/// - 22001 (string data right truncated for bookmark): not applicable; bookmarks are not
///   supported (the `Backend` trait has no concept of stable row identifiers).
/// - 22002 (indicator variable required but not supplied): returned when a bound column
///   contains NULL data and `str_len_or_ind_ptr` for that binding is null.
/// - 22003 (numeric value out of range): returned via `write_column_value` for whole-part
///   numeric overflow.
/// - 22007 (invalid datetime format): returned via `write_column_value`.
/// - 22012 (division by zero): propagated from the backend if the data source reports it.
/// - 22015 (interval field overflow): returned via `write_column_value`.
/// - 22018 (invalid character value for cast specification): returned via `write_column_value`.
/// - 24000 (invalid cursor state): returned when the statement was executed but produced no
///   result set (ODBC state S4), and when it is prepared but not yet executed (S2/S3). The
///   row carries no (DM) marker, so the driver owes it.
/// - 40001 (serialization failure): propagated from the backend.
/// - 40003 (statement completion unknown): propagated from the backend.
/// - HY000 (general error): propagated from the backend.
/// - HY001 (memory allocation error): not returned; Rust panics on allocation failure.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY010 (function sequence error): returned when `stmt.statement` is `None`, i.e. no
///   result set is open. (DM) variants (async context, `SQLExtendedFetch` mixing) are
///   driver-manager-handled; not returned here.
/// - HY013 (memory management error): not returned.
/// - HY090 (invalid string or buffer length): not applicable, and the row carries no `(DM)`
///   marker, so this is the driver's answer to give. Its single clause describes a column-0
///   binding under `SQL_UB_VARIABLE`, and no such binding can exist: `sql_bind_col` refuses
///   column 0, and `SQL_ATTR_USE_BOOKMARKS` cannot be set away from `SQL_UB_OFF`.
/// - HY107 (row value out of range): not applicable; keyset cursors are not supported.
/// - HY117 (connection suspended): driver-manager-handled; not returned here.
/// - HYC00 (optional feature not implemented): returned via `write_column_value` when an
///   unsupported type conversion is requested.
/// - HYT00 (timeout expired): **returned by this driver**. The row carries no `(DM)` marker and
///   names `SQL_ATTR_QUERY_TIMEOUT` directly, and the spec's "Errors and Warnings on the Entire
///   Function" section gives `HYT00` as its example of a whole-function error. For a backend
///   answering [`crate::types::QueryTimeout::CoreCancels`], core arms a deadline over this call
///   and relabels the resulting failure `HYT00`; for a backend answering `DataSource`, the data
///   source's own timeout error is propagated. Armed here and not only at the
///   statement-producing calls because a data source may return column metadata long before it
///   computes a row, which puts the whole wait on the fetch.
/// - HYT01 (connection timeout expired): not implemented.
/// - IM001 (driver does not support this function): driver-manager-handled; not returned here.
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_fetch<B: Backend>(statement_handle: *mut c_void) -> SqlReturn {
    tracing::debug!("SQLFetch(stmt={:?})", statement_handle);
    // SAFETY: same preconditions as this function.
    let ret = unsafe { fetch_with_report::<B>(statement_handle, RowReport::Attributes) };
    tracing::debug!("SQLFetch -> {:?}", ret);
    ret
}

/// The shared body of `SQLFetch`, `SQLFetchScroll` and `SQLExtendedFetch`.
///
/// Carries no entry or exit logging: each public entry point logs under its own
/// name, so a delegating caller does not report the wrong function. Every
/// SQLSTATE it can produce is documented on [`sql_fetch`], whose diagnostics
/// table it implements.
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`, and `report`
/// must satisfy [`report_rows_fetched`]'s contract.
unsafe fn fetch_with_report<B: Backend>(
    statement_handle: *mut c_void,
    report: RowReport,
) -> SqlReturn {
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            scope
                .get::<StatementHandle<B>>(statement_handle)?
                .diagnostics
                .clear();

            // Everything this call needs from the ARD, read before the statement
            // is borrowed: the descriptor is its own allocation, and a bind
            // cannot happen while this call holds the connection's group, so one
            // read up front is the same answer as one read later.
            //
            // `SQL_ATTR_ROW_BIND_OFFSET_PTR` points at an `SQLULEN` the
            // application may change between fetches; the spec has the driver add
            // that value to every bound address rather than fold it into the
            // binding once. Read once per fetch so one row uses one offset
            // throughout.
            //
            // SAFETY: `row_bind_offset`'s contract — the stored attribute is null
            // or a pointer to a valid `SQLULEN`, which is the application's
            // undertaking when it sets the attribute.
            let (bind_offset, bindings) = {
                let ard = scope.desc_of::<B>(statement_handle, DescriptorRole::Ard)?;
                (row_bind_offset(ard), collect_bindings(ard))
            };

            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;

            // Read before the mutable borrow of `stmt.statement` below.
            let cursor_open = stmt.cursor_open;

            // `SQLFetch` consumes the cursor a previous execution opened, so it
            // observes *that* execution's token rather than minting one — the
            // token the backend was handed when it produced this statement.
            // `None` means no backend call has run here, so nothing could have
            // been cancelled. Resolved off the registry, which needs no borrow
            // of `stmt`.
            let cancel_token = crate::handles::current_cancel_token(statement_handle);
            let cancel = cancel_token
                .as_ref()
                .map(crate::handles::cancel_as::<B>)
                .transpose()?;

            // Core-enforced deadline, if the backend asked core to own one.
            //
            // Armed here and not only at the statement-producing calls because
            // `SQL_ATTR_QUERY_TIMEOUT` is a deadline on *returning the result
            // set*, and a data source is free to answer with column metadata
            // long before it has computed a row. Against such a source every
            // execute finishes in milliseconds and the whole wait lands on
            // `SQLFetch` — measured at 0.1s for the execute and 24.6s for the
            // following fetch, under a 2-second deadline. `SQLFetch`'s
            // diagnostics table carries `HYT00` with no `(DM)` marker for
            // exactly this attribute, so the site is the driver's to arm.
            //
            // Disarmed by `Drop` at the end of this scope, so a fetch that
            // returns promptly leaves no thread behind. `None` for the token
            // means no backend call has run on this statement yet, which is
            // also the case where there is nothing for a timer to cancel.
            let timer = match cancel_token.as_ref() {
                Some(token) => {
                    crate::query_timer::QueryTimer::arm::<B>(stmt.core_query_timeout, token)
                }
                None => crate::query_timer::QueryTimer::disarmed(),
            };

            // Spec HY010: the handle was never put in an executed state. Every
            // sentence of this SQLSTATE's row is (DM)-annotated; the check is
            // kept as defence in depth for a driver loaded without a Driver
            // Manager.
            let Some(ref mut statement) = stmt.statement else {
                return Err(OdbcError::general(
                    "No result set available; statement not executed",
                    SqlState::function_sequence_error(),
                ));
            };

            // Spec 24000, which carries no (DM) marker and so is the driver's
            // to return: "The StatementHandle was in an executed state but no
            // result set was associated with the StatementHandle."
            //
            // `cursor_open`, not `statement.is_some()`: a statement outlives
            // its cursor. `set_result_set` leaves `cursor_open` false when the
            // backend reports zero columns — an UPDATE, ODBC state S4 — and
            // `set_prepared_statement` leaves it false in the prepared states
            // S2/S3. Both keep a statement, so testing for one would drive the
            // backend in exactly the two states this SQLSTATE names.
            if !cursor_open {
                return Err(OdbcError::general(
                    "No cursor is open on this statement",
                    SqlState::invalid_cursor_state(),
                ));
            }

            // The row is about to change, so any `SQLGetData` position into the
            // outgoing row's value is meaningless against the incoming one.
            // Cleared before the fetch rather than after, so an error partway
            // through cannot leave a stale position behind.
            stmt.get_data_cursor = None;

            match timer.check_opt::<B, _, _>(statement.fetch(), cancel)? {
                FetchResult::Row => {
                    // Populate bound columns. The bindings were collected from
                    // the ARD before this borrow of `stmt.statement`; the offset
                    // is applied here, so one row uses one offset throughout.
                    //
                    // The application supplied both the base pointer and the
                    // offset, and the spec makes the sum its responsibility to
                    // keep in bounds — the same contract as the unoffset pointer.
                    // Byte arithmetic, because the offset is in bytes.
                    //
                    // A null pointer is left alone rather than shifted:
                    // `SQL_ATTR_ROW_BIND_OFFSET_PTR` shifts a *buffer*, and a
                    // null `SQL_DESC_DATA_PTR` or `SQL_DESC_INDICATOR_PTR` means
                    // there is no buffer to shift. `collect_bindings` admits a
                    // record with either pointer null — the indicator-only
                    // binding the spec allows — and offsetting `0` would turn
                    // that absence into a non-null address the 22002 check and
                    // `write_column_value` would then treat as real, writing
                    // through whatever the offset happens to land on.
                    let binding_info: Vec<(
                        u16,
                        odbc_sys::CDataType,
                        *mut c_void,
                        isize,
                        *mut isize,
                    )> = bindings
                        .iter()
                        .map(
                            |&(col, concise_type, data_ptr, octet_length, indicator_ptr)| {
                                Ok((
                                    col,
                                    c_type_of(concise_type)?,
                                    offset_non_null(data_ptr, bind_offset),
                                    octet_length,
                                    offset_non_null(indicator_ptr.cast::<c_void>(), bind_offset)
                                        .cast::<isize>(),
                                ))
                            },
                        )
                        .collect::<Result<Vec<_>, OdbcError>>()?;

                    let mut truncated = false;
                    if let Some(ref mut statement) = stmt.statement {
                        for (col, c_type, target_ptr, buf_len, ind_ptr) in &binding_info {
                            // Under the same deadline as the `fetch` above: these
                            // reads are part of `SQLFetch`'s own execution, so
                            // they fall under `SQLFetch`'s `HYT00` even though
                            // `SQLGetData` called directly has no such row.
                            let value = timer
                                .check_opt::<B, _, _>(statement.get_data(*col, *c_type), cancel)?;
                            // Spec 22002: if data is NULL and no indicator variable was supplied, return error.
                            if matches!(*value, ColumnValue::Null) && ind_ptr.is_null() {
                                return Err(OdbcError::general(
                                    format!(
                                        "Column {col} is NULL but no indicator variable was supplied (str_len_or_ind_ptr is null)"
                                    ),
                                    SqlState::indicator_variable_required(),
                                ));
                            }
                            let written = write_column_value(
                                &value,
                                *c_type,
                                *target_ptr,
                                *buf_len,
                                *ind_ptr,
                            )?;
                            if written == SqlReturn::SUCCESS_WITH_INFO {
                                truncated = true;
                            }
                        }
                    }

                    // Spec 01004: "If the data is truncated because the length of
                    // the data buffer is too small ... SQLFetch returns SQLSTATE
                    // 01004 (Data truncated) and SQL_SUCCESS_WITH_INFO."
                    // Returning plain SQL_SUCCESS would leave the application
                    // reading truncated data believing it complete.
                    //
                    // Fractional truncation (01S07) arrives as an `Err` from
                    // write_column_value and is handled by panic_safe.
                    if truncated {
                        // SAFETY: application-supplied pointers; see
                        // `report_rows_fetched`.
                        report_rows_fetched(stmt, report, 1, SQL_ROW_SUCCESS_WITH_INFO);
                        stmt.diagnostics.push(&OdbcError::StringTruncated);
                        return Ok(SqlReturn::SUCCESS_WITH_INFO);
                    }

                    // SAFETY: application-supplied pointers; see
                    // `report_rows_fetched`.
                    report_rows_fetched(stmt, report, 1, SQL_ROW_SUCCESS);
                    Ok(SqlReturn::SUCCESS)
                }
                FetchResult::NoData => {
                    // Spec: on SQL_NO_DATA the rows-fetched buffer is set to 0.
                    // The row-status array is left alone — with no row fetched
                    // there is no status to report, and SQL_ROW_NOROW describes
                    // an element of a rowset larger than the one row this
                    // driver ever produces.
                    // SAFETY: application-supplied pointer; see
                    // `report_rows_fetched`.
                    report_rows_fetched_only(stmt, report, 0);
                    Ok(SqlReturn::NO_DATA)
                }
            }
        })
    }
}

/// Generic implementation of SQLFetchScroll.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfetchscroll-function>
///
/// This driver supports forward-only cursors only. `SQL_FETCH_NEXT` delegates
/// to `sql_fetch`; all other orientations return HY106 (fetch type out of range).
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (input).
/// - `fetch_orientation`: Type of fetch — one of `SQL_FETCH_NEXT`, `SQL_FETCH_PRIOR`,
///   `SQL_FETCH_FIRST`, `SQL_FETCH_LAST`, `SQL_FETCH_ABSOLUTE`, `SQL_FETCH_RELATIVE`,
///   or `SQL_FETCH_BOOKMARK`. Only `SQL_FETCH_NEXT` is supported.
/// - `fetch_offset`: Row offset used with `SQL_FETCH_ABSOLUTE`, `SQL_FETCH_RELATIVE`, and
///   `SQL_FETCH_BOOKMARK`; ignored for all other orientations (input).
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; see `sql_fetch`.
/// - 01004 (string data right truncated): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 01S01 (error in row): not returned; see `sql_fetch`.
/// - 01S06 (attempt to fetch before result set returned first rowset): not applicable; only
///   `SQL_FETCH_NEXT` is supported, which cannot reach before-start.
/// - 01S07 (fractional truncation): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 07006 (restricted data type attribute violation): delegated to `sql_fetch`.
/// - 07009 (invalid descriptor index): not returned here, and the row carries no `(DM)`
///   marker; see `sql_fetch`, whose identical row it delegates to.
/// - 08S01 (communication link failure): delegated to `sql_fetch`.
/// - 22001 (string data right truncated for bookmark): not applicable; bookmarks are not
///   supported.
/// - 22002 (indicator variable required but not supplied): delegated to `sql_fetch` for
///   `SQL_FETCH_NEXT`.
/// - 22003 (numeric value out of range): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 22007 (invalid datetime format): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 22012 (division by zero): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 22015 (interval field overflow): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 22018 (invalid character value for cast specification): delegated to `sql_fetch` for
///   `SQL_FETCH_NEXT`.
/// - 24000 (invalid cursor state): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - 40001 (serialization failure): delegated to `sql_fetch`.
/// - 40003 (statement completion unknown): delegated to `sql_fetch`.
/// - HY000 (general error): propagated from the backend.
/// - HY001 (memory allocation error): not returned; Rust panics on allocation failure.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY010 (function sequence error): delegated to `sql_fetch` for `SQL_FETCH_NEXT`. (DM)
///   variants are driver-manager-handled; not returned here.
/// - HY013 (memory management error): not returned.
/// - HY090 (invalid string or buffer length): not applicable, and the row carries no `(DM)`
///   marker; see `sql_fetch`, whose identical row it delegates to.
/// - HY106 (fetch type out of range): **returned by this driver** for any `FetchOrientation`
///   other than `SQL_FETCH_NEXT`. The row is `(DM)`-marked on only two of its four clauses —
///   an invalid value, and `SQL_FETCH_BOOKMARK` with `SQL_UB_OFF` — and the two that are not
///   are the ones core acts on: `SQL_CURSOR_FORWARD_ONLY` with an orientation that is not
///   `SQL_FETCH_NEXT`, and the same for `SQL_NONSCROLLABLE`. This driver's cursors are
///   forward-only and non-scrollable, so both apply and `HY106` is the driver's to return.
/// - HY107 (row value out of range): not applicable; keyset cursors are not supported.
/// - HY111 (invalid bookmark value): not applicable; bookmarks are not supported.
/// - HY117 (connection suspended): driver-manager-handled; not returned here.
/// - HYC00 (optional feature not implemented): delegated to `sql_fetch` for `SQL_FETCH_NEXT`.
/// - HYT00 (timeout expired): **returned by this driver**, via the same delegation. The row
///   carries no `(DM)` marker here either, and `SQL_FETCH_NEXT` is the only orientation that
///   reaches the backend at all — every other one is rejected with `HY106` above — so arming
///   the deadline in `sql_fetch` covers this function completely.
/// - HYT01 (connection timeout expired): not implemented.
/// - IM001 (driver does not support this function): driver-manager-handled.
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_fetch_scroll<B: Backend>(
    statement_handle: *mut c_void,
    fetch_orientation: i16,
    fetch_offset: isize,
) -> SqlReturn {
    tracing::trace!(
        "SQLFetchScroll(stmt={:?}, orientation_raw={}, offset={})",
        statement_handle,
        fetch_orientation,
        fetch_offset
    );
    let orientation = fetch_orientation_from_raw(fetch_orientation);
    tracing::debug!(
        "SQLFetchScroll(stmt={:?}, orientation={:?}, offset={})",
        statement_handle,
        orientation,
        fetch_offset
    );
    let ret = if orientation == Some(FetchOrientation::Next) {
        // Forward-only: the one orientation this driver's cursors serve. Routed
        // through the shared body rather than through `sql_fetch`, so the exit
        // log below names this function — `sql_fetch` logs under its own name,
        // and a `return` through it left a trace claiming a `SQLFetch` the
        // application never called. Same reasoning, and the same shape, as
        // `sql_extended_fetch`.
        //
        // SAFETY: same preconditions as this function: statement_handle is null
        // or a valid StatementHandle<B> allocated by sql_alloc_handle.
        unsafe { fetch_with_report::<B>(statement_handle, RowReport::Attributes) }
    } else {
        // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
        // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
        unsafe {
            panic_safe::<B, _>(statement_handle, |scope| {
                let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
                // Spec: clear diagnostics at the start of each ODBC call. The
                // SQL_FETCH_NEXT branch above clears inside the shared body, so
                // only this non-delegating branch needs its own clear.
                stmt.diagnostics.clear();

                Err(OdbcError::general(
                    format!("SQLFetchScroll: unsupported fetch orientation {fetch_orientation}"),
                    SqlState::fetch_type_out_of_range(),
                ))
            })
        }
    };
    tracing::debug!("SQLFetchScroll -> {:?}", ret);
    ret
}

/// Generic implementation of SQLExtendedFetch.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlextendedfetch-function>
///
/// The ODBC 2.x block-fetch entry point. ODBC 3.x replaced it with
/// `SQLFetchScroll`, but the spec asks a 3.x driver to keep it: "ODBC 3.x
/// drivers should support **SQLExtendedFetch** if they want to work with ODBC
/// 2.x applications that call it." So a 2.x application calling it directly is
/// the only way this function is reached, and it is the only thing standing
/// between that application and a failed fetch. The Driver Manager's mapping in
/// the other direction does not help: it maps `SQLFetchScroll` onto
/// **SQLExtendedFetch** "when working with an ODBC 2.x *driver*", and core is a
/// 3.x driver, so that mapping never fires here.
///
/// Its Comments open with "the behavior of **SQLExtendedFetch** is identical to
/// that of **SQLFetchScroll**, with the following exceptions", and for a
/// forward-only driver with a rowset of one only two of those exceptions have
/// any effect — both about *where* the results are reported:
///
/// - the row count goes to `*RowCountPtr`, because that buffer "is used only by
///   **SQLExtendedFetch**. It is not used by **SQLFetch** or **SQLFetchScroll**";
/// - the row status goes to `RowStatusArray`, because "the address of this array
///   is not stored in the SQL_DESC_STATUS_ARRAY_PTR field in the IRD".
///
/// So this shares [`sql_fetch`]'s body through a caller-directed report and does
/// **not** touch `SQL_ATTR_ROWS_FETCHED_PTR` or `SQL_ATTR_ROW_STATUS_PTR`. The
/// remaining exceptions are inert here: bookmarks are unsupported, binding
/// offsets are read per fetch either way, and the rowset size —
/// `SQL_ROWSET_SIZE` rather than `SQL_ATTR_ROW_ARRAY_SIZE` — is pinned at 1 by
/// `ffi/stmt_attr.rs` through both doors.
///
/// # Parameters
///
/// - `statement_handle`: statement handle (SQL_HANDLE_STMT).
/// - `fetch_orientation`: `SQL_FETCH_NEXT`, `SQL_FETCH_PRIOR`,
///   `SQL_FETCH_FIRST`, `SQL_FETCH_LAST`, `SQL_FETCH_ABSOLUTE`,
///   `SQL_FETCH_RELATIVE` or `SQL_FETCH_BOOKMARK`. Only `SQL_FETCH_NEXT` is
///   supported. Note this is `SQLUSMALLINT` here where `SQLFetchScroll` types it
///   `SQLSMALLINT`.
/// - `fetch_offset`: row offset for the orientations that take one; ignored,
///   since none of those is supported.
/// - `row_count_ptr`: receives the number of rows fetched. May be null, in which
///   case nothing is written through it.
/// - `row_status_array`: receives the status of each row. May be null; the spec
///   calls the application's behaviour undefined in that case, and this driver
///   writes nothing rather than faulting.
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; driver-specific informational
///   messages are not generated.
/// - 01004 (string data right truncated): delegated to the shared fetch body for
///   `SQL_FETCH_NEXT`.
/// - 01S01 (error in row): not returned; only single-row rowsets are processed.
/// - 01S06 (attempt to fetch before the result set returned the first rowset):
///   not applicable; only `SQL_FETCH_NEXT` is supported, which cannot reach
///   before-start.
/// - 01S07 (fractional truncation): delegated to the shared fetch body.
/// - 07006 (restricted data type attribute violation): delegated, via
///   `write_column_value`.
/// - 07009 (invalid descriptor index): the row's single clause describes a
///   column 0 binding with `SQL_ATTR_USE_BOOKMARKS` off, and it carries no
///   `(DM)` marker, so this is the driver's answer to give. It cannot arise:
///   `sql_bind_col` refuses column 0 with `HYC00` before a binding is stored,
///   so there is no column-0 binding for this row to describe. Not returned
///   here.
/// - 08S01 (communication link failure): propagated from the backend.
/// - 22002 (indicator variable required but not supplied): delegated.
/// - 22003 (numeric value out of range): delegated, via `write_column_value`.
/// - 22007 (invalid datetime format): delegated.
/// - 22012 (division by zero): propagated from the backend if the data source
///   reports it.
/// - 22015 (interval field overflow): delegated.
/// - 22018 (invalid character value for cast specification): delegated.
/// - 24000 (invalid cursor state): delegated; returned when the statement was
///   executed but produced no result set. The row carries no (DM) marker.
/// - HY000 (general error): propagated from the backend.
/// - HY001 (memory allocation error): not returned; Rust panics on allocation
///   failure.
/// - HY008 (operation canceled): **returned by this driver**, via the shared
///   body. The row's asynchronous clauses do not apply — core implements no
///   asynchronous execution and never returns `SQL_STILL_EXECUTING` — but its
///   "from a different thread in a multithread application" clause carries no
///   (DM) marker and is honoured.
/// - HY010 (function sequence error): every clause on this row carries (DM),
///   including the "cannot be mixed with `SQLFetch`" ordering rule, so all are
///   driver-manager-handled. The shared body still returns `HY010` when no
///   result set is open, which is `SQLFetch`'s own unmarked clause.
/// - HY013 (memory management error): not returned.
/// - HY106 (fetch type out of range): **returned by this driver** for any
///   orientation other than `SQL_FETCH_NEXT`. The row's first clause carries
///   (DM), but the clause describing this driver — "The value of the
///   SQL_CURSOR_TYPE statement option was SQL_CURSOR_FORWARD_ONLY, and the value
///   of argument *FetchOrientation* was not SQL_FETCH_NEXT" — does not.
/// - HY107 (row value out of range): not applicable; keyset cursors are not
///   supported and `SQL_KEYSET_SIZE` is pinned at its default.
/// - HY111 (invalid bookmark value): not applicable; bookmarks are not
///   supported.
/// - HY117 (connection suspended): driver-manager-handled; not returned here.
/// - HYC00 (optional feature not implemented): delegated, via
///   `write_column_value`, for an unsupported conversion. The row's fetch-type
///   clause is answered with `HY106` instead, per the unmarked clause above.
/// - HYT00 (timeout expired): **returned by this driver**, via the shared body,
///   which arms the core-side deadline for a backend answering
///   [`crate::types::QueryTimeout::CoreCancels`].
/// - HYT01 (connection timeout expired): not implemented.
/// - IM001 (driver does not support this function): driver-manager-handled; not
///   returned here.
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `row_count_ptr` and `row_status_array` must each be null or point to a valid
/// writable `SQLULEN` / `SQLUSMALLINT`.
pub unsafe fn sql_extended_fetch<B: Backend>(
    statement_handle: *mut c_void,
    fetch_orientation: u16,
    fetch_offset: isize,
    row_count_ptr: *mut usize,
    row_status_array: *mut u16,
) -> SqlReturn {
    tracing::trace!(
        "SQLExtendedFetch(stmt={:?}, orientation_raw={}, offset={})",
        statement_handle,
        fetch_orientation,
        fetch_offset
    );
    // SQLExtendedFetch types FetchOrientation as SQLUSMALLINT where
    // SQLFetchScroll uses SQLSMALLINT. A value outside i16 cannot name any
    // orientation, so it narrows to the same "unrecognised" answer.
    let orientation = i16::try_from(fetch_orientation)
        .ok()
        .and_then(fetch_orientation_from_raw);
    tracing::debug!(
        "SQLExtendedFetch(stmt={:?}, orientation={:?}, offset={})",
        statement_handle,
        orientation,
        fetch_offset
    );

    let ret = if orientation == Some(FetchOrientation::Next) {
        // SAFETY: same preconditions as this function: statement_handle is null
        // or a valid StatementHandle<B>, and the two output pointers are null or
        // writable, which is `report_rows_fetched`'s contract.
        unsafe {
            fetch_with_report::<B>(
                statement_handle,
                RowReport::Arguments {
                    row_count: row_count_ptr,
                    row_status: row_status_array,
                },
            )
        }
    } else {
        // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated
        // by sql_alloc_handle; kind and group are validated by scope.get inside
        // the closure.
        unsafe {
            panic_safe::<B, _>(statement_handle, |scope| {
                let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
                // Spec: clear diagnostics at the start of each ODBC call. The
                // SQL_FETCH_NEXT branch above clears inside the shared body, so
                // only this non-delegating branch needs its own clear.
                stmt.diagnostics.clear();

                Err(OdbcError::general(
                    format!(
                        "SQLExtendedFetch: unsupported fetch orientation {fetch_orientation} \
                         (this driver's cursors are forward-only)"
                    ),
                    SqlState::fetch_type_out_of_range(),
                ))
            })
        }
    };
    tracing::debug!("SQLExtendedFetch -> {:?}", ret);
    ret
}

/// Generic implementation of SQLGetData.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetdata-function>
///
/// Retrieves data for a single column in the current row. The data is
/// converted to the requested C type and written into the caller's buffer.
///
/// # Retrieving variable-length data in parts
///
/// A character or binary column too large for the caller's buffer is returned
/// over several calls. Each call delivers the next part and returns
/// `SQL_SUCCESS_WITH_INFO` with `01004`; the call that delivers the last part
/// returns `SQL_SUCCESS`; a further call returns `SQL_NO_DATA`. `*StrLen_or_Ind`
/// carries the length *still to come at the start of that call*, so it shrinks
/// as the loop proceeds. This is what makes the documented application pattern
///
/// ```text
/// while ((rc = SQLGetData(...)) == SQL_SUCCESS_WITH_INFO) { /* append */ }
/// ```
///
/// terminate. A call with a non-null buffer and `buffer_length` 0 — the
/// standard length-probe idiom, used to size a buffer before a second call
/// fetches the value — is one iteration of this same loop, not a special
/// case: it reports `SQL_SUCCESS_WITH_INFO` with `01004` and the byte length
/// still needed, and the column remains fully readable afterwards. A
/// **null** buffer also stays plain `SQL_SUCCESS`, but this function's own
/// spec has no such case (`target_value_ptr` "cannot be NULL"); see the
/// `01004` entry below for where that branch is actually exercised.
///
/// Three limits come straight from the spec rather than from this
/// implementation. Fixed-width targets cannot be read in parts at all — the
/// second call for one returns `SQL_NO_DATA`. The position is per *statement*,
/// not per column: reading a different column discards it, so
/// `SQLGetData(n)`, `SQLGetData(m)`, `SQLGetData(n)` restarts column `n` from
/// the beginning. And it is dropped whenever the cursor moves or the result set
/// is discarded, since an offset into one row's value means nothing in the next.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (input).
/// - `col_or_param_num`: Column number (1-based) of the column to retrieve. Column 0 is the
///   bookmark column and is not supported (input).
/// - `target_type`: C data type identifier for the target buffer (input).
/// - `target_value_ptr`: Pointer to the buffer to receive the data (output). Must not be null.
/// - `buffer_length`: Length in bytes of the `*target_value_ptr` buffer (input).
/// - `str_len_or_ind_ptr`: Pointer to the buffer to receive the length or indicator value
///   (output). May be null if the caller does not need the length or indicator.
///
/// # Spec compliance
///
/// - 01000 (general warning): not returned; driver-specific informational messages are not
///   generated.
/// - 01004 (string data right truncated): returned via `write_column_value` when character
///   or binary data does not fit the buffer; diagnostic is pushed to the statement queue.
///   **`buffer_length == 0` with a non-null `target_value_ptr` is this case, not
///   `SQL_SUCCESS`** — the standard "how large a buffer do I need" probe supplies somewhere
///   to write and nothing is written there, which is total truncation per the spec's own step
///   5: "If the data buffer supplied is too small to hold the null-termination character,
///   SQLGetData returns SQL_SUCCESS_WITH_INFO and SQLSTATE 01004." A **null** `target_value_ptr`
///   still stays plain `SQL_SUCCESS`, but that is not this function's own spec sanctioning a null
///   buffer — the Arguments section says plainly "TargetValuePtr cannot be NULL," and the
///   `HY009` clause for that below is simply not checked. The writers
///   (`write_wchar`/`write_char`/`write_binary` in `column_value.rs`) are shared with
///   `sql_fetch`'s bound-column loop, where a null `SQL_DESC_DATA_PTR` paired with a live
///   `SQL_DESC_INDICATOR_PTR` is the spec-legal indicator-only binding, and that caller still
///   needs the length written with nothing to write the data into. Getting the `buffer_length ==
///   0` half of this wrong made the second half of the documented probe-then-fetch idiom —
///   `SQLGetData(col, type, buf, 0, &ind)` to size the buffer, then a second call with a
///   `buf`-sized allocation — return `SQL_NO_DATA`, because `SQL_SUCCESS` marks a chunkable
///   column exhausted (`cursor.done` below).
/// - 01S07 (fractional truncation): returned via `write_column_value` for numeric fractional
///   truncation, and when a non-zero `ColumnValue::Time` fraction is dropped writing to
///   `SQL_C_TYPE_TIME` (`SQL_TIME_STRUCT` has no fraction field).
/// - 07006 (restricted data type attribute violation): returned via `write_column_value`'s
///   two "unsupported conversion" fallthroughs — the column value's variant has no defined
///   conversion to the requested C type (e.g. `Bytes` requested as a numeric C type, or any
///   value/target-type combination not covered by a specific arm).
/// - 07009 (invalid descriptor index): the row's first three clauses carry no `(DM)` marker
///   and are the driver's. The column-0 one is **returned by this driver**, when
///   `col_or_param_num` is 0 (bookmark). The one naming a column number greater than the number
///   of columns in the result set is delegated to the backend, which returns `HY000` rather
///   than `07009`; a precise check would require an extra round-trip to obtain the column
///   count. The third names a parameter ordinal, which cannot arise because core returns no
///   streamed output parameters. The five clauses that follow — bound column, column ordering,
///   ARD consistency and ARD count — are all `(DM)`-marked and not returned here.
/// - 08S01 (communication link failure): propagated from the backend.
/// - 22002 (indicator variable required but not supplied): returned when the column value
///   is NULL and `str_len_or_ind_ptr` is null.
/// - 22003 (numeric value out of range): returned via `write_column_value` when a numeric
///   pivot does not fit the requested C target type.
/// - 22007 (invalid datetime format): returned via `write_column_value` when character data
///   parses as a date/time/timestamp but a field value is out of range (e.g. month 13, or a
///   numeric field too large for its target width, e.g. year `"99999"`). Per this function's
///   diagnostics table this code (like 22018) is scoped to a character column source; a
///   non-character `ColumnValue` (e.g. a plain integer or float) requested as a datetime C type
///   has no defined conversion at all and instead falls into the generic 07006 case above. A
///   backend whose data source stores a datetime column using some other, numeric physical
///   encoding is responsible for decoding it into a proper `ColumnValue::Date`/`Time`/`Timestamp`
///   at fetch time, before it ever reaches this function — stackable-odbc-core has no such backend-specific
///   knowledge.
/// - 22012 (division by zero): propagated from the backend.
/// - 22015 (interval field overflow): not returned; `write_column_value` has no interval C type
///   arms, so a request for one falls through to the generic 07006 case above.
/// - 22018 (invalid character value for cast specification): returned via `write_column_value`
///   when character data does not parse as the requested numeric or datetime C type.
/// - 24000 (invalid cursor state): returned when no cursor is open (`stmt.cursor_open` is
///   `false`), which includes a statement that is only prepared, one that executed without
///   producing a result set, and one whose cursor `SQLEndTran` closed under `SQL_CB_CLOSE`.
///   Two of the row's three clauses are `(DM)`-marked — not yet fetched, and an executed
///   statement with no result set — and are not returned here. The third carries no marker
///   and is the driver's: a cursor open and fetched, but positioned before the start of the
///   result set or after its end. It cannot arise, because this driver's cursor is
///   forward-only and never rests outside the result set with a value still to read.
/// - HY000 (general error): propagated from the backend; `write_column_value` does not produce
///   this code — its coercion-failure paths return 07006 (see above).
/// - HY001 (memory allocation error): not returned; Rust panics on allocation failure.
/// - HY003 (program type out of range): returned both when `target_type` is not a recognized C
///   data type, and via `write_column_value`'s numeric-pivot catch-all for a `CDataType` with no
///   numeric arm. (DM) variants (column 0 with wrong bookmark type) are driver-manager-handled.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY009 (invalid use of null pointer): not checked; `target_value_ptr` null is not
///   validated. (DM) — driver-manager-handled.
/// - HY010 (function sequence error): driver-manager-handled; not returned here.
/// - HY013 (memory management error): not returned.
/// - HY090 (invalid string or buffer length): **returned by this driver** when
///   `buffer_length < 0`. That clause is `(DM)`-marked, and the check is kept anyway because
///   the Arguments section attributes it to the driver in as many words: "**SQLGetData**
///   returns SQLSTATE HY090 (Invalid string or buffer length) when *BufferLength* is less
///   than 0 but not when *BufferLength* is 0." So a negative length is refused here and a
///   zero length is accepted, which is what that sentence asks for — "accepted" meaning this
///   check does not fire and the call proceeds, not that it always answers `SQL_SUCCESS`: for a
///   chunkable target a zero length still reaches the writer, which now reports `01004` (see
///   above) rather than silently discarding the column. The row's second clause carries no
///   marker and is the driver's — a `BufferLength` less than 4 with `Col_or_Param_Num` 0 on an
///   ODBC 2.x driver — and cannot arise, since core is a 3.x driver and refuses column 0
///   regardless.
/// - HY109 (invalid cursor position): not checked; detecting deleted/unfetchable rows
///   requires backend support.
/// - HY117 (connection suspended): driver-manager-handled; not returned here.
/// - HYC00 (optional feature not implemented): not returned by `write_column_value`; its
///   unsupported-conversion paths return 07006 (see above).
/// - HYT01 (connection timeout expired): not implemented.
/// - HYT00 is **absent from this function's diagnostics table** — deliberately, and not an
///   oversight in this list. `SQLFetch` and `SQLFetchScroll` both carry it; `SQLGetData` carries
///   only `HYT01`. So core arms no `SQL_ATTR_QUERY_TIMEOUT` deadline here, and a driver must not
///   add one: the query timeout governs returning the result set, which has already happened by
///   the time this function is reachable. The bound-column reads that run *inside* `SQLFetch` are
///   a different matter and do fall under that call's deadline.
/// - IM001 (driver does not support this function): driver-manager-handled.
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is not
///   supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification model
///   is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `target_value_ptr` and `str_len_or_ind_ptr` must be valid writable pointers
/// (or null where documented by the ODBC spec).
pub unsafe fn sql_get_data<B: Backend>(
    statement_handle: *mut c_void,
    col_or_param_num: u16,
    target_type: i16,
    target_value_ptr: *mut c_void,
    buffer_length: isize,
    str_len_or_ind_ptr: *mut isize,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetData(stmt={:?}, col={}, target_type_raw={}, buf_len={:?})",
        statement_handle,
        col_or_param_num,
        target_type,
        buffer_length
    );
    let c_type_log = crate::types::c_data_type_from_raw(target_type);
    tracing::debug!(
        "SQLGetData(stmt={:?}, col={}, c_type={:?})",
        statement_handle,
        col_or_param_num,
        c_type_log
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle; kind and group are validated by scope.get inside the closure.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec 24000 / HY010: No cursor open. A statement that is only
            // prepared, or whose cursor `SQLEndTran` closed under
            // `SQL_CB_CLOSE`, still holds a backend statement but has no cursor
            // to read from.
            if !stmt.cursor_open {
                return Err(OdbcError::general(
                    "No cursor is open",
                    SqlState::invalid_cursor_state(),
                ));
            }
            // As in `SQLFetch`: this consumes a cursor an earlier execution
            // opened, so it observes that execution's token rather than minting
            // one. Resolved before the mutable borrow below, off the registry.
            let cancel_token = crate::handles::current_cancel_token(statement_handle);
            let cancel = cancel_token
                .as_ref()
                .map(crate::handles::cancel_as::<B>)
                .transpose()?;

            // `cursor_open` implies `statement.is_some()`; this arm keeps the
            // invariant honest rather than unwrapping.
            let Some(ref mut statement) = stmt.statement else {
                return Err(OdbcError::general(
                    "No cursor is open",
                    SqlState::invalid_cursor_state(),
                ));
            };

            // Spec 07009: Column number 0 means bookmark, which we don't support.
            if col_or_param_num == 0 {
                return Err(OdbcError::general(
                    "Column number 0 (bookmark) is not supported",
                    SqlState::invalid_descriptor_index(),
                ));
            }

            // Spec HY090: buffer_length must be >= 0.
            if buffer_length < 0 {
                return Err(OdbcError::general(
                    format!("Invalid buffer_length: {buffer_length}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            }

            let c_type = crate::types::c_data_type_from_raw(target_type).ok_or_else(|| {
                OdbcError::general(
                    format!("Unknown C data type: {target_type}"),
                    SqlState::invalid_application_buffer_type(),
                )
            })?;

            // Spec, "Retrieving Data with SQLGetData" step 1: "Returns
            // SQL_NO_DATA if it has already returned all of the data for the
            // column." This precedes even the NULL check, so it is the first
            // thing done once the arguments are known good.
            //
            // A cursor for a *different* column is discarded rather than kept:
            // "Successive calls to SQLGetData will retrieve data from the last
            // column requested; prior offsets become invalid."
            let mut cursor = match stmt.get_data_cursor {
                Some(c) if c.column == col_or_param_num => c,
                _ => GetDataCursor {
                    column: col_or_param_num,
                    delivered: 0,
                    done: false,
                },
            };
            if cursor.done {
                stmt.get_data_cursor = Some(cursor);
                return Ok(SqlReturn::NO_DATA);
            }

            let value = reclassify_cancelled_opt::<B, _, _>(
                statement.get_data(col_or_param_num, c_type),
                cancel,
            )?;
            // Spec 22002: data is NULL but no indicator variable was supplied.
            if matches!(*value, ColumnValue::Null) && str_len_or_ind_ptr.is_null() {
                return Err(OdbcError::general(
                    "Data is NULL but no indicator variable was supplied (str_len_or_ind_ptr is null)",
                    SqlState::indicator_variable_required(),
                ));
            }
            let write = write_column_value_at(
                &value,
                c_type,
                target_value_ptr,
                buffer_length,
                str_len_or_ind_ptr,
                cursor.delivered,
            )?;

            // A fixed-width target is finished by definition — the spec forbids
            // reading it in parts — and so is a chunkable one that reported
            // anything other than truncation, since SQL_SUCCESS marks the last
            // part. Either way the next call for this column is SQL_NO_DATA.
            cursor.delivered += write.delivered;
            cursor.done = !write.chunkable || write.ret != SqlReturn::SUCCESS_WITH_INFO;
            stmt.get_data_cursor = Some(cursor);

            // Spec 01004: If data was truncated, push a diagnostic.
            if write.ret == SqlReturn::SUCCESS_WITH_INFO {
                stmt.diagnostics.push(&OdbcError::StringTruncated);
            }

            Ok(write.ret)
        })
    };
    tracing::debug!("SQLGetData -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use crate::ffi::handle::{sql_alloc_handle, sql_free_handle};
    use crate::test_utils::{
        LONG_BYTES, LONG_TEXT, MockBackend, MockCancelAwareBackend, MockCancelToken,
        MockConnection, MockError, MockFetchTimeoutBackend, MockLongDataBackend,
        alloc_env_conn_stmt, cleanup_env_conn_stmt, with_descriptor, with_handle,
    };
    use crate::types::CDataType;
    use odbc_sys::HandleType;

    /// Env + connection + statement for an arbitrary backend, connected and
    /// executed so a cursor is open and the execution's cancel token exists.
    ///
    /// `fetch.rs`'s other helpers are `MockBackend`- or
    /// `MockLongDataBackend`-specific, and neither can be made to fail a fetch
    /// on demand.
    unsafe fn executed_stmt_for<B: Backend>() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Env as i16, std::ptr::null_mut(), &mut env);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Dbc as i16, env, &mut conn);
            let wide: Vec<u16> = "Host=localhost;Database=test".encode_utf16().collect();
            assert_eq!(
                crate::ffi::connect::sql_driver_connect_w::<B>(
                    conn,
                    std::ptr::null_mut(),
                    wide.as_ptr(),
                    i16::try_from(wide.len()).expect("connection string fits in i16"),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = sql_alloc_handle::<B>(HandleType::Stmt as i16, conn, &mut stmt);

            let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<B>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("SQL fits in i32"),
                ),
                SqlReturn::SUCCESS,
                "precondition: a cursor is open and a cancel token exists",
            );
            (env, conn, stmt)
        }
    }

    unsafe fn cleanup_stmt_for<B: Backend>(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<B>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<B>(conn);
            let _ = sql_free_handle::<B>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<B>(HandleType::Env as i16, env);
        }
    }

    /// Spec, `SQLFetch` `HY008` — the row carries no `(DM)` marker, and its
    /// second clause is exactly this crate's cross-thread cancel: "the
    /// `SQLFetch` function was called, and before it completed execution,
    /// `SQLCancel` … was called on the `StatementHandle` from a different
    /// thread in a multithread application."
    ///
    /// `SQLFetch` has no token of its own — `StatementBackend::fetch` takes
    /// none — so this also pins that core reads the token the *producing*
    /// execution minted.
    #[test]
    fn a_cancelled_fetch_reports_hy008() {
        unsafe {
            let (env, conn, stmt) = executed_stmt_for::<MockCancelAwareBackend>();

            MockCancelAwareBackend::fail_next_fetch();
            assert_eq!(
                crate::ffi::cursor::sql_cancel::<MockCancelAwareBackend>(stmt),
                SqlReturn::SUCCESS,
            );

            let ret = sql_fetch::<MockCancelAwareBackend>(stmt);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockCancelAwareBackend, StatementHandle<MockCancelAwareBackend>, _>(
                stmt,
                |h| {
                    let rec = h.diagnostics.get(0).expect("record 1 exists");
                    assert_eq!(rec.sqlstate.as_str(), "HY008");
                },
            );
            cleanup_stmt_for::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// The other half: a fetch that fails without a cancel keeps the backend's
    /// own SQLSTATE. Reclassifying unconditionally would relabel every fetch
    /// error in the crate as `HY008`.
    #[test]
    fn an_uncancelled_fetch_failure_keeps_its_own_state() {
        unsafe {
            let (env, conn, stmt) = executed_stmt_for::<MockCancelAwareBackend>();

            MockCancelAwareBackend::fail_next_fetch();

            let ret = sql_fetch::<MockCancelAwareBackend>(stmt);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockCancelAwareBackend, StatementHandle<MockCancelAwareBackend>, _>(
                stmt,
                |h| {
                    let rec = h.diagnostics.get(0).expect("record 1 exists");
                    assert_ne!(rec.sqlstate.as_str(), "HY008");
                },
            );
            cleanup_stmt_for::<MockCancelAwareBackend>(env, conn, stmt);
        }
    }

    /// Env + connection + statement for [`MockLongDataBackend`], executed and
    /// positioned on the first row, so `SQLGetData` has something to read.
    unsafe fn long_data_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
        let (env, conn, stmt) = unsafe { long_data_stmt_no_fetch() };
        unsafe {
            let ret = sql_fetch::<MockLongDataBackend>(stmt);
            assert_eq!(ret, SqlReturn::SUCCESS, "precondition: first row fetched");
        }
        (env, conn, stmt)
    }

    /// Env + connection + statement, executed but with the cursor still before
    /// the first row.
    unsafe fn long_data_stmt_no_fetch() -> (*mut c_void, *mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let mut conn: *mut c_void = std::ptr::null_mut();
        let mut stmt: *mut c_void = std::ptr::null_mut();
        unsafe {
            let _ = sql_alloc_handle::<MockLongDataBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let _ = sql_alloc_handle::<MockLongDataBackend>(HandleType::Dbc as i16, env, &mut conn);
            let mut wide: Vec<u16> = "DRIVER=mock;".encode_utf16().collect();
            wide.push(0);
            let _ = crate::ffi::connect::sql_driver_connect_w::<MockLongDataBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                crate::types::SQL_NTS as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            let _ =
                sql_alloc_handle::<MockLongDataBackend>(HandleType::Stmt as i16, conn, &mut stmt);

            let mut sql: Vec<u16> = "SELECT a, b, c FROM t".encode_utf16().collect();
            sql.push(0);
            let ret = crate::ffi::execute::sql_exec_direct_w::<MockLongDataBackend>(
                stmt,
                sql.as_ptr(),
                crate::types::SQL_NTS,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "precondition: execute");
        }
        (env, conn, stmt)
    }

    unsafe fn cleanup_long_data(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockLongDataBackend>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<MockLongDataBackend>(conn);
            let _ = sql_free_handle::<MockLongDataBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockLongDataBackend>(HandleType::Env as i16, env);
        }
    }

    /// A delegating entry point must still log under its own name. The
    /// `SQL_FETCH_NEXT` branch used to `return` through `sql_fetch`, so the only
    /// branch that actually fetches never reached `SQLFetchScroll`'s own exit
    /// log and a trace read `SQLFetch -> SUCCESS` for a call the application
    /// never made. `fetch_with_report` exists precisely so this cannot happen.
    #[test]
    fn fetch_scroll_logs_its_own_return_value() {
        use crate::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::{Context, Layer};
        use tracing_subscriber::prelude::*;

        /// Collects the `message` field of every event, so the test can assert
        /// on which function name the exit log carried.
        struct MessageCollector(Arc<Mutex<Vec<String>>>);

        struct MessageVisitor<'a>(&'a mut Option<String>);

        impl Visit for MessageVisitor<'_> {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    *self.0 = Some(format!("{value:?}"));
                }
            }
        }

        impl<S: tracing::Subscriber> Layer<S> for MessageCollector {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                let mut message = None;
                event.record(&mut MessageVisitor(&mut message));
                if let Some(m) = message {
                    self.0.lock().expect("collector mutex").push(m);
                }
            }
        }

        let messages: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(MessageCollector(Arc::clone(&messages)));

        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            tracing::subscriber::with_default(subscriber, || {
                assert_eq!(
                    sql_fetch_scroll::<MockLongDataBackend>(stmt, FetchOrientation::Next as i16, 0,),
                    SqlReturn::SUCCESS
                );
            });

            cleanup_long_data(env, conn, stmt);
        }

        let messages = messages.lock().expect("collector mutex");
        assert!(
            messages.iter().any(|m| m.contains("SQLFetchScroll -> ")),
            "SQLFetchScroll never logged its own return value: {messages:?}",
        );
        assert!(
            !messages.iter().any(|m| m.contains("SQLFetch -> ")),
            "the exit log named the delegate rather than the caller: {messages:?}",
        );
    }

    /// One `SQLGetData` call for `col` into a `buf_len`-byte `SQL_C_CHAR`
    /// buffer, returning the code, the bytes before the null terminator, and the
    /// indicator.
    unsafe fn get_char_chunk(
        stmt: *mut c_void,
        col: u16,
        buf_len: usize,
    ) -> (SqlReturn, Vec<u8>, isize) {
        let mut buf = vec![0u8; buf_len];
        let mut ind: isize = 0;
        let ret = unsafe {
            sql_get_data::<MockLongDataBackend>(
                stmt,
                col,
                CDataType::Char as i16,
                buf.as_mut_ptr().cast::<c_void>(),
                buf_len as isize,
                &mut ind,
            )
        };
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        (ret, buf, ind)
    }

    /// The canonical ODBC long-data loop must terminate and reassemble the
    /// value. Before chunking existed every call restarted at the beginning, so
    /// this loop returned `SQL_SUCCESS_WITH_INFO` forever and an application
    /// following the documented pattern hung.
    #[test]
    fn get_data_returns_a_long_value_in_parts_until_the_loop_terminates() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            // 8-byte buffer => 7 payload bytes per call plus a null terminator.
            let mut assembled = Vec::new();
            let mut calls = 0;
            loop {
                let (ret, chunk, _) = get_char_chunk(stmt, 1, 8);
                calls += 1;
                assert!(
                    calls <= 100,
                    "SQLGetData never reported completion; the loop does not terminate"
                );
                assert!(
                    ret == SqlReturn::SUCCESS || ret == SqlReturn::SUCCESS_WITH_INFO,
                    "unexpected return {ret:?} on call {calls}"
                );
                assembled.extend_from_slice(&chunk);
                if ret == SqlReturn::SUCCESS {
                    break;
                }
            }

            assert_eq!(
                String::from_utf8(assembled).expect("chunks reassemble to UTF-8"),
                LONG_TEXT,
                "the reassembled value differs from what the backend served"
            );
            assert!(
                calls > 1,
                "precondition: the buffer must be small enough to force chunking"
            );

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// Spec: "If SQLGetData is called after this, it returns SQL_NO_DATA."
    #[test]
    fn get_data_after_the_last_part_returns_no_data() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            // Bounded, like every other chunk loop in this module: an
            // implementation that never reports completion must fail the test
            // rather than hang the job until CI's timeout kills it.
            let mut drained = false;
            for _ in 0..100 {
                let (ret, _, _) = get_char_chunk(stmt, 1, 8);
                if ret == SqlReturn::SUCCESS {
                    drained = true;
                    break;
                }
            }
            assert!(drained, "SQLGetData never reported the last part");

            let (ret, _, _) = get_char_chunk(stmt, 1, 8);
            assert_eq!(ret, SqlReturn::NO_DATA);
            // And it stays NO_DATA rather than restarting.
            let (ret, _, _) = get_char_chunk(stmt, 1, 8);
            assert_eq!(ret, SqlReturn::NO_DATA);

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// Task 1.3 / B2: a length-probe call — `SQLGetData` with a non-null
    /// buffer and `BufferLength` 0, the documented idiom for "tell me how
    /// much room I need" — must not consume the column. Spec step 5: "If the
    /// data buffer supplied is too small to hold the null-termination
    /// character, SQLGetData returns SQL_SUCCESS_WITH_INFO and SQLSTATE
    /// 01004." A zero-length buffer is always too small to hold it, so this
    /// is the same case as any other total truncation and must leave the
    /// cursor resumable (`cursor.done` false), not mark the column
    /// exhausted the way a bare `SQL_SUCCESS` would.
    #[test]
    fn get_data_length_probe_with_zero_buffer_keeps_the_column_readable_wchar() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            // Non-null target, zero BufferLength: the probe.
            let mut probe_buf = [0xAAu16; 4];
            let mut ind: isize = 0;
            let ret = sql_get_data::<MockLongDataBackend>(
                stmt,
                1,
                CDataType::WChar as i16,
                probe_buf.as_mut_ptr().cast::<c_void>(),
                0,
                &mut ind,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(
                first_sqlstate::<MockLongDataBackend>(stmt),
                "01004",
                "a zero-length buffer is total truncation, not a plain success"
            );
            assert_eq!(ind, (LONG_TEXT.len() * 2) as isize, "byte length reported");
            assert_eq!(
                probe_buf, [0xAAu16; 4],
                "nothing written into the probe buffer"
            );

            // The column must still be readable: a full-size buffer now gets
            // the complete value, not SQL_NO_DATA.
            let mut full_buf = vec![0u16; LONG_TEXT.len() + 1];
            let mut ind2: isize = -1;
            let ret2 = sql_get_data::<MockLongDataBackend>(
                stmt,
                1,
                CDataType::WChar as i16,
                full_buf.as_mut_ptr().cast::<c_void>(),
                (full_buf.len() * 2) as isize,
                &mut ind2,
            );
            assert_eq!(
                ret2,
                SqlReturn::SUCCESS,
                "the data must still be retrievable"
            );
            let end = full_buf
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(full_buf.len());
            assert_eq!(
                String::from_utf16(&full_buf[..end]).unwrap(),
                LONG_TEXT,
                "the complete value must arrive, not an empty or partial one"
            );
            assert_eq!(ind2, (LONG_TEXT.len() * 2) as isize);

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// `SQL_C_CHAR` sibling of
    /// `get_data_length_probe_with_zero_buffer_keeps_the_column_readable_wchar`.
    #[test]
    fn get_data_length_probe_with_zero_buffer_keeps_the_column_readable_char() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            let mut probe_buf = [0xAAu8; 4];
            let mut ind: isize = 0;
            let ret = sql_get_data::<MockLongDataBackend>(
                stmt,
                1,
                CDataType::Char as i16,
                probe_buf.as_mut_ptr().cast::<c_void>(),
                0,
                &mut ind,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(first_sqlstate::<MockLongDataBackend>(stmt), "01004");
            assert_eq!(ind, LONG_TEXT.len() as isize);
            assert_eq!(
                probe_buf, [0xAAu8; 4],
                "nothing written into the probe buffer"
            );

            let mut full_buf = vec![0u8; LONG_TEXT.len() + 1];
            let mut ind2: isize = -1;
            let ret2 = sql_get_data::<MockLongDataBackend>(
                stmt,
                1,
                CDataType::Char as i16,
                full_buf.as_mut_ptr().cast::<c_void>(),
                full_buf.len() as isize,
                &mut ind2,
            );
            assert_eq!(
                ret2,
                SqlReturn::SUCCESS,
                "the data must still be retrievable"
            );
            let end = full_buf
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(full_buf.len());
            assert_eq!(
                std::str::from_utf8(&full_buf[..end]).unwrap(),
                LONG_TEXT,
                "the complete value must arrive, not an empty or partial one"
            );
            assert_eq!(ind2, LONG_TEXT.len() as isize);

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// `SQL_C_BINARY` sibling. Binary reserves no null terminator, so the
    /// "no room to make progress" condition is `buf_len <= 0` alone, and the
    /// second call's buffer is sized exactly to the data (no `+1`).
    #[test]
    fn get_data_length_probe_with_zero_buffer_keeps_the_column_readable_binary() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            let mut probe_buf = [0xAAu8; 4];
            let mut ind: isize = 0;
            let ret = sql_get_data::<MockLongDataBackend>(
                stmt,
                3,
                CDataType::Binary as i16,
                probe_buf.as_mut_ptr().cast::<c_void>(),
                0,
                &mut ind,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(first_sqlstate::<MockLongDataBackend>(stmt), "01004");
            assert_eq!(ind, LONG_BYTES.len() as isize);
            assert_eq!(
                probe_buf, [0xAAu8; 4],
                "nothing written into the probe buffer"
            );

            let mut full_buf = vec![0u8; LONG_BYTES.len()];
            let mut ind2: isize = -1;
            let ret2 = sql_get_data::<MockLongDataBackend>(
                stmt,
                3,
                CDataType::Binary as i16,
                full_buf.as_mut_ptr().cast::<c_void>(),
                full_buf.len() as isize,
                &mut ind2,
            );
            assert_eq!(
                ret2,
                SqlReturn::SUCCESS,
                "the data must still be retrievable"
            );
            assert_eq!(full_buf, LONG_BYTES, "the complete value must arrive");
            assert_eq!(ind2, LONG_BYTES.len() as isize);

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// Spec: "SQLGetData cannot be used to return fixed-length data in parts. If
    /// SQLGetData is called more than one time in a row for a column containing
    /// fixed-length data, it returns SQL_NO_DATA for all calls after the first."
    #[test]
    fn get_data_for_fixed_width_data_returns_no_data_after_the_first_call() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            let mut value: i32 = 0;
            let mut ind: isize = 0;
            let value_ptr = std::ptr::from_mut(&mut value).cast::<c_void>();
            let first = sql_get_data::<MockLongDataBackend>(
                stmt,
                2,
                CDataType::SLong as i16,
                value_ptr,
                4,
                &mut ind,
            );
            assert_eq!(first, SqlReturn::SUCCESS);
            assert_eq!(value, 4242);

            let second = sql_get_data::<MockLongDataBackend>(
                stmt,
                2,
                CDataType::SLong as i16,
                value_ptr,
                4,
                &mut ind,
            );
            assert_eq!(second, SqlReturn::NO_DATA);

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// Spec: "Successive calls to SQLGetData will retrieve data from the last
    /// column requested; prior offsets become invalid ... the second call to
    /// SQLGetData(icol=n) retrieves data from the start of the n column."
    ///
    /// A per-column position would preserve the offset here and fail.
    #[test]
    fn reading_another_column_restarts_the_first_one_from_the_beginning() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            let (ret, first, _) = get_char_chunk(stmt, 1, 8);
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(first, LONG_TEXT.as_bytes()[..7]);

            // Touch a different column, which invalidates column 1's offset.
            let (_, _, _) = get_char_chunk(stmt, 3, 8);

            let (ret, again, _) = get_char_chunk(stmt, 1, 8);
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(
                again, first,
                "column 1 resumed mid-value instead of restarting"
            );

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// Spec step 7: "When SQLGetData is called multiple times in succession for
    /// the same column, this is the length of the data available at the start of
    /// the current call; that is, the length decreases with each subsequent
    /// call." Reporting the whole value's length every time would leave an
    /// application unable to size its final read.
    #[test]
    fn the_indicator_reports_the_length_remaining_not_the_total() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            let total = LONG_TEXT.len() as isize;
            let (ret, _, first_ind) = get_char_chunk(stmt, 1, 8);
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(
                first_ind, total,
                "the first call still reports the whole length"
            );

            let (_, _, second_ind) = get_char_chunk(stmt, 1, 8);
            assert_eq!(
                second_ind,
                total - 7,
                "the indicator did not decrease by the 7 bytes already delivered"
            );

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// A position into the previous row's value means nothing in the next row.
    #[test]
    fn fetching_the_next_row_restarts_the_column() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            let (ret, first, _) = get_char_chunk(stmt, 1, 8);
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);

            assert_eq!(sql_fetch::<MockLongDataBackend>(stmt), SqlReturn::SUCCESS);

            let (ret, after_fetch, _) = get_char_chunk(stmt, 1, 8);
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(
                after_fetch, first,
                "the new row resumed at the previous row's offset"
            );

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// `SQL_C_BINARY` reserves no null terminator, so it delivers `BufferLength`
    /// bytes per call rather than `BufferLength - 1`. Chunking has to follow the
    /// writer's own arithmetic rather than assume the character case.
    #[test]
    fn binary_data_is_returned_in_parts_and_reassembles() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt();

            let mut assembled = Vec::new();
            let mut calls = 0;
            loop {
                let mut buf = [0u8; 5];
                let mut ind: isize = 0;
                let ret = sql_get_data::<MockLongDataBackend>(
                    stmt,
                    3,
                    CDataType::Binary as i16,
                    buf.as_mut_ptr().cast::<c_void>(),
                    5,
                    &mut ind,
                );
                calls += 1;
                assert!(calls <= 100, "binary chunk loop does not terminate");
                let take = if ret == SqlReturn::SUCCESS {
                    ind as usize
                } else {
                    5
                };
                assembled.extend_from_slice(&buf[..take]);
                if ret == SqlReturn::SUCCESS {
                    break;
                }
            }

            assert_eq!(assembled, LONG_BYTES);
            assert!(calls > 1, "precondition: chunking must actually occur");

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// `SQL_ATTR_ROWS_FETCHED_PTR` and `SQL_ATTR_ROW_STATUS_PTR` were accepted
    /// and then ignored, so an application driving its loop off the fetched
    /// count read whatever it had initialised the variable to — forever, if it
    /// waited for zero.
    #[test]
    fn fetch_reports_rows_fetched_and_row_status_through_the_bound_pointers() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            let mut rows_fetched: usize = 999;
            let mut row_status: u16 = 999;
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockLongDataBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::RowsFetchedPtr as i32,
                    std::ptr::from_mut(&mut rows_fetched).cast::<c_void>(),
                    0,
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockLongDataBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::RowStatusPtr as i32,
                    std::ptr::from_mut(&mut row_status).cast::<c_void>(),
                    0,
                ),
                SqlReturn::SUCCESS
            );

            assert_eq!(sql_fetch::<MockLongDataBackend>(stmt), SqlReturn::SUCCESS);
            assert_eq!(rows_fetched, 1, "rows-fetched buffer not written");
            assert_eq!(row_status, SQL_ROW_SUCCESS, "row-status buffer not written");

            // Drain to SQL_NO_DATA; the spec sets rows-fetched to 0 there.
            assert_eq!(sql_fetch::<MockLongDataBackend>(stmt), SqlReturn::SUCCESS);
            assert_eq!(sql_fetch::<MockLongDataBackend>(stmt), SqlReturn::NO_DATA);
            assert_eq!(rows_fetched, 0, "rows-fetched not zeroed at end of cursor");

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// The other half of the split: [`RowReport::Arguments`] writes through the
    /// caller's own pointers and leaves the statement attributes alone. The
    /// spec makes them separate storage — that buffer "is used only by
    /// **SQLExtendedFetch**" — so a destination that wrote both would satisfy
    /// the attribute test above and still be wrong.
    #[test]
    fn a_report_to_arguments_bypasses_the_statement_attributes() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            let mut attr_rows: usize = 999;
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockLongDataBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::RowsFetchedPtr as i32,
                    std::ptr::from_mut(&mut attr_rows).cast::<c_void>(),
                    0,
                ),
                SqlReturn::SUCCESS
            );

            let mut arg_rows: usize = 0;
            let mut arg_status: u16 = 0;
            assert_eq!(
                fetch_with_report::<MockLongDataBackend>(
                    stmt,
                    RowReport::Arguments {
                        row_count: std::ptr::from_mut(&mut arg_rows),
                        row_status: std::ptr::from_mut(&mut arg_status),
                    },
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(arg_rows, 1, "the argument is the destination");
            assert_eq!(arg_status, SQL_ROW_SUCCESS);
            assert_eq!(attr_rows, 999, "the attribute must be untouched");

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// `SQL_FETCH_NEXT` is the one orientation a forward-only cursor serves, and
    /// the count and status must land in the *arguments*.
    #[test]
    fn extended_fetch_next_reports_through_its_arguments() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            let mut rows_fetched: usize = 999;
            let mut row_status: u16 = 999;
            assert_eq!(
                sql_extended_fetch::<MockLongDataBackend>(
                    stmt,
                    FetchOrientation::Next as u16,
                    0,
                    std::ptr::from_mut(&mut rows_fetched),
                    std::ptr::from_mut(&mut row_status),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(rows_fetched, 1, "RowCountPtr not written");
            assert_eq!(row_status, SQL_ROW_SUCCESS, "RowStatusArray not written");

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// The spec: the `RowCountPtr` buffer "is used only by **SQLExtendedFetch**",
    /// and the status array's address "is not stored in the
    /// SQL_DESC_STATUS_ARRAY_PTR field in the IRD". So the statement attributes
    /// must be left untouched, even when the application has set them.
    #[test]
    fn extended_fetch_does_not_write_the_statement_attributes() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            let mut attr_rows: usize = 999;
            let mut attr_status: u16 = 999;
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockLongDataBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::RowsFetchedPtr as i32,
                    std::ptr::from_mut(&mut attr_rows).cast::<c_void>(),
                    0,
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockLongDataBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::RowStatusPtr as i32,
                    std::ptr::from_mut(&mut attr_status).cast::<c_void>(),
                    0,
                ),
                SqlReturn::SUCCESS
            );

            let mut arg_rows: usize = 0;
            let mut arg_status: u16 = 0;
            assert_eq!(
                sql_extended_fetch::<MockLongDataBackend>(
                    stmt,
                    FetchOrientation::Next as u16,
                    0,
                    std::ptr::from_mut(&mut arg_rows),
                    std::ptr::from_mut(&mut arg_status),
                ),
                SqlReturn::SUCCESS
            );

            assert_eq!(arg_rows, 1, "the arguments are the destination");
            assert_eq!(arg_status, SQL_ROW_SUCCESS);
            assert_eq!(
                attr_rows, 999,
                "SQL_ATTR_ROWS_FETCHED_PTR must be untouched"
            );
            assert_eq!(
                attr_status, 999,
                "SQL_ATTR_ROW_STATUS_PTR must be untouched"
            );

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// Every orientation but `SQL_FETCH_NEXT` is `HY106` — and, unlike the macro
    /// stub this replaces, with a diagnostic the application can actually read.
    #[test]
    fn extended_fetch_rejects_every_other_orientation_with_a_readable_hy106() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            // Named rather than looped over the enum: `SQL_FETCH_BOOKMARK` has no
            // `odbc_sys::FetchOrientation` variant, and it is the one the spec's
            // HY106 row calls out for a forward-only cursor alongside the rest.
            let rejected: [(u16, &str); 6] = [
                (FetchOrientation::Prior as u16, "SQL_FETCH_PRIOR"),
                (FetchOrientation::First as u16, "SQL_FETCH_FIRST"),
                (FetchOrientation::Last as u16, "SQL_FETCH_LAST"),
                (FetchOrientation::Absolute as u16, "SQL_FETCH_ABSOLUTE"),
                (FetchOrientation::Relative as u16, "SQL_FETCH_RELATIVE"),
                (
                    crate::types::SQL_FETCH_BOOKMARK as u16,
                    "SQL_FETCH_BOOKMARK",
                ),
            ];

            for (orientation, name) in rejected {
                let mut rows_fetched: usize = 999;
                assert_eq!(
                    sql_extended_fetch::<MockLongDataBackend>(
                        stmt,
                        orientation,
                        0,
                        std::ptr::from_mut(&mut rows_fetched),
                        std::ptr::null_mut(),
                    ),
                    SqlReturn::ERROR,
                    "{name} must be rejected"
                );

                let mut state = [0u16; 6];
                let mut msg = [0u16; 256];
                let mut native: i32 = 0;
                let mut msg_len: i16 = 0;
                assert_eq!(
                    crate::ffi::diag::sql_get_diag_rec_w::<MockLongDataBackend>(
                        HandleType::Stmt as i16,
                        stmt,
                        1,
                        state.as_mut_ptr(),
                        std::ptr::from_mut(&mut native),
                        msg.as_mut_ptr(),
                        256,
                        std::ptr::from_mut(&mut msg_len),
                    ),
                    SqlReturn::SUCCESS,
                    "{name} returned SQL_ERROR with no diagnostic"
                );
                let sqlstate = String::from_utf16_lossy(&state[..5]);
                assert_eq!(sqlstate, "HY106", "{name} reported {sqlstate}");
            }

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// The spec calls the application's behaviour undefined when it supplies no
    /// `RowStatusArray`. Declining to write through a null target is the
    /// discipline `write_column_value` already follows, and it must not fault.
    #[test]
    fn extended_fetch_tolerates_null_output_arguments() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            assert_eq!(
                sql_extended_fetch::<MockLongDataBackend>(
                    stmt,
                    FetchOrientation::Next as u16,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// Draining the cursor returns `SQL_NO_DATA` and zeroes the count, as the
    /// spec's rows-fetched rule requires. The status array is left alone: with no
    /// row there is no status to report.
    #[test]
    fn extended_fetch_zeroes_the_count_at_end_of_cursor() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            let mut rows_fetched: usize = 999;
            let mut row_status: u16 = 42;
            let next = FetchOrientation::Next as u16;
            let count = std::ptr::from_mut(&mut rows_fetched);
            let status = std::ptr::from_mut(&mut row_status);

            assert_eq!(
                sql_extended_fetch::<MockLongDataBackend>(stmt, next, 0, count, status),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                sql_extended_fetch::<MockLongDataBackend>(stmt, next, 0, count, status),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                sql_extended_fetch::<MockLongDataBackend>(stmt, next, 0, count, status),
                SqlReturn::NO_DATA
            );
            assert_eq!(rows_fetched, 0, "count not zeroed at end of cursor");

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// The stub this replaces returned `SQL_ERROR` for a bad handle. Going
    /// through `panic_safe` is what makes it `SQL_INVALID_HANDLE`.
    #[test]
    fn extended_fetch_rejects_an_invalid_handle() {
        unsafe {
            assert_eq!(
                sql_extended_fetch::<MockBackend>(
                    std::ptr::null_mut(),
                    FetchOrientation::Next as u16,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                SqlReturn::INVALID_HANDLE
            );
        }
    }

    /// A record that exists with neither `SQL_DESC_DATA_PTR` nor
    /// `SQL_DESC_INDICATOR_PTR` is not a binding, and `SQLFetch` must pass over
    /// it entirely.
    ///
    /// `SQLSetDescField` creates such a record by setting any single field, so
    /// presence in the map does not by itself mean a binding. The record with a
    /// null data pointer *and* a live indicator is a different thing — the spec
    /// makes it a legal `SQLBindCol` state and `SQLFetch` serves it; see
    /// [`fetch_writes_the_indicator_of_an_indicator_only_binding`].
    ///
    /// Driven on `MockLongDataBackend` rather than `MockBackend`: the record is
    /// inserted for column 2, which really does carry a value, so a `SQLFetch`
    /// that treated the record as a binding would have something to write.
    ///
    /// This test cannot fail by mutating the filter, and that is a property of
    /// the state rather than a weakness that can be tested away: a record with
    /// neither pointer has nothing to write *through*, so admitting it costs a
    /// wasted column read and produces no observable difference. It is kept as
    /// a statement of the rule, and the filter's other half — the indicator-only
    /// record — is what carries the mutation coverage.
    #[test]
    fn fetch_skips_a_record_that_carries_neither_pointer() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            // Sentinel: no ODBC length is negative, so any write is visible.
            // The pointer is kept out of the record on purpose — this test is
            // about a record with no indicator at all — and used only to prove
            // afterwards that nothing wrote through the neighbourhood.
            let mut indicator: isize = -99;
            let indicator_ptr = std::ptr::from_mut(&mut indicator);

            with_descriptor::<MockLongDataBackend, _>(
                stmt,
                crate::descriptor::DescriptorRole::Ard,
                |ard| {
                    ard.records.insert(
                        2,
                        crate::descriptor::DescriptorRecord {
                            concise_type: CDataType::SLong as i16,
                            verbose_type: CDataType::SLong as i16,
                            octet_length: 4,
                            ..Default::default()
                        },
                    );
                },
            );

            assert_eq!(sql_fetch::<MockLongDataBackend>(stmt), SqlReturn::SUCCESS);
            assert_eq!(
                std::ptr::read(indicator_ptr),
                -99,
                "SQLFetch wrote through a record that is not a binding"
            );

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// An indicator-only binding is what `SQLBindCol` leaves behind when the
    /// application passes a null `TargetValuePtr` and a real `StrLen_or_IndPtr`,
    /// and `SQLFetch` must serve it: the length is written, the value is not.
    ///
    /// `write_column_value` already declines the null target and writes the
    /// indicator unconditionally, so the only thing that stood in the way was
    /// `collect_bindings` filtering the record out before it got there.
    #[test]
    fn fetch_writes_the_indicator_of_an_indicator_only_binding() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            // Sentinel: no ODBC length is negative, so any write is visible.
            let mut indicator: isize = -99;
            let indicator_ptr = std::ptr::from_mut(&mut indicator);

            // Column 2 is the fixed-width i32 4242, so there is a length to
            // report and a value that would be written if the null target were
            // ignored.
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockLongDataBackend>(
                    stmt,
                    2,
                    CDataType::SLong as i16,
                    std::ptr::null_mut(),
                    0,
                    indicator_ptr,
                ),
                SqlReturn::SUCCESS,
            );

            assert_eq!(sql_fetch::<MockLongDataBackend>(stmt), SqlReturn::SUCCESS);
            assert_eq!(
                std::ptr::read(indicator_ptr),
                isize::try_from(std::mem::size_of::<i32>()).expect("small"),
                "SQLFetch did not write the length of a data-unbound column",
            );

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// `SQL_ATTR_ROW_BIND_OFFSET_PTR` was accepted and ignored, so a bound
    /// column landed at the base address instead of the offset one — the
    /// application reads the wrong slot of its own buffer and never learns why.
    #[test]
    fn fetch_applies_the_row_bind_offset_to_bound_columns() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            // Two i32 slots; bind column 2 (the fixed-width 4242) to slot 0 and
            // then shift writes into slot 1 with an offset.
            //
            // Every access after the bind goes through `slots_ptr`, never
            // through `slots`. The driver holds that raw pointer across the
            // fetches, and writing through the local would invalidate it under
            // Stacked Borrows — Miri rejects the test, not the driver. An ODBC
            // application has no such constraint: it owns the buffer outright.
            let mut slots: [i32; 2] = [0, 0];
            let mut indicators: [isize; 2] = [0, 0];
            let slots_ptr = slots.as_mut_ptr();
            let offset = std::mem::size_of::<i32>();
            let mut bind_offset: usize = 0;
            let slot = |i: usize| std::ptr::read(slots_ptr.add(i));

            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockLongDataBackend>(
                    stmt,
                    2,
                    CDataType::SLong as i16,
                    slots_ptr.cast::<c_void>(),
                    4,
                    indicators.as_mut_ptr(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockLongDataBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::RowBindOffsetPtr as i32,
                    std::ptr::from_mut(&mut bind_offset).cast::<c_void>(),
                    0,
                ),
                SqlReturn::SUCCESS
            );

            // Offset 0: the value lands in slot 0.
            assert_eq!(sql_fetch::<MockLongDataBackend>(stmt), SqlReturn::SUCCESS);
            assert_eq!((slot(0), slot(1)), (4242, 0));

            // Clear through the same pointer the driver was given, then move the
            // offset — which is the whole reason the attribute is a pointer *to*
            // the offset rather than the offset itself.
            std::ptr::write(slots_ptr, 0);
            std::ptr::write(slots_ptr.add(1), 0);
            bind_offset = offset;
            assert_eq!(sql_fetch::<MockLongDataBackend>(stmt), SqlReturn::SUCCESS);
            assert_eq!(
                (slot(0), slot(1)),
                (0, 4242),
                "the bind offset was ignored; the value landed at the base address"
            );
            // `sql_fetch` reads `bind_offset` through the raw pointer it was
            // given, which `unused_assignments` cannot see; assert on it so the
            // store is observable to the compiler as well as to the driver.
            assert_eq!(bind_offset, offset);

            cleanup_long_data(env, conn, stmt);
        }
    }

    // -------------------------------------------------------------------
    // MockNullBackend — a single row whose one column is `ColumnValue::Null`
    // -------------------------------------------------------------------

    /// A one-column statement whose row is always `ColumnValue::Null`.
    ///
    /// No mock in `test_utils.rs` returns `ColumnValue::Null` from `get_data`
    /// — every one of them stands for a real value `SQLGetData` can read —
    /// and this module's tests are scoped to `fetch.rs`, so this one is local
    /// rather than added there.
    struct MockNullStatement {
        rows_left: u8,
    }

    impl StatementBackend for MockNullStatement {
        type Error = MockError;

        fn column_count(&self) -> i16 {
            1
        }

        fn fetch(&mut self) -> Result<FetchResult, Self::Error> {
            if self.rows_left == 0 {
                return Ok(FetchResult::NoData);
            }
            self.rows_left -= 1;
            Ok(FetchResult::Row)
        }

        fn get_data(
            &mut self,
            _col: u16,
            _target_type: CDataType,
        ) -> Result<Cow<'_, ColumnValue>, Self::Error> {
            Ok(Cow::Owned(ColumnValue::Null))
        }
    }

    /// Hands out one [`MockNullStatement`] per execution. Every capability
    /// declaration below is the least-capable value the spec defines (see
    /// `test_utils.rs`'s `minimal_capability_decls!` for the same list) —
    /// this mock exists only to drive `get_data` down the `ColumnValue::Null`
    /// branch, so none of them is exercised by the tests that use it.
    struct MockNullBackend;

    impl Backend for MockNullBackend {
        type Connection = MockConnection;
        type Statement = MockNullStatement;
        type Error = MockError;
        type CancelToken = MockCancelToken;

        fn connect(_: &crate::types::ConnectParams) -> Result<Self::Connection, Self::Error> {
            Ok(MockConnection)
        }
        fn disconnect(_: &mut Self::Connection) -> Result<(), Self::Error> {
            Ok(())
        }
        fn cancel_token(_conn: &Self::Connection) -> Self::CancelToken {
            MockCancelToken::default()
        }
        fn exec_direct(
            _: &Self::Connection,
            _: &Self::CancelToken,
            _: &str,
        ) -> Result<Self::Statement, Self::Error> {
            Ok(MockNullStatement { rows_left: 1 })
        }
        fn prepare(
            _: &Self::Connection,
            _: &Self::CancelToken,
            _: &str,
        ) -> Result<Self::Statement, Self::Error> {
            Ok(MockNullStatement { rows_left: 1 })
        }
        fn execute(
            _: &Self::Connection,
            _: &Self::CancelToken,
            _: &mut Self::Statement,
            _: &[ColumnValue],
        ) -> Result<crate::types::ExecuteOutcome, Self::Error> {
            Ok(crate::types::ExecuteOutcome::default())
        }
        fn get_info(
            _: &Self::Connection,
            _: crate::types::InfoType,
        ) -> Result<crate::types::InfoValue, Self::Error> {
            Err(MockError)
        }
        fn get_functions() -> Cow<'static, [crate::function_id::FunctionId]> {
            Cow::Borrowed(&[])
        }
        fn get_type_info(_conn: &Self::Connection) -> Cow<'static, [crate::types::TypeInfoRow]> {
            Cow::Borrowed(&[])
        }
        fn tables(
            _: &Self::Connection,
            _: &Self::CancelToken,
            _: &crate::types::TablesQuery<'_>,
        ) -> Result<Vec<crate::types::TableRow>, Self::Error> {
            Err(MockError)
        }
        fn columns(
            _: &Self::Connection,
            _: &Self::CancelToken,
            _: &crate::types::ColumnsQuery<'_>,
        ) -> Result<Vec<crate::types::ColumnRow>, Self::Error> {
            Err(MockError)
        }
        fn table_types(_conn: &Self::Connection) -> Vec<Cow<'static, str>> {
            Vec::new()
        }

        fn supports_catalogs(_conn: &Self::Connection) -> bool {
            false
        }
        fn supports_schemas(_conn: &Self::Connection) -> bool {
            false
        }
        fn alter_table_support(_conn: &Self::Connection) -> u32 {
            0
        }
        fn outer_join_capabilities(_conn: &Self::Connection) -> u32 {
            0
        }
        fn default_txn_isolation(_conn: &Self::Connection) -> u32 {
            0
        }
        fn txn_isolation_options(_conn: &Self::Connection) -> u32 {
            0
        }
        fn group_by(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_GB_NOT_SUPPORTED
        }
        fn null_collation(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_NC_HIGH
        }
        fn identifier_case(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_IC_SENSITIVE
        }
        fn correlation_name(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_CN_NONE
        }
        fn non_nullable_columns(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_NNC_NULL
        }
        fn expressions_in_order_by(_conn: &Self::Connection) -> bool {
            false
        }
        fn sql_conformance(_conn: &Self::Connection) -> u32 {
            0
        }
        fn timedate_add_intervals(_conn: &Self::Connection) -> u32 {
            0
        }
        fn timedate_diff_intervals(_conn: &Self::Connection) -> u32 {
            0
        }
        fn subqueries(_conn: &Self::Connection) -> u32 {
            0
        }
        fn column_alias(_conn: &Self::Connection) -> bool {
            false
        }
        fn concat_null_behavior(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_CB_NULL
        }
        fn union_support(_conn: &Self::Connection) -> u32 {
            0
        }
        fn convert_functions(_conn: &Self::Connection) -> u32 {
            0
        }
        fn order_by_columns_in_select(_conn: &Self::Connection) -> bool {
            true
        }
        fn accessible_tables(_conn: &Self::Connection) -> bool {
            false
        }
        fn data_source_read_only(_conn: &Self::Connection) -> bool {
            false
        }
        fn search_pattern_escape(_conn: &Self::Connection) -> Cow<'static, str> {
            Cow::Borrowed("")
        }
        fn keywords(_conn: &Self::Connection) -> Cow<'static, [Cow<'static, str>]> {
            Cow::Borrowed(&[])
        }
        fn quoted_identifier_case(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_IC_SENSITIVE
        }
        fn txn_capable(_conn: &Self::Connection) -> u16 {
            crate::types::SQL_TC_NONE as u16
        }
        fn integrity(_conn: &Self::Connection) -> bool {
            false
        }
        fn multiple_active_txn(_conn: &Self::Connection) -> bool {
            false
        }
        fn special_characters(_conn: &Self::Connection) -> Cow<'static, str> {
            Cow::Borrowed("")
        }
        fn accessible_procedures(_conn: &Self::Connection) -> bool {
            false
        }
        fn driver_name() -> Cow<'static, str> {
            Cow::Borrowed("Mock ODBC Driver")
        }
        fn driver_version() -> Cow<'static, str> {
            Cow::Borrowed("00.00.0000")
        }
        fn dbms_name(_conn: &Self::Connection) -> Cow<'static, str> {
            Cow::Borrowed("MockDB")
        }
        fn dbms_version(_conn: &Self::Connection) -> Cow<'static, str> {
            Cow::Borrowed("00.00.0000")
        }
    }

    /// Env + connection + statement for [`MockNullBackend`], executed but with
    /// the cursor still before the first row — the same shape as
    /// `long_data_stmt_no_fetch`, for a backend whose one row is NULL instead
    /// of a real value.
    unsafe fn null_stmt_no_fetch() -> (*mut c_void, *mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let mut conn: *mut c_void = std::ptr::null_mut();
        let mut stmt: *mut c_void = std::ptr::null_mut();
        unsafe {
            let _ = sql_alloc_handle::<MockNullBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let _ = sql_alloc_handle::<MockNullBackend>(HandleType::Dbc as i16, env, &mut conn);
            let mut wide: Vec<u16> = "DRIVER=mock;".encode_utf16().collect();
            wide.push(0);
            let _ = crate::ffi::connect::sql_driver_connect_w::<MockNullBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                crate::types::SQL_NTS as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            let _ = sql_alloc_handle::<MockNullBackend>(HandleType::Stmt as i16, conn, &mut stmt);

            let mut sql: Vec<u16> = "SELECT a FROM t".encode_utf16().collect();
            sql.push(0);
            let ret = crate::ffi::execute::sql_exec_direct_w::<MockNullBackend>(
                stmt,
                sql.as_ptr(),
                crate::types::SQL_NTS,
            );
            assert_eq!(ret, SqlReturn::SUCCESS, "precondition: execute");
        }
        (env, conn, stmt)
    }

    unsafe fn cleanup_null_stmt(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockNullBackend>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<MockNullBackend>(conn);
            let _ = sql_free_handle::<MockNullBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockNullBackend>(HandleType::Env as i16, env);
        }
    }

    /// B1: `SQL_ATTR_ROW_BIND_OFFSET_PTR` must not turn a null
    /// `SQL_DESC_INDICATOR_PTR` into a non-null one.
    ///
    /// The column has a real, bound data buffer but no indicator — the
    /// application never supplied one — so the fetched `NULL` value must be
    /// reported as spec 22002 ("indicator variable required but not
    /// supplied"), exactly as it would be with no offset in force at all. Before
    /// the fix, `indicator_ptr.wrapping_byte_add(64)` turned the null pointer
    /// into address `0x40`: the 22002 check saw a non-null pointer and skipped
    /// the error, and `write_column_value` then wrote the `SQL_NULL_DATA`
    /// sentinel through that wild address.
    #[test]
    fn row_bind_offset_does_not_offset_a_null_indicator_pointer() {
        unsafe {
            let (env, conn, stmt) = null_stmt_no_fetch();

            let mut data_buf: i32 = -1;
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockNullBackend>(
                    stmt,
                    1,
                    CDataType::SLong as i16,
                    std::ptr::from_mut(&mut data_buf).cast::<c_void>(),
                    4,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS,
            );

            // A live SQLULEN the driver reads through on every fetch.
            let mut bind_offset: usize = 64;
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockNullBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::RowBindOffsetPtr as i32,
                    std::ptr::from_mut(&mut bind_offset).cast::<c_void>(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );

            assert_eq!(sql_fetch::<MockNullBackend>(stmt), SqlReturn::ERROR);
            assert_eq!(
                first_sqlstate::<MockNullBackend>(stmt),
                "22002",
                "a bound column with no indicator must report 22002 for a NULL \
                 value regardless of SQL_ATTR_ROW_BIND_OFFSET_PTR",
            );

            cleanup_null_stmt(env, conn, stmt);
        }
    }

    /// B1: `SQL_ATTR_ROW_BIND_OFFSET_PTR` must not turn a null
    /// `SQL_DESC_DATA_PTR` into a non-null one.
    ///
    /// Indicator-only binding — `TargetValuePtr` is null, `StrLen_or_IndPtr` is
    /// real — is the spec-legal state
    /// [`fetch_writes_the_indicator_of_an_indicator_only_binding`] already
    /// covers without an offset; this pins the same behaviour with a non-zero
    /// `SQL_ATTR_ROW_BIND_OFFSET_PTR` in force. Before the fix,
    /// `data_ptr.wrapping_byte_add(offset)` turned the null data pointer into a
    /// non-null address, and `write_column_value` attempted to write the
    /// column's real (non-NULL) value through it.
    ///
    /// `data_canary` stands in for the Miri wild-write demonstration the plan
    /// called for: it is never bound to anything, so if a future regression
    /// resurrected the null data pointer and wrote through the wrong address,
    /// this is the buffer most likely to sit in the corrupted neighbourhood.
    #[test]
    fn row_bind_offset_does_not_offset_a_null_data_pointer() {
        unsafe {
            let (env, conn, stmt) = long_data_stmt_no_fetch();

            let data_canary: [u8; 8] = [0xAB; 8];

            // Two slots, so the offset can be proven to have moved the write
            // from slot 0 to slot 1 rather than merely "somewhere".
            let mut indicators: [isize; 2] = [0, 0];
            let indicators_ptr = indicators.as_mut_ptr();
            let offset = std::mem::size_of::<isize>();

            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockLongDataBackend>(
                    stmt,
                    2,
                    CDataType::SLong as i16,
                    std::ptr::null_mut(),
                    0,
                    indicators_ptr,
                ),
                SqlReturn::SUCCESS,
            );

            let mut bind_offset: usize = offset;
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockLongDataBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::RowBindOffsetPtr as i32,
                    std::ptr::from_mut(&mut bind_offset).cast::<c_void>(),
                    0,
                ),
                SqlReturn::SUCCESS,
            );

            assert_eq!(sql_fetch::<MockLongDataBackend>(stmt), SqlReturn::SUCCESS);
            assert_eq!(
                (
                    std::ptr::read(indicators_ptr),
                    std::ptr::read(indicators_ptr.add(1))
                ),
                (0, 4),
                "the indicator must land at base + offset, carrying column 2's \
                 4-byte length, and slot 0 must stay untouched",
            );
            assert_eq!(
                data_canary, [0xAB; 8],
                "a null SQL_DESC_DATA_PTR must stay null under an offset; no \
                 write should have reached this buffer at all",
            );

            cleanup_long_data(env, conn, stmt);
        }
    }

    /// Read the SQLSTATE of the statement's first diagnostic record.
    ///
    /// Generic over the backend because the token names a
    /// `StatementHandle<B>`, and reading it back as any other `B` is unsound —
    /// see `handles/scope.rs`. Every call site must name the same backend it
    /// allocated the statement against.
    unsafe fn first_sqlstate<B: Backend>(stmt: *mut c_void) -> String {
        let mut state = [0u16; 6];
        let mut native: i32 = 0;
        let mut msg = [0u16; 256];
        let mut msg_len: i16 = 0;
        let ret = unsafe {
            crate::ffi::diag::sql_get_diag_rec_w::<B>(
                HandleType::Stmt as i16,
                stmt,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                msg.len() as i16,
                &mut msg_len,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS, "no diagnostic record was posted");
        String::from_utf16_lossy(&state[..5])
    }

    #[test]
    fn fetch_after_a_statement_with_no_result_set_returns_24000() {
        // Spec 24000, which carries no (DM) marker: "The StatementHandle was in
        // an executed state but no result set was associated with the
        // StatementHandle."
        //
        // `MockStatement` reports zero columns, so this stands for an UPDATE:
        // ODBC state S4, executed with no cursor. Both states checked here keep
        // a statement on the handle, which is what makes `statement.is_some()`
        // the wrong guard and this test worth having.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let input = "Host=localhost;Port=8080;Database=test;User=me";
            let wide: Vec<u16> = input.encode_utf16().collect();
            assert_eq!(
                crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
                    conn,
                    std::ptr::null_mut(),
                    wide.as_ptr(),
                    wide.len() as i16,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    0,
                ),
                SqlReturn::SUCCESS
            );

            let sql = "UPDATE t SET a = 1";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_prepare_w::<MockBackend>(
                    stmt,
                    wide.as_ptr(),
                    wide.len() as i32
                ),
                SqlReturn::SUCCESS
            );

            // Prepared but not executed: states S2/S3, no cursor.
            assert_eq!(sql_fetch::<MockBackend>(stmt), SqlReturn::ERROR);
            assert_eq!(first_sqlstate::<MockBackend>(stmt), "24000");

            // Executed, produced no result set: state S4, still no cursor.
            assert_eq!(
                crate::ffi::execute::sql_execute::<MockBackend>(stmt),
                SqlReturn::SUCCESS
            );
            assert_eq!(sql_fetch::<MockBackend>(stmt), SqlReturn::ERROR);
            assert_eq!(first_sqlstate::<MockBackend>(stmt), "24000");

            // SQLDisconnect frees every statement on the connection as a side
            // effect, so `stmt` is already gone by here — pass null rather
            // than the now-stale token, per `cleanup_env_conn_stmt`'s
            // documented "must be live, not already freed" precondition.
            let _ = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            cleanup_env_conn_stmt(env, conn, std::ptr::null_mut());
        }
    }

    #[test]
    fn fetch_without_execute_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_fetch::<MockBackend>(stmt);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn get_data_without_cursor_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut buf: i32 = 0;
            let mut ind: isize = 0;
            let ret = sql_get_data::<MockBackend>(
                stmt,
                1,
                CDataType::SLong as i16,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn fetch_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_fetch::<MockBackend>(std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn fetch_scroll_unsupported_orientation_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // SQL_FETCH_PRIOR (4) is not supported by forward-only cursor
            let ret = sql_fetch_scroll::<MockBackend>(stmt, 4, 0);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A stale record from a failed call must not still be on the queue during
    /// a later one; the non-delegating orientation branch must clear on entry
    /// just like every other statement-level function.
    #[test]
    fn fetch_scroll_clears_diagnostics_from_an_earlier_call() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let ret = sql_fetch_scroll::<MockBackend>(stmt, FetchOrientation::Prior as i16, 0);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                assert_eq!(h.diagnostics.len(), 1, "precondition: a record is queued");
            });

            let ret = sql_fetch_scroll::<MockBackend>(stmt, FetchOrientation::Prior as i16, 0);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                assert_eq!(
                    h.diagnostics.len(),
                    1,
                    "the queue must be cleared at entry, not appended to"
                );
            });

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn get_data_negative_buffer_length_without_cursor_returns_error() {
        // Without a cursor open, sql_get_data returns ERROR (HY010).
        // The HY090 buffer_length check fires only when a cursor is open.
        // This test verifies existing behavior is not disturbed.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_get_data::<MockBackend>(
                stmt,
                1,
                CDataType::Char as i16,
                std::ptr::null_mut(),
                -1, // negative — HY090 check fires after cursor check
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn get_data_col_zero_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Manually set a statement to have a cursor open
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.set_result_set(crate::handles::StatementData::Synthetic(
                    crate::test_utils::synthetic_result_set(vec![vec![
                        crate::types::ColumnValue::I32(42),
                    ]]),
                ));
            });

            let mut buf: i32 = 0;
            let mut ind: isize = 0;
            let ret = sql_get_data::<MockBackend>(
                stmt,
                0, // bookmark column — not supported
                CDataType::SLong as i16,
                &mut buf as *mut i32 as *mut c_void,
                4,
                &mut ind,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The Trino case: a data source that answers with column metadata long
    /// before it computes a row, so `SQLExecDirect` is fast and the whole wait
    /// lands on `SQLFetch`.
    ///
    /// `execute.rs`'s `an_execution_that_overruns_its_query_timeout_reports_hyt00`
    /// is the sibling of this test and cannot stand in for it: it blocks in
    /// `exec_direct`, so it keeps passing with `SQLFetch` entirely unarmed —
    /// which is exactly the state this driver shipped in. Measured against a
    /// live coordinator under a 2-second deadline, the execute returned
    /// `SQL_SUCCESS` in 0.1s and the fetch in 24.6s.
    ///
    /// Not run under Miri: a real one-second deadline and a spin-until-cancelled
    /// fetch, which Miri would stretch unpredictably for no memory-safety gain.
    #[test]
    #[cfg_attr(miri, ignore = "wall-clock deadline; no unsafe to check")]
    fn a_fetch_that_overruns_its_query_timeout_reports_hyt00() {
        unsafe {
            let (env, conn, stmt) =
                crate::test_utils::alloc_connected_env_conn_stmt::<MockFetchTimeoutBackend>();

            // The mock answers `CoreCancels`, so this records a one-second
            // deadline for core's timer rather than handing it to the source.
            assert_eq!(
                crate::ffi::stmt_attr::sql_set_stmt_attr_w::<MockFetchTimeoutBackend>(
                    stmt,
                    odbc_sys::StatementAttribute::QueryTimeout as i32,
                    // An integer-valued attribute, not a pointer; the ODBC ABI
                    // passes it through a pointer-typed parameter.
                    std::ptr::without_provenance_mut::<c_void>(1),
                    0,
                ),
                SqlReturn::SUCCESS,
                "the mock delegates the deadline to core rather than refusing it",
            );

            // Returns at once, as Trino's does: metadata is known, no row is.
            let sql: Vec<u16> = "SELECT 1".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<MockFetchTimeoutBackend>(
                    stmt,
                    sql.as_ptr(),
                    i32::try_from(sql.len()).expect("SQL fits in i32"),
                ),
                SqlReturn::SUCCESS,
                "precondition: the execute beats the deadline, so only the fetch can time out",
            );

            assert_eq!(
                sql_fetch::<MockFetchTimeoutBackend>(stmt),
                SqlReturn::ERROR,
                "a fetch cancelled by its deadline must not report success",
            );

            let state = with_handle::<
                MockFetchTimeoutBackend,
                StatementHandle<MockFetchTimeoutBackend>,
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

            cleanup_stmt_for::<MockFetchTimeoutBackend>(env, conn, stmt);
        }
    }
}
