//! Generic implementations of SQLSetStmtAttrW and SQLGetStmtAttrW.

use std::ffi::c_void;

use odbc_sys::StatementAttribute;

use crate::backend::Backend;
use crate::descriptor::DescriptorRole;
use crate::errors::{IntoOdbc, OdbcError};
use crate::handles::StatementHandle;
use crate::handles::registry::{HandleKind, registry};
use crate::handles::scope::HandleScope;
use crate::panic::panic_safe;
use crate::types::{
    QueryTimeout, SQL_CURSOR_FORWARD_ONLY, SQL_FALSE, SQL_NULL_DESC, SQL_UNSPECIFIED, SqlReturn,
    SqlState, statement_attribute_from_raw,
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
    scope: &mut HandleScope<'_>,
    stmt_token: *mut c_void,
    attribute: i32,
    attr_name: &str,
    requested: usize,
    substituted: usize,
    substituted_display: &str,
) -> Result<SqlReturn, OdbcError> {
    scope.attr_set::<B>(stmt_token, attribute, substituted)?;
    let stmt = scope.get::<StatementHandle<B>>(stmt_token)?;
    Ok(substitution_warning(
        &mut stmt.diagnostics,
        attr_name,
        requested,
        substituted_display,
    ))
}

/// The diagnostic half of [`substitute_stmt_attr`], against an explicit queue.
///
/// Split out because `SQLSetDescField` performs the same substitution on the
/// same stored value through the other door — `SQL_DESC_ARRAY_SIZE` *is*
/// `SQL_ATTR_ROW_ARRAY_SIZE` — but posts to the *descriptor's* queue rather
/// than the statement's, since that is the handle the application named. A
/// door that accepts what the other refuses is the disagreement single storage
/// exists to remove, so the two share the substitution rather than restating it.
pub(crate) fn substitution_warning(
    diagnostics: &mut crate::diagnostics::DiagnosticQueue,
    attr_name: &str,
    requested: usize,
    substituted_display: &str,
) -> SqlReturn {
    tracing::warn!(
        "{}={} not supported, substituting {} (01S02)",
        attr_name,
        requested,
        substituted_display
    );
    diagnostics.push(&OdbcError::general(
        format!("{attr_name} {requested} is not supported; substituted {substituted_display}"),
        SqlState::option_value_changed(),
    ));
    SqlReturn::SUCCESS_WITH_INFO
}

/// Offer a "reduce load at the data source" attribute to the backend, falling
/// back to the spec's `01S02` substitution when it cannot apply it.
///
/// Shared by `SQL_ATTR_QUERY_TIMEOUT`, `SQL_ATTR_MAX_ROWS` and
/// `SQL_ATTR_MAX_LENGTH`, which the spec treats alike. All three exist to
/// reduce work or traffic at the *data source* — the `MAX_ROWS` and
/// `MAX_LENGTH` rows both say "this attribute is intended to reduce network
/// traffic" in as many words — so core emulating any of them client-side would
/// move the data anyway and discard it afterwards, achieving nothing the
/// application asked for. The spec makes that explicit for both: "a driver
/// should not emulate SQL_ATTR_MAX_ROWS behavior", and `MAX_LENGTH` "should be
/// supported only when the data source (as opposed to the driver) ... can
/// implement it".
///
/// So the only honest answers are "the data source is doing it" or "nobody is",
/// and all three attributes sit on the spec's closed `01S02` list, which is how
/// to say the second.
///
/// Returns `Some(value)` when the backend accepted — the caller stores it and
/// may do extra bookkeeping — or `None` when the substitution was applied.
/// The `01S02` substitution to fall back on, as one value.
///
/// Grouped rather than passed as five loose arguments so the call sites read as
/// "this attribute, substituted to this" instead of a positional list where
/// `requested` and `fallback` are both `usize` and swapping them compiles.
struct Substitution<'a> {
    attribute: i32,
    name: &'a str,
    requested: usize,
    fallback: usize,
    fallback_display: &'a str,
}

fn offer_to_data_source<B: Backend, T>(
    scope: &mut HandleScope<'_>,
    stmt_token: *mut c_void,
    sub: Substitution<'_>,
    apply: impl FnOnce(&B::Connection) -> Result<T, OdbcError>,
) -> Result<(Option<T>, SqlReturn), OdbcError> {
    // The backend call runs inside its own borrow of the connection, which ends
    // before the attribute is stored: a header-field attribute lives on a
    // descriptor, and reaching one is a registry lookup through this same scope.
    //
    // No connection means nothing to ask. The Driver Manager's 08003 keeps a
    // statement from existing on an unconnected connection in the first place,
    // so this is core being defensive rather than a path an application reaches.
    let outcome = {
        let (_stmt, conn) = scope.stmt_with_parent::<B>(stmt_token)?;
        conn.connection.as_ref().map(apply)
    };
    let substitute = |scope: &mut HandleScope<'_>| {
        substitute_stmt_attr::<B>(
            scope,
            stmt_token,
            sub.attribute,
            sub.name,
            sub.requested,
            sub.fallback,
            sub.fallback_display,
        )
    };
    match outcome {
        None => Ok((None, substitute(scope)?)),
        Some(Ok(value)) => {
            tracing::debug!(
                "SQLSetStmtAttrW: {}={} applied by the data source",
                sub.name,
                sub.requested
            );
            scope.attr_set::<B>(stmt_token, sub.attribute, sub.requested)?;
            Ok((Some(value), SqlReturn::SUCCESS))
        }
        // The backend says it cannot do this at all: substitute and report.
        Some(Err(OdbcError::NotImplemented { .. })) => Ok((None, substitute(scope)?)),
        // A *real* failure is propagated instead of substituted. 01S02 tells an
        // application "this driver capped your value", which is a different
        // claim from "the connection is broken", and quietly reporting the
        // first for the second sends it on to execute against a connection it
        // has been told is fine.
        Some(Err(e)) => Err(e),
    }
}

