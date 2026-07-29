//! Generic implementations of SQLSetStmtAttrW and SQLGetStmtAttrW.

use std::ffi::c_void;

use odbc_sys::StatementAttribute;

use crate::backend::Backend;
use crate::errors::OdbcError;
use crate::handles::StatementHandle;
use crate::panic::panic_safe;
use crate::types::{
    SQL_CURSOR_FORWARD_ONLY, SQL_FALSE, SQL_UNSPECIFIED, SqlReturn, SqlState,
    statement_attribute_from_raw,
};

// SQL_ATTR_CURSOR_SCROLLABLE values
const SQL_NONSCROLLABLE: usize = 0;

// SQL_ATTR_CONCURRENCY values
const SQL_CONCUR_READ_ONLY: usize = 1;
// SQL_ATTR_RETRIEVE_DATA values
const SQL_RD_ON: usize = 1;
// SQL_ATTR_USE_BOOKMARKS values
const SQL_UB_OFF: usize = 0;
// SQL_ATTR_ASYNC_ENABLE values
const SQL_ASYNC_ENABLE_OFF: usize = 0;
// SQL_ATTR_NOSCAN values
const SQL_NOSCAN_OFF: usize = 0;
// SQL_ATTR_ROW_BIND_TYPE values
const SQL_BIND_BY_COLUMN: usize = 0;
// SQL_ATTR_CURSOR_SENSITIVITY deliberately has no constant here: it uses the
// shared `SQL_UNSPECIFIED` from `types::constants`, which is what
// `default_get_info` answers SQL_CURSOR_SENSITIVITY with. The two draw on the
// same value set, so a local copy is a second place for the same statement to
// describe its cursor — and `sql.h` puts SQL_INSENSITIVE at 1 and
// SQL_SENSITIVE at 2, either side of it, which is what a copy would most
// likely drift to.

// SQL_ATTR_SIMULATE_CURSOR values. Named `SQL_SC_*` in `sqlext.h`, which
// collides with the `SQL_SC_*` SQL-conformance family in `types::constants`
// (`SQL_SC_SQL92_ENTRY` and friends) — an unrelated value set that happens to
// share the prefix. Kept local for that reason, like the other value sets
// above.
const SQL_SC_NON_UNIQUE: usize = 0;

// SQL_ATTR_ROW_ARRAY_SIZE default
const SQL_ROW_ARRAY_SIZE_DEFAULT: usize = 1;
// SQL_ATTR_KEYSET_SIZE default: 0, "the cursor is fully keyset-driven".
const SQL_KEYSET_SIZE_DEFAULT: usize = 0;
// SQL_ATTR_MAX_LENGTH default: 0, "the driver attempts to return all
// available data".
const SQL_MAX_LENGTH_DEFAULT: usize = 0;
// SQL_ATTR_PARAMSET_SIZE default
const SQL_PARAMSET_SIZE_DEFAULT: usize = 1;
// SQL_ATTR_MAX_ROWS default: 0, "return all rows".
const SQL_MAX_ROWS_DEFAULT: usize = 0;
// SQL_ATTR_QUERY_TIMEOUT default: 0, "no timeout".
const SQL_QUERY_TIMEOUT_DEFAULT: usize = 0;

/// Apply the spec's `01S02` substitution to one statement attribute: store the
/// value the driver will actually use, post the warning that says so, and
/// return `SQL_SUCCESS_WITH_INFO`.
///
/// Spec, `SQLSetStmtAttr` `01S02`: "The driver did not support the value
/// specified in *ValuePtr* … so the driver substituted a similar value.
/// (**SQLGetStmtAttr** can be called to determine the temporarily substituted
/// value.)" Storing the substituted value rather than the requested one is
/// what makes that sentence true, and is why every caller here writes through
/// this function instead of inserting directly.
///
/// The row then closes the set: "The statement attributes that can be changed
/// are: SQL_ATTR_CONCURRENCY SQL_ATTR_CURSOR_TYPE SQL_ATTR_KEYSET_SIZE
/// SQL_ATTR_MAX_LENGTH SQL_ATTR_MAX_ROWS SQL_ATTR_QUERY_TIMEOUT
/// SQL_ATTR_ROW_ARRAY_SIZE SQL_ATTR_SIMULATE_CURSOR". An attribute outside
/// that list must not be substituted — it takes `HYC00` instead — with the two
/// documented exceptions noted at their call sites.
fn substitute_stmt_attr<B: Backend>(
    stmt: &mut StatementHandle<B>,
    attribute: i32,
    attr_name: &str,
    requested: usize,
    substituted: usize,
    substituted_display: &str,
) -> SqlReturn {
    tracing::warn!(
        "SQLSetStmtAttrW: {}={} not supported, substituting {} (01S02)",
        attr_name,
        requested,
        substituted_display
    );
    stmt.attrs.insert(attribute, substituted);
    stmt.diagnostics.push(&OdbcError::general(
        format!("{attr_name} {requested} is not supported; substituted {substituted_display}"),
        SqlState::option_value_changed(),
    ));
    SqlReturn::SUCCESS_WITH_INFO
}