/// Offer the *default* value of a "reduce load at the data source" attribute
/// to the backend, and store it.
///
/// The counterpart of [`offer_to_data_source`], for the value that withdraws
/// the limit. Three things differ, and each follows from the value being the
/// default rather than one core cannot honour:
///
/// - **There is nothing to substitute.** `01S02` says the driver "substituted
///   a similar value", and the value it would substitute *is* the one the
///   application asked for. Posting the warning would report a change that did
///   not happen, and would put the attribute on the diagnostic queue of every
///   application that resets it.
/// - **A backend that does not implement the hook has nothing to undo.**
///   `NotImplemented` is therefore success here rather than a fallback: such a
///   backend never applied the limit in the first place, so the data source is
///   already in the state being asked for.
/// - **The value is always stored**, so `SQLGetStmtAttr` reports the default.
///
/// Without this path the default reached the store-only arm at the bottom of
/// the match instead, which calls nothing — so a data source told to cap a
/// result set at ten rows was never told to stop, and `SQLGetStmtAttr`
/// reported no limit for a connection still enforcing one.
///
/// A *real* backend failure propagates rather than being swallowed, for the
/// reason [`offer_to_data_source`] gives: "the connection is broken" is a
/// different claim from "this driver capped your value".
///
/// Takes loose arguments rather than a [`Substitution`], deliberately: that
/// struct exists because `requested` and `fallback` are both `usize` and
/// swapping them compiles. There is one `usize` here — the default is the
/// requested value — so there is nothing to transpose.
fn reset_at_data_source<B: Backend, T>(
    scope: &mut HandleScope<'_>,
    stmt_token: *mut c_void,
    attribute: i32,
    name: &str,
    default: usize,
    apply: impl FnOnce(&B::Connection) -> Result<T, OdbcError>,
) -> Result<SqlReturn, OdbcError> {
    // The backend call runs inside its own borrow of the connection, which
    // ends before the attribute is stored — same shape, and same reason, as
    // `offer_to_data_source`.
    let outcome = {
        let (_stmt, conn) = scope.stmt_with_parent::<B>(stmt_token)?;
        conn.connection.as_ref().map(apply)
    };
    match outcome {
        None | Some(Err(OdbcError::NotImplemented { .. })) => {
            tracing::debug!(
                "SQLSetStmtAttrW: {} reset to its default; nothing to undo at the data source",
                name
            );
        }
        Some(Ok(_)) => {
            tracing::debug!(
                "SQLSetStmtAttrW: {} reset to its default at the data source",
                name
            );
        }
        Some(Err(e)) => return Err(e),
    }
    scope.attr_set::<B>(stmt_token, attribute, default)?;
    Ok(SqlReturn::SUCCESS)
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
///
///   Three members of the list have a **conditional** substitution:
///   `SQL_ATTR_QUERY_TIMEOUT`, `SQL_ATTR_MAX_ROWS` and `SQL_ATTR_MAX_LENGTH` are
///   offered to [`Backend::set_query_timeout`], [`Backend::set_max_rows`] and
///   [`Backend::set_max_length`] first, and substituted only when the backend
///   reports `NotImplemented` (or there is no open connection to offer them
///   to). A backend that accepts gets `SQL_SUCCESS` and the requested value
///   stored. See `offer_to_data_source` for why all three go to the data source
///   rather than being emulated here.
///
///   Their **default** values — `0` for each, meaning no timeout, no row limit
///   and no length limit — take a separate path and are never substituted, for
///   the plain reason that the value core would substitute is the value the
///   application asked for. They are still offered to the same three hooks, so
///   a data source told to apply a limit is told to lift it; see
///   `reset_at_data_source`.
/// - 08S01 Communication link failure: not raised by core, but any of the three
///   data-source hooks above that fails while talking to the data source is
///   reported with whatever SQLSTATE the backend's error mapping produced, and
///   this is the one that mapping should produce for a broken link. Such a
///   failure is deliberately *not* converted into an `01S02` substitution.
/// - 24000 Invalid cursor state: returned when setting `SQL_ATTR_CONCURRENCY`,
///   `SQL_ATTR_CURSOR_TYPE`, `SQL_ATTR_SIMULATE_CURSOR`, or
///   `SQL_ATTR_USE_BOOKMARKS` while a cursor is open (`stmt.cursor_open`). A
///   statement that is only prepared, or whose cursor `SQLEndTran` closed under
///   `SQL_CB_CLOSE`, has no open cursor and is not rejected here (a prepared one
///   is rejected by the HY011 check below instead).
/// - HY000 General error: returned for unexpected internal errors, and for a
///   failure in one of the three data-source hooks whose own mapping produced
///   no more specific state.
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
/// - HY017 Invalid use of an automatically allocated descriptor handle: **(DM)** on
///   *both* of its clauses, and core adds neither check. The DM rejects
///   `SQL_ATTR_IMP_ROW_DESC` / `SQL_ATTR_IMP_PARAM_DESC`, and rejects an implicitly
///   allocated handle passed to `SQL_ATTR_APP_ROW_DESC` / `SQL_ATTR_APP_PARAM_DESC`
///   that is not the one originally allocated for that statement's ARD or APD. Both
///   therefore reach core unchecked, and the two implementation descriptors are
///   accepted here. The second clause's wording — "other than the handle
///   originally allocated" — implies the original *is* allowed, and core accepts
///   it: the check it does make is the HY024 one below, which a statement's own
///   descriptor passes.
/// - HY024 Invalid attribute value: returned when `SQL_ATTR_APP_ROW_DESC` or
///   `SQL_ATTR_APP_PARAM_DESC` is given a value that is not a descriptor on this
///   statement's connection — a descriptor allocated on another connection, or a
///   value that names no live descriptor at all. This row is **not** (DM): it
///   states the case verbatim, and closes with the general rule that makes it
///   core's, "For all other connection and statement attributes, the driver must
///   verify the value specified in *ValuePtr*". The check compares the parent
///   *chain*, so both an explicit descriptor of this connection and one of this
///   connection's statements' own four are accepted. For every *other* attribute,
///   a value core cannot honour takes one of the two paths above — 01S02
///   substitution on the spec's list, HYC00 off it — rather than being rejected
///   as invalid, since those are valid ODBC values this driver does not implement.
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
/// - HYT01 Connection timeout expired: not raised by core. Since
///   `SQL_ATTR_QUERY_TIMEOUT`, `SQL_ATTR_MAX_ROWS` and `SQL_ATTR_MAX_LENGTH`
///   now reach the backend, this function *can* communicate with the data
///   source, so a backend whose own connection timeout expires during one of
///   those calls may report it — but core neither imposes nor recognises a
///   connection timeout of its own.
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
            // Resolved per arm rather than once up front. A header-field
            // attribute lives on a descriptor, which is a separate allocation
            // reached through this same scope, so holding the statement across
            // the match would make every such write unreachable. The connection —
            // needed by the one family of attributes that reaches the backend —
            // is likewise taken inside `offer_to_data_source`, which is the only
            // thing that wants it.
            scope
                .get::<StatementHandle<B>>(statement_handle)?
                .diagnostics
                .clear();

            let int_val = value_ptr as usize;

            // SQL_ROWSET_SIZE is the rowset SQLExtendedFetch reads, and this
            // driver's rowset is one row. It reaches this function as an
            // *unrecognised* attribute -- odbc-sys models only the 3.x
            // SQL_ATTR_ROW_ARRAY_SIZE -- so without this arm it falls to the
            // catch-all below and is accepted silently, leaving an application
            // that asked for ten rows to receive one under SQL_SUCCESS.
            //
            // A value core cannot honour must be refused identically through
            // every door. This is the third, beside SQL_ATTR_ROW_ARRAY_SIZE here
            // and SQL_DESC_ARRAY_SIZE in SQLSetDescField.
            //
            // The spec's 01S02 list is closed and names SQL_ATTR_ROW_ARRAY_SIZE
            // rather than this ODBC 2.x spelling, so this is the same deliberate
            // deviation already recorded for SQL_ATTR_PARAMSET_SIZE and
            // SQL_ATTR_CURSOR_SCROLLABLE below: substitution is the least-bad
            // answer, because the alternative is an undetectable short read.
            if attribute == crate::types::SQL_ROWSET_SIZE {
                if int_val == SQL_ROW_ARRAY_SIZE_DEFAULT {
                    scope.attr_set::<B>(statement_handle, attribute, int_val)?;
                    return Ok(SqlReturn::SUCCESS);
                }
                return substitute_stmt_attr::<B>(
                    scope,
                    statement_handle,
                    attribute,
                    "SQL_ROWSET_SIZE",
                    int_val,
                    SQL_ROW_ARRAY_SIZE_DEFAULT,
                    "1",
                );
            }

            match attr {
                // Spec 24000 + HY011: these attributes cannot be set while a cursor is
                // open or after the statement has been prepared.
                Some(
                    StatementAttribute::CursorType
                    | StatementAttribute::Concurrency
                    | StatementAttribute::SimulateCursor
                    | StatementAttribute::UseBookmarks,
                ) => {
                    let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
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
                        return substitute_stmt_attr::<B>(
                            scope,
                            statement_handle,
                            attribute,
                            "SQL_ATTR_CURSOR_TYPE",
                            int_val,
                            SQL_CURSOR_FORWARD_ONLY,
                            "SQL_CURSOR_FORWARD_ONLY",
                        );
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
                        return substitute_stmt_attr::<B>(
                            scope,
                            statement_handle,
                            attribute,
                            "SQL_ATTR_CONCURRENCY",
                            int_val,
                            SQL_CONCUR_READ_ONLY,
                            "SQL_CONCUR_READ_ONLY",
                        );
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
                        return substitute_stmt_attr::<B>(
                            scope,
                            statement_handle,
                            attribute,
                            "SQL_ATTR_SIMULATE_CURSOR",
                            int_val,
                            SQL_SC_NON_UNIQUE,
                            "SQL_SC_NON_UNIQUE",
                        );
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
                    scope.attr_set::<B>(statement_handle, attribute, int_val)?;
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
                        return substitute_stmt_attr::<B>(
                            scope,
                            statement_handle,
                            attribute,
                            "SQL_ATTR_CURSOR_SCROLLABLE",
                            int_val,
                            SQL_NONSCROLLABLE,
                            "SQL_NONSCROLLABLE",
                        );
                    }
                    scope.attr_set::<B>(statement_handle, attribute, int_val)?;
                    Ok(SqlReturn::SUCCESS)
                }

                // The two implementation descriptors are read-only, but saying
                // so is the Driver Manager's job, not core's: `SQLSetStmtAttr`'s
                // HY017 row marks *both* of its clauses (DM). A driver-side
                // check here would return a code the spec assigns to the DM,
                // which the project's non-negotiable rule forbids.
                Some(StatementAttribute::ImpRowDesc | StatementAttribute::ImpParamDesc) => {
                    tracing::debug!(
                        "SQLSetStmtAttrW: {:?} reached the driver; HY017 is (DM), so it is \
                         accepted here",
                        attr
                    );
                    Ok(SqlReturn::SUCCESS)
                }

                // An application descriptor. `SQL_NULL_DESC` reverts to the one
                // implicitly allocated with this statement; any other value is a
                // descriptor the application allocated, which this statement then
                // uses in place of its own.
                Some(
                    attr_desc @ (StatementAttribute::AppRowDesc | StatementAttribute::AppParamDesc),
                ) => {
                    let role = if attr_desc == StatementAttribute::AppRowDesc {
                        DescriptorRole::Ard
                    } else {
                        DescriptorRole::Apd
                    };
                    if int_val == SQL_NULL_DESC {
                        tracing::debug!(
                            "SQLSetStmtAttrW: {:?} set to SQL_NULL_DESC; reverting to the \
                             implicit descriptor",
                            attr_desc
                        );
                        scope
                            .get::<StatementHandle<B>>(statement_handle)?
                            .set_app_descriptor(role, None);
                        return Ok(SqlReturn::SUCCESS);
                    }
                    let token = int_val as *mut c_void;
                    // Spec HY024, and *not* (DM): "The Attribute argument was
                    // SQL_ATTR_APP_ROW_DESC or SQL_ATTR_APP_PARAM_DESC, and
                    // ValuePtr was an explicitly allocated descriptor handle that
                    // is not on the same connection as the StatementHandle
                    // argument." The same row makes the general case core's too:
                    // "For all other connection and statement attributes, the
                    // driver must verify the value specified in ValuePtr."
                    //
                    // The check is group-independent on purpose: a descriptor on
                    // another connection is in another lock group, and telling it
                    // apart from a garbage value is exactly what this SQLSTATE is
                    // for.
                    //
                    // The parent *chain*, not just the parent: a descriptor
                    // allocated by SQLAllocHandle is parented to the connection,
                    // and one of a statement's own four is parented to that
                    // statement. Both are "on this connection", and the second is
                    // legitimate — HY017's clause is "an implicitly allocated
                    // descriptor handle *other than the handle originally
                    // allocated*", which implies the original is allowed. That
                    // clause is (DM) in any case, so core does not check it.
                    let conn = scope.get::<StatementHandle<B>>(statement_handle)?.conn;
                    let on_this_connection = registry()
                        .parent_of(token, HandleKind::Desc)
                        .is_some_and(|parent| {
                            parent == conn
                                || registry().parent_of(parent, HandleKind::Stmt) == Some(conn)
                        });
                    if !on_this_connection {
                        return Err(OdbcError::general(
                            format!(
                                "SQLSetStmtAttr: {attr_desc:?} was given a value that is not a \
                                 descriptor on this statement's connection"
                            ),
                            SqlState::invalid_attribute_value(),
                        ));
                    }
                    tracing::debug!(
                        "SQLSetStmtAttrW: {:?} now uses descriptor {:?}",
                        attr_desc,
                        token
                    );
                    scope
                        .get::<StatementHandle<B>>(statement_handle)?
                        .set_app_descriptor(role, Some(token));
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
                    substitute_stmt_attr::<B>(
                        scope,
                        statement_handle,
                        attribute,
                        "SQL_ATTR_ROW_ARRAY_SIZE",
                        int_val,
                        SQL_ROW_ARRAY_SIZE_DEFAULT,
                        "1",
                    )
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
                    substitute_stmt_attr::<B>(
                        scope,
                        statement_handle,
                        attribute,
                        "SQL_ATTR_PARAMSET_SIZE",
                        int_val,
                        SQL_PARAMSET_SIZE_DEFAULT,
                        "1",
                    )
                }

                // No row limit is applied anywhere: `SQLFetch` asks the backend
                // for the next row until the backend says there are none, and
                // nothing counts. Storing a non-zero limit verbatim would have
                // `SQLGetStmtAttr` report a cap the driver then does not honour,
                // so an application asking for 10 rows quietly receives all of
                // them. SQL_ATTR_MAX_ROWS is on the spec's own 01S02
                // substitution list, so say so instead.
                Some(StatementAttribute::MaxRows) if int_val != SQL_MAX_ROWS_DEFAULT => {
                    offer_to_data_source::<B, _>(
                        scope,
                        statement_handle,
                        Substitution {
                            attribute,
                            name: "SQL_ATTR_MAX_ROWS",
                            requested: int_val,
                            fallback: SQL_MAX_ROWS_DEFAULT,
                            fallback_display: "0 (no limit)",
                        },
                        |c| B::set_max_rows(c, int_val).into_odbc(),
                    )
                    .map(|(_, ret)| ret)
                }

                // The application withdrew the row limit. The backend that was
                // told to apply it is the only party that can lift it, so the
                // reset goes to the same hook the cap did — see
                // `reset_at_data_source` for why it carries no `01S02`.
                Some(StatementAttribute::MaxRows) if int_val == SQL_MAX_ROWS_DEFAULT => {
                    reset_at_data_source::<B, _>(
                        scope,
                        statement_handle,
                        attribute,
                        "SQL_ATTR_MAX_ROWS",
                        SQL_MAX_ROWS_DEFAULT,
                        |c| B::set_max_rows(c, SQL_MAX_ROWS_DEFAULT).into_odbc(),
                    )
                }

                // The counterpart of SQL_ATTR_MAX_ROWS, one column over: no
                // character or binary value is truncated to this limit on the
                // way out, since neither `sql_fetch` nor `sql_get_data`
                // consults it. 0 — "the driver attempts to return all available
                // data" — is therefore the only value core can report honestly,
                // and SQL_ATTR_MAX_LENGTH is on the spec's 01S02 list, which
                // says how to report it.
                Some(StatementAttribute::MaxLength) if int_val != SQL_MAX_LENGTH_DEFAULT => {
                    offer_to_data_source::<B, _>(
                        scope,
                        statement_handle,
                        Substitution {
                            attribute,
                            name: "SQL_ATTR_MAX_LENGTH",
                            requested: int_val,
                            fallback: SQL_MAX_LENGTH_DEFAULT,
                            fallback_display: "0 (all available data)",
                        },
                        |c| B::set_max_length(c, int_val).into_odbc(),
                    )
                    .map(|(_, ret)| ret)
                }

                // As SQL_ATTR_MAX_ROWS above, one column over: a data source
                // still truncating to 4096 bytes while SQLGetStmtAttr reports
                // "all available data" is the same defect in the other
                // attribute.
                Some(StatementAttribute::MaxLength) if int_val == SQL_MAX_LENGTH_DEFAULT => {
                    reset_at_data_source::<B, _>(
                        scope,
                        statement_handle,
                        attribute,
                        "SQL_ATTR_MAX_LENGTH",
                        SQL_MAX_LENGTH_DEFAULT,
                        |c| B::set_max_length(c, SQL_MAX_LENGTH_DEFAULT).into_odbc(),
                    )
                }

                // A keyset is a keyset-driven cursor's window, and core has no
                // keyset-driven cursor: `SQL_ATTR_CURSOR_TYPE` is substituted
                // back to forward-only a few arms above, so a non-zero keyset
                // size describes a cursor that cannot exist on this statement.
                // Also on the spec's 01S02 list.
                Some(StatementAttribute::KeysetSize) if int_val != SQL_KEYSET_SIZE_DEFAULT => {
                    substitute_stmt_attr::<B>(
                        scope,
                        statement_handle,
                        attribute,
                        "SQL_ATTR_KEYSET_SIZE",
                        int_val,
                        SQL_KEYSET_SIZE_DEFAULT,
                        "0",
                    )
                }

                // A non-zero timeout is offered to the data source, which is the
                // only party that can enforce one well: it knows when the query
                // started and can stop the work rather than abandon it.
                //
                // `Backend::set_query_timeout` defaults to `NotImplemented`, so
                // a backend that has said nothing about timeouts still gets the
                // substitution this arm has always applied — an application
                // that sets 30 seconds and receives SQL_SUCCESS is entitled to
                // believe a runaway query will be cut off, and none would be.
                // SQL_ATTR_QUERY_TIMEOUT is on the spec's own closed 01S02 list,
                // which is what makes the substitution the right way to say so.
                //
                // A *real* backend failure is propagated instead of substituted.
                // 01S02 tells an application "this driver capped your timeout",
                // which is a different claim from "the connection is broken",
                // and quietly reporting the first for the second sends it on to
                // execute against a connection it has been told is fine.
                //
                // The spec's own 01S02 case for this attribute is clamping —
                // "if the specified timeout exceeds the maximum timeout in the
                // data source ... SQLSetStmtAttr substitutes that value" — which
                // core cannot report, because `set_query_timeout` returns no
                // clamped value. A backend that clamps returns `Ok` and core
                // stores what was asked for. Reporting the clamped number needs
                // a `Result<usize, _>` there; deliberately not done, since it
                // would put a value core never uses into the hook's contract.
                Some(StatementAttribute::QueryTimeout) if int_val != SQL_QUERY_TIMEOUT_DEFAULT => {
                    let enforcer = offer_to_data_source::<B, _>(
                        scope,
                        statement_handle,
                        Substitution {
                            attribute,
                            name: "SQL_ATTR_QUERY_TIMEOUT",
                            requested: int_val,
                            fallback: SQL_QUERY_TIMEOUT_DEFAULT,
                            fallback_display: "0 (no timeout)",
                        },
                        |c| B::set_query_timeout(c, int_val).into_odbc(),
                    )?;
                    let (enforcer, ret) = enforcer;
                    // The one member of the family with a core-side fallback:
                    // a backend that cannot set a server-side deadline but can
                    // be cancelled hands the deadline to core's timer. Recorded
                    // only for `CoreCancels` — arming a timer for a deadline the
                    // data source is already managing would give the statement
                    // two independent cancellers racing the same query.
                    scope
                        .get::<StatementHandle<B>>(statement_handle)?
                        .core_query_timeout = match enforcer {
                        Some(QueryTimeout::CoreCancels) => Some(int_val),
                        Some(QueryTimeout::DataSource) | None => None,
                    };
                    Ok(ret)
                }

                // The application withdrew the deadline. The spec's row for
                // this attribute is explicit — "if the value is 0, there is
                // no timeout" — and there are two enforcers to tell, not one:
                // the data source, which may be holding a server-side
                // deadline, and core's own timer.
                //
                // Guarded on `==` rather than left to the store-only
                // catch-all below, which is where this value used to land:
                // that arm stores the attribute and touches nothing else, so
                // an earlier non-zero deadline stayed armed and the next
                // query was cancelled with an `HYT00` the application had
                // just opted out of.
                Some(StatementAttribute::QueryTimeout) if int_val == SQL_QUERY_TIMEOUT_DEFAULT => {
                    let ret = reset_at_data_source::<B, _>(
                        scope,
                        statement_handle,
                        attribute,
                        "SQL_ATTR_QUERY_TIMEOUT",
                        SQL_QUERY_TIMEOUT_DEFAULT,
                        |c| B::set_query_timeout(c, SQL_QUERY_TIMEOUT_DEFAULT).into_odbc(),
                    )?;
                    // Core's timer is armed from this field at every
                    // statement-producing call and at `SQLFetch`, so a
                    // deadline recorded by an earlier set has to go with the
                    // attribute. Unconditionally `None`: 0 is "no timeout"
                    // whatever the hook answered.
                    scope
                        .get::<StatementHandle<B>>(statement_handle)?
                        .core_query_timeout = None;
                    Ok(ret)
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

                // All other recognised attributes: store value, in whichever
                // map owns it — see `HeaderOwner`. The six remaining
                // header-field attributes reach here, and land on the ARD or
                // APD rather than on `stmt.attrs`.
                //
                // SQL_ATTR_ROWS_FETCHED_PTR, SQL_ATTR_ROW_STATUS_PTR and
                // SQL_ATTR_ROW_BIND_OFFSET_PTR reach here and are stored
                // verbatim, which is correct because `SQLFetch` now reads and
                // honours all three. They are deliberately *not* substituted:
                // the spec's 01S02 list is closed and names none of them, and
                // there is no "similar value" to substitute for a pointer.
                Some(_) => {
                    scope.attr_set::<B>(statement_handle, attribute, int_val)?;
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
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(0),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::MaxRows) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(0),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::MaxLength) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(0),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::NoScan) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_NOSCAN_OFF),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowBindType) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_BIND_BY_COLUMN),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::CursorType) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_CURSOR_FORWARD_ONLY),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::Concurrency) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_CONCUR_READ_ONLY),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RetrieveData) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_RD_ON),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::UseBookmarks) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_UB_OFF),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowArraySize) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_ROW_ARRAY_SIZE_DEFAULT),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ParamsetSize) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
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
                    let v = scope
                        .attr_get::<B>(statement_handle, attribute)?
                        .unwrap_or(0);
                    write_ulen(v);
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::EnableAutoIpd) => {
                    write_ulen(SQL_FALSE as usize); // not supported
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::AsyncEnable) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(0),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::MetadataId) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_FALSE as usize),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::CursorScrollable) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_NONSCROLLABLE),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::CursorSensitivity) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(usize::from(SQL_UNSPECIFIED)),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::SimulateCursor) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(0),
                    );
                    Ok(SqlReturn::SUCCESS)
                }

                // Pointer-valued attributes.
                Some(StatementAttribute::RowStatusPtr) => {
                    write_ptr(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(0),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowsFetchedPtr) => {
                    write_ptr(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(0),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::RowBindOffsetPtr) => {
                    write_ptr(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(0),
                    );
                    Ok(SqlReturn::SUCCESS)
                }

                // Descriptor handle attrs: return the descriptor handles in
                // *effect*, which for the two application descriptors is an
                // application-supplied one when it has set one and the implicit
                // one otherwise — `desc_of` applies the override, so this needs no
                // branch of its own. The Windows DM requires these to build its
                // CLI dispatch table.
                Some(StatementAttribute::AppRowDesc) => {
                    write_ptr(
                        scope
                            .desc_of::<B>(statement_handle, DescriptorRole::Ard)?
                            .token() as usize,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::AppParamDesc) => {
                    write_ptr(
                        scope
                            .desc_of::<B>(statement_handle, DescriptorRole::Apd)?
                            .token() as usize,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ImpRowDesc) => {
                    write_ptr(
                        scope
                            .desc_of::<B>(statement_handle, DescriptorRole::Ird)?
                            .token() as usize,
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ImpParamDesc) => {
                    write_ptr(
                        scope
                            .desc_of::<B>(statement_handle, DescriptorRole::Ipd)?
                            .token() as usize,
                    );
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
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_KEYSET_SIZE_DEFAULT),
                    );
                    Ok(SqlReturn::SUCCESS)
                }
                Some(StatementAttribute::ParamBindType) => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
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
                    write_ptr(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(0),
                    );
                    Ok(SqlReturn::SUCCESS)
                }

                // SQL_ROWSET_SIZE has no `StatementAttribute` variant, so it lands
                // here rather than in an arm above. It must be readable: the
                // `01S02` its setter returns closes with "(SQLGetStmtAttr can be
                // called to determine the temporarily substituted value)", and a
                // substitution the application cannot read back is not a
                // substitution it was told about.
                None if attribute == crate::types::SQL_ROWSET_SIZE => {
                    write_ulen(
                        scope
                            .attr_get::<B>(statement_handle, attribute)?
                            .unwrap_or(SQL_ROW_ARRAY_SIZE_DEFAULT),
                    );
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
    use odbc_sys::Desc;

    use crate::handles::{ConnectionHandle, HeaderOwner};
    use crate::test_utils::{
        MockBackend, MockCoreCancelsTimeoutBackend, MockFailingQueryTimeoutBackend,
        MockNoQueryTimeoutBackend, MockQueryTimeoutBackend, alloc_env_conn_stmt,
        cleanup_env_conn_stmt, with_descriptor, with_handle,
    };
    use crate::types::SQL_ROWSET_SIZE;
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

    /// `SQLSetStmtAttr`'s mapping table makes this attribute the ARD's
    /// `SQL_DESC_BIND_TYPE`, and says setting one sets the other. Two copies of
    /// the value is what this removes, so the assertion is in two parts: the
    /// descriptor holds it, and `stmt.attrs` does *not*.
    ///
    /// The second half is the one that matters. Writing to both maps leaves
    /// every other test green — a reader that finds the right value cannot tell
    /// a duplicate write from a single one — so without it the guarantee here
    /// would be structural only.
    #[test]
    fn a_row_header_attribute_is_stored_on_the_ard() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let attribute = StatementAttribute::RowBindType as i32;

            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                attribute,
                std::ptr::without_provenance_mut(SQL_BIND_BY_COLUMN + 8),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                assert_eq!(
                    ard.attrs.get(&(Desc::BindType as u16)).copied(),
                    Some(SQL_BIND_BY_COLUMN + 8),
                    "SQL_ATTR_ROW_BIND_TYPE did not land on the ARD's header"
                );
            });
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert!(
                    !handle.attrs.contains_key(&attribute),
                    "the value is also in stmt.attrs, so there are two copies to disagree"
                );
            });

            // And it still reads back through the statement attribute, which is
            // the whole point of the descriptor being the single copy.
            let mut out: usize = 0;
            assert_eq!(
                sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    attribute,
                    std::ptr::from_mut(&mut out).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(out, SQL_BIND_BY_COLUMN + 8);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The parameter-side counterpart. `SQL_ATTR_PARAM_BIND_TYPE` is the APD's
    /// `SQL_DESC_BIND_TYPE`.
    #[test]
    fn a_param_header_attribute_is_stored_on_the_apd() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let attribute = StatementAttribute::ParamBindType as i32;

            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                attribute,
                std::ptr::without_provenance_mut(SQL_BIND_BY_COLUMN + 16),
                0,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Apd, |apd| {
                assert_eq!(
                    apd.attrs.get(&(Desc::BindType as u16)).copied(),
                    Some(SQL_BIND_BY_COLUMN + 16),
                    "SQL_ATTR_PARAM_BIND_TYPE did not land on the APD's header"
                );
            });
            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ard, |ard| {
                assert!(
                    !ard.attrs.contains_key(&(Desc::BindType as u16)),
                    "a parameter-side attribute landed on the row descriptor"
                );
            });
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                assert!(
                    !handle.attrs.contains_key(&attribute),
                    "the value is also in stmt.attrs, so there are two copies to disagree"
                );
            });

            let mut out: usize = 0;
            assert_eq!(
                sql_get_stmt_attr_w::<MockBackend>(
                    stmt,
                    attribute,
                    std::ptr::from_mut(&mut out).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(out, SQL_BIND_BY_COLUMN + 16);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// The substituted value goes to the descriptor too, not just the accepted
    /// one. `SQL_ATTR_ROW_ARRAY_SIZE` and `SQL_ATTR_PARAMSET_SIZE` are the two
    /// header-field attributes that take the `01S02` path, and that path writes
    /// through `substitute_stmt_attr` rather than the catch-all arm — a second
    /// write site, and so a second place for the routing to be missed.
    #[test]
    fn a_substituted_header_attribute_lands_on_the_descriptor() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            for (attribute, owner) in [
                (StatementAttribute::RowArraySize as i32, HeaderOwner::Ard),
                (StatementAttribute::ParamsetSize as i32, HeaderOwner::Apd),
            ] {
                let ret = sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    attribute,
                    std::ptr::without_provenance_mut(10usize),
                    0,
                );
                assert_eq!(ret, SqlReturn::SUCCESS_WITH_INFO, "attr {attribute}");

                // Both attributes name `SQL_DESC_ARRAY_SIZE`; which descriptor
                // it lands on is what tells the two apart.
                let role = match owner {
                    HeaderOwner::Ard => DescriptorRole::Ard,
                    HeaderOwner::Apd => DescriptorRole::Apd,
                };
                with_descriptor::<MockBackend, _>(stmt, role, |desc| {
                    assert_eq!(
                        desc.attrs.get(&(Desc::ArraySize as u16)).copied(),
                        Some(1),
                        "the substituted value did not land on the {owner:?} header"
                    );
                });
                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    assert!(
                        !handle.attrs.contains_key(&attribute),
                        "the substituted value is also in stmt.attrs"
                    );
                });
            }

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `SQL_NULL_DESC` means "revert to the descriptor implicitly allocated
    /// with this statement", and that implicit descriptor is the only state
    /// core has — so this is a legitimate no-op success, not a refusal. The
    /// regression guard against over-refusing once the arm below started
    /// refusing anything.
    #[test]
    fn setting_an_application_descriptor_to_null_desc_succeeds() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            for attr in [
                StatementAttribute::AppRowDesc,
                StatementAttribute::AppParamDesc,
            ] {
                let ret = sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    attr as i32,
                    std::ptr::without_provenance_mut(SQL_NULL_DESC),
                    0,
                );
                assert_eq!(ret, SqlReturn::SUCCESS, "{attr:?} with SQL_NULL_DESC");

                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    assert_eq!(
                        handle.diagnostics.len(),
                        0,
                        "{attr:?} with SQL_NULL_DESC posted a diagnostic"
                    );
                });
            }

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// An explicit descriptor associated as the ARD is where `SQLBindCol` writes,
    /// and `SQLGetStmtAttr` reports it rather than the implicit one.
    ///
    /// There is deliberately no counterpart for `SQL_ATTR_IMP_ROW_DESC` or
    /// `SQL_ATTR_IMP_PARAM_DESC`: HY017 is **(DM)** on both clauses, so a test
    /// there would pin behaviour the driver must not have.
    #[test]
    fn an_explicit_ard_replaces_the_implicit_one() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let implicit = app_row_desc_of(stmt);
            let explicit = alloc_explicit_desc(conn);

            assert_eq!(
                sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::AppRowDesc as i32,
                    explicit,
                    0
                ),
                SqlReturn::SUCCESS
            );

            let reported = app_row_desc_of(stmt);
            assert_eq!(reported, explicit, "the override must be what is reported");
            assert_ne!(reported, implicit);

            let mut buf = [0u8; 4];
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockBackend>(
                    stmt,
                    1,
                    odbc_sys::CDataType::SLong as i16,
                    buf.as_mut_ptr().cast(),
                    4,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                desc_count(explicit),
                1,
                "SQLBindCol must have written the explicit ARD"
            );
            assert_eq!(
                desc_count(implicit),
                0,
                "SQLBindCol wrote the implicit ARD as well"
            );

            free_explicit_desc(explicit);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Two statements sharing one explicit ARD share one set of bindings — there
    /// is one storage, so a bind through either is visible through both, and
    /// `SQLFreeStmt(SQL_UNBIND)` on one clears the other's too.
    ///
    /// That last part is spec-correct rather than a wart: the spec makes the
    /// descriptor *be* the binding, so two statements pointed at one descriptor
    /// have one binding set between them.
    #[test]
    fn two_statements_can_share_one_explicit_descriptor() {
        unsafe {
            let (env, conn, stmt_a) = alloc_env_conn_stmt();
            let mut stmt_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::handle::sql_alloc_handle::<MockBackend>(
                    odbc_sys::HandleType::Stmt as i16,
                    conn,
                    &mut stmt_b
                ),
                SqlReturn::SUCCESS
            );
            let explicit = alloc_explicit_desc(conn);

            for stmt in [stmt_a, stmt_b] {
                assert_eq!(
                    sql_set_stmt_attr_w::<MockBackend>(
                        stmt,
                        StatementAttribute::AppRowDesc as i32,
                        explicit,
                        0
                    ),
                    SqlReturn::SUCCESS
                );
                assert_eq!(app_row_desc_of(stmt), explicit);
            }

            let mut buf = [0u8; 4];
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockBackend>(
                    stmt_a,
                    1,
                    odbc_sys::CDataType::SLong as i16,
                    buf.as_mut_ptr().cast(),
                    4,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(desc_count(explicit), 1);

            // Unbinding through the *other* statement clears it, because there
            // is one descriptor and therefore one binding.
            assert_eq!(
                crate::ffi::handle::sql_free_stmt::<MockBackend>(
                    stmt_b,
                    odbc_sys::FreeStmtOption::Unbind as u16
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(
                desc_count(explicit),
                0,
                "the shared descriptor still holds a binding after SQL_UNBIND"
            );

            free_explicit_desc(explicit);
            let _ = crate::ffi::handle::sql_free_handle::<MockBackend>(
                odbc_sys::HandleType::Stmt as i16,
                stmt_b,
            );
            cleanup_env_conn_stmt(env, conn, stmt_a);
        }
    }

    /// Freeing the explicit descriptor reverts every statement that used it.
    ///
    /// The spec: "all statement handles to which the freed descriptor applied
    /// automatically revert to the descriptors implicitly allocated for them."
    #[test]
    fn freeing_an_explicit_descriptor_reverts_its_statements() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let implicit = app_row_desc_of(stmt);
            let explicit = alloc_explicit_desc(conn);
            assert_eq!(
                sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::AppRowDesc as i32,
                    explicit,
                    0
                ),
                SqlReturn::SUCCESS
            );

            free_explicit_desc(explicit);

            assert_eq!(
                app_row_desc_of(stmt),
                implicit,
                "the statement did not revert to its own ARD"
            );
            // And it still works, which is the point of reverting rather than
            // leaving a dangling override.
            let mut buf = [0u8; 4];
            assert_eq!(
                crate::ffi::bind::sql_bind_col::<MockBackend>(
                    stmt,
                    1,
                    odbc_sys::CDataType::SLong as i16,
                    buf.as_mut_ptr().cast(),
                    4,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(desc_count(implicit), 1);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A descriptor allocated on another connection is `HY024`, and that row is
    /// **not** (DM): "For all other connection and statement attributes, the
    /// driver must verify the value specified in ValuePtr."
    #[test]
    fn a_descriptor_from_another_connection_is_rejected() {
        unsafe {
            let (env, conn_a, stmt) = alloc_env_conn_stmt();
            let mut conn_b: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                crate::ffi::handle::sql_alloc_handle::<MockBackend>(
                    odbc_sys::HandleType::Dbc as i16,
                    env,
                    &mut conn_b
                ),
                SqlReturn::SUCCESS
            );
            let foreign = alloc_explicit_desc(conn_b);

            assert_eq!(
                sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::AppRowDesc as i32,
                    foreign,
                    0
                ),
                SqlReturn::ERROR
            );
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let record = handle
                    .diagnostics
                    .get(0)
                    .expect("no diagnostic for the cross-connection descriptor");
                assert_eq!(
                    record.sqlstate.as_str(),
                    crate::types::sql_state::INVALID_ATTRIBUTE_VALUE
                );
            });

            free_explicit_desc(foreign);
            let _ = crate::ffi::handle::sql_free_handle::<MockBackend>(
                odbc_sys::HandleType::Dbc as i16,
                conn_b,
            );
            cleanup_env_conn_stmt(env, conn_a, stmt);
        }
    }

    /// A value that is not a descriptor token at all is `HY024` too — the
    /// question core answers is "is this a descriptor on my connection", and a
    /// garbage value fails it for the same reason a foreign one does.
    #[test]
    fn a_non_descriptor_value_is_rejected() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(
                sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::AppRowDesc as i32,
                    std::ptr::without_provenance_mut(0x1234usize),
                    0
                ),
                SqlReturn::ERROR
            );
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let record = handle.diagnostics.get(0).expect("no diagnostic");
                assert_eq!(
                    record.sqlstate.as_str(),
                    crate::types::sql_state::INVALID_ATTRIBUTE_VALUE
                );
            });
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A statement's *own* ARD token set back onto itself is accepted.
    ///
    /// `HY017`'s clause is "an implicitly allocated descriptor handle **other
    /// than the handle originally allocated** for the ARD or APD", which implies
    /// the original is allowed — and that clause is (DM) anyway. So the check
    /// core does make compares the parent *chain*: a token whose parent is this
    /// connection, or whose parent is a statement on this connection, is on this
    /// connection either way.
    #[test]
    fn a_statements_own_descriptor_is_accepted() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let implicit = app_row_desc_of(stmt);
            assert_eq!(
                sql_set_stmt_attr_w::<MockBackend>(
                    stmt,
                    StatementAttribute::AppRowDesc as i32,
                    implicit,
                    0
                ),
                SqlReturn::SUCCESS
            );
            assert_eq!(app_row_desc_of(stmt), implicit);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `SQL_ATTR_APP_ROW_DESC` as the application sees it.
    unsafe fn app_row_desc_of(stmt: *mut c_void) -> *mut c_void {
        let mut out: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            sql_get_stmt_attr_w::<MockBackend>(
                stmt,
                StatementAttribute::AppRowDesc as i32,
                std::ptr::from_mut(&mut out).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS);
        out
    }

    /// `SQL_DESC_COUNT` of a descriptor, read through `SQLGetDescField`.
    unsafe fn desc_count(desc: *mut c_void) -> isize {
        let mut count: isize = -1;
        let ret = unsafe {
            crate::ffi::desc::sql_get_desc_field_w::<MockBackend>(
                desc,
                0,
                Desc::Count as i16,
                std::ptr::from_mut(&mut count).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS);
        count
    }

    unsafe fn alloc_explicit_desc(conn: *mut c_void) -> *mut c_void {
        let mut desc: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            crate::ffi::handle::sql_alloc_handle::<MockBackend>(
                odbc_sys::HandleType::Desc as i16,
                conn,
                &mut desc,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS);
        desc
    }

    unsafe fn free_explicit_desc(desc: *mut c_void) {
        let ret = unsafe {
            crate::ffi::handle::sql_free_handle::<MockBackend>(
                odbc_sys::HandleType::Desc as i16,
                desc,
            )
        };
        assert_eq!(ret, SqlReturn::SUCCESS);
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

    /// `SQL_ROWSET_SIZE` is the rowset `SQLExtendedFetch` reads, and this driver
    /// produces exactly one row. It arrives as an *unrecognised* attribute —
    /// `odbc-sys` models only the 3.x `SQL_ATTR_ROW_ARRAY_SIZE` (27) — so without
    /// its own arm it falls to the catch-all that accepts unknown attributes
    /// silently, and an application asking for ten rows would receive one under
    /// `SQL_SUCCESS`. It is also where the Driver Manager's `SQLSetScrollOptions`
    /// mapping lands.
    #[test]
    fn rowset_size_greater_than_one_is_substituted_with_01s02() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                SQL_ROWSET_SIZE,
                std::ptr::without_provenance_mut(10usize),
                0,
            );
            assert_eq!(
                ret,
                SqlReturn::SUCCESS_WITH_INFO,
                "a rowset core cannot produce was accepted silently"
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
                    SQL_ROWSET_SIZE,
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

    /// The value the driver will actually use is 1, so setting 1 is not a
    /// substitution and must not warn.
    #[test]
    fn rowset_size_of_one_is_accepted_without_warning() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_set_stmt_attr_w::<MockBackend>(
                stmt,
                SQL_ROWSET_SIZE,
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
        // An application told SUCCESS would wait forever on a runaway query, and
        // SQL_ATTR_QUERY_TIMEOUT is named on the spec's own 01S02 substitution
        // list for exactly this case.
        //
        // `alloc_env_conn_stmt` does not connect, so this covers the branch
        // where there is no connection to offer the timeout to. The three tests
        // below cover the connected branches, which are the ones that reach
        // `Backend::set_query_timeout` at all.
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

    /// Read `SQL_ATTR_QUERY_TIMEOUT` back for a backend `B`, after setting it
    /// to `seconds`. Returns the `SQLSetStmtAttr` return code, the first
    /// diagnostic's SQLSTATE if there is one, and the read-back value.
    unsafe fn set_then_get_query_timeout<B: Backend>(
        seconds: usize,
    ) -> (
        SqlReturn,
        Option<String>,
        usize,
        *mut c_void,
        *mut c_void,
        *mut c_void,
    ) {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();
            let set = sql_set_stmt_attr_w::<B>(
                stmt,
                StatementAttribute::QueryTimeout as i32,
                seconds as *mut c_void,
                0,
            );
            let state = crate::test_utils::with_handle::<B, StatementHandle<B>, _>(stmt, |h| {
                h.diagnostics
                    .get(0)
                    .map(|r| r.sqlstate.as_str().to_string())
            });
            let mut val: usize = usize::MAX;
            let get = sql_get_stmt_attr_w::<B>(
                stmt,
                StatementAttribute::QueryTimeout as i32,
                std::ptr::from_mut(&mut val).cast(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(get, SqlReturn::SUCCESS, "reading the attribute back");
            (set, state, val, env, conn, stmt)
        }
    }

    #[test]
    fn a_data_source_enforced_timeout_arms_no_core_timer() {
        // Two enforcers for one deadline would mean two independent cancellers
        // racing the same query, so `DataSource` must leave `core_query_timeout`
        // clear even though the attribute itself is stored.
        unsafe {
            let (set, _state, val, env, conn, stmt) =
                set_then_get_query_timeout::<MockQueryTimeoutBackend>(30);
            assert_eq!(set, SqlReturn::SUCCESS);
            assert_eq!(val, 30);

            with_handle::<MockQueryTimeoutBackend, StatementHandle<MockQueryTimeoutBackend>, _>(
                stmt,
                |s| {
                    assert_eq!(
                        s.core_query_timeout, None,
                        "the data source owns this deadline; core must not arm a second one"
                    );
                },
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockQueryTimeoutBackend>(
                env, conn, stmt,
            );
        }
    }

    #[test]
    fn a_backend_that_delegates_to_core_records_the_deadline_for_the_timer() {
        unsafe {
            let (set, state, val, env, conn, stmt) =
                set_then_get_query_timeout::<MockCoreCancelsTimeoutBackend>(30);

            assert_eq!(set, SqlReturn::SUCCESS, "delegating is still accepting");
            assert_eq!(state, None, "a delegated timeout is not a substitution");
            assert_eq!(val, 30, "SQLGetStmtAttr must report what was asked for");

            with_handle::<
                MockCoreCancelsTimeoutBackend,
                StatementHandle<MockCoreCancelsTimeoutBackend>,
                _,
            >(stmt, |s| {
                assert_eq!(
                    s.core_query_timeout,
                    Some(30),
                    "core was asked to own this deadline and did not record it"
                );
            });

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockCoreCancelsTimeoutBackend>(
                env, conn, stmt,
            );
        }
    }

    /// `SQL_ATTR_QUERY_TIMEOUT` = 0 is the spec's "there is no timeout", so the
    /// deadline core recorded for its own timer has to go with it.
    ///
    /// `core_query_timeout` is read at every statement-producing call and at
    /// `SQLFetch`, and it had one writer, inside the arm that runs only for a
    /// *non-default* value. Withdrawing the timeout therefore left the old
    /// deadline armed and the application's next query died with an `HYT00` it
    /// had already opted out of.
    ///
    /// The backend is `MockCoreCancelsTimeoutBackend` because it is the only one
    /// for which core arms a timer at all: a `DataSource` backend leaves the field
    /// `None` throughout, so the assertion would pass without proving anything.
    #[test]
    fn withdrawing_a_query_timeout_disarms_cores_timer() {
        unsafe {
            type B = MockCoreCancelsTimeoutBackend;
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();

            assert_eq!(
                sql_set_stmt_attr_w::<B>(
                    stmt,
                    StatementAttribute::QueryTimeout as i32,
                    std::ptr::without_provenance_mut::<c_void>(30),
                    0,
                ),
                SqlReturn::SUCCESS,
            );
            with_handle::<B, StatementHandle<B>, _>(stmt, |s| {
                assert_eq!(
                    s.core_query_timeout,
                    Some(30),
                    "the deadline core was asked to own was never recorded",
                );
            });

            assert_eq!(
                sql_set_stmt_attr_w::<B>(
                    stmt,
                    StatementAttribute::QueryTimeout as i32,
                    std::ptr::without_provenance_mut::<c_void>(SQL_QUERY_TIMEOUT_DEFAULT),
                    0,
                ),
                SqlReturn::SUCCESS,
                "withdrawing a timeout is not an error",
            );

            let mut val: usize = usize::MAX;
            assert_eq!(
                sql_get_stmt_attr_w::<B>(
                    stmt,
                    StatementAttribute::QueryTimeout as i32,
                    std::ptr::from_mut(&mut val).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS,
            );
            assert_eq!(
                val, SQL_QUERY_TIMEOUT_DEFAULT,
                "SQLGetStmtAttr must report that there is no timeout",
            );

            with_handle::<B, StatementHandle<B>, _>(stmt, |s| {
                assert_eq!(
                    s.core_query_timeout, None,
                    "the application withdrew the deadline; core must not keep enforcing it",
                );
            });

            crate::test_utils::cleanup_connected_env_conn_stmt::<B>(env, conn, stmt);
        }
    }

    #[test]
    fn a_backend_that_can_set_a_query_timeout_gets_the_value_and_no_substitution() {
        // The point of the hook: the timeout reaches the data source, so core
        // may honestly store what the application asked for.
        unsafe {
            let (set, state, val, env, conn, stmt) =
                set_then_get_query_timeout::<MockQueryTimeoutBackend>(30);

            assert_eq!(set, SqlReturn::SUCCESS, "the backend accepted the timeout");
            assert_eq!(state, None, "an accepted timeout is not a substitution");
            assert_eq!(val, 30, "SQLGetStmtAttr must report what was asked for");

            // The read-back above passes whether or not the hook ran, because
            // core stores the value either way. This is the assertion that
            // distinguishes "applied" from "merely stored".
            with_handle::<MockQueryTimeoutBackend, ConnectionHandle<MockQueryTimeoutBackend>, _>(
                conn,
                |c| {
                    let applied = c
                        .connection
                        .as_ref()
                        .expect("the helper connected")
                        .query_timeout
                        .load(std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(applied, 30, "Backend::set_query_timeout was never called");
                },
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockQueryTimeoutBackend>(
                env, conn, stmt,
            );
        }
    }

    #[test]
    fn a_backend_that_does_not_implement_the_hook_still_gets_the_01s02_substitution() {
        // Every driver that predates the hook is this backend. Its behaviour
        // must be exactly what it was before: 0 stored, 01S02 posted.
        unsafe {
            let (set, state, val, env, conn, stmt) =
                set_then_get_query_timeout::<MockNoQueryTimeoutBackend>(30);

            assert_eq!(set, SqlReturn::SUCCESS_WITH_INFO);
            assert_eq!(state.as_deref(), Some("01S02"));
            assert_eq!(
                val, 0,
                "an unenforceable timeout must read back as no timeout"
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockNoQueryTimeoutBackend>(
                env, conn, stmt,
            );
        }
    }

    #[test]
    fn a_backend_whose_set_query_timeout_really_fails_reports_the_failure() {
        // The distinction the arm exists to preserve. 01S02 says "your timeout
        // was capped"; this backend is saying "I could not talk to the data
        // source". Reporting the first for the second sends the application on
        // to execute against a connection it has been told is healthy.
        unsafe {
            let (set, state, _val, env, conn, stmt) =
                set_then_get_query_timeout::<MockFailingQueryTimeoutBackend>(30);

            assert_eq!(set, SqlReturn::ERROR, "a real backend failure is an error");
            assert_ne!(
                state.as_deref(),
                Some("01S02"),
                "a broken connection must not be reported as a capped timeout"
            );

            crate::test_utils::cleanup_connected_env_conn_stmt::<MockFailingQueryTimeoutBackend>(
                env, conn, stmt,
            );
        }
    }

    /// Set an attribute to `value` on backend `B` and report the return code,
    /// the first diagnostic's SQLSTATE, and the value read back.
    unsafe fn set_then_get_limit<B: Backend>(
        attribute: StatementAttribute,
        value: usize,
    ) -> (SqlReturn, Option<String>, usize) {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();
            let set = sql_set_stmt_attr_w::<B>(
                stmt,
                attribute as i32,
                std::ptr::without_provenance_mut::<c_void>(value),
                0,
            );
            let state = crate::test_utils::with_handle::<B, StatementHandle<B>, _>(stmt, |h| {
                h.diagnostics
                    .get(0)
                    .map(|r| r.sqlstate.as_str().to_string())
            });
            let mut val: usize = usize::MAX;
            assert_eq!(
                sql_get_stmt_attr_w::<B>(
                    stmt,
                    attribute as i32,
                    std::ptr::from_mut(&mut val).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SqlReturn::SUCCESS,
            );
            crate::test_utils::cleanup_connected_env_conn_stmt::<B>(env, conn, stmt);
            (set, state, val)
        }
    }

    #[test]
    fn a_data_source_that_can_cap_a_result_set_gets_max_rows_and_max_length() {
        // The spec confines both attributes to the data source: "a driver
        // should not emulate SQL_ATTR_MAX_ROWS behavior", and SQL_ATTR_MAX_LENGTH
        // "should be supported only when the data source (as opposed to the
        // driver) ... can implement it". A backend that really can gets the
        // value, and SQLGetStmtAttr reports what was asked for.
        unsafe {
            type B = crate::test_utils::MockLimitsBackend;
            for (attribute, value) in [
                (StatementAttribute::MaxRows, 10usize),
                (StatementAttribute::MaxLength, 4096usize),
            ] {
                let (set, state, val) = set_then_get_limit::<B>(attribute, value);
                assert_eq!(set, SqlReturn::SUCCESS, "{attribute:?} was accepted");
                assert_eq!(state, None, "an accepted limit is not a substitution");
                assert_eq!(val, value, "{attribute:?} must read back as requested");
            }
        }
    }

    #[test]
    fn a_backend_that_cannot_cap_keeps_the_01s02_substitution_for_both_limits() {
        // Every driver that predates these hooks is this backend, and core
        // deliberately does not emulate either limit on its behalf: both exist
        // to "reduce network traffic", which counting rows or bytes on the
        // client after they have already crossed the wire cannot do. So the
        // honest answer stays "no limit", reported via the spec's closed 01S02
        // list.
        unsafe {
            type B = crate::test_utils::MockNoQueryTimeoutBackend;
            for (attribute, value) in [
                (StatementAttribute::MaxRows, 10usize),
                (StatementAttribute::MaxLength, 4096usize),
            ] {
                let (set, state, val) = set_then_get_limit::<B>(attribute, value);
                assert_eq!(set, SqlReturn::SUCCESS_WITH_INFO, "{attribute:?}");
                assert_eq!(state.as_deref(), Some("01S02"), "{attribute:?}");
                assert_eq!(val, 0, "{attribute:?} must read back as no limit");
            }
        }
    }

    /// A backend that was told to cap the result set has to be told to stop.
    ///
    /// The three attributes core offers to the data source were guarded on the
    /// value not being the default, so a reset fell through to the store-only
    /// catch-all and the hook was never called. `SQLGetStmtAttr` then reported "no
    /// limit" for a data source still enforcing one, which is the read-back
    /// contract broken in the direction that silently loses rows.
    ///
    /// The call *sequence* is asserted, not a latest value: `[10]` and `[10, 0]`
    /// are what "the reset never arrived" and "it did" look like, and a
    /// single-slot recorder initialised to 0 cannot tell the second from a hook
    /// that was never called at all.
    #[test]
    fn resetting_a_limit_to_its_default_reaches_the_data_source() {
        unsafe {
            type B = crate::test_utils::MockLimitsBackend;
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();

            for (attribute, capped, default) in [
                (StatementAttribute::MaxRows, 10usize, SQL_MAX_ROWS_DEFAULT),
                (
                    StatementAttribute::MaxLength,
                    4096usize,
                    SQL_MAX_LENGTH_DEFAULT,
                ),
            ] {
                for value in [capped, default] {
                    assert_eq!(
                        sql_set_stmt_attr_w::<B>(
                            stmt,
                            attribute as i32,
                            std::ptr::without_provenance_mut::<c_void>(value),
                            0,
                        ),
                        SqlReturn::SUCCESS,
                        "{attribute:?} = {value} was refused",
                    );
                }
            }

            with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
                let applied = c.connection.as_ref().expect("the helper connected");
                assert_eq!(
                    *applied.max_rows_calls.lock().expect("not poisoned"),
                    vec![10, SQL_MAX_ROWS_DEFAULT],
                    "the reset never reached Backend::set_max_rows",
                );
                assert_eq!(
                    *applied.max_length_calls.lock().expect("not poisoned"),
                    vec![4096, SQL_MAX_LENGTH_DEFAULT],
                    "the reset never reached Backend::set_max_length",
                );
            });

            crate::test_utils::cleanup_connected_env_conn_stmt::<B>(env, conn, stmt);
        }
    }

    /// The third member of the family, whose reset has a second thing to do.
    ///
    /// Core clearing its own `core_query_timeout` is not enough when the data
    /// source is the enforcer: `MockQueryTimeoutBackend` answers
    /// `QueryTimeout::DataSource`, so the deadline lives at the data source and
    /// only `Backend::set_query_timeout` can withdraw it. Asserting the recorded
    /// value is 30 in between is what makes the final 0 proof the hook ran, rather
    /// than proof it never did — the recorder starts at 0.
    #[test]
    fn withdrawing_a_query_timeout_reaches_the_data_source() {
        unsafe {
            type B = MockQueryTimeoutBackend;
            let (env, conn, stmt) = crate::test_utils::alloc_connected_env_conn_stmt::<B>();

            let applied_timeout = |conn: *mut c_void| {
                with_handle::<B, ConnectionHandle<B>, _>(conn, |c| {
                    c.connection
                        .as_ref()
                        .expect("the helper connected")
                        .query_timeout
                        .load(std::sync::atomic::Ordering::SeqCst)
                })
            };

            for (value, expected) in [(30usize, 30usize), (SQL_QUERY_TIMEOUT_DEFAULT, 0usize)] {
                assert_eq!(
                    sql_set_stmt_attr_w::<B>(
                        stmt,
                        StatementAttribute::QueryTimeout as i32,
                        std::ptr::without_provenance_mut::<c_void>(value),
                        0,
                    ),
                    SqlReturn::SUCCESS,
                    "SQL_ATTR_QUERY_TIMEOUT = {value} was refused",
                );
                assert_eq!(
                    applied_timeout(conn),
                    expected,
                    "Backend::set_query_timeout was not called with {value}",
                );
            }

            crate::test_utils::cleanup_connected_env_conn_stmt::<B>(env, conn, stmt);
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