/// Generic implementation of SQLSetStmtAttrW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlsetstmtattr-function>
///
/// Accepts and stores all recognised integer/pointer attributes.
///
/// Core drives one forward-only, read-only cursor over one parameter set, and
/// two rules divide the attributes that ask for anything else. An attribute on
/// the spec's closed `01S02` list is stored at the value core will actually
/// use, with `01S02` posted so `SQLGetStmtAttr` reports what the application
/// got. An attribute off that list has no substitution to offer and reports
/// `HYC00`. See `substitute_stmt_attr` for the list itself.
///
/// # Parameters
///
/// - `statement_handle`: statement handle (SQL_HANDLE_STMT).
/// - `attribute`: the statement attribute to set (e.g. `SQL_ATTR_CURSOR_TYPE`).
/// - `value_ptr`: the value to associate with `attribute`. Either an integer
///   cast to a pointer, or a pointer to a null-terminated UTF-16 string, or a
///   descriptor handle — depending on the attribute.
/// - `_string_length`: byte length of `*value_ptr` when it is a string;
///   ignored for integer-valued attributes.
///
/// # Spec compliance
///
/// - 01000 General warning: not currently returned here.
/// - 01S02 Option value changed: returned for `SQL_ATTR_CONCURRENCY`,
///   `SQL_ATTR_CURSOR_TYPE`, `SQL_ATTR_KEYSET_SIZE`, `SQL_ATTR_MAX_LENGTH`,
///   `SQL_ATTR_MAX_ROWS`, `SQL_ATTR_QUERY_TIMEOUT`, `SQL_ATTR_ROW_ARRAY_SIZE`
///   and `SQL_ATTR_SIMULATE_CURSOR` — the eight the spec's `01S02` row names —
///   plus `SQL_ATTR_CURSOR_SCROLLABLE` and `SQL_ATTR_PARAMSET_SIZE`, two
///   deviations documented at their arms. Each has exactly one value core can
///   honour; the driver stores that value and `SQLGetStmtAttr` reports it back,
///   which is how the application learns what it was given. Attributes with no
///   single such value, the pointer-valued ones among them, are stored
///   verbatim.
/// - 24000 Invalid cursor state: returned when setting `SQL_ATTR_CONCURRENCY`,
///   `SQL_ATTR_CURSOR_TYPE`, `SQL_ATTR_SIMULATE_CURSOR`, or
///   `SQL_ATTR_USE_BOOKMARKS` while a cursor is open (`stmt.cursor_open`). A
///   statement that is only prepared, or whose cursor `SQLEndTran` closed under
///   `SQL_CB_CLOSE`, has no open cursor and is not rejected here (a prepared one
///   is rejected by the HY011 check below instead).
/// - HY000 General error: returned for unexpected internal errors.
/// - HY001 Memory allocation error: not returned; Rust panics on allocation
///   failure, which is caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY009 Invalid use of null pointer: not currently checked; the spec
///   requires HY009 when an attribute that requires a string value receives a
///   null `value_ptr`. No string-valued set-attributes exist, so the
///   condition does not arise in practice.
/// - HY010 Function sequence error: (driver-manager-handled; not returned here).
/// - HY011 Attribute cannot be set now: returned when setting the above
///   attributes after the statement has been prepared (`stmt.prepared_sql` is
///   `Some`).
/// - HY013 Memory management error: not returned; Rust panics on memory errors,
///   caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY017 Invalid use of an automatically allocated descriptor handle:
///   (driver-manager-handled; not returned here). Descriptor
///   attributes are accepted silently because descriptors are not yet
///   fully implemented.
/// - HY024 Invalid attribute value: not returned. A value core cannot honour
///   takes one of the two paths above — 01S02 substitution on the spec's list,
///   HYC00 off it — rather than being rejected as invalid, since the values in
///   question are valid ODBC values that this driver does not implement.
/// - HY090 Invalid string or buffer length: (driver-manager-handled; not
///   returned here).
/// - HY092 Invalid attribute/option identifier: (driver-manager-handled; not
///   returned here). Unknown attributes are accepted silently.
/// - HY117 Connection is suspended due to unknown transaction state:
///   (driver-manager-handled; not returned here).
/// - HYC00 Optional feature not implemented: returned for
///   `SQL_ATTR_USE_BOOKMARKS` other than `SQL_UB_OFF`,
///   `SQL_ATTR_RETRIEVE_DATA` = `SQL_RD_OFF`, `SQL_ATTR_CURSOR_SENSITIVITY` =
///   `SQL_SENSITIVE`, `SQL_ATTR_ENABLE_AUTO_IPD` = `SQL_TRUE` (a case the
///   spec's own HYC00 row names), and `SQL_ATTR_ASYNC_ENABLE` =
///   `SQL_ASYNC_ENABLE_ON`. These are the unsupported values that the spec's
///   `01S02` row does not cover, so there is no substitution to report instead.
/// - HYT01 Connection timeout expired: not returned; this function does not
///   communicate with the data source.
/// - IM001 Driver does not support this function: (driver-manager-handled; not
///   returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_set_stmt_attr_w<B: Backend>(
    statement_handle: *mut c_void,
    attribute: i32,
    value_ptr: *mut c_void,
    _string_length: i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLSetStmtAttrW(stmt={:?}, attr_raw={}, value={:?})",
        statement_handle,
        attribute,
        value_ptr
    );
    let attr = statement_attribute_from_raw(attribute);
    tracing::debug!("SQLSetStmtAttrW: attr={:?}", attr);
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle. scope.get validates kind and group before any cast, and
    // panic_safe catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            let int_val = value_ptr as usize;

            match attr {
                // Spec 24000 + HY011: these attributes cannot be set while a cursor is
                // open or after the statement has been prepared.
                Some(
                    StatementAttribute::CursorType
                    | StatementAttribute::Concurrency
                    | StatementAttribute::SimulateCursor
                    | StatementAttribute::UseBookmarks,
                ) => {
                    // Spec 24000: cursor is open.
                    if stmt.cursor_open {
                        return Err(OdbcError::general(
                            format!("Cannot set {attr:?} while a cursor is open"),
                            SqlState::invalid_cursor_state(),
                        ));
                    }
                    // Spec HY011: statement has been prepared.
                    if stmt.prepared_sql.is_some() {
                        return Err(OdbcError::general(
                            format!("Cannot set {attr:?} after the statement has been prepared"),
                            SqlState::attribute_cannot_be_set_now(),
                        ));
                    }
                    // Three of these four are on the spec's closed 01S02 list
                    // and take the substitution path; `SQL_ATTR_USE_BOOKMARKS`
                    // is not on it and takes HYC00. See `substitute_stmt_attr`.
                    //
                    // Cursor type: only forward-only is supported. If a driver
                    // ever needs scrollable cursors, this check would have to
                    // move from stackable-odbc-core into the backend trait so
                    // each driver could decide independently.
                    if matches!(attr, Some(StatementAttribute::CursorType))
                        && int_val != SQL_CURSOR_FORWARD_ONLY
                    {
                        return Ok(substitute_stmt_attr(
                            stmt,
                            attribute,
                            "SQL_ATTR_CURSOR_TYPE",
                            int_val,
                            SQL_CURSOR_FORWARD_ONLY,
                            "SQL_CURSOR_FORWARD_ONLY",
                        ));
                    }
                    // Concurrency: core's cursor is read-only — nothing here
                    // implements a positioned update or delete — and
                    // `SQL_CONCUR_READ_ONLY` is the spec's own default. The
                    // spec uses this exact attribute as its worked example of
                    // the substitution rule: "if Attribute is
                    // SQL_ATTR_CONCURRENCY and ValuePtr is SQL_CONCUR_ROWVER,
                    // and if the data source does not support this, the driver
                    // substitutes SQL_CONCUR_VALUES and returns
                    // SQL_SUCCESS_WITH_INFO."
                    if matches!(attr, Some(StatementAttribute::Concurrency))
                        && int_val != SQL_CONCUR_READ_ONLY
                    {
                        return Ok(substitute_stmt_attr(
                            stmt,
                            attribute,
                            "SQL_ATTR_CONCURRENCY",
                            int_val,
                            SQL_CONCUR_READ_ONLY,
                            "SQL_CONCUR_READ_ONLY",
                        ));
                    }
                    // Simulated positioned updates: core constructs no searched
                    // UPDATE or DELETE and so guarantees nothing about how many
                    // rows one would affect. `SQL_SC_NON_UNIQUE` is the value
                    // that says exactly that — "the driver does not guarantee
                    // that simulated positioned update or delete statements
                    // will affect only one row" — so claiming either of the
                    // other two would be a promise core cannot keep.
                    if matches!(attr, Some(StatementAttribute::SimulateCursor))
                        && int_val != SQL_SC_NON_UNIQUE
                    {
                        return Ok(substitute_stmt_attr(
                            stmt,
                            attribute,
                            "SQL_ATTR_SIMULATE_CURSOR",
                            int_val,
                            SQL_SC_NON_UNIQUE,
                            "SQL_SC_NON_UNIQUE",
                        ));
                    }
                    // Bookmarks: core implements none, and nothing reads
                    // `SQL_ATTR_FETCH_BOOKMARK_PTR`. The attribute is *not* on
                    // the 01S02 list, so there is no substitution to offer;
                    // `HYC00` is the row that fits — "a valid ODBC statement
                    // attribute for the version of ODBC supported by the driver
                    // but was not supported by the driver".
                    if matches!(attr, Some(StatementAttribute::UseBookmarks))
                        && int_val != SQL_UB_OFF
                    {
                        return Err(OdbcError::NotImplemented {
                            feature: format!("SQL_ATTR_USE_BOOKMARKS = {int_val} (bookmarks)"),
                        });
                    }
                    stmt.attrs.insert(attribute, int_val);
                    Ok(SqlReturn::SUCCESS)
                }

                // SQL_ATTR_CURSOR_SCROLLABLE: only non-scrollable is supported.
                Some(StatementAttribute::CursorScrollable) => {
                    if int_val != SQL_NONSCROLLABLE {
                        // Substituted and reported as 01S02 for the same reason
                        // as SQL_ATTR_CURSOR_TYPE above: the two describe one
                        // cursor, and refusing this one while substituting the
                        // other would leave them disagreeing.
                        //
                        // A deliberate deviation: this attribute is *not* on
                        // the spec's closed 01S02 list, so the letter of the
                        // spec is HYC00. Keeping the pair consistent is judged
                        // worth more than that, because an application that
                        // reads back a forward-only cursor type and a
                        // scrollable cursor has been told two contradictory
                        // things about one cursor.
                        return Ok(substitute_stmt_attr(
                            stmt,
                            attribute,
                            "SQL_ATTR_CURSOR_SCROLLABLE",
                            int_val,
                            SQL_NONSCROLLABLE,
                            "SQL_NONSCROLLABLE",
                        ));
                    }
                    stmt.attrs.insert(attribute, int_val);
                    Ok(SqlReturn::SUCCESS)
                }

                // Descriptor handle attrs (AppRowDesc, AppParamDesc, ImpRowDesc,
                // ImpParamDesc): accept silently; we don't implement descriptors yet.
                Some(
                    StatementAttribute::AppRowDesc
                    | StatementAttribute::AppParamDesc
                    | StatementAttribute::ImpRowDesc
                    | StatementAttribute::ImpParamDesc,
                ) => {
                    tracing::warn!(
                        "SQLSetStmtAttrW: descriptor attribute {} ({:?}) ignored \
                         (descriptors not yet implemented)",
                        attribute,
                        attr
                    );
                    Ok(SqlReturn::SUCCESS)
                }

                // Only single-row rowsets are implemented: SQLFetch reads one
                // row and does not write through SQL_ATTR_ROWS_FETCHED_PTR or
                // the row status array. Accepting a larger size verbatim would
                // leave the application reading uninitialised buffer elements
                // as though they were rows.
                //
                // Spec 01S02: the driver substitutes a similar value and
                // returns SQL_SUCCESS_WITH_INFO; SQL_ATTR_ROW_ARRAY_SIZE is
                // explicitly listed as substitutable. SQLGetStmtAttr then
                // reports the substituted value.
                Some(StatementAttribute::RowArraySize) if int_val != SQL_ROW_ARRAY_SIZE_DEFAULT => {
                    Ok(substitute_stmt_attr(
                        stmt,
                        attribute,
                        "SQL_ATTR_ROW_ARRAY_SIZE",
                        int_val,
                        SQL_ROW_ARRAY_SIZE_DEFAULT,
                        "1",
                    ))
                }

                // Only a single parameter set is executed: SQLExecute binds and
                // runs the parameter buffers once and does not iterate over an
                // array. Accepting a larger size verbatim would silently drop
                // every parameter set past the first and cause an undetectable
                // batch-insert data loss.
                //
                // The second deliberate deviation, alongside
                // SQL_ATTR_CURSOR_SCROLLABLE above: SQL_ATTR_PARAMSET_SIZE is
                // not on the spec's closed 01S02 list either. Substitution is
                // still the least-bad answer here, because the alternatives are
                // to accept a size core will not honour — undetectable data
                // loss — or to fail a call every parameter-array-capable tool
                // makes.
                Some(StatementAttribute::ParamsetSize) if int_val != SQL_PARAMSET_SIZE_DEFAULT => {
                    Ok(substitute_stmt_attr(
                        stmt,
                        attribute,
                        "SQL_ATTR_PARAMSET_SIZE",
                        int_val,
                        SQL_PARAMSET_SIZE_DEFAULT,
                        "1",
                    ))
                }

                // No row limit is applied anywhere: `SQLFetch` asks the backend
                // for the next row until the backend says there are none, and
                // nothing counts. Storing a non-zero limit verbatim would have
                // `SQLGetStmtAttr` report a cap the driver then does not honour,
                // so an application asking for 10 rows quietly receives all of
                // them. SQL_ATTR_MAX_ROWS is on the spec's own 01S02
                // substitution list, so say so instead.
                Some(StatementAttribute::MaxRows) if int_val != SQL_MAX_ROWS_DEFAULT => {
                    Ok(substitute_stmt_attr(
                        stmt,
                        attribute,
                        "SQL_ATTR_MAX_ROWS",
                        int_val,
                        SQL_MAX_ROWS_DEFAULT,
                        "0 (no limit)",
                    ))
                }

                // The counterpart of SQL_ATTR_MAX_ROWS, one column over: no
                // character or binary value is truncated to this limit on the
                // way out, since neither `sql_fetch` nor `sql_get_data`
                // consults it. 0 — "the driver attempts to return all available
                // data" — is therefore the only value core can report honestly,
                // and SQL_ATTR_MAX_LENGTH is on the spec's 01S02 list, which
                // says how to report it.
                Some(StatementAttribute::MaxLength) if int_val != SQL_MAX_LENGTH_DEFAULT => {
                    Ok(substitute_stmt_attr(
                        stmt,
                        attribute,
                        "SQL_ATTR_MAX_LENGTH",
                        int_val,
                        SQL_MAX_LENGTH_DEFAULT,
                        "0 (all available data)",
                    ))
                }

                // A keyset is a keyset-driven cursor's window, and core has no
                // keyset-driven cursor: `SQL_ATTR_CURSOR_TYPE` is substituted
                // back to forward-only a few arms above, so a non-zero keyset
                // size describes a cursor that cannot exist on this statement.
                // Also on the spec's 01S02 list.
                Some(StatementAttribute::KeysetSize) if int_val != SQL_KEYSET_SIZE_DEFAULT => {
                    Ok(substitute_stmt_attr(
                        stmt,
                        attribute,
                        "SQL_ATTR_KEYSET_SIZE",
                        int_val,
                        SQL_KEYSET_SIZE_DEFAULT,
                        "0",
                    ))
                }

                // `Backend` is synchronous and has no cancellation deadline, so
                // no timeout is ever applied. Same reasoning as MAX_ROWS above,
                // and SQL_ATTR_QUERY_TIMEOUT is likewise on the 01S02 list — an
                // application that sets a 30-second timeout and gets SUCCESS is
                // entitled to believe a runaway query will be cut off.
                Some(StatementAttribute::QueryTimeout) if int_val != SQL_QUERY_TIMEOUT_DEFAULT => {
                    Ok(substitute_stmt_attr(
                        stmt,
                        attribute,
                        "SQL_ATTR_QUERY_TIMEOUT",
                        int_val,
                        SQL_QUERY_TIMEOUT_DEFAULT,
                        "0 (no timeout)",
                    ))
                }

                // `sql_fetch` retrieves and writes bound columns
                // unconditionally, so SQL_RD_OFF — "do not retrieve data into
                // the bound buffers" — is not something core can honour. Not on
                // the 01S02 list, so HYC00 rather than a substitution.
                Some(StatementAttribute::RetrieveData) if int_val != SQL_RD_ON => {
                    Err(OdbcError::NotImplemented {
                        feature: format!("SQL_ATTR_RETRIEVE_DATA = {int_val} (SQL_RD_OFF)"),
                    })
                }

                // `SQL_UNSPECIFIED` promises nothing, which is exactly what a
                // streaming forward-only cursor can guarantee, and it is what
                // `SQLGetInfo(SQL_CURSOR_SENSITIVITY)` reports. The other two
                // are promises core cannot keep: `SQL_INSENSITIVE` says no
                // other cursor's changes ever become visible, `SQL_SENSITIVE`
                // says they all do. Neither is on the 01S02 list, so HYC00.
                Some(StatementAttribute::CursorSensitivity)
                    if int_val != usize::from(SQL_UNSPECIFIED) =>
                {
                    Err(OdbcError::NotImplemented {
                        feature: format!("SQL_ATTR_CURSOR_SENSITIVITY = {int_val}"),
                    })
                }

                // Spec HYC00, verbatim: "The Attribute argument was
                // SQL_ATTR_ENABLE_AUTO_IPD, and the value of the connection
                // attribute SQL_ATTR_AUTO_IPD was SQL_FALSE."
                // `SQLGetConnectAttr` reports exactly that, and
                // `SQLGetStmtAttr` reports SQL_FALSE for this attribute, so
                // SQL_TRUE is the one value the three cannot agree on.
                Some(StatementAttribute::EnableAutoIpd) if int_val != SQL_FALSE as usize => {
                    Err(OdbcError::NotImplemented {
                        feature:
                            "SQL_ATTR_ENABLE_AUTO_IPD = SQL_TRUE (SQL_ATTR_AUTO_IPD is SQL_FALSE)"
                                .into(),
                    })
                }

                // `SQLGetInfo(SQL_ASYNC_MODE)` reports SQL_AM_NONE and the
                // `Backend` trait is synchronous, so there is no asynchronous
                // execution to enable. Not on the 01S02 list.
                Some(StatementAttribute::AsyncEnable) if int_val != SQL_ASYNC_ENABLE_OFF => {
                    Err(OdbcError::NotImplemented {
                        feature: "SQL_ATTR_ASYNC_ENABLE = SQL_ASYNC_ENABLE_ON".into(),
                    })
                }

                // All other recognised attributes: store value.
                //
                // SQL_ATTR_ROWS_FETCHED_PTR, SQL_ATTR_ROW_STATUS_PTR and
                // SQL_ATTR_ROW_BIND_OFFSET_PTR reach here and are stored
                // verbatim, which is correct because `SQLFetch` now reads and
                // honours all three. They are deliberately *not* substituted:
                // the spec's 01S02 list is closed and names none of them, and
                // there is no "similar value" to substitute for a pointer.
                Some(_) => {
                    stmt.attrs.insert(attribute, int_val);
                    Ok(SqlReturn::SUCCESS)
                }

                None => {
                    tracing::warn!(
                        "SQLSetStmtAttrW: unrecognized attribute {} accepted silently \
                         (spec requires HYC00; relaxed for DM/tool compatibility)",
                        attribute
                    );
                    Ok(SqlReturn::SUCCESS)
                }
            }
        })
    };
    tracing::debug!("SQLSetStmtAttrW -> {:?}", ret);
    ret
}

/// Generic implementation of SQLGetStmtAttrW.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlgetstmtattr-function>
///
/// Returns integer attributes as `u32` written to `*value_ptr`.
/// Pointer attributes are returned as pointer-sized values.
///
/// # Parameters
///
/// - `statement_handle`: statement handle (SQL_HANDLE_STMT).
/// - `attribute`: the statement attribute to retrieve (e.g. `SQL_ATTR_CURSOR_TYPE`).
/// - `value_ptr`: output buffer to receive the attribute value. For integer
///   attributes this receives a `u32`; for pointer attributes a pointer-sized
///   value; for descriptor-handle attributes a pointer to the descriptor handle.
///   May be null — the driver still writes `*string_length_ptr` in that case.
/// - `_buffer_length`: maximum byte length of `*value_ptr` when it is a string;
///   ignored for integer and pointer attributes.
/// - `string_length_ptr`: output pointer that receives the number of bytes
///   written to `*value_ptr`. May be null.
///
/// # Spec compliance
///
/// - 01000 General warning: not currently returned here.
/// - 01004 String data, right truncated: not applicable; no string-valued
///   statement attributes are returned (all returned values are integer or
///   pointer types).
/// - 24000 Invalid cursor state: returned when `SQL_ATTR_ROW_NUMBER` is
///   requested and no cursor is open (`stmt.cursor_open` is `false`), which
///   includes a statement that is only prepared and one whose cursor
///   `SQLEndTran` closed under `SQL_CB_CLOSE`.
/// - HY000 General error: returned for unexpected internal errors.
/// - HY001 Memory allocation error: not returned; Rust panics on allocation
///   failure, which is caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY010 Function sequence error: (driver-manager-handled; not returned here).
/// - HY013 Memory management error: not returned; Rust panics on memory errors,
///   caught by `panic_safe` and converted to `SQL_ERROR`/HY000.
/// - HY090 Invalid string or buffer length: (driver-manager-handled; not
///   returned here).
/// - HY092 Invalid attribute/option identifier: returning HYC00 rather than
///   HY092 for unrecognised identifiers is a deliberate DM-compatibility
///   choice. HYC00 is accepted by all common Driver Managers.
/// - HY109 Invalid cursor position: detecting deleted or unfetchable rows
///   requires backend support for tracking row validity. Not currently
///   implemented. Deferred.
/// - HY117 Connection is suspended due to unknown transaction state:
///   (driver-manager-handled; not returned here).
/// - HYC00 Optional feature not implemented: returned for unrecognised or
///   unsupported attribute identifiers.
/// - HYT01 Connection timeout expired: not returned; this function does not
///   communicate with the data source.
/// - IM001 Driver does not support this function: (driver-manager-handled; not
///   returned here).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `value_ptr` must be writable for the appropriate size.
pub unsafe fn sql_get_stmt_attr_w<B: Backend>(
    statement_handle: *mut c_void,
    attribute: i32,
    value_ptr: *mut c_void,
    _buffer_length: i32,
    string_length_ptr: *mut i32,
) -> SqlReturn {
    tracing::trace!(
        "SQLGetStmtAttrW(stmt={:?}, attr_raw={})",
        statement_handle,
        attribute
    );
    let attr = statement_attribute_from_raw(attribute);
    tracing::debug!("SQLGetStmtAttrW: attr={:?}", attr);
    // SAFETY: statement_handle is null or a valid StatementHandle<B> allocated by
    // sql_alloc_handle. scope.get validates kind and group before any cast, and
    // panic_safe catches any panics.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Helper: write an SQLULEN to value_ptr and report its size.
            //
            // `SQLULEN`, not `SQLUINTEGER`: every non-pointer attribute on the
            // `SQLSetStmtAttr` page is declared "An SQLULEN value", and
            // `BufferLength` is ignored for them, so the application's buffer
            // is SQLULEN-wide (8 bytes on LP64) and it is the driver's job to
            // fill it. `SQLGetStmtAttr`'s Comments describe the alternative as
            // a defect to be worked around — "some drivers may only write the
            // lower 32-bit or 16-bit of a buffer and leave the higher-order bit
            // unchanged. Therefore, applications should use a buffer of SQLULEN
            // and initialize the value to 0 before calling this function" — and
            // an application that does not zero its buffer reads a `MAX_ROWS`
            // of `0xFFFFFFFF00000000` where the driver means "no limit".
            //
            // This is the one place the statement attributes differ from the
            // connection ones, where all but `SQL_ATTR_ASYNC_ENABLE` and
            // `SQL_ATTR_ODBC_CURSORS` really are `SQLUINTEGER`.
            //
            // SAFETY: value_ptr is non-null (checked); caller guarantees it points to
            // writable memory for at least an SQLULEN. string_length_ptr likewise.
            // Alignment is not guaranteed (row-wise binding may place the buffer at an
            // arbitrary offset), so use unaligned writes.
            let write_ulen = |v: usize| {
                if !value_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable SQLULEN
                    std::ptr::write_unaligned(value_ptr as *mut usize, v);
                }
                if !string_length_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable i32
                    std::ptr::write_unaligned(
                        string_length_ptr,
                        std::mem::size_of::<usize>() as i32,
                    );
                }
            };

            // Helper: write a pointer-sized value to value_ptr.
            // SAFETY: value_ptr is non-null (checked); caller guarantees it points to
            // writable memory for at least a usize. string_length_ptr likewise. Alignment is
            // not guaranteed (row-wise binding may place the buffer at an arbitrary offset),
            // so use unaligned writes.
            let write_ptr = |v: usize| {
                if !value_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable usize
                    std::ptr::write_unaligned(value_ptr as *mut usize, v);
                }
                if !string_length_ptr.is_null() {
                    // SAFETY: non-null checked above; caller guarantees writable i32
                    std::ptr::write_unaligned(
                        string_length_ptr,
                        std::mem::size_of::<usize>() as i32,
                    );
                }
            };

            match attr {
                Some(StatementAttribute::QueryTimeout) => {
                    write_ulen(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::MaxRows) => {
                    write_ulen(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::MaxLength) => {
                    write_ulen(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::NoScan) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_NOSCAN_OFF),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowBindType) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_BIND_BY_COLUMN),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::CursorType) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_CURSOR_FORWARD_ONLY),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::Concurrency) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_CONCUR_READ_ONLY),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RetrieveData) => {
                    write_ulen(stmt.attrs.get(&attribute).copied().unwrap_or(SQL_RD_ON));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::UseBookmarks) => {
                    write_ulen(stmt.attrs.get(&attribute).copied().unwrap_or(SQL_UB_OFF));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowArraySize) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_ROW_ARRAY_SIZE_DEFAULT),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ParamsetSize) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_PARAMSET_SIZE_DEFAULT),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowNumber) => {
                    // Spec 24000: no cursor is open.
                    if !stmt.cursor_open {
                        return Err(OdbcError::general(
                            "SQL_ATTR_ROW_NUMBER requires an open cursor",
                            SqlState::invalid_cursor_state(),
                        ));
                    }
                    let v = stmt.attrs.get(&attribute).copied().unwrap_or(0);
                    write_ulen(v);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::EnableAutoIpd) => {
                    write_ulen(SQL_FALSE as usize); // not supported
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::AsyncEnable) => {
                    write_ulen(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::MetadataId) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_FALSE as usize),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::CursorScrollable) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_NONSCROLLABLE),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::CursorSensitivity) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(usize::from(SQL_UNSPECIFIED)),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::SimulateCursor) => {
                    write_ulen(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }

                // Pointer-valued attributes.
                Some(StatementAttribute::RowStatusPtr) => {
                    write_ptr(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowsFetchedPtr) => {
                    write_ptr(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowBindOffsetPtr) => {
                    write_ptr(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }

                // Descriptor handle attrs: return the allocated descriptor handles.
                // The Windows DM requires these to build its CLI dispatch table.
                Some(StatementAttribute::AppRowDesc) => {
                    write_ptr(stmt.app_row_desc.token() as usize);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::AppParamDesc) => {
                    write_ptr(stmt.app_param_desc.token() as usize);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ImpRowDesc) => {
                    write_ptr(stmt.imp_row_desc.token() as usize);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ImpParamDesc) => {
                    write_ptr(stmt.imp_param_desc.token() as usize);
                    Ok(SqlReturn::SUCCESS)
                }

                // The parameter-side counterparts of the row-side attributes
                // above, plus the two remaining rowset pointers. `SQLSetStmtAttr`
                // stores every one of them, and an attribute this driver stores
                // is an attribute it can report: the spec makes
                // `SQLGetStmtAttr` the way an application reads back what it
                // set, so refusing one here would leave a value it accepted
                // unreadable.
                Some(StatementAttribute::KeysetSize) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_KEYSET_SIZE_DEFAULT),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ParamBindType) => {
                    write_ulen(
                        stmt.attrs
                            .get(&attribute)
                            .copied()
                            .unwrap_or(SQL_BIND_BY_COLUMN),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(
                    StatementAttribute::ParamsProcessedPtr
                    | StatementAttribute::ParamStatusPtr
                    | StatementAttribute::ParamBindOffsetPtr
                    | StatementAttribute::ParamOpterationPtr
                    | StatementAttribute::RowOperationPtr
                    | StatementAttribute::FetchBookmarkPtr
                    | StatementAttribute::AsyncStmtEvent,
                ) => {
                    write_ptr(stmt.attrs.get(&attribute).copied().unwrap_or(0));
                    Ok(SqlReturn::SUCCESS)
                }

                _ => Err(OdbcError::general(
                    format!("SQLGetStmtAttrW: unsupported attribute {attribute}"),
                    SqlState::optional_feature_not_implemented(),
                )),
            }
        })
    };
    tracing::debug!("SQLGetStmtAttrW -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockBackend, alloc_env_conn_stmt, cleanup_env_conn_stmt, with_handle};
    use crate::types::{SQL_SENSITIVE, SQL_TRUE};

    #[test]
    fn cursor_sensitivity_agrees_with_the_value_sqlgetinfo_reports() {
        // `SQL_ATTR_CURSOR_SENSITIVITY` and `SQL_CURSOR_SENSITIVITY` draw from
        // the same value set, so a statement must not describe itself two ways.
        assert_eq!(SQL_UNSPECIFIED, 0, "sql.h defines SQL_UNSPECIFIED as 0");

        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let mut value: usize = 0;
            let mut str_len: i32 = 0;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorSensitivity as i32,
                std::ptr::from_mut(&mut value).cast(),
                0,
                &mut str_len,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                value,
                usize::from(SQL_UNSPECIFIED),
                "SQLGetStmtAttr reported a different cursor sensitivity than SQLGetInfo"
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn row_array_size_greater_than_one_is_substituted_with_01s02() {
        // Only single-row fetch is implemented. Accepting a larger rowset with
        // plain SQL_SUCCESS would let the application read uninitialised
        // buffer elements as rows.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::RowArraySize as i32,
                std::ptr::without_provenance_mut(10usize),
                0,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS_WITH_INFO,
                "oversized rowset was accepted silently"
            );

            // Checked before any other call: every FFI function clears the
            // handle's diagnostics on entry.
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(
                    handle.diagnostics.len(),
                    1,
                    "no 01S02 diagnostic was recorded"
                );
            });

            // SQLGetStmtAttr must report the substituted value, per 01S02.
            let mut out: usize = 0;
            assert_eq!(
                sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::RowArraySize as i32,
                    std::ptr::from_mut(&mut out).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(out, 1, "substituted value not reported back");

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn row_array_size_of_one_is_accepted_without_warning() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::RowArraySize as i32,
                std::ptr::without_provenance_mut(1usize),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(handle.diagnostics.len(), 0);
            });
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn paramset_size_greater_than_one_is_substituted_with_01s02() {
        // Only a single parameter set is executed. Accepting a larger
        // SQL_ATTR_PARAMSET_SIZE with plain SQL_SUCCESS silently drops every
        // parameter set past the first, an undetectable batch-insert data
        // loss. Mirror SQL_ATTR_ROW_ARRAY_SIZE: substitute 1 with 01S02.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::ParamsetSize as i32,
                std::ptr::without_provenance_mut(10usize),
                0,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS_WITH_INFO,
                "oversized parameter set was accepted silently"
            );

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(
                    handle.diagnostics.len(),
                    1,
                    "no 01S02 diagnostic was recorded"
                );
            });

            // SQLGetStmtAttr must report the substituted value, per 01S02.
            let mut out: usize = 0;
            assert_eq!(
                sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::ParamsetSize as i32,
                    std::ptr::from_mut(&mut out).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(out, 1, "substituted value not reported back");

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn paramset_size_of_one_is_accepted_without_warning() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::ParamsetSize as i32,
                std::ptr::without_provenance_mut(1usize),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(handle.diagnostics.len(), 0);
            });
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn descriptor_attrs_return_four_distinct_handles() {
        // The Windows DM reads these four attributes to build its CLI dispatch
        // table, so each must be a distinct, non-null address.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let mut addrs = Vec::new();
            for attr in [
                StatementAttribute::AppRowDesc,
                StatementAttribute::AppParamDesc,
                StatementAttribute::ImpRowDesc,
                StatementAttribute::ImpParamDesc,
            ] {
                let mut out: usize = 0;
                let ret = sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    attr as i32,
                    std::ptr::from_mut(&mut out).cast(),
                    0,
                    std::ptr::null_mut(),
                );
                assert_eq!(ret, SqlReturn::SUCCESS, "{attr:?}");
                assert_ne!(out, 0, "{attr:?} returned a null descriptor handle");
                addrs.push(out);
            }

            addrs.sort_unstable();
            let distinct = addrs.len();
            addrs.dedup();
            assert_eq!(
                addrs.len(),
                distinct,
                "descriptor handles were not distinct"
            );

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_and_get_cursor_type_forward_only() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                SQL_CURSOR_FORWARD_ONLY as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut val: usize = 99;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                std::ptr::from_mut(&mut val).cast(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, SQL_CURSOR_FORWARD_ONLY);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_cursor_type_static_substitutes_forward_only_with_01s02() {
        // Spec: an unsupported value is substituted and reported as 01S02, not
        // refused. `SQLGetStmtAttr` then reports what the driver actually used,
        // which is the application's only way to find out.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                3usize as *mut c_void, // SQL_CURSOR_STATIC
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(
                    handle
                        .diagnostics
                        .get(0)
                        .expect("a 01S02 record")
                        .sqlstate
                        .as_str(),
                    "01S02"
                );
            });

            let mut val: usize = 99;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                std::ptr::from_mut(&mut val).cast(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                val, SQL_CURSOR_FORWARD_ONLY,
                "SQLGetStmtAttr must report the substituted value"
            );
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The spec's `01S02` row closes the set of statement attributes a driver
    /// may substitute for: "SQL_ATTR_CONCURRENCY SQL_ATTR_CURSOR_TYPE
    /// SQL_ATTR_KEYSET_SIZE SQL_ATTR_MAX_LENGTH SQL_ATTR_MAX_ROWS
    /// SQL_ATTR_QUERY_TIMEOUT SQL_ATTR_ROW_ARRAY_SIZE
    /// SQL_ATTR_SIMULATE_CURSOR". Cursor type, max rows, query timeout and row
    /// array size have tests of their own above; these are the other four.
    /// Each has exactly one value core can honour, and `SQLGetStmtAttr` reports
    /// that value, which is how the application learns what it was given.
    #[test]
    fn the_remaining_substitutable_attributes_report_01s02_and_the_value_used() {
        // (attribute, name, requested, the value core uses)
        let cases: &[(StatementAttribute, &str, usize, usize)] = &[
            (
                StatementAttribute::Concurrency,
                "SQL_ATTR_CONCURRENCY",
                2, // SQL_CONCUR_LOCK
                SQL_CONCUR_READ_ONLY,
            ),
            (
                StatementAttribute::MaxLength,
                "SQL_ATTR_MAX_LENGTH",
                4096,
                SQL_MAX_LENGTH_DEFAULT,
            ),
            (
                StatementAttribute::KeysetSize,
                "SQL_ATTR_KEYSET_SIZE",
                10,
                SQL_KEYSET_SIZE_DEFAULT,
            ),
            (
                StatementAttribute::SimulateCursor,
                "SQL_ATTR_SIMULATE_CURSOR",
                2, // SQL_SC_UNIQUE
                SQL_SC_NON_UNIQUE,
            ),
        ];
        for (attribute, name, requested, used) in cases {
            unsafe {
                let (env, conn, stmt) = alloc_env_conn_stmt();

                let ret = sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    *attribute as i32,
                    std::ptr::without_provenance_mut(*requested),
                    0,
                );
                assert_eq!(
                    ret,
                    SqlReturn::SUCCESS_WITH_INFO,
                    "{name} was accepted without 01S02"
                );

                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    assert_eq!(
                        handle
                            .diagnostics
                            .get(0)
                            .unwrap_or_else(|| panic!("{name}: a 01S02 record"))
                            .sqlstate
                            .as_str(),
                        "01S02",
                        "{name} posted the wrong SQLSTATE"
                    );
                });

                let mut out: usize = 99;
                assert_eq!(
                    sql_get_stmt_attr_w::<MockBackend>(
                        stmt,
                        *attribute as i32,
                        std::ptr::from_mut(&mut out).cast(),
                        0,
                        std::ptr::null_mut(),
                    ),
                    SqlReturn::SUCCESS
                );
                assert_eq!(
                    out, *used,
                    "{name}: SQLGetStmtAttr must report the value the driver uses"
                );

                cleanup_env_conn_stmt(env, conn, stmt);
            }
        }
    }

    /// The other half of that rule. An attribute the `01S02` row does not name
    /// has no substitution to offer, so a value core cannot honour reports
    /// `HYC00` — "a valid ODBC statement attribute for the version of ODBC
    /// supported by the driver but was not supported by the driver" — rather
    /// than being stored and echoed back.
    #[test]
    fn unsupported_values_off_the_01s02_list_report_hyc00() {
        // (attribute, name, value core cannot honour)
        let cases: &[(StatementAttribute, &str, usize)] = &[
            (
                StatementAttribute::UseBookmarks,
                "SQL_ATTR_USE_BOOKMARKS",
                2, // SQL_UB_VARIABLE
            ),
            (
                StatementAttribute::RetrieveData,
                "SQL_ATTR_RETRIEVE_DATA",
                0, // SQL_RD_OFF
            ),
            (
                StatementAttribute::CursorSensitivity,
                "SQL_ATTR_CURSOR_SENSITIVITY",
                SQL_SENSITIVE as usize,
            ),
            (
                StatementAttribute::EnableAutoIpd,
                "SQL_ATTR_ENABLE_AUTO_IPD",
                SQL_TRUE as usize,
            ),
            (
                StatementAttribute::AsyncEnable,
                "SQL_ATTR_ASYNC_ENABLE",
                1, // SQL_ASYNC_ENABLE_ON
            ),
        ];
        for (attribute, name, value) in cases {
            unsafe {
                let (env, conn, stmt) = alloc_env_conn_stmt();

                let ret = sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    *attribute as i32,
                    std::ptr::without_provenance_mut(*value),
                    0,
                );
                assert_eq!(ret, SqlReturn::ERROR, "{name} was accepted");

                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    assert_eq!(
                        handle
                            .diagnostics
                            .get(0)
                            .unwrap_or_else(|| panic!("{name}: a HYC00 record"))
                            .sqlstate
                            .as_str(),
                        "HYC00",
                        "{name} posted the wrong SQLSTATE"
                    );
                });

                cleanup_env_conn_stmt(env, conn, stmt);
            }
        }
    }

    /// Spec, `SQLGetStmtAttr` Comments: "A call to **SQLGetStmtAttr** returns
    /// in \**ValuePtr* the value of the statement attribute specified in
    /// *Attribute*. That value can either be a SQLULEN value or a
    /// null-terminated character string. If the value is a SQLULEN value, some
    /// drivers may only write the lower 32-bit or 16-bit of a buffer and leave
    /// the higher-order bit unchanged."
    ///
    /// The spec is describing a defect and telling applications to work around
    /// it; a driver's job is not to be one of those drivers. Every non-pointer
    /// attribute on the `SQLSetStmtAttr` page is declared `SQLULEN` — not one
    /// is `SQLUINTEGER`, unlike the connection attributes — and `BufferLength`
    /// is ignored for them, so the application's buffer is `SQLULEN`-wide and a
    /// four-byte write leaves the top half holding whatever was there.
    ///
    /// The buffer is poisoned with ones so a short write is visible: an
    /// application reading `SQL_ATTR_MAX_ROWS` would see a vast row limit where
    /// the driver means "no limit".
    #[test]
    fn integer_attributes_are_written_at_full_sqlulen_width() {
        // One representative per default: 0, and a non-zero enum value.
        let cases: &[(StatementAttribute, &str, usize)] = &[
            (StatementAttribute::MaxRows, "SQL_ATTR_MAX_ROWS", 0),
            (
                StatementAttribute::QueryTimeout,
                "SQL_ATTR_QUERY_TIMEOUT",
                0,
            ),
            (
                StatementAttribute::CursorType,
                "SQL_ATTR_CURSOR_TYPE",
                SQL_CURSOR_FORWARD_ONLY,
            ),
            (
                StatementAttribute::RowArraySize,
                "SQL_ATTR_ROW_ARRAY_SIZE",
                SQL_ROW_ARRAY_SIZE_DEFAULT,
            ),
        ];
        for (attribute, name, expected) in cases {
            unsafe {
                let (env, conn, stmt) = alloc_env_conn_stmt();

                let mut value: usize = usize::MAX;
                let ret = sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    *attribute as i32,
                    std::ptr::from_mut(&mut value).cast(),
                    0,
                    std::ptr::null_mut(),
                );
                assert_eq!(ret, SqlReturn::SUCCESS);
                assert_eq!(
                    value, *expected,
                    "{name}: the high half of the SQLULEN buffer kept its poison, \
                     so only part of the value was written"
                );

                cleanup_env_conn_stmt(env, conn, stmt);
            }
        }
    }

    /// An attribute this driver recognises is an attribute it can report.
    /// `SQLGetStmtAttr` is how an application reads back what `SQLSetStmtAttr`
    /// accepted, so a recognised attribute that answers `HYC00` here would hide
    /// a value the driver is holding.
    ///
    /// Driven off `statement_attribute_from_raw` rather than a hand-written
    /// list, so an attribute that becomes recognised without becoming readable
    /// fails this test.
    #[test]
    fn every_recognised_statement_attribute_is_readable() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            for raw in 0..=10_100i32 {
                let Some(attr) = statement_attribute_from_raw(raw) else {
                    continue;
                };
                // The one exception, and the spec's own: `SQL_ATTR_ROW_NUMBER`
                // is 24000 while no cursor is open, which is the state a fresh
                // statement is in.
                if matches!(attr, StatementAttribute::RowNumber) {
                    continue;
                }
                // Wide enough for both the u32 and the pointer-valued writes.
                let mut out: usize = 0;
                let ret = sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    raw,
                    std::ptr::from_mut(&mut out).cast(),
                    0,
                    std::ptr::null_mut(),
                );
                assert_eq!(
                    ret,
                    SqlReturn::SUCCESS,
                    "SQLGetStmtAttr({attr:?}) does not report a value"
                );
            }
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `SQL_UNSPECIFIED` promises nothing, which is what a streaming
    /// forward-only cursor can guarantee and what
    /// `SQLGetInfo(SQL_CURSOR_SENSITIVITY)` reports. It is therefore the one
    /// value this attribute accepts; the test above pins the other two.
    #[test]
    fn the_satisfiable_cursor_sensitivity_is_accepted() {
        for value in [SQL_UNSPECIFIED] {
            unsafe {
                let (env, conn, stmt) = alloc_env_conn_stmt();
                let ret = sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::CursorSensitivity as i32,
                    std::ptr::without_provenance_mut(usize::from(value)),
                    0,
                );
                assert_eq!(
                    ret,
                    SqlReturn::SUCCESS,
                    "cursor sensitivity {value} refused"
                );
                cleanup_env_conn_stmt(env, conn, stmt);
            }
        }
    }

    #[test]
    fn get_cursor_type_default_is_forward_only() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: usize = 99;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                std::ptr::from_mut(&mut val).cast(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, 0);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_scrollable_cursor_substitutes_nonscrollable_with_01s02() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorScrollable as i32,
                std::ptr::dangling_mut::<c_void>(), // SQL_SCROLLABLE (non-null)
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(
                    handle
                        .diagnostics
                        .get(0)
                        .expect("a 01S02 record")
                        .sqlstate
                        .as_str(),
                    "01S02"
                );
            });

            let mut val: usize = 99;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorScrollable as i32,
                std::ptr::from_mut(&mut val).cast(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                val, SQL_NONSCROLLABLE,
                "SQLGetStmtAttr must report the substituted value"
            );
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_and_get_query_timeout() {
        // This test previously asserted SUCCESS and a read-back of 30 — that is,
        // it pinned the driver claiming to honour a timeout it never applies.
        // No timeout is applied anywhere: `Backend` is synchronous and has no
        // deadline, so an application told SUCCESS would wait forever on a
        // runaway query. SQL_ATTR_QUERY_TIMEOUT is named on the spec's own 01S02
        // substitution list for exactly this case.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::QueryTimeout as i32,
                30usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(
                    handle
                        .diagnostics
                        .get(0)
                        .expect("a 01S02 record")
                        .sqlstate
                        .as_str(),
                    "01S02"
                );
            });

            let mut val: usize = 99;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::QueryTimeout as i32,
                std::ptr::from_mut(&mut val).cast(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(
                val, 0,
                "SQLGetStmtAttr must report the substituted no-timeout value"
            );
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    /// The same reasoning as SQL_ATTR_QUERY_TIMEOUT: nothing counts rows, so a
    /// stored limit would be a cap `SQLGetStmtAttr` reports and `SQLFetch` never
    /// applies. Also on the spec's 01S02 list.
    fn set_max_rows_substitutes_no_limit_with_01s02() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::MaxRows as i32,
                10usize as *mut c_void,
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert_eq!(
                    handle
                        .diagnostics
                        .get(0)
                        .expect("a 01S02 record")
                        .sqlstate
                        .as_str(),
                    "01S02"
                );
            });

            let mut val: usize = 99;
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::MaxRows as i32,
                std::ptr::from_mut(&mut val).cast(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(val, 0, "SQLGetStmtAttr must report the substituted value");
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_unknown_attr_returns_success() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(stmt, 9999, std::ptr::null_mut(), 0);
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn null_handle_returns_invalid() {
        unsafe {
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                std::ptr::null_mut(),
                StatementAttribute::CursorType as i32,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn set_cursor_type_with_cursor_open_returns_24000() {
        // 24000: cannot set SQL_ATTR_CURSOR_TYPE when a cursor is open.
        unsafe {
            use crate::handles::StatementData;

            let (env, conn, stmt) = alloc_env_conn_stmt();
            // Inject a synthetic cursor to simulate "cursor open".
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.set_result_set(StatementData::Synthetic(
                    crate::test_utils::synthetic_result_set(vec![]),
                ));
            });
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                std::ptr::null_mut::<c_void>(), // SQL_CURSOR_FORWARD_ONLY
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn set_cursor_type_after_prepare_returns_hy011() {
        // HY011: cannot set SQL_ATTR_CURSOR_TYPE after SQLPrepare.
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.prepared_sql = Some("SELECT 1".into());
            });
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::CursorType as i32,
                std::ptr::null_mut::<c_void>(),
                0,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    #[test]
    fn get_stmt_attr_null_handle_returns_invalid() {
        unsafe {
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                std::ptr::null_mut(),
                StatementAttribute::CursorType as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::INVALID_HANDLE);
        }
    }

    #[test]
    fn get_stmt_attr_unsupported_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: usize = 0;
            // Use a known-unsupported attribute to cover the error branch
            let ret = sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                9999,
                std::ptr::from_mut(&mut val).cast(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }
}
