//! Generic implementations of SQLBindParameter and SQLNumParams.

#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;

use odbc_sys::SqlDataType;

use crate::{
    backend::Backend,
    cancel::reclassify_cancelled_opt,
    descriptor::{DescriptorRecord, DescriptorRole},
    errors::OdbcError,
    handles::{ParamRecord, ParamRecords, PutDataState, StatementHandle},
    panic::panic_safe,
    types::{
        ColumnValue, ParamDescriptor, SQL_DATA_AT_EXEC, SQL_DEFAULT_PARAM, SQL_DEFAULT_PARAM_SIZE,
        SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_NTS, SQL_NULL_DATA, SQL_PARAM_ERROR, SQL_PARAM_SUCCESS,
        SqlReturn, SqlState, ULen, c_data_type_from_raw, param_type_from_raw,
    },
    utf16::utf16_to_string,
};

/// Generic implementation of SQLBindParameter.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlbindparameter-function>
///
/// Stores the parameter binding in the statement handle. Parameter values are
/// read from the bound buffers when `SQLExecute` is called.
///
/// Passing a null `parameter_value_ptr` **and** a null `str_len_or_ind_ptr`
/// removes an existing binding for that parameter number.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `parameter_number`: Parameter number, ordered sequentially starting at 1.
/// - `input_output_type`: Type of the parameter (`SQL_PARAM_INPUT`, `SQL_PARAM_INPUT_OUTPUT`,
///   `SQL_PARAM_OUTPUT`, etc.).
/// - `value_type`: C data type of the parameter (`SQL_C_*`).
/// - `parameter_type`: SQL data type of the parameter.
/// - `column_size`: Size of the column or expression of the parameter marker.
/// - `decimal_digits`: Decimal digits of the column or expression.
/// - `parameter_value_ptr`: Pointer to a buffer for the parameter's data. If null and
///   `str_len_or_ind_ptr` is also null, the existing binding for this parameter is removed.
/// - `buffer_length`: Length of the `parameter_value_ptr` buffer in bytes.
/// - `str_len_or_ind_ptr`: Pointer to a buffer for the parameter's length or indicator value.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - `01000` General warning — not returned here; core emits no driver-specific
///   informational message from this function. The row carries no `(DM)` marker.
/// - `07006` Restricted data type attribute violation — returned here when `value_type` is
///   `SQL_C_BINARY` and `parameter_type` is a target core cannot convert it to: the
///   `DECIMAL`/`NUMERIC` and character rows of the "C to SQL: Binary" table, whose byte
///   layout or encoding ODBC leaves unspecified (`crate::binary_convert`). That pairing is
///   fixed at bind and needs no backend metadata, so it is refused before the query runs.
///   Every other incompatibility is still detected at execute time by the data source, the
///   binding being stored without validating it here.
/// - `07009` Invalid descriptor index — the spec annotates this `(DM)`, and its single
///   clause is a `ParameterNumber` less than 1; it is guarded defensively here, because
///   parameter numbering is 1-based throughout `ParamRecords` and a zero would key a
///   record no execution can find.
/// - `HY000` General error — returned for unexpected failures
/// - `HY001` Memory allocation error — not applicable: Rust's allocator aborts on OOM
///   rather than returning an error, and `panic_safe` contains any unwind. The row
///   carries no `(DM)` marker.
/// - `HY003` Invalid application buffer type — returned when `value_type` is not a valid
///   C data type (`c_data_type_from_raw` returns `None`)
/// - `HY004` Invalid SQL data type — a driver-returned code; the row carries no `(DM)`
///   marker, so the driver is responsible for rejecting a `parameter_type` that is neither a
///   valid ODBC SQL type nor a driver-specific type it supports. Here `parameter_type` is accepted as-is
///   and any incompatibility surfaces at execute time (`07006`), because the backend exposes no
///   bind-time SQL-type metadata to validate against. Validation is intentionally deferred.
/// - `HY009` Invalid argument value — (driver-manager-handled; not returned here)
/// - `HY010` Function sequence error — (driver-manager-handled; not returned here)
/// - `HY013` Memory management error — not applicable, for the same reason as `HY001`.
///   The row carries no `(DM)` marker.
/// - `HY021` Inconsistent descriptor information — **returned by this driver**. The row
///   carries no `(DM)` marker, and `SQLSetDescRec`'s "Consistency Checks" section says when
///   the check runs: "This check is always performed when **SQLBindParameter** or
///   **SQLBindCol** is called". Both halves of the binding are checked before either
///   descriptor is written, so a rejected bind leaves neither changed
///   (`crate::descriptor::consistency_check`).
/// - `HY090` Invalid string or buffer length — (driver-manager-handled; not returned here)
/// - `HY104` Invalid precision or scale value — a driver-returned code; the row carries no
///   `(DM)` marker. `column_size` and `decimal_digits` are stored verbatim without range
///   validation.
///   This row is about a precision or scale "outside the range of values supported by the data
///   source", which needs backend metadata not available at bind time, so it is not returned.
///   The values are not merely stored, though: at execute time the value is checked against
///   them and reports `22001` if it does not fit — a `SQL_DECIMAL` or `SQL_NUMERIC` parameter
///   whose conversion would truncate, a character parameter longer than `column_size`
///   characters, or a binary one longer than `column_size` bytes (`crate::param_convert`).
///   That is a different question — whether *this value* fits *this declaration* — and a
///   different SQLSTATE.
/// - `HY105` Invalid parameter type — (DM) the spec marks HY105 as DM-only. When the driver
///   receives an unrecognised `input_output_type`, it returns `HY024` (invalid attribute value)
///   instead, since the DM should have rejected it first.
/// - `HY117` Connection is suspended — (driver-manager-handled; not returned here)
/// - `HYC00` Optional feature not implemented — not returned here, and the row carries no
///   `(DM)` marker. Core refuses a C-type/SQL-type pairing its three conversion tables do
///   not define with `07006` ("restricted data type attribute violation") at bind time
///   instead, which is the state those tables' own rows name. See
///   `numeric_convert::numeric_pairing_is_supported`.
/// - `HYT01` Connection timeout expired — not returned here; core implements no connection
///   timeout, so no deadline exists to expire. The row carries no `(DM)` marker.
/// - `IM001` Driver does not support this function — (driver-manager-handled; not returned
///   here)
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
pub unsafe fn sql_bind_parameter<B: Backend>(
    statement_handle: *mut c_void,
    parameter_number: u16,
    input_output_type: i16,
    value_type: i16,
    parameter_type: i16,
    column_size: ULen,
    decimal_digits: i16,
    parameter_value_ptr: *mut c_void,
    buffer_length: isize,
    str_len_or_ind_ptr: *mut isize,
) -> SqlReturn {
    tracing::trace!(
        "SQLBindParameter(stmt={:?}, param={}, io_type_raw={}, c_type_raw={}, sql_type_raw={})",
        statement_handle,
        parameter_number,
        input_output_type,
        value_type,
        parameter_type,
    );
    let param_type = param_type_from_raw(input_output_type);
    let c_type = c_data_type_from_raw(value_type);
    let sql_type = SqlDataType(parameter_type);
    tracing::debug!(
        "SQLBindParameter(stmt={:?}, param={}, io_type={:?}, c_type={:?}, sql_type={:?})",
        statement_handle,
        parameter_number,
        param_type,
        c_type,
        sql_type
    );
    // SAFETY: statement_handle is a valid *mut StatementHandle<B> as required by the caller
    // (kind and group validated by scope.get inside the closure).
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();
            // Copied out now, so each descriptor below costs one registry lookup
            // rather than resolving this statement again for both.
            let apd_token = stmt.descriptor_token(DescriptorRole::Apd);
            let ipd_token = stmt.descriptor_token(DescriptorRole::Ipd);

            // Spec: parameter number must be >= 1 (07009).
            if parameter_number == 0 {
                return Err(OdbcError::general(
                    "Parameter number must be >= 1",
                    SqlState::invalid_descriptor_index(),
                ));
            }

            let io_type = param_type.ok_or_else(|| {
                OdbcError::general(
                    format!("Unknown input/output type: {input_output_type}"),
                    SqlState::invalid_attribute_value(),
                )
            })?;

            let c_data_type = c_type.ok_or_else(|| {
                OdbcError::general(
                    format!("Unknown C data type: {value_type}"),
                    SqlState::invalid_application_buffer_type(),
                )
            })?;

            // Null value pointer AND null indicator removes the binding — from
            // both descriptors, or the next read finds one half of a parameter.
            if parameter_value_ptr.is_null() && str_len_or_ind_ptr.is_null() {
                scope
                    .descriptor(apd_token)?
                    .records
                    .remove(&parameter_number);
                scope
                    .descriptor(ipd_token)?
                    .records
                    .remove(&parameter_number);
            } else {
                // "C to SQL: Binary" — core converts SQL_C_BINARY only to the
                // targets whose byte layout ODBC defines. Refused here rather
                // than at execute time because the pairing is fixed at bind,
                // needs no backend metadata and never depends on the data: the
                // application fails before running its query, and the
                // SQLPutData path is covered by this one check.
                if c_data_type == odbc_sys::CDataType::Binary
                    && !crate::binary_convert::binary_target_is_supported(sql_type)
                {
                    return Err(crate::binary_convert::unsupported_target(sql_type));
                }
                // "C to SQL: Numeric" — the same reasoning as the binary
                // refusal above and the same timing. This one is asked of the
                // *pairing* rather than of the target alone, because the
                // table's interval footnote is a statement about both: an
                // interval target is legal from an exact numeric C type and
                // not from SQL_C_FLOAT or SQL_C_DOUBLE.
                if crate::numeric_convert::is_numeric_c_type(c_data_type)
                    && !crate::numeric_convert::numeric_pairing_is_supported(c_data_type, sql_type)
                {
                    return Err(crate::numeric_convert::unsupported_target(sql_type));
                }
                // One call, two descriptors: the spec's own `SQLBindParameter`
                // page maps the C-side arguments onto APD fields and the
                // declared-type arguments onto IPD fields. Both are written
                // under the same key, and both are removed together above.
                let mut apd = DescriptorRecord {
                    data_ptr: parameter_value_ptr,
                    octet_length: buffer_length,
                    indicator_ptr: str_len_or_ind_ptr,
                    ..DescriptorRecord::default()
                };
                apd.set_concise_type(c_data_type as i16);

                let mut ipd = DescriptorRecord {
                    length: column_size,
                    scale: decimal_digits,
                    parameter_type: io_type,
                    ..DescriptorRecord::default()
                };
                ipd.set_concise_type(sql_type.0);
                // `ColumnSize` is `SQL_DESC_PRECISION` for the exact numerics
                // and `SQL_DESC_LENGTH` for everything else, per
                // `SQLBindParameter`'s own mapping table. `length` carries it
                // in both cases because `param_convert` and `SQLDescribeParam`
                // read it there; `precision` is what the consistency check and
                // `SQLGetDescField` need, so a DECIMAL sets both.
                if sql_type == SqlDataType::DECIMAL || sql_type == SqlDataType::NUMERIC {
                    ipd.precision = i16::try_from(column_size).unwrap_or(i16::MAX);
                }

                // Spec HY021, `SQLSetDescRec`'s "Consistency Checks": "This
                // check is always performed when SQLBindParameter or
                // SQLBindCol is called". Both halves, before either map is
                // written, so a rejected bind leaves neither descriptor
                // changed.
                crate::descriptor::consistency_check(&apd, DescriptorRole::Apd)?;
                crate::descriptor::consistency_check(&ipd, DescriptorRole::Ipd)?;

                scope
                    .descriptor(apd_token)?
                    .records
                    .insert(parameter_number, apd);
                scope
                    .descriptor(ipd_token)?
                    .records
                    .insert(parameter_number, ipd);
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLBindParameter -> {:?}", ret);
    ret
}

/// Generic implementation of SQLNumParams.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlnumparams-function>
///
/// Returns the number of `?` parameter markers in the prepared SQL statement.
/// Fails with HY010 (function sequence error) if no SQL has been prepared.
///
/// A `?` counts only where the grammar admits a parameter marker: one inside a
/// string literal, a delimited identifier or a comment is part of that token,
/// not a marker.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle.
/// - `parameter_count_ptr`: Output pointer to a buffer in which to return the number of
///   parameters in the statement. May be null, in which case the count is computed but not
///   written.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - `01000` General warning — not returned here; core emits no driver-specific
///   informational message from this function. The row carries no `(DM)` marker.
/// - `08S01` Communication link failure — not applicable; parameter count is evaluated
///   locally without a round-trip to the data source
/// - `HY000` General error — returned for unexpected failures
/// - `HY001` Memory allocation error — not applicable: Rust's allocator aborts on OOM
///   rather than returning an error, and `panic_safe` contains any unwind. The row
///   carries no `(DM)` marker.
/// - HY008: Operation canceled; not returned here. This call makes no fallible backend call —
///   `SQLNumParams` reads the parameter count cached at prepare time — so there is no error for a
///   cancellation to be reported through. The asynchronous clause is inapplicable: core never
///   returns `SQL_STILL_EXECUTING`.
/// - `HY010` Function sequence error — every clause of this row is `(DM)`, including
///   "called prior to calling **SQLPrepare** or **SQLExecDirect**". That check is
///   unavoidable and stays: without a prepared statement there is no parameter count to
///   report, so the alternative is answering a question about nothing. It fires when
///   `stmt.param_count` is `None`.
/// - `HY013` Memory management error — not applicable: Rust's allocator aborts on OOM
///   rather than returning an error. The row carries no `(DM)` marker.
/// - `HY117` Connection is suspended — (driver-manager-handled; not returned here)
/// - `HYT01` Connection timeout expired — not returned here; core implements no connection
///   timeout, so no deadline exists to expire. The row carries no `(DM)` marker.
/// - `IM001` Driver does not support this function — (driver-manager-handled; not returned
///   here)
/// - `IM017` Polling disabled; not returned here (the asynchronous notification model is
///   not supported — not DM-annotated in the spec).
/// - `IM018` SQLCompleteAsync not called; not returned here (the asynchronous notification
///   model is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `parameter_count_ptr` must be writable if non-null.
pub unsafe fn sql_num_params<B: Backend>(
    statement_handle: *mut c_void,
    parameter_count_ptr: *mut i16,
) -> SqlReturn {
    tracing::debug!("SQLNumParams(stmt={:?})", statement_handle);
    // SAFETY: statement_handle is a valid *mut StatementHandle<B> as required by the caller
    // (kind and group validated by scope.get inside the closure).
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            let count = stmt.param_count.ok_or_else(|| {
                OdbcError::general(
                    "No SQL has been prepared (call SQLPrepare first)",
                    SqlState::function_sequence_error(),
                )
            })?;

            if !parameter_count_ptr.is_null() {
                let count_i16 = i16::try_from(count).unwrap_or_else(|_| {
                    tracing::warn!(
                        "SQLNumParams: parameter count {} exceeds i16::MAX; clamping to i16::MAX",
                        count
                    );
                    i16::MAX
                });
                std::ptr::write_unaligned(parameter_count_ptr, count_i16);
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLNumParams -> {:?}", ret);
    ret
}

/// Generic implementation of SQLDescribeParam.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldescribeparam-function>
///
/// Returns generic parameter type information. Backends that cannot introspect parameter
/// types receive this generic fallback: all parameters are reported as `SQL_VARCHAR` with
/// column size 4000, decimal digits 0, and nullable. This is the standard fallback used by
/// most ODBC drivers.
///
/// # Parameters
///
/// - `statement_handle`: \[Input\] Statement handle.
/// - `parameter_number`: \[Input\] Parameter marker number, ordered sequentially starting at 1.
/// - `data_type_ptr`: \[Output\] Pointer to a buffer for the SQL data type. May be null.
/// - `parameter_size_ptr`: \[Output\] Pointer to a buffer for the column/expression size. May be null.
/// - `decimal_digits_ptr`: \[Output\] Pointer to a buffer for the decimal digits. May be null.
/// - `nullable_ptr`: \[Output\] Pointer to a buffer for nullability. May be null.
///
/// # Spec compliance
///
/// Diagnostics from the ODBC spec Diagnostics table:
///
/// - 01000: General warning — not returned here; core emits no driver-specific
///   informational message from this function. The row carries no `(DM)` marker.
/// - 07009: Invalid descriptor index — only the "less than 1" clause is `(DM)`-marked,
///   and it is guarded defensively here. The three that follow carry no marker and are
///   the driver's; core returns the first of them, a `ParameterNumber` greater than the
///   number of parameters in the associated SQL statement, from the `?` markers
///   `count_params` found. The other two — a parameter marker in a non-DML statement or
///   in a **SELECT** list — need the data source's own parse and are left to the
///   backend.
/// - 21S01: Insert value list does not match column list — not returned here. The row is
///   about an `INSERT` whose parameter count differs from the target table's column count,
///   which needs the data source's catalog: core parses the statement only far enough to
///   count `?` markers (`count_params`) and never resolves a table. A backend that describes
///   parameters itself is where this would originate.
/// - 08S01: Communication link failure — propagated from the backend unchanged.
///   `Backend::describe_param` is a real, fallible call to the data source, so a failing
///   link surfaces here.
/// - HY000: General error — returned for unexpected failures.
/// - HY001: Memory allocation error — not applicable; Rust allocation panics are caught by `panic_safe`.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY010: Function sequence error — every clause of this row is `(DM)`, including
///   "called before calling **SQLPrepare** or **SQLExecDirect**". The check is
///   unavoidable and stays: with no prepared statement there is no parameter to describe.
/// - HY013: Memory management error — not applicable.
/// - HY117: Connection suspended — (DM) (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired — propagated from the backend unchanged, for the
///   same reason as `08S01`. The row carries no `(DM)` marker.
/// - IM001: Driver does not support this function — (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is
///   not supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification
///   model is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// Output pointers must be writable if non-null.
pub unsafe fn sql_describe_param<B: Backend>(
    statement_handle: *mut c_void,
    parameter_number: u16,
    data_type_ptr: *mut i16,
    parameter_size_ptr: *mut ULen,
    decimal_digits_ptr: *mut i16,
    nullable_ptr: *mut i16,
) -> SqlReturn {
    tracing::debug!(
        "SQLDescribeParam(stmt={:?}, param={})",
        statement_handle,
        parameter_number
    );
    // SAFETY: statement_handle is a valid *mut StatementHandle<B> as required by the caller
    // (kind and group validated by scope.get inside the closure).
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec: HY010 — no SQL has been prepared.
            let param_count = stmt.param_count.ok_or_else(|| {
                OdbcError::general(
                    "No SQL has been prepared (call SQLPrepare first)",
                    SqlState::function_sequence_error(),
                )
            })?;

            // Spec: 07009 — parameter_number is 0 or exceeds param_count.
            if parameter_number == 0 || parameter_number > param_count {
                return Err(OdbcError::general(
                    format!(
                        "Parameter number {parameter_number} is out of range (statement has {param_count} parameter(s))"
                    ),
                    SqlState::invalid_descriptor_index(),
                ));
            }

            // Ask the backend first. `None` — and a backend that never
            // overrides the hook — falls back to a generic VARCHAR, which is
            // usable but wrong for any parameter that is not a string.
            // Like `SQLFetch`, this runs against a statement an earlier
            // `SQLPrepare` set up, so it observes that call's token rather than
            // minting one. `B::describe_param` takes no token itself — it is a
            // metadata lookup, not a query — but a backend that answers it over
            // the wire can still be cancelled mid-lookup.
            let cancel_token = crate::handles::current_cancel_token(statement_handle);
            let cancel = cancel_token
                .as_ref()
                .map(crate::handles::cancel_as::<B>)
                .transpose()?;

            let described = match (conn.connection.as_ref(), stmt.prepared_sql.as_deref()) {
                (Some(connection), Some(sql)) => reclassify_cancelled_opt::<B, _, _>(
                    B::describe_param(connection, sql, parameter_number),
                    cancel,
                )?,
                // No connection or no stored text: nothing to ask with. Not an
                // error — `param_count` above already established the statement
                // is prepared, and the fallback still answers the call.
                _ => None,
            };

            let descriptor = described.unwrap_or_else(|| {
                tracing::warn!(
                    "SQLDescribeParam(param={}): backend did not describe this parameter; \
                     reporting VARCHAR({}) (see Backend::describe_param)",
                    parameter_number,
                    SQL_DEFAULT_PARAM_SIZE
                );
                ParamDescriptor::new(SqlDataType::VARCHAR)
                    .with_parameter_size(u64::from(SQL_DEFAULT_PARAM_SIZE))
            });

            if !data_type_ptr.is_null() {
                std::ptr::write_unaligned(data_type_ptr, descriptor.data_type().0);
            }
            if !parameter_size_ptr.is_null() {
                std::ptr::write_unaligned(
                    parameter_size_ptr,
                    ULen::try_from(descriptor.parameter_size()).unwrap_or(ULen::MAX),
                );
            }
            if !decimal_digits_ptr.is_null() {
                std::ptr::write_unaligned(decimal_digits_ptr, descriptor.decimal_digits());
            }
            if !nullable_ptr.is_null() {
                std::ptr::write_unaligned(nullable_ptr, descriptor.nullable() as i16);
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLDescribeParam -> {:?}", ret);
    ret
}

/// Count `?` parameter markers in an SQL string.
///
/// A `?` counts only where the SQL grammar admits a parameter marker, so the
/// scan skips the four regions where one cannot appear: single-quoted string
/// literals (`''` doubling included), delimited identifiers, `--` line comments
/// and `/* … */` block comments. The identifier delimiters come from the
/// backend's [`EscapeDialect`](crate::escape::EscapeDialect) rather than being
/// assumed to be `"`, because that is where a backend already states them.
///
/// Miscounting is not a cosmetic problem. The count is what `SQLNumParams`
/// reports, what bounds [`collect_params`]'s `1..=param_count` walk, and what
/// the value list handed to `Backend::execute` is sized by — so a `?` inside
/// `"a?b"` counted as a marker makes the driver ask for a value that the
/// statement has no place for.
///
/// The region helpers are `escape`'s own, shared with
/// [`crate::escape::translate_escapes`] so the two scans cannot drift apart.
pub(crate) fn count_params(sql: &str, dialect: &crate::escape::EscapeDialect) -> u16 {
    use crate::escape::{skip_block_comment, skip_line_comment, skip_quoted_ident, skip_string};

    let chars: Vec<char> = sql.chars().collect();
    let mut count = 0u16;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            skip_string(&chars, &mut i);
        } else if let Some(close) = dialect.ident_close(c) {
            skip_quoted_ident(&chars, &mut i, c, close);
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            skip_line_comment(&chars, &mut i);
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            skip_block_comment(&chars, &mut i);
        } else {
            if c == '?' {
                // `SQLNumParams` reports a `u16`, so a statement with more than
                // 65 535 markers has no representable answer. Saturating keeps
                // the count at the maximum rather than wrapping to a small
                // number that would silently drop every parameter past it.
                count = count.saturating_add(1);
            }
            i += 1;
        }
    }
    count
}

/// Read the current value from a parameter binding's buffers.
///
/// Returns `ColumnValue::Null` when the indicator signals `SQL_NULL_DATA`,
/// when the value pointer is null, or for unsupported C data types.
///
/// Bound an application-supplied length indicator by the buffer the application
/// actually bound.
///
/// `str_len_or_ind_ptr` is written by the application and is not trustworthy on
/// its own: an indicator larger than the buffer would build a slice over memory
/// past the end of it, which the backend then sends to the data source.
/// `buffer_length` is the driver's own record of the buffer's size, taken at
/// `SQLBindParameter` time, so it is the bound to apply.
///
/// A non-positive `buffer_length` carries no bound and is left alone. Zero is
/// how an application says "not applicable" for a fixed C type, and character
/// buffers are bound that way too when the indicator is meant to carry the
/// length; negative values are rejected by the Driver Manager before reaching
/// the driver.
fn clamp_to_bound_buffer(byte_len: usize, buffer_length: isize) -> usize {
    if buffer_length <= 0 {
        return byte_len;
    }
    let bound = buffer_length as usize;
    if byte_len > bound {
        tracing::warn!(
            "read_param_value: length indicator {byte_len} exceeds the bound buffer of {bound} \
             bytes; clamping to the buffer"
        );
        return bound;
    }
    byte_len
}

/// Read one of the *C to SQL: Numeric* table's source types out of the
/// application's buffer.
///
/// The spec fixes the width: "The driver ignores the length/indicator value
/// when converting data from the numeric C data types and assumes that the size
/// of the data buffer is the size of the numeric C data type." So each arm
/// reads exactly its own type and no indicator is consulted.
///
/// **The unsigned types widen rather than reinterpret.** `SQL_C_UBIGINT` used to
/// be read as a `u64` and cast to `i64`, which turned any value above
/// `i64::MAX` into a negative number on its way to the data source; the same
/// shape made `SQL_C_USHORT` and `SQL_C_UTINYINT` wrap. Going through `i128`
/// there is no cast that can wrap, and the table's own range checks decide what
/// the declared target can hold.
///
/// # Safety
///
/// `ptr` must be non-null and point to a valid value of `c_type`.
unsafe fn read_numeric_param(
    c_type: odbc_sys::CDataType,
    ptr: crate::types::Pointer,
) -> Result<crate::numeric_convert::NumericParam, OdbcError> {
    use crate::numeric_convert::NumericParam;
    use odbc_sys::CDataType;

    // SAFETY for every read below: the caller guarantees `ptr` points to a
    // valid value of `c_type`. `read_unaligned` tolerates the arbitrary offsets
    // row-wise binding can place an application buffer at, where a plain
    // dereference of a multi-byte type would be UB.
    let exact = |v: i128| Ok(NumericParam::exact_integer(v));
    match c_type {
        CDataType::STinyInt => exact(i128::from(unsafe {
            std::ptr::read_unaligned(ptr as *const i8)
        })),
        CDataType::UTinyInt => exact(i128::from(unsafe {
            std::ptr::read_unaligned(ptr as *const u8)
        })),
        CDataType::SShort => exact(i128::from(unsafe {
            std::ptr::read_unaligned(ptr as *const i16)
        })),
        CDataType::UShort => exact(i128::from(unsafe {
            std::ptr::read_unaligned(ptr as *const u16)
        })),
        CDataType::SLong => exact(i128::from(unsafe {
            std::ptr::read_unaligned(ptr as *const i32)
        })),
        CDataType::ULong => exact(i128::from(unsafe {
            std::ptr::read_unaligned(ptr as *const u32)
        })),
        CDataType::SBigInt => exact(i128::from(unsafe {
            std::ptr::read_unaligned(ptr as *const i64)
        })),
        CDataType::UBigInt => exact(i128::from(unsafe {
            std::ptr::read_unaligned(ptr as *const u64)
        })),
        CDataType::Float => Ok(NumericParam::approx(
            f64::from(unsafe { std::ptr::read_unaligned(ptr as *const f32) }),
            true,
        )),
        CDataType::Double => Ok(NumericParam::approx(
            unsafe { std::ptr::read_unaligned(ptr as *const f64) },
            false,
        )),
        CDataType::Numeric => {
            let n = unsafe { std::ptr::read_unaligned(ptr as *const odbc_sys::Numeric) };
            let text = numeric_struct_to_decimal_string(&n);
            NumericParam::exact_text(&text).ok_or_else(|| {
                // Unreachable: `numeric_struct_to_decimal_string` and
                // `parse_numeric_literal` are both core's, and the first always
                // renders a *numeric-literal*. Reported rather than unwrapped
                // because this runs inside an FFI call.
                OdbcError::general(
                    format!("SQL_C_NUMERIC parameter rendered as {text:?}, which is not a number"),
                    SqlState::general_error(),
                )
            })
        }
        // `is_numeric_c_type` gates every caller, and it lists exactly the arms
        // above.
        other => Err(OdbcError::general(
            format!("{other:?} is not a numeric C data type"),
            SqlState::general_error(),
        )),
    }
}

/// A parameter value, and the optional warning reading it raised.
///
/// The warning exists because the *C to SQL: Numeric* table's fractional
/// truncation is a `SQL_SUCCESS_WITH_INFO` outcome rather than a failure: the
/// value is still sent, so it cannot be an `Err`, and the application is still
/// told, so it cannot be dropped. Every other conversion path produces
/// `warning: None`, which the [`From`] impl below spells once instead of at
/// forty call sites.
pub(crate) struct ParamValue {
    /// The value to hand the backend.
    pub value: ColumnValue,
    /// A diagnostic to post alongside it, without failing the call.
    pub warning: Option<OdbcError>,
}

impl From<ColumnValue> for ParamValue {
    fn from(value: ColumnValue) -> Self {
        Self {
            value,
            warning: None,
        }
    }
}

/// # Safety
///
/// `binding.value_ptr` and `binding.str_len_or_ind_ptr` must point to valid
/// memory of the appropriate type and size.
pub(crate) unsafe fn read_param_value(rec: ParamRecord<'_>) -> Result<ParamValue, OdbcError> {
    use odbc_sys::CDataType;

    // The APD says where the value is and how it is laid out; the IPD says what
    // it is declared to be. Destructured once so the reads below name which
    // descriptor each field came from.
    let ParamRecord { apd, ipd } = rec;

    // Check indicator for NULL.
    if !apd.indicator_ptr.is_null() {
        // SAFETY: str_len_or_ind_ptr is non-null and the caller guarantees it points to a valid isize.
        let indicator = unsafe { std::ptr::read_unaligned(apd.indicator_ptr) };
        if indicator == SQL_NULL_DATA {
            return Ok(ColumnValue::Null.into());
        }
    }

    if apd.data_ptr.is_null() {
        return Ok(ColumnValue::Null.into());
    }

    // The whole "C to SQL: Numeric" table: every one of its fourteen source
    // types, converted to whatever `SQLBindParameter`'s ParameterType declared.
    // Placed before the arms below so it captures `SQL_C_NUMERIC` too, and
    // guarded rather than listed so the table's own source list stays in one
    // place. `SQL_C_BIT` is deliberately not captured: it has its own table.
    //
    // `SQLBindParameter` has already refused any pairing this cannot convert,
    // so a target reaching here is one of the table's six rows.
    let c_type = apd.c_type()?;
    if crate::numeric_convert::is_numeric_c_type(c_type) {
        // SAFETY: data_ptr is non-null (guarded above) and the caller
        // guarantees it points to a valid value of that C type.
        let param = unsafe { read_numeric_param(c_type, apd.data_ptr) }?;
        let converted = crate::numeric_convert::numeric_to_sql_type(
            param,
            ipd.sql_type(),
            ipd.length,
            ipd.scale,
            ipd.datetime_interval_precision,
        )?;
        return Ok(ParamValue {
            value: converted.value,
            warning: converted.warning,
        });
    }

    Ok(ParamValue::from(match c_type {
        CDataType::Bit => {
            ColumnValue::Bool(unsafe { std::ptr::read_unaligned(apd.data_ptr as *const u8) } != 0)
        }
        CDataType::Char => {
            let ptr = apd.data_ptr as *const u8;
            let byte_len = if apd.indicator_ptr.is_null() {
                None
            } else {
                // SAFETY: str_len_or_ind_ptr is non-null and the caller guarantees it points
                // to a valid isize provided by the ODBC caller.
                let l = unsafe { std::ptr::read_unaligned(apd.indicator_ptr) };
                if l == SQL_NTS as isize || l < 0 {
                    None
                } else {
                    Some(clamp_to_bound_buffer(l as usize, apd.octet_length))
                }
            };
            let bytes = if let Some(n) = byte_len {
                // SAFETY: value_ptr is non-null and the caller guarantees it points to at
                // least `n` valid bytes as indicated by str_len_or_ind_ptr.
                unsafe { std::slice::from_raw_parts(ptr, n) }
            } else {
                // Indicator is SQL_NTS or absent: the string is null-terminated.
                // SAFETY: caller guarantees ptr is a valid, null-terminated C string (ODBC
                // SQL_C_CHAR buffers are always null-terminated when SQL_NTS is used).
                unsafe { std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char) }.to_bytes()
            };
            return crate::param_convert::text_to_sql_type(
                &String::from_utf8_lossy(bytes),
                ipd.sql_type(),
                ipd.length,
                ipd.scale,
            )
            .map(ParamValue::from);
        }
        CDataType::WChar => {
            let ptr = apd.data_ptr as *const u16;
            let code_units = if apd.indicator_ptr.is_null() {
                // Indicator pointer absent: treat as null-terminated (SQL_NTS).
                // Use utf16_to_string which bounds the scan to MAX_NTS_SCAN code units.
                // SAFETY: caller guarantees ptr is a valid, null-terminated UTF-16 string.
                // value_ptr null case is excluded by the guard above; unwrap_or_default is unreachable.
                debug_assert!(!ptr.is_null(), "value_ptr null case excluded above");
                return crate::param_convert::text_to_sql_type(
                    &unsafe { utf16_to_string(ptr, SQL_NTS) }.unwrap_or_default(),
                    ipd.sql_type(),
                    ipd.length,
                    ipd.scale,
                )
                .map(ParamValue::from);
            } else {
                // SAFETY: str_len_or_ind_ptr is non-null and the caller guarantees it points
                // to a valid isize provided by the ODBC caller.
                let l = unsafe { std::ptr::read_unaligned(apd.indicator_ptr) };
                if l == SQL_NTS as isize || l < 0 {
                    // Null-terminated: delegate to bounded NTS scan helper.
                    // SAFETY: caller guarantees ptr is a valid, null-terminated UTF-16 string.
                    // value_ptr null case is excluded by the guard above; unwrap_or_default is unreachable.
                    debug_assert!(!ptr.is_null(), "value_ptr null case excluded above");
                    return crate::param_convert::text_to_sql_type(
                        &unsafe { utf16_to_string(ptr, SQL_NTS) }.unwrap_or_default(),
                        ipd.sql_type(),
                        ipd.length,
                        ipd.scale,
                    )
                    .map(ParamValue::from);
                } else {
                    // Explicit byte length: ODBC reports lengths in bytes for WChar.
                    // Clamp before halving, because buffer_length is in bytes too.
                    // Divide by 2 because UTF-16 encodes each code unit as exactly 2 bytes.
                    clamp_to_bound_buffer(l as usize, apd.octet_length) / 2
                }
            };
            // SAFETY: value_ptr is non-null and the caller guarantees it points to at least
            // `code_units` valid u16 elements as indicated by str_len_or_ind_ptr.
            // Read element-wise: `from_raw_parts` would require `ptr` to be
            // u16-aligned, and a bound parameter buffer inside a packed
            // row-wise structure is not.
            let units: Vec<u16> = (0..code_units)
                .map(|i| unsafe { std::ptr::read_unaligned(ptr.add(i)) })
                .collect();
            return crate::param_convert::text_to_sql_type(
                &String::from_utf16_lossy(&units),
                ipd.sql_type(),
                ipd.length,
                ipd.scale,
            )
            .map(ParamValue::from);
        }
        CDataType::Binary => {
            let ptr = apd.data_ptr as *const u8;
            let byte_len = if apd.indicator_ptr.is_null() {
                None
            } else {
                // SAFETY: str_len_or_ind_ptr is non-null and the caller guarantees it points
                // to a valid isize provided by the ODBC caller.
                let l = unsafe { std::ptr::read_unaligned(apd.indicator_ptr) };
                if l < 0 {
                    None
                } else {
                    Some(clamp_to_bound_buffer(l as usize, apd.octet_length))
                }
            };
            match byte_len {
                Some(n) => {
                    // SAFETY: value_ptr is non-null (guarded above) and the caller guarantees
                    // it points to at least `n` valid bytes as indicated by str_len_or_ind_ptr.
                    let bytes = unsafe { std::slice::from_raw_parts(ptr, n) };
                    // The whole "C to SQL: Binary" table, including its binary
                    // row's declared-size check. `SQLBindParameter` has already
                    // refused any target this cannot convert.
                    return crate::binary_convert::binary_to_sql_type(
                        bytes,
                        ipd.sql_type(),
                        ipd.length,
                    )
                    .map(ParamValue::from);
                }
                None => {
                    tracing::warn!(
                        "read_param_value: SQL_C_BINARY parameter has no length indicator; \
                         binary length cannot be determined, treating as NULL"
                    );
                    ColumnValue::Null
                }
            }
        }
        // Temporal, numeric and GUID structs. SAFETY for all reads below:
        // value_ptr is non-null (guarded above) and the caller guarantees it
        // points to a valid struct of the matching C type. `read_unaligned`
        // tolerates the arbitrary offsets row-wise binding can place an
        // application buffer at, where a plain dereference would be UB.
        CDataType::TypeTimestamp | CDataType::TimeStamp => {
            let ts =
                unsafe { std::ptr::read_unaligned(apd.data_ptr as *const odbc_sys::Timestamp) };
            ColumnValue::Timestamp {
                year: ts.year,
                month: ts.month,
                day: ts.day,
                hour: ts.hour,
                minute: ts.minute,
                second: ts.second,
                fraction: ts.fraction,
            }
        }
        CDataType::TypeDate | CDataType::Date => {
            let d = unsafe { std::ptr::read_unaligned(apd.data_ptr as *const odbc_sys::Date) };
            ColumnValue::Date {
                year: d.year,
                month: d.month,
                day: d.day,
            }
        }
        CDataType::TypeTime | CDataType::Time => {
            // SQL_TIME_STRUCT carries no fractional seconds; report 0.
            let t = unsafe { std::ptr::read_unaligned(apd.data_ptr as *const odbc_sys::Time) };
            ColumnValue::Time {
                hour: t.hour,
                minute: t.minute,
                second: t.second,
                fraction: 0,
            }
        }
        // `SQL_C_NUMERIC` is handled by the numeric table above, which is why
        // it is absent here.
        CDataType::Guid => {
            let g = unsafe { std::ptr::read_unaligned(apd.data_ptr as *const odbc_sys::Guid) };
            ColumnValue::Guid(guid_struct_to_bytes(&g))
        }
        // Interval and SQL Server extended C types are not marshalled. Emitting
        // NULL loses data silently, so warn rather than accept in silence.
        _ => {
            tracing::warn!(
                c_type = ?apd.c_type()?,
                "read_param_value: unsupported C data type for input parameter; treating as NULL"
            );
            ColumnValue::Null
        }
    }))
}

/// Render a `SQL_NUMERIC_STRUCT` as its exact decimal string.
///
/// The struct holds an unsigned 128-bit little-endian mantissa (`val`), a
/// `sign` (1 = positive, 0 = negative) and a `scale` (digits to the right of
/// the decimal point); the value is `mantissa / 10^scale`, signed.
///
/// The ODBC spec says a driver should take precision and scale for an input
/// `SQL_C_NUMERIC` from the application parameter descriptor, not the struct.
/// This driver exposes no descriptor-field setter for them, so the struct's
/// own `scale` is the only scale available and is used directly.
fn numeric_struct_to_decimal_string(n: &odbc_sys::Numeric) -> String {
    let mantissa = u128::from_le_bytes(n.val);
    let sign = if n.sign == 0 && mantissa != 0 {
        "-"
    } else {
        ""
    };
    if n.scale <= 0 {
        // scale == 0 emits the integer; a negative scale (rare but valid)
        // appends that many trailing zeros.
        let zeros = "0".repeat(usize::try_from(-i32::from(n.scale)).unwrap_or(0));
        return format!("{sign}{mantissa}{zeros}");
    }
    let scale = n.scale as usize;
    let digits = mantissa.to_string();
    if digits.len() > scale {
        let point = digits.len() - scale;
        format!("{sign}{}.{}", &digits[..point], &digits[point..])
    } else {
        // Fewer digits than the scale: zero-pad the fraction to `scale` digits,
        // e.g. mantissa 5, scale 2 -> "0.05".
        format!("{sign}0.{digits:0>scale$}")
    }
}

/// Convert a `SQLGUID` to the canonical string-order 16 bytes that
/// [`ColumnValue::Guid`] carries: `d1`/`d2`/`d3` big-endian, `d4` verbatim, so
/// printing the bytes in order yields `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`.
fn guid_struct_to_bytes(g: &odbc_sys::Guid) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&g.d1.to_be_bytes());
    bytes[4..6].copy_from_slice(&g.d2.to_be_bytes());
    bytes[6..8].copy_from_slice(&g.d3.to_be_bytes());
    bytes[8..16].copy_from_slice(&g.d4);
    bytes
}

/// The 07002 an unbound parameter marker earns.
///
/// Spec, the first clause of the `07002` row shared by the `SQLExecute` and
/// `SQLExecDirect` diagnostics tables, neither of them `(DM)`-marked: "The
/// number of parameters specified in **SQLBindParameter** was less than the
/// number of parameters in the SQL statement".
pub(crate) fn unbound_parameter(number: u16) -> OdbcError {
    OdbcError::general(
        format!("No value was bound for parameter {number}"),
        SqlState::count_field_incorrect(),
    )
}

/// Collect bound parameter values in order 1..=param_count.
///
/// A marker with no binding is 07002 (see [`unbound_parameter`]). Padding the
/// gap with NULL, as an earlier revision did, runs a statement the application
/// never wrote: `WHERE x = ?` with nothing bound becomes `WHERE x = NULL`,
/// which matches no row and reports success, so the application sees an empty
/// result set rather than its own mistake.
///
/// A `SQL_PARAM_OUTPUT` binding *is* emitted as `ColumnValue::Null`, and that
/// is not the same case: an output-only parameter has no input value, and its
/// buffer is where the *driver* is expected to put something. Reading it is not
/// merely meaningless, it is unsound — the application never had to initialise
/// it, so for `SQL_C_CHAR` with an absent or `SQL_NTS` indicator
/// [`read_param_value`] would scan uninitialised memory for a terminator that
/// need not be inside the buffer at all.
///
/// This is the mirror image of [`write_output_params`], which refuses to write
/// through an input-only binding for the same reason in the other direction.
///
/// # Safety
///
/// All APD value and indicator pointers must point to valid memory.
/// Returns the values alongside any warnings the conversions raised. A warning
/// is not a failure — the value it accompanies is in the `Vec` and is sent —
/// so the caller posts each to the statement's diagnostic queue and returns
/// `SQL_SUCCESS_WITH_INFO`. See [`ParamValue`].
pub(crate) unsafe fn collect_params(
    records: ParamRecords<'_>,
    param_count: u16,
) -> Result<(Vec<ColumnValue>, Vec<OdbcError>), OdbcError> {
    use odbc_sys::ParamType;

    let mut params = Vec::with_capacity(param_count as usize);
    let mut warnings = Vec::new();
    for i in 1..=param_count {
        match records.get(i)? {
            // `InputOutput` is read: it carries an input value the application
            // did initialise, and `write_output_params` writes the result back
            // through the same binding afterwards.
            Some(rec) if rec.ipd.parameter_type != ParamType::Output => {
                // SAFETY: the caller guarantees all APD value and indicator
                // pointers point to valid memory of the appropriate type.
                let read = unsafe { read_param_value(rec) }?;
                params.push(read.value);
                warnings.extend(read.warning);
            }
            Some(_) => params.push(ColumnValue::Null),
            None => return Err(unbound_parameter(i)),
        }
    }
    Ok((params, warnings))
}

/// Write the values of OUTPUT / INOUT parameters produced by `Backend::execute`
/// back into the application's bound parameter buffers.
///
/// Called by `SQLExecute` / `SQLExecDirectW` immediately after `Backend::execute`
/// returns. For each [`crate::types::OutputParam`], the matching binding is
/// located by 1-based parameter number and the value is marshalled with
/// [`write_column_value`], the *same* routine `SQLGetData` uses, so NULL,
/// truncation and type coercion behave identically. The value is written **only**
/// when the application actually bound that parameter as `SQL_PARAM_OUTPUT` or
/// `SQL_PARAM_INPUT_OUTPUT`; a binding that is input-only, or absent entirely, is
/// skipped, so a backend cannot clobber a buffer the application did not offer
/// for output. This is the symmetric counterpart of [`collect_params`], which
/// reads input values *out* of the same bindings and applies the mirror-image
/// filter: it skips `SQL_PARAM_OUTPUT` bindings, whose buffers the application
/// never had to initialise.
///
/// TODO(spec): output values are written as soon as `execute` returns. The ODBC
/// spec (and the SQL Server driver) only guarantee that output parameters are
/// valid after the result set is fully consumed (`SQLMoreResults` ->
/// `SQL_NO_DATA`). Modelling that requires multiple-result-set support, which
/// this framework does not yet have; revisit when a backend needs both a result
/// set and output parameters on the same statement. For the stored-procedure-
/// without-a-result-set case that a forward-only backend produces today,
/// writing at `execute` return is correct.
///
/// # Safety
/// Every output binding's `value_ptr` / `str_len_or_ind_ptr` must point to a
/// valid writable buffer, as guaranteed by the `SQLBindParameter` contract.
/// Write one processed parameter set through `SQL_ATTR_PARAMS_PROCESSED_PTR`
/// and `status` into the first element of `SQL_ATTR_PARAM_STATUS_PTR`, when the
/// application set either.
///
/// The parameter-side counterpart of `ffi::fetch`'s `report_rows_fetched`, and
/// bounded the same way: `SQL_ATTR_PARAMSET_SIZE` is pinned at 1
/// (`ffi/stmt_attr.rs` substitutes anything else back with `01S02`), so an
/// execution processes exactly one parameter set and the application's status
/// array is required to be at least that long. The count is written for every
/// execution, including one with no bound parameters — the set count is a
/// property of `SQL_ATTR_PARAMSET_SIZE`, not of how many parameters are in the
/// set.
///
/// # Safety
///
/// Each stored attribute must be null or a pointer to a valid, writable
/// `usize` / `u16` respectively — the application's undertaking when it set
/// them.
pub(crate) unsafe fn report_params_processed<B: Backend>(stmt: &StatementHandle<B>, status: u16) {
    let processed = stmt
        .attrs
        .get(&(odbc_sys::StatementAttribute::ParamsProcessedPtr as i32))
        .copied()
        .unwrap_or(0);
    if processed != 0 {
        // SAFETY: non-zero means the application supplied a writable SQLULEN.
        // Unaligned because ODBC applications place these in packed structures.
        unsafe { std::ptr::write_unaligned(processed as *mut usize, 1) };
    }

    let statuses = stmt
        .attrs
        .get(&(odbc_sys::StatementAttribute::ParamStatusPtr as i32))
        .copied()
        .unwrap_or(0);
    if statuses != 0 {
        // SAFETY: non-zero means the application supplied a parameter-status
        // array of at least SQL_ATTR_PARAMSET_SIZE (= 1) elements. Unaligned
        // for the same reason as above.
        unsafe { std::ptr::write_unaligned(statuses as *mut u16, status) };
    }
}

pub(crate) unsafe fn write_output_params(
    records: ParamRecords<'_>,
    output_params: &[crate::types::OutputParam],
) -> Result<(), OdbcError> {
    use odbc_sys::ParamType;

    for out in output_params {
        let Some(rec) = records.get(out.parameter_number)? else {
            // The application never bound this parameter; nothing to write into.
            continue;
        };
        if !matches!(
            rec.ipd.parameter_type,
            ParamType::Output | ParamType::InputOutput
        ) {
            // Input-only binding: never write back through it.
            continue;
        }
        if !rec.apd.is_bound() {
            // No buffer to write into. `ParamRecords::get` counts a record
            // with an indicator but no data pointer as a binding, because
            // `SQLBindParameter` allows a null `ParameterValuePtr` alongside
            // `SQL_NULL_DATA` — but that allowance is scoped: "(This applies
            // only to input or input/output parameters.)" Writing needs a real
            // buffer, and `write_column_value` declines the null target while
            // still writing the length indicator, so a record admitted here
            // would report a length for a value it never stored.
            continue;
        }
        // SAFETY: the caller guarantees this output binding's value and
        // indicator pointers are valid writable buffers of the bound size.
        //
        // The returned `SqlReturn` (possibly `SUCCESS_WITH_INFO` for a truncated
        // output value) is intentionally dropped: this helper has no diagnostic
        // queue to raise 01004 on, and no in-tree backend produces output
        // parameters yet. See the TODO(spec) note above.
        let _ = unsafe {
            crate::column_value::write_column_value(
                &out.value,
                rec.apd.c_type()?,
                rec.apd.data_ptr,
                rec.apd.octet_length,
                rec.apd.indicator_ptr,
            )
        }?;
    }
    Ok(())
}

/// Returns `true` if the given indicator value signals data-at-execution.
///
/// An indicator of `SQL_DATA_AT_EXEC` (-2) or the result of
/// `SQL_LEN_DATA_AT_EXEC(len)` (any value <= `SQL_LEN_DATA_AT_EXEC_OFFSET`)
/// means the parameter's data will be supplied at execution time via
/// `SQLPutData`.
pub(crate) fn is_data_at_exec(indicator: isize) -> bool {
    indicator == SQL_DATA_AT_EXEC || indicator <= SQL_LEN_DATA_AT_EXEC_OFFSET
}

/// Convert an accumulated data-at-execution buffer into a `ColumnValue` using the
/// parameter's bound C type. Binary data must not pass through a UTF-8 conversion
/// (which corrupts non-UTF-8 bytes); `SQL_C_WCHAR` data is UTF-16; everything else
/// is treated as text.
///
/// An empty buffer is a **zero-length value**, not NULL. The caller decides that
/// from `PutDataState`, because `SQLPutData(ptr, 0)` and
/// `SQLPutData(_, SQL_NULL_DATA)` send two different parameters and inferring
/// NULL from emptiness made the first of them unexpressible.
///
/// Text is then converted to `sql_type` by [`crate::param_convert::text_to_sql_type`],
/// the same way [`read_param_value`] converts a value delivered in one piece.
/// `SQLPutData` is only a different way to hand over the same parameter, so it
/// must not be a way to reach the backend with the declared type discarded.
fn dae_buffer_to_value(
    c_type: Option<odbc_sys::CDataType>,
    sql_type: SqlDataType,
    col_size: ULen,
    decimal_digits: i16,
    buffer: &[u8],
) -> Result<ColumnValue, OdbcError> {
    use odbc_sys::CDataType;
    let text = match c_type {
        Some(CDataType::Binary) => {
            // The same table `read_param_value` applies, for the same reason:
            // `SQLPutData` is only a different way to hand over one parameter.
            return crate::binary_convert::binary_to_sql_type(buffer, sql_type, col_size);
        }
        Some(CDataType::WChar) => {
            let units: Vec<u16> = buffer
                .chunks_exact(2)
                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(buffer).into_owned(),
    };
    crate::param_convert::text_to_sql_type(&text, sql_type, col_size, decimal_digits)
}

/// What [`find_data_at_exec_params`] found: the values it could read now, the
/// parameter numbers still owing data, and any warnings the reads raised.
pub(crate) type DataAtExecScan = (
    std::collections::HashMap<u16, crate::types::ColumnValue>,
    Vec<u16>,
    Vec<OdbcError>,
);

/// Scan bound parameters for data-at-execution indicators.
///
/// Returns `(non_dae_values, dae_param_numbers, warnings)`:
/// - `non_dae_values`: HashMap mapping 1-based param number to ColumnValue
///   for parameters that are NOT data-at-execution.
/// - `dae_param_numbers`: Ordered list of 1-based param numbers that ARE
///   data-at-execution, in ascending order.
/// - `warnings`: diagnostics the conversions raised without failing, which the
///   caller posts before returning `SQL_NEED_DATA`. See [`ParamValue`].
///
/// # Safety
///
/// All APD value and indicator pointers must point to valid memory.
pub(crate) unsafe fn find_data_at_exec_params(
    records: ParamRecords<'_>,
    param_count: u16,
) -> Result<DataAtExecScan, OdbcError> {
    let mut non_dae = std::collections::HashMap::new();
    let mut dae_params = Vec::new();
    let mut warnings = Vec::new();

    for i in 1..=param_count {
        if let Some(rec) = records.get(i)? {
            let is_dae = if !rec.apd.indicator_ptr.is_null() {
                // SAFETY: caller guarantees str_len_or_ind_ptr points to valid memory.
                let indicator = unsafe { std::ptr::read_unaligned(rec.apd.indicator_ptr) };
                is_data_at_exec(indicator)
            } else {
                false
            };

            if is_dae {
                dae_params.push(i);
            } else {
                // SAFETY: caller guarantees all APD pointers are valid.
                let read = unsafe { read_param_value(rec) }?;
                warnings.extend(read.warning);
                non_dae.insert(i, read.value);
            }
        } else {
            // The same 07002 `collect_params` reports. This is the other route
            // to the identical gap, and letting it pad with NULL would make
            // data-at-execution a way around the check.
            return Err(unbound_parameter(i));
        }
    }

    Ok((non_dae, dae_params, warnings))
}

/// Resolve `SQL_NTS` for a data-at-execution chunk, in the C type the parameter
/// was bound with.
///
/// The spec's *StrLen_or_Ind* description: "The data must be in the C data type
/// specified in the *ValueType* argument of **SQLBindParameter**." So a
/// `SQL_C_WCHAR` parameter's terminator is a zero *code unit*, not a zero byte.
/// Scanning it byte-wise stops inside the first character of any ASCII text and
/// `dae_buffer_to_value`'s `chunks_exact(2)` then has nothing to pair, so the
/// parameter arrives empty with no diagnostic — the bound-parameter path has
/// always got this right, via `utf16_to_string`.
///
/// A C type that is neither known nor `SQL_C_WCHAR` scans bytes, which is what
/// every single-byte C type wants and what `SQL_C_BINARY` gets by default. A
/// binary value containing a zero byte cannot be sent with `SQL_NTS` at all,
/// under any reading of the spec: the terminator is the only length there is.
///
/// Both mature drivers dispatch on the C type here, which is the evidence
/// rather than the conclusion. psqlODBC's `PGAPI_PutData` (`execute.c`):
/// `putlen = WCLEN * ucs2strlen((SQLWCHAR *) rgbValue)` under
/// `SQL_C_WCHAR == ctype`, and `putlen = strlen(rgbValue)` under
/// `SQL_C_CHAR == ctype`. MySQL Connector/ODBC's `SQLPutData` (`execute.cc`):
/// `cbValue = sqlwcharlen((SQLWCHAR *)rgbValue) * sizeof(SQLWCHAR)` when
/// `aprec->concise_type == SQL_C_WCHAR`, `strlen((const char*)rgbValue)`
/// otherwise. Neither scans bytes for a wide parameter.
///
/// # Safety
///
/// `data_ptr` must be non-null and satisfy [`crate::utf16::nts_byte_len`]'s
/// contract, or [`crate::utf16::nts_utf16_len`]'s when the C type is
/// `SQL_C_WCHAR`.
unsafe fn dae_nts_byte_count(c_type: Option<odbc_sys::CDataType>, data_ptr: *const u8) -> usize {
    match c_type {
        // Two bytes per code unit, exactly as `dae_buffer_to_value` reads them
        // back out of the accumulated buffer.
        Some(odbc_sys::CDataType::WChar) => {
            // SAFETY: forwarded from this function's own contract.
            unsafe { crate::utf16::nts_utf16_len(data_ptr.cast::<u16>()) * 2 }
        }
        // SAFETY: forwarded from this function's own contract.
        _ => unsafe { crate::utf16::nts_byte_len(data_ptr) },
    }
}

/// Generic implementation of SQLPutData.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlputdata-function>
///
/// Allows an application to send data for a parameter at statement execution
/// time. Called one or more times for each data-at-execution parameter after
/// `SQLParamData` identifies the parameter.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (`SQLHSTMT`).
/// - `data_ptr`: Pointer to a buffer containing the actual data for the parameter.
/// - `str_len_or_ind`: The length of `*data_ptr` in bytes, or `SQL_NTS` (-3) for
///   null-terminated strings, or `SQL_NULL_DATA` (-1) to set the parameter to NULL.
///
/// # Spec compliance
///
/// - 01000: General warning — not returned here; core emits no driver-specific
///   informational message from this function. The row carries no `(DM)` marker.
/// - 01004: String data, right truncated — not applicable; data is accumulated without
///   truncation.
/// - 07006: Restricted data type attribute violation — not returned here. The pairing is
///   fixed at `SQLBindParameter`, which refuses the C-to-SQL combinations core cannot convert
///   before the query runs (`crate::binary_convert`, `crate::numeric_convert`), so a chunk
///   arriving here is already of a pairing that was accepted.
/// - 08S01: Communication link failure — not returned here. `SQLPutData` accumulates into a
///   buffer on the statement handle and makes no backend call at all; the link is next touched
///   by the `SQLParamData` that completes the execution, which is where this arrives.
/// - 22001: String data, right truncation — not applicable; no target column size check
///   at this stage.
/// - 22003: Numeric value out of range — not applicable; type conversion happens at execute
///   time.
/// - 22007: Invalid datetime format — not applicable; type conversion happens at execute
///   time.
/// - 22008: Datetime field overflow — not applicable; type conversion happens at execute
///   time.
/// - 22012: Division by zero — not applicable.
/// - 22015: Interval field overflow — not applicable.
/// - 22018: Invalid character value for cast specification — not applicable; type conversion
///   happens at execute time.
/// - HY000: General error — returned for unexpected failures.
/// - HY001: Memory allocation error — not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008: Operation canceled — not returned here. This call makes no fallible backend call,
///   so there is no error for a cancellation to be reported through, and the asynchronous
///   clause is inapplicable: core never returns `SQL_STILL_EXECUTING`. A `SQLCancel` during a
///   data-at-execution sequence discards the sequence; the following `SQLParamData` is what
///   reports it.
/// - HY009: Invalid use of null pointer — (DM) the row is driver-manager-marked. Core keeps a
///   guard anyway, because unixODBC does not always run it and the alternative is
///   dereferencing a null pointer, and the guard matches the clause exactly: "(DM) The
///   argument DataPtr was a null pointer, and the argument StrLen_or_Ind was not 0,
///   SQL_DEFAULT_PARAM, or SQL_NULL_DATA." A null pointer with a length of 0 is a legal
///   zero-length put and is accepted.
/// - HY010: Function sequence error — returned when no data-at-execution is in progress
///   (no prior `SQL_NEED_DATA` from `SQLExecute`/`SQLExecDirectW`), or when
///   `SQLParamData` has not yet been called to identify the current parameter.
///   (DM cases for async/NEED_DATA: driver-manager-handled; not returned here.)
/// - HY013: Memory management error — not applicable.
/// - HY019: Non-character and non-binary data sent in pieces — not applicable; we accept
///   all data types in pieces.
/// - HY020: Attempt to concatenate a null value — **returned by this driver**. The row
///   carries no `(DM)` marker: "SQLPutData was called more than once since the call that
///   returned SQL_NEED_DATA, and in one of those calls, the StrLen_or_Ind argument contained
///   SQL_NULL_DATA or SQL_DEFAULT_PARAM." A NULL is the whole value of a parameter, so
///   `SQLPutData(SQL_NULL_DATA)` after data, and data after `SQLPutData(SQL_NULL_DATA)`, are
///   both refused, and `SQL_DEFAULT_PARAM` is treated the same because the row names it too.
///   Two calls both carrying data are not: the spec's objection is to the null, not to the
///   concatenation.
/// - 07S01: Invalid use of default parameter — (not returned here). `SQL_DEFAULT_PARAM` is
///   **accepted**, following psqlODBC, whose `PGAPI_PutData` pairs it with `SQL_NULL_DATA`
///   and answers `SQL_SUCCESS` while raising "Invalid string or buffer length" for every
///   other negative value; MySQL Connector/ODBC does not recognise the constant at all. The
///   row describes a parameter that "did not have a default value", and no mature driver
///   returns it here. It resolves to NULL, which is the only value it can take in core:
///   `SQL_DEFAULT_PARAM` names a *procedure* parameter's default and `crate::escape` refuses
///   `{call ...}` with `HYC00`, so no statement core executes has one.
/// - HY090: Invalid string or buffer length — returned when `str_len_or_ind` is negative
///   and none of `SQL_NTS`, `SQL_NULL_DATA` or `SQL_DEFAULT_PARAM`, which are the three the
///   spec's *StrLen_or_Ind* description lists.
/// - HY117: Connection suspended — (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired — not returned here; core implements no connection
///   timeout, so no deadline exists to expire. The row carries no `(DM)` marker.
/// - IM001: Driver does not support this function — (driver-manager-handled; not returned
///   here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is
///   not supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification
///   model is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `data_ptr` must point to valid readable memory of at least `str_len_or_ind` bytes
/// (unless `str_len_or_ind` is `SQL_NTS` or `SQL_NULL_DATA`).
pub unsafe fn sql_put_data<B: Backend>(
    statement_handle: *mut c_void,
    data_ptr: *mut c_void,
    str_len_or_ind: isize,
) -> SqlReturn {
    tracing::trace!(
        "SQLPutData(stmt={:?}, data={:?}, raw_len={})",
        statement_handle,
        data_ptr,
        str_len_or_ind
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.get inside the closure. data_ptr is checked for null
    // before use and is valid for the specified length per the caller's contract.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, _conn, records) =
                scope.stmt_with_parent_and_params::<B>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec HY010: must be in DAE state.
            let dae = stmt.data_at_exec.as_mut().ok_or_else(|| {
                OdbcError::general(
                    "No data-at-execution operation in progress",
                    SqlState::function_sequence_error(),
                )
            })?;

            // Spec HY010: SQLParamData must have been called first to set current_param.
            let Some(param_num) = dae.current_param else {
                return Err(OdbcError::general(
                    "SQLParamData must be called before SQLPutData to identify the current parameter",
                    SqlState::function_sequence_error(),
                ));
            };

            // The C type this chunk is declared to be in. Resolved before the
            // branches below so the log names it on every path, not only the
            // one that scans.
            let c_type = records
                .get(param_num)?
                .map(|r| r.apd.c_type())
                .transpose()?;
            tracing::debug!(
                "SQLPutData(stmt={:?}, param={}, c_type={:?}, str_len_or_ind={})",
                statement_handle,
                param_num,
                c_type,
                str_len_or_ind
            );

            // Spec HY020, with no `(DM)` marker: "SQLPutData was called more
            // than once since the call that returned SQL_NEED_DATA, and in one
            // of those calls, the StrLen_or_Ind argument contained
            // SQL_NULL_DATA or SQL_DEFAULT_PARAM." A NULL is the whole value of
            // a parameter, so it can neither follow data nor be followed by it.
            // Both orderings used to succeed: the first discarded what the
            // application had already sent, the second concatenated onto a
            // value it had declared NULL.
            //
            // The row names `SQL_DEFAULT_PARAM` alongside `SQL_NULL_DATA`, so
            // accepting that value below does not exempt it from this rule.
            let whole_value =
                str_len_or_ind == SQL_NULL_DATA || str_len_or_ind == SQL_DEFAULT_PARAM;
            if dae.put_state != PutDataState::NotCalled
                && (whole_value || dae.put_state == PutDataState::Null)
            {
                return Err(OdbcError::general(
                    "A null value cannot be concatenated with other data for one parameter",
                    SqlState::attempt_to_concatenate_a_null_value(),
                ));
            }

            // `SQL_NULL_DATA` sets the parameter to NULL by clearing the
            // buffer, and `SQL_DEFAULT_PARAM` joins it.
            //
            // The spec's *StrLen_or_Ind* description lists `SQL_DEFAULT_PARAM`
            // as a value the argument may carry — "is SQL_NTS, SQL_NULL_DATA,
            // or SQL_DEFAULT_PARAM" — so reporting `HY090` for it, as core did
            // while the constant was unknown, told the application its length
            // was malformed when it was not.
            //
            // Accepted rather than refused, following psqlODBC, which pairs the
            // two constants in `PGAPI_PutData` and answers `SQL_SUCCESS` for
            // both, while raising "Invalid string or buffer length" for every
            // other negative value — so the exemption is deliberate there.
            // MySQL Connector/ODBC does not recognise the constant at all.
            // `SQLPutData`'s `07S01` row was considered and not taken: it
            // describes a parameter that "did not have a default value", and no
            // driver returns it here.
            //
            // NULL is the only value the request can resolve to, and that is a
            // fact about core rather than a guess about a data source:
            // `SQL_DEFAULT_PARAM` names a *procedure* parameter's default, and
            // `crate::escape` refuses `{call ...}` and `{?= call ...}` with
            // `HYC00`, so no statement core executes has a parameter carrying
            // one.
            if whole_value {
                dae.buffer.clear();
                dae.put_state = PutDataState::Null;
                return Ok(SqlReturn::SUCCESS);
            }

            // A refusal to dereference a null pointer, not a spec check: the
            // row is `(DM)`-marked and belongs to the Driver Manager. It is
            // kept because unixODBC does not always run it, and it is written
            // to match the clause exactly — "(DM) The argument DataPtr was a
            // null pointer, and the argument StrLen_or_Ind was not 0,
            // SQL_DEFAULT_PARAM, or SQL_NULL_DATA". The other two values are
            // handled above, so only the zero remains, and a null pointer with
            // a length of zero is a legal zero-length put that this guard used
            // to refuse.
            if data_ptr.is_null() && str_len_or_ind != 0 {
                return Err(OdbcError::general(
                    "DataPtr is null",
                    SqlState::invalid_use_of_null_pointer(),
                ));
            }

            // Determine byte count.
            let byte_count = if str_len_or_ind == SQL_NTS as isize {
                // SAFETY: data_ptr is non-null (guarded above) and the caller
                // guarantees it is null-terminated in the bound C type. Covered
                // by this function's own `unsafe` block, as the reads below are.
                dae_nts_byte_count(c_type, data_ptr.cast::<u8>())
            } else if str_len_or_ind < 0 {
                // Spec HY090: a negative length that is none of the three the
                // *StrLen_or_Ind* description lists — SQL_NTS, SQL_NULL_DATA
                // and SQL_DEFAULT_PARAM, the last two handled above.
                return Err(OdbcError::general(
                    format!("Invalid string or buffer length: {str_len_or_ind}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            } else {
                str_len_or_ind as usize
            };

            if byte_count > 0 {
                // SAFETY: caller guarantees data_ptr is valid for byte_count
                // bytes, and it is non-null: the guard above admits a null only
                // with a length of zero, which this branch excludes.
                // `from_raw_parts` on a null pointer is undefined behaviour
                // even at length zero.
                let data = std::slice::from_raw_parts(data_ptr as *const u8, byte_count);
                dae.buffer.extend_from_slice(data);
            }
            // A zero-length put is still a put: it is what distinguishes an
            // empty value from a parameter nobody supplied.
            dae.put_state = PutDataState::Data;

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLPutData -> {:?}", ret);
    ret
}

/// Generic implementation of SQLParamData.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlparamdata-function>
///
/// Used in conjunction with `SQLPutData` to supply parameter data at execution
/// time. Returns `SQL_NEED_DATA` to identify the next parameter needing data,
/// or executes the statement and returns `SQL_SUCCESS` when all parameters have
/// been supplied.
///
/// # Parameters
///
/// - `statement_handle`: Statement handle (`SQLHSTMT`).
/// - `value_ptr_ptr`: Output pointer. When returning `SQL_NEED_DATA`, receives the
///   `ParameterValuePtr` that was passed to `SQLBindParameter` for the parameter
///   that needs data. The application uses this to identify which parameter is being
///   requested.
///
/// # Spec compliance
///
/// Several rows below are **absent from this function's own diagnostics table** and are
/// documented anyway, because the page grants them: "If **SQLParamData** is called while
/// sending data for a parameter in a SQL statement, it can return any SQLSTATE that can be
/// returned by the function called to execute the statement (**SQLExecute** or
/// **SQLExecDirect**)." Each is marked where it appears.
///
/// - 01000: General warning — propagated from backend (during final execution).
/// - 01004: String data, right truncated — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - 07006: Restricted data type attribute violation — propagated from backend.
/// - 08S01: Communication link failure — propagated from backend.
/// - 22001: String data, right truncation — **absent from this function's diagnostics
///   table**, inherited as above. Returned here when the accumulated
///   data-at-execution value does not survive conversion to the declared SQL type: text
///   truncated by an exact-numeric target, or a value longer than the declared
///   `ColumnSize` for a character or binary target. This is the same check `SQLExecute`
///   applies to a value delivered in one piece (`crate::param_convert`), because
///   `SQLPutData` is only a different way to hand over the same parameter. Also
///   propagated from backend.
/// - 22003: Numeric value out of range — **absent from this function's diagnostics
///   table**, inherited as above. Returned here when that text falls outside the
///   range of the declared numeric type (`crate::param_convert`). Also propagated from
///   backend.
/// - 22007: Invalid datetime format — **absent from this function's diagnostics table**,
///   inherited as above. Returned here for a datetime literal with an
///   out-of-range field (`crate::param_convert`). Also propagated from backend.
/// - 22008: Datetime field overflow — **absent from this function's diagnostics table**,
///   inherited as above. Returned here when that text carries a datetime
///   component the declared type cannot hold (`crate::param_convert`). Also propagated from
///   backend.
/// - 22012: Division by zero — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - 22015: Interval field overflow — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - 22018: Invalid character value for cast specification — **absent from this function's
///   diagnostics table**, inherited as above. Returned here when the
///   accumulated data-at-execution text is not a valid literal of the SQL type declared for
///   the parameter at `SQLBindParameter` (`crate::param_convert`). Also propagated from
///   backend.
/// - 23000: Integrity constraint violation — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - 24000: Invalid cursor state — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - 22026: String data, length mismatch — not returned here. The row's condition opens with
///   "The SQL_NEED_LONG_DATA_LEN information type in `SQLGetInfo` was 'Y'", and core answers
///   `"N"` for it (`default_get_info`), so the driver never asked the application to declare a
///   long parameter's length in advance and has nothing to compare against.
/// - 40001: Serialization failure — propagated from backend.
/// - 40003: Statement completion unknown — propagated from backend.
/// - 42000: Syntax error or access violation — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - 44000: WITH CHECK OPTION violation — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - HY000: General error — propagated from backend.
/// - HY001: Memory allocation error — not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY010: Function sequence error — returned when no data-at-execution operation is in
///   progress, and, per the row's unmarked sentence "The previous function call was a call to
///   SQLParamData", when this call would finalise a parameter for which `SQLPutData` was never
///   called. The data-at-execution state survives that error, so the application recovers by
///   calling `SQLPutData` for the parameter it was already asked for. Three of the row's
///   five clauses are `(DM)`-marked and not returned here; the other unmarked one is the
///   case where SQLCancel was called before data was sent for all data-at-execution
///   parameters, which core reports as `HY008` instead.
/// - HY013: Memory management error — not applicable.
/// - HY090: Invalid string or buffer length — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - HY105: Invalid parameter type — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - HY117: Connection suspended — (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - HYT00: Timeout expired — propagated from backend. **Absent from this function's diagnostics table**; inherited, as above.
/// - HYT01: Connection timeout expired — propagated from backend.
/// - IM001: Driver does not support this function — (driver-manager-handled; not returned
///   here).
/// - IM017: Polling disabled; not returned here (the asynchronous notification model is
///   not supported — not DM-annotated in the spec).
/// - IM018: SQLCompleteAsync not called; not returned here (the asynchronous notification
///   model is not supported — not DM-annotated in the spec).
///
/// # Safety
///
/// `statement_handle` must point to a valid `StatementHandle<B>`.
/// `value_ptr_ptr` must be writable if non-null.
pub unsafe fn sql_param_data<B: Backend>(
    statement_handle: *mut c_void,
    value_ptr_ptr: *mut *mut c_void,
) -> SqlReturn {
    tracing::debug!(
        "SQLParamData(stmt={:?}, value_ptr_ptr={:?})",
        statement_handle,
        value_ptr_ptr
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.stmt_with_parent inside the closure. value_ptr_ptr is checked for
    // null before write. Bound parameter buffer pointers in the APD were registered
    // via SQLBindParameter under the caller's guarantee that they remain valid.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn, records) = scope.stmt_with_parent_and_params::<B>(statement_handle)?;
            // Spec: clear diagnostics at the start of each ODBC call.
            stmt.diagnostics.clear();

            // Spec HY010: must be in DAE state. Take ownership so we can
            // work with it directly without repeated borrows into stmt.
            let mut dae = stmt.data_at_exec.take().ok_or_else(|| {
                OdbcError::general(
                    "No data-at-execution operation in progress",
                    SqlState::function_sequence_error(),
                )
            })?;

            // Spec HY010, the sentence after the `(DM)` clause and itself
            // unmarked: "The previous function call was a call to
            // SQLParamData." An empty buffer cannot stand in for that check —
            // `SQLPutData(ptr, 0)` sends a zero-length value and produces the
            // same empty buffer — so two SQLParamData calls in a row used to
            // send NULL for the parameter and move on.
            //
            // The state goes back on the statement first: an HY010 does not
            // cancel the data-at-execution sequence, and the application
            // recovers by calling SQLPutData for the parameter it was already
            // asked for.
            if dae.current_param.is_some() && dae.put_state == PutDataState::NotCalled {
                stmt.data_at_exec = Some(dae);
                return Err(OdbcError::general(
                    "SQLPutData must be called for the requested parameter before SQLParamData is called again",
                    SqlState::function_sequence_error(),
                ));
            }

            // If there's a current param being filled, finalize it.
            if let Some(param_num) = dae.current_param.take() {
                let value = if dae.put_state == PutDataState::Null {
                    ColumnValue::Null
                } else {
                    let rec = records.get(param_num)?;
                    let c_type = rec.map(|r| r.apd.c_type()).transpose()?;
                    // An absent binding cannot reach here — `SQLParamData` only
                    // offers a parameter that `find_data_at_exec_params` found
                    // a data-at-execution indicator on, which requires one.
                    let sql_type = rec.map_or(SqlDataType::UNKNOWN_TYPE, |r| r.ipd.sql_type());
                    let col_size = rec.map_or(0, |r| r.ipd.length);
                    let decimal_digits = rec.map_or(0, |r| r.ipd.scale);
                    dae_buffer_to_value(c_type, sql_type, col_size, decimal_digits, &dae.buffer)?
                };
                dae.collected_values.insert(param_num, value);
                dae.buffer.clear();
            }

            // Check if there's another pending parameter.
            if let Some(next_param) = dae.pending_params.pop_front() {
                dae.current_param = Some(next_param);
                // A fresh parameter has had nothing put for it, whatever the
                // previous one received. Without this, `HY020` would fire on
                // the first `SQLPutData` for the second parameter of a batch.
                dae.put_state = PutDataState::NotCalled;

                // Write the value_ptr from the binding to *value_ptr_ptr so the app
                // can identify which parameter is being requested.
                if !value_ptr_ptr.is_null() {
                    if let Some(rec) = records.get(next_param)? {
                        std::ptr::write_unaligned(value_ptr_ptr, rec.apd.data_ptr);
                    } else {
                        std::ptr::write_unaligned(value_ptr_ptr, std::ptr::null_mut());
                    }
                }
                // Put the state back — more params still pending.
                stmt.data_at_exec = Some(dae);
                return Ok(SqlReturn::NEED_DATA);
            }

            // All parameters collected — execute the statement.
            let param_count = stmt.param_count.unwrap_or(0);
            let sql = dae.sql.clone();
            // The conversions ran at the call that returned SQL_NEED_DATA, and
            // their warnings travelled here rather than being posted there —
            // see `DataAtExecState::warnings`. This is the call that sends the
            // values, so this is the call that reports them.
            let converted_with_info = !dae.warnings.is_empty();
            for warning in &dae.warnings {
                stmt.diagnostics.push(warning);
            }

            // Assemble complete parameter vector from collected values.
            let mut params = Vec::with_capacity(param_count as usize);
            for i in 1..=param_count {
                params.push(
                    dae.collected_values
                        .get(&i)
                        .cloned()
                        .unwrap_or(ColumnValue::Null),
                );
            }

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

            // If statement was closed (e.g. SQLFreeStmt(SQL_CLOSE)), re-prepare.
            if stmt.statement.is_none() {
                let prepared =
                    timer.check::<B, _, _>(B::prepare(connection, cancel, &sql), cancel)?;
                stmt.set_prepared_statement(crate::handles::StatementData::Backend(prepared));
            }

            let stmt_data = stmt.statement.as_mut().ok_or_else(|| {
                OdbcError::general("No prepared statement", SqlState::function_sequence_error())
            })?;

            let executed = match stmt_data {
                crate::handles::StatementData::Backend(backend_stmt) => timer.check::<B, _, _>(
                    B::execute(connection, cancel, backend_stmt, &params),
                    cancel,
                ),
                crate::handles::StatementData::Synthetic(_) => {
                    return Err(OdbcError::general(
                        "Cannot execute a synthetic statement",
                        SqlState::general_error(),
                    ));
                }
            };
            // The data-at-execution path completes an execution like
            // `SQLExecute` does, so it reports its parameter set the same way.
            // Before the error is propagated, so a failed set is reported as
            // SQL_PARAM_ERROR.
            // SAFETY: the application's parameter-status pointers remain valid
            // per the `SQLSetStmtAttr` contract.
            report_params_processed(
                stmt,
                if executed.is_ok() {
                    SQL_PARAM_SUCCESS
                } else {
                    SQL_PARAM_ERROR
                },
            );
            executed?;

            // A cursor is open only if the execution produced columns; an
            // `UPDATE` leaves the statement in S4, not S5.
            stmt.note_executed();

            Ok(if converted_with_info {
                SqlReturn::SUCCESS_WITH_INFO
            } else {
                SqlReturn::SUCCESS
            })
        })
    };
    tracing::debug!("SQLParamData -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    /// One bound parameter in the shape `SQLBindParameter` receives it, before
    /// core splits it across the APD and the IPD.
    ///
    /// The conversion tests below are about what
    /// [`read_param_value`] does with a C buffer and a declared SQL type, not
    /// about which descriptor each field ends up in. Writing them against two
    /// structs would restate the split forty times without testing it once —
    /// so they keep the single shape and this converts. The split itself is
    /// pinned by `a_bound_parameter_splits_across_the_apd_and_the_ipd`, which
    /// goes through the real `SQLBindParameter` and reads the two descriptors
    /// directly.
    struct BoundParam {
        input_output_type: ParamType,
        c_type: CDataType,
        sql_type: SqlDataType,
        col_size: ULen,
        decimal_digits: i16,
        value_ptr: *mut c_void,
        buffer_length: isize,
        str_len_or_ind_ptr: *mut isize,
    }

    impl BoundParam {
        fn split(&self) -> (DescriptorRecord, DescriptorRecord) {
            (
                DescriptorRecord {
                    concise_type: self.c_type as i16,
                    verbose_type: self.c_type as i16,
                    data_ptr: self.value_ptr,
                    octet_length: self.buffer_length,
                    indicator_ptr: self.str_len_or_ind_ptr,
                    ..Default::default()
                },
                DescriptorRecord {
                    concise_type: self.sql_type.0,
                    verbose_type: self.sql_type.0,
                    length: self.col_size,
                    scale: self.decimal_digits,
                    parameter_type: self.input_output_type,
                    ..Default::default()
                },
            )
        }
    }

    /// [`read_param_value`] for a caller holding a [`BoundParam`].
    ///
    /// # Safety
    ///
    /// As [`read_param_value`]: the value and indicator pointers must be valid.
    unsafe fn read_bound_param(param: &BoundParam) -> Result<ColumnValue, OdbcError> {
        // SAFETY: forwarded from this function's own contract.
        unsafe { read_bound_param_full(param) }.map(|p| p.value)
    }

    /// As [`read_bound_param`], keeping the warning as well as the value, for
    /// the conversions that raise one.
    ///
    /// # Safety
    ///
    /// As [`read_param_value`]: the value and indicator pointers must be valid.
    unsafe fn read_bound_param_full(param: &BoundParam) -> Result<ParamValue, OdbcError> {
        let (apd, ipd) = param.split();
        // SAFETY: forwarded from this function's own contract.
        unsafe {
            read_param_value(ParamRecord {
                apd: &apd,
                ipd: &ipd,
            })
        }
    }

    /// A set of [`BoundParam`]s, split into the two record maps the readers
    /// take.
    struct BoundParams {
        apd: std::collections::HashMap<u16, DescriptorRecord>,
        ipd: std::collections::HashMap<u16, DescriptorRecord>,
    }

    impl BoundParams {
        fn new() -> Self {
            Self {
                apd: std::collections::HashMap::new(),
                ipd: std::collections::HashMap::new(),
            }
        }

        fn insert(&mut self, number: u16, param: BoundParam) {
            let (apd, ipd) = param.split();
            self.apd.insert(number, apd);
            self.ipd.insert(number, ipd);
        }

        fn records(&self) -> ParamRecords<'_> {
            ParamRecords {
                apd: &self.apd,
                ipd: &self.ipd,
            }
        }
    }

    use odbc_sys::HandleType;

    use super::*;
    use crate::{
        ffi::{execute::sql_prepare_w, handle::sql_free_handle},
        handles::ConnectionHandle,
        test_utils::{
            MockBackend, MockCancelAwareBackend, MockConnection, MockLongDataBackend,
            alloc_env_conn_stmt, with_descriptor, with_handle,
        },
        types::{CDataType, ParamType, SQL_INTERVAL_YEAR, SQL_INTERVAL_YEAR_TO_MONTH},
    };

    unsafe fn cleanup(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockBackend>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockBackend>(HandleType::Env as i16, env);
        }
    }

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

    unsafe fn prepare_sql(stmt: *mut c_void, sql: &str) -> SqlReturn {
        let wide: Vec<u16> = sql.encode_utf16().collect();
        unsafe { sql_prepare_w::<MockBackend>(stmt, wide.as_ptr(), wide.len() as i32) }
    }

    /// [`alloc_env_conn_stmt`] with the connection actually open, which the
    /// data-at-execution path needs: `SQLExecDirectW` reads
    /// `ConnectionHandle::connection` before it ever looks at the parameters.
    ///
    /// # Safety
    ///
    /// The caller must free the three tokens with [`cleanup`].
    unsafe fn connected_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            assert_eq!(
                connect_handle(conn),
                SqlReturn::SUCCESS,
                "precondition: the connection is open",
            );
            (env, conn, stmt)
        }
    }

    /// Drive `stmt` into the data-at-execution loop with parameter 1 bound as
    /// `SQL_DATA_AT_EXEC`, stopping where `SQLPutData` is the next legal call.
    ///
    /// The value pointer is null and the indicator is not, which is a binding:
    /// `ParamRecords::get` counts a record carrying either pointer, and
    /// `SQLBindParameter`'s own *ParameterValuePtr* text allows the null "as
    /// long as *StrLen_or_IndPtr is SQL_NULL_DATA or SQL_DATA_AT_EXEC".
    ///
    /// # Safety
    ///
    /// `indicator` must outlive the whole loop: core reads it again at execute
    /// time.
    unsafe fn start_dae_loop(
        stmt: *mut c_void,
        c_type: CDataType,
        sql_type: SqlDataType,
        indicator: *mut isize,
    ) {
        unsafe {
            assert_eq!(
                sql_bind_parameter::<MockBackend>(
                    stmt,
                    1,
                    ParamType::Input as i16,
                    c_type as i16,
                    sql_type.0,
                    50,
                    0,
                    std::ptr::null_mut(),
                    0,
                    indicator,
                ),
                SqlReturn::SUCCESS,
                "precondition: parameter 1 is bound",
            );

            let wide: Vec<u16> = "INSERT INTO t VALUES (?)".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<MockBackend>(
                    stmt,
                    wide.as_ptr(),
                    i32::try_from(wide.len()).expect("short"),
                ),
                SqlReturn::NEED_DATA,
                "precondition: the data-at-execution loop starts",
            );

            let mut value_ptr: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_param_data::<MockBackend>(stmt, &raw mut value_ptr),
                SqlReturn::NEED_DATA,
                "precondition: parameter 1 is requested",
            );
        }
    }

    /// Read the accumulated data-at-execution buffer, which is what
    /// `SQLPutData` appends to and `SQLParamData` later converts.
    fn dae_buffer(stmt: *mut c_void) -> Vec<u8> {
        with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
            h.data_at_exec
                .as_ref()
                .expect("the data-at-execution loop is still open")
                .buffer
                .clone()
        })
    }

    /// `SQL_NTS` must be resolved in the C type the parameter was bound with:
    /// "The data must be in the C data type specified in the ValueType argument
    /// of SQLBindParameter."
    ///
    /// A byte-wise scan stops inside the first character of any ASCII text —
    /// "Hello" in UTF-16LE carries a zero byte at index 1 — and
    /// `dae_buffer_to_value`'s `chunks_exact(2)` then has nothing to pair, so
    /// the parameter arrived as an empty string with no diagnostic at all.
    #[test]
    fn put_data_resolves_sql_nts_in_the_bound_c_type() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::WChar,
                SqlDataType::EXT_W_VARCHAR,
                &raw mut indicator,
            );

            let mut wide: Vec<u16> = "Hello".encode_utf16().collect();
            wide.push(0);
            assert_eq!(
                sql_put_data::<MockBackend>(
                    stmt,
                    wide.as_mut_ptr().cast::<c_void>(),
                    SQL_NTS as isize,
                ),
                SqlReturn::SUCCESS,
            );

            let buffer = dae_buffer(stmt);
            assert_eq!(
                buffer.len(),
                10,
                "five UTF-16 code units are ten bytes; a byte-wise scan keeps one",
            );
            let units: Vec<u16> = buffer
                .chunks_exact(2)
                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                .collect();
            assert_eq!(String::from_utf16_lossy(&units), "Hello");

            cleanup(env, conn, stmt);
        }
    }

    /// The narrow half of the same rule: `SQL_C_CHAR` still terminates on a
    /// zero *byte*, so this is the behaviour the C-type dispatch must not
    /// change.
    #[test]
    fn put_data_resolves_sql_nts_bytewise_for_sql_c_char() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            let mut data = *b"Hello\0";
            assert_eq!(
                sql_put_data::<MockBackend>(
                    stmt,
                    data.as_mut_ptr().cast::<c_void>(),
                    SQL_NTS as isize,
                ),
                SqlReturn::SUCCESS,
            );

            assert_eq!(dae_buffer(stmt), b"Hello".to_vec());

            cleanup(env, conn, stmt);
        }
    }

    /// Spec, `SQLPutData` `HY020`, with no `(DM)` marker: "SQLPutData was
    /// called more than once since the call that returned SQL_NEED_DATA, and in
    /// one of those calls, the StrLen_or_Ind argument contained SQL_NULL_DATA
    /// or SQL_DEFAULT_PARAM."
    ///
    /// The old behaviour cleared the buffer and answered SQL_SUCCESS, so the
    /// data the application had already sent vanished without a diagnostic.
    #[test]
    fn put_data_null_after_data_reports_hy020() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            let mut data = *b"abc";
            assert_eq!(
                sql_put_data::<MockBackend>(stmt, data.as_mut_ptr().cast::<c_void>(), 3),
                SqlReturn::SUCCESS,
                "precondition: data is put for the current parameter",
            );

            assert_eq!(
                sql_put_data::<MockBackend>(stmt, std::ptr::null_mut(), SQL_NULL_DATA),
                SqlReturn::ERROR,
            );
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY020");
            });

            cleanup(env, conn, stmt);
        }
    }

    /// The same row read the other way round: a NULL already sent cannot be
    /// concatenated onto. This is the ordering that silently produced "abc"
    /// where the application had declared the parameter NULL.
    #[test]
    fn put_data_data_after_null_reports_hy020() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            assert_eq!(
                sql_put_data::<MockBackend>(stmt, std::ptr::null_mut(), SQL_NULL_DATA),
                SqlReturn::SUCCESS,
                "precondition: the parameter is set to NULL",
            );

            let mut data = *b"abc";
            assert_eq!(
                sql_put_data::<MockBackend>(stmt, data.as_mut_ptr().cast::<c_void>(), 3),
                SqlReturn::ERROR,
            );
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY020");
            });

            cleanup(env, conn, stmt);
        }
    }

    /// The row says "more than once", so the *first* call carrying
    /// `SQL_NULL_DATA` is legal and so is a second call carrying more data.
    /// Neither may be turned into HY020 by an over-eager check.
    #[test]
    fn put_data_accepts_a_first_null_and_repeated_data() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            let mut first = *b"ab";
            let mut second = *b"cd";
            assert_eq!(
                sql_put_data::<MockBackend>(stmt, first.as_mut_ptr().cast::<c_void>(), 2),
                SqlReturn::SUCCESS,
            );
            assert_eq!(
                sql_put_data::<MockBackend>(stmt, second.as_mut_ptr().cast::<c_void>(), 2),
                SqlReturn::SUCCESS,
                "data may be sent in as many pieces as the application likes",
            );
            assert_eq!(dae_buffer(stmt), b"abcd".to_vec());

            cleanup(env, conn, stmt);
        }
    }

    /// Spec, `SQLParamData` `HY010`, the sentence after the `(DM)` clause and
    /// itself unmarked: "The previous function call was a call to
    /// SQLParamData."
    ///
    /// The old finaliser read an empty buffer as NULL, so calling SQLParamData
    /// twice in a row sent NULL for the parameter and moved on. An application
    /// that lost track of its own loop got a silently wrong row inserted.
    #[test]
    fn param_data_called_twice_in_a_row_reports_hy010() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            // `start_dae_loop` left parameter 1 requested and no SQLPutData
            // called for it.
            let mut value_ptr: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_param_data::<MockBackend>(stmt, &raw mut value_ptr),
                SqlReturn::ERROR,
            );
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY010");
                assert!(
                    h.data_at_exec.is_some(),
                    "the loop must survive so the application can recover by calling SQLPutData",
                );
            });

            cleanup(env, conn, stmt);
        }
    }

    /// `SQLPutData(ptr, 0)` sends a zero-length value; `SQLPutData(_,
    /// SQL_NULL_DATA)` sends NULL. Reading an empty accumulated buffer as NULL
    /// collapsed the two, so an empty string could not be sent at all.
    ///
    /// Two parameters, because a finalised value is only observable while the
    /// loop is still open — the second `SQLParamData` finalises the first
    /// parameter and then asks for the second.
    #[test]
    fn param_data_keeps_a_zero_length_value_distinct_from_null() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut ind1: isize = SQL_DATA_AT_EXEC;
            let mut ind2: isize = SQL_DATA_AT_EXEC;

            for (number, indicator) in [(1u16, &raw mut ind1), (2u16, &raw mut ind2)] {
                assert_eq!(
                    sql_bind_parameter::<MockBackend>(
                        stmt,
                        number,
                        ParamType::Input as i16,
                        CDataType::Char as i16,
                        SqlDataType::VARCHAR.0,
                        50,
                        0,
                        std::ptr::null_mut(),
                        0,
                        indicator,
                    ),
                    SqlReturn::SUCCESS,
                );
            }

            let wide: Vec<u16> = "INSERT INTO t VALUES (?, ?)".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<MockBackend>(
                    stmt,
                    wide.as_ptr(),
                    i32::try_from(wide.len()).expect("short"),
                ),
                SqlReturn::NEED_DATA,
            );

            let mut value_ptr: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_param_data::<MockBackend>(stmt, &raw mut value_ptr),
                SqlReturn::NEED_DATA,
                "precondition: parameter 1 is requested",
            );

            let mut nothing = [0u8; 1];
            assert_eq!(
                sql_put_data::<MockBackend>(stmt, nothing.as_mut_ptr().cast::<c_void>(), 0),
                SqlReturn::SUCCESS,
                "a zero-length put is legal",
            );

            assert_eq!(
                sql_param_data::<MockBackend>(stmt, &raw mut value_ptr),
                SqlReturn::NEED_DATA,
                "parameter 1 is finalised and parameter 2 requested",
            );

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                let dae = h.data_at_exec.as_ref().expect("the loop is still open");
                assert_eq!(
                    dae.collected_values.get(&1),
                    Some(&ColumnValue::String(String::new())),
                    "a zero-length put must not become NULL",
                );
            });

            cleanup(env, conn, stmt);
        }
    }

    /// The state is per parameter, not per loop: the second parameter of a
    /// batch starts from "nothing put", or its first `SQLPutData` would be
    /// refused as a concatenated null.
    #[test]
    fn param_data_resets_the_put_state_for_each_parameter() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut ind1: isize = SQL_DATA_AT_EXEC;
            let mut ind2: isize = SQL_DATA_AT_EXEC;

            for (number, indicator) in [(1u16, &raw mut ind1), (2u16, &raw mut ind2)] {
                assert_eq!(
                    sql_bind_parameter::<MockBackend>(
                        stmt,
                        number,
                        ParamType::Input as i16,
                        CDataType::Char as i16,
                        SqlDataType::VARCHAR.0,
                        50,
                        0,
                        std::ptr::null_mut(),
                        0,
                        indicator,
                    ),
                    SqlReturn::SUCCESS,
                );
            }

            let wide: Vec<u16> = "INSERT INTO t VALUES (?, ?)".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<MockBackend>(
                    stmt,
                    wide.as_ptr(),
                    i32::try_from(wide.len()).expect("short"),
                ),
                SqlReturn::NEED_DATA,
            );

            let mut value_ptr: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_param_data::<MockBackend>(stmt, &raw mut value_ptr),
                SqlReturn::NEED_DATA,
            );
            // Parameter 1 is NULL.
            assert_eq!(
                sql_put_data::<MockBackend>(stmt, std::ptr::null_mut(), SQL_NULL_DATA),
                SqlReturn::SUCCESS,
            );
            assert_eq!(
                sql_param_data::<MockBackend>(stmt, &raw mut value_ptr),
                SqlReturn::NEED_DATA,
            );
            // Parameter 2 takes data, which a state left at Null would refuse.
            let mut data = *b"abc";
            assert_eq!(
                sql_put_data::<MockBackend>(stmt, data.as_mut_ptr().cast::<c_void>(), 3),
                SqlReturn::SUCCESS,
                "the put state must reset when SQLParamData names a new parameter",
            );

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                let dae = h.data_at_exec.as_ref().expect("the loop is still open");
                assert_eq!(dae.collected_values.get(&1), Some(&ColumnValue::Null));
            });

            cleanup(env, conn, stmt);
        }
    }

    /// `SQL_DEFAULT_PARAM` is a value the spec's own *StrLen_or_Ind*
    /// description lists — "is SQL_NTS, SQL_NULL_DATA, or SQL_DEFAULT_PARAM" —
    /// so it must not be reported as a malformed length. It was `HY090` only
    /// because the constant was unknown to core.
    ///
    /// Accepted rather than refused, following psqlODBC, whose `PGAPI_PutData`
    /// pairs it with `SQL_NULL_DATA` in both directions: `if (cbValue ==
    /// SQL_NULL_DATA || cbValue == SQL_DEFAULT_PARAM) putlen = cbValue;` and
    /// then `if (cbValue == SQL_NULL_DATA || cbValue == SQL_DEFAULT_PARAM) {
    /// retval = SQL_SUCCESS; goto cleanup; }`. Its unrecognised-negative branch
    /// raises "Invalid string or buffer length", so exempting `-5` from that
    /// path is deliberate there rather than incidental.
    #[test]
    fn put_data_accepts_sql_default_param_as_a_null_like_value() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            assert_eq!(
                sql_put_data::<MockBackend>(stmt, std::ptr::null_mut(), SQL_DEFAULT_PARAM),
                SqlReturn::SUCCESS,
                "SQL_DEFAULT_PARAM is a listed value, not a malformed length",
            );

            // Core refuses `{call ...}` and `{?= call ...}` with `HYC00`, so no
            // statement it executes has a parameter with a data-source default
            // to substitute. NULL is the only value the request can resolve to,
            // which is what the recorded state says it will become.
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                let dae = h.data_at_exec.as_ref().expect("the loop is still open");
                assert_eq!(
                    dae.put_state,
                    crate::handles::PutDataState::Null,
                    "SQL_DEFAULT_PARAM resolves to NULL when there is no default",
                );
                assert!(dae.buffer.is_empty(), "no data may be left to concatenate");
            });

            cleanup(env, conn, stmt);
        }
    }

    /// The spec's `HY020` row names `SQL_DEFAULT_PARAM` beside `SQL_NULL_DATA`
    /// — "in one of those calls, the StrLen_or_Ind argument contained
    /// SQL_NULL_DATA or SQL_DEFAULT_PARAM" — so accepting the value must not
    /// exempt it from the concatenation rule.
    #[test]
    fn put_data_default_param_after_data_reports_hy020() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            let mut data = *b"abc";
            assert_eq!(
                sql_put_data::<MockBackend>(stmt, data.as_mut_ptr().cast::<c_void>(), 3),
                SqlReturn::SUCCESS,
                "precondition: data is put for the current parameter",
            );

            assert_eq!(
                sql_put_data::<MockBackend>(stmt, std::ptr::null_mut(), SQL_DEFAULT_PARAM),
                SqlReturn::ERROR,
            );
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY020");
            });

            cleanup(env, conn, stmt);
        }
    }

    /// Every *other* negative length is still `HY090`, whose row exempts only
    /// `SQL_NTS` and `SQL_NULL_DATA`. Recognising one constant must not open
    /// the door to the rest.
    #[test]
    fn put_data_still_reports_hy090_for_an_unlisted_negative_length() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            let mut data = *b"abc";
            assert_eq!(
                sql_put_data::<MockBackend>(stmt, data.as_mut_ptr().cast::<c_void>(), -7),
                SqlReturn::ERROR,
            );
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY090");
            });

            cleanup(env, conn, stmt);
        }
    }

    /// The spec's clause, read as written: "(DM) The argument DataPtr was a
    /// null pointer, and the argument StrLen_or_Ind was **not** 0,
    /// SQL_DEFAULT_PARAM, or SQL_NULL_DATA." A null pointer with a length of
    /// zero is therefore a legal zero-length put, and core's own guard — which
    /// exists to avoid dereferencing a null pointer, not to enforce a `(DM)`
    /// row — must not be stricter than the clause it stands in for.
    #[test]
    fn put_data_accepts_a_null_pointer_with_a_zero_length() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            assert_eq!(
                sql_put_data::<MockBackend>(stmt, std::ptr::null_mut(), 0),
                SqlReturn::SUCCESS,
            );
            assert!(
                dae_buffer(stmt).is_empty(),
                "a zero-length put appends nothing",
            );

            cleanup(env, conn, stmt);
        }
    }

    /// The half of the clause that stays: a null pointer with a real length is
    /// still HY009, because there is nothing to read those bytes from.
    #[test]
    fn put_data_rejects_a_null_pointer_with_a_real_length() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            assert_eq!(
                sql_put_data::<MockBackend>(stmt, std::ptr::null_mut(), 3),
                SqlReturn::ERROR,
            );
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                let rec = h.diagnostics.get(0).expect("record 1 exists");
                assert_eq!(rec.sqlstate.as_str(), "HY009");
            });

            cleanup(env, conn, stmt);
        }
    }

    /// `CStr::from_ptr` scans without a bound, so a buffer whose terminator is
    /// missing is read past its own allocation — the only such scan in the
    /// crate, since `utf16_to_string` caps its own at `MAX_NTS_SCAN`.
    ///
    /// The buffer is exactly the cap, so a correctly bounded scan stops on its
    /// last byte and reads nothing beyond it. One byte further and Miri reports
    /// the over-read, which is what makes this test worth its size.
    #[test]
    fn put_data_bounds_an_unterminated_nts_scan() {
        unsafe {
            let (env, conn, stmt) = connected_stmt();
            let mut indicator: isize = SQL_DATA_AT_EXEC;
            start_dae_loop(
                stmt,
                CDataType::Char,
                SqlDataType::VARCHAR,
                &raw mut indicator,
            );

            let mut data = vec![b'a'; crate::utf16::MAX_NTS_SCAN];
            assert_eq!(
                sql_put_data::<MockBackend>(
                    stmt,
                    data.as_mut_ptr().cast::<c_void>(),
                    SQL_NTS as isize,
                ),
                SqlReturn::SUCCESS,
            );

            assert_eq!(
                dae_buffer(stmt).len(),
                crate::utf16::MAX_NTS_SCAN,
                "the scan did not stop at its bound",
            );

            cleanup(env, conn, stmt);
        }
    }

    /// Spec, `SQLParamData` `HY008`, second clause: `SQLCancel` "was called on
    /// the *StatementHandle* from a different thread in a multithread
    /// application". No `(DM)` marker, so the driver returns it.
    ///
    /// `SQLParamData` is the one entry point that reaches the backend only at
    /// the *end* of the data-at-execution loop, so the whole loop has to run
    /// before the reclassified call site is reachable at all.
    #[test]
    fn a_cancelled_param_data_reports_hy008() {
        unsafe {
            let (env, conn, stmt) = alloc_cancel_aware_stmt();

            // Bind parameter 1 as data-at-execution, so SQLExecDirectW defers
            // to the SQLPutData / SQLParamData loop instead of executing.
            let mut ind: isize = SQL_DATA_AT_EXEC;
            let mut val: i32 = 0;
            assert_eq!(
                sql_bind_parameter::<MockCancelAwareBackend>(
                    stmt,
                    1,
                    ParamType::Input as i16,
                    CDataType::Char as i16,
                    SqlDataType::VARCHAR.0,
                    10,
                    0,
                    std::ptr::from_mut(&mut val).cast::<c_void>(),
                    0,
                    &raw mut ind,
                ),
                SqlReturn::SUCCESS,
            );

            let wide: Vec<u16> = "SELECT ?".encode_utf16().collect();
            assert_eq!(
                crate::ffi::execute::sql_exec_direct_w::<MockCancelAwareBackend>(
                    stmt,
                    wide.as_ptr(),
                    wide.len() as i32,
                ),
                SqlReturn::NEED_DATA,
                "precondition: the data-at-execution loop starts",
            );

            let mut value_ptr: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                sql_param_data::<MockCancelAwareBackend>(stmt, &raw mut value_ptr),
                SqlReturn::NEED_DATA,
                "precondition: parameter 1 is requested",
            );

            let data = b"x";
            assert_eq!(
                sql_put_data::<MockCancelAwareBackend>(
                    stmt,
                    data.as_ptr().cast::<c_void>().cast_mut(),
                    1,
                ),
                SqlReturn::SUCCESS,
                "precondition: the parameter's data is supplied",
            );

            // The next SQLParamData has every parameter and calls the backend.
            MockCancelAwareBackend::fail_next_execution();
            MockCancelAwareBackend::cancel_before_returning();
            assert_eq!(
                sql_param_data::<MockCancelAwareBackend>(stmt, &raw mut value_ptr),
                SqlReturn::ERROR,
            );
            with_handle::<MockCancelAwareBackend, StatementHandle<MockCancelAwareBackend>, _>(
                stmt,
                |h| {
                    let rec = h.diagnostics.get(0).expect("record 1 exists");
                    assert_eq!(rec.sqlstate.as_str(), "HY008");
                },
            );

            cleanup_cancel_aware(env, conn, stmt);
        }
    }

    /// Env + connection + statement for [`MockCancelAwareBackend`], connected
    /// directly rather than through `SQLDriverConnectW`.
    unsafe fn alloc_cancel_aware_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = crate::ffi::handle::sql_alloc_handle::<MockCancelAwareBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = crate::ffi::handle::sql_alloc_handle::<MockCancelAwareBackend>(
                HandleType::Dbc as i16,
                env,
                &mut conn,
            );
            with_handle::<MockCancelAwareBackend, ConnectionHandle<MockCancelAwareBackend>, _>(
                conn,
                |c| {
                    c.connection = Some(MockConnection);
                },
            );
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = crate::ffi::handle::sql_alloc_handle::<MockCancelAwareBackend>(
                HandleType::Stmt as i16,
                conn,
                &mut stmt,
            );
            (env, conn, stmt)
        }
    }

    unsafe fn cleanup_cancel_aware(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = sql_free_handle::<MockCancelAwareBackend>(HandleType::Stmt as i16, stmt);
            let _ = crate::ffi::connect::sql_disconnect::<MockCancelAwareBackend>(conn);
            let _ = sql_free_handle::<MockCancelAwareBackend>(HandleType::Dbc as i16, conn);
            let _ = sql_free_handle::<MockCancelAwareBackend>(HandleType::Env as i16, env);
        }
    }

    #[test]
    fn bind_parameter_stores_binding() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 42;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SqlDataType::INTEGER.0,
                10,
                0,
                &mut val as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    /// `SQLBindParameter` writes two descriptors, not one: the C-side buffer
    /// fields are an APD record and the declared SQL type is an IPD record.
    ///
    /// Keeping both halves in a single struct is what would make
    /// `SQLSetDescField` unimplementable — setting `SQL_DESC_DATA_PTR` on the
    /// APD would have to reach into a record that also claims to be the IPD's —
    /// so the split is pinned here rather than left to the descriptor accessors to
    /// discover.
    #[test]
    fn a_bound_parameter_splits_across_the_apd_and_the_ipd() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 42;
            let mut indicator: isize = 4;
            let val_ptr = std::ptr::from_mut(&mut val).cast::<c_void>();
            let indicator_ptr = std::ptr::from_mut(&mut indicator);

            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SqlDataType::INTEGER.0,
                10,
                0,
                val_ptr,
                4,
                indicator_ptr,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Apd, |desc| {
                let apd = desc.records.get(&1).expect("no APD record was written");
                assert_eq!(
                    apd.c_type()
                        .expect("SQLBindParameter stored a valid C type"),
                    CDataType::SLong
                );
                assert_eq!(apd.data_ptr, val_ptr);
                assert_eq!(apd.octet_length, 4);
                assert_eq!(apd.indicator_ptr, indicator_ptr);
            });
            with_descriptor::<MockBackend, _>(stmt, DescriptorRole::Ipd, |desc| {
                let ipd = desc.records.get(&1).expect("no IPD record was written");
                assert_eq!(ipd.sql_type(), SqlDataType::INTEGER);
                assert_eq!(ipd.length, 10);
                assert_eq!(ipd.scale, 0);
                assert_eq!(ipd.parameter_type, ParamType::Input);
            });

            cleanup(env, conn, stmt);
        }
    }

    /// Unbinding must clear both descriptors. Leaving a record in one is the
    /// half-bound state [`ParamRecords::get`] reports as an internal error, and
    /// it would surface on the next execution rather than here.
    #[test]
    fn unbinding_a_parameter_clears_both_descriptors() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 42;

            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SqlDataType::INTEGER.0,
                10,
                0,
                std::ptr::from_mut(&mut val).cast::<c_void>(),
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            // Null value pointer and null indicator: the unbind form.
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SqlDataType::INTEGER.0,
                10,
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);

            for (role, which) in [(DescriptorRole::Apd, "APD"), (DescriptorRole::Ipd, "IPD")] {
                with_descriptor::<MockBackend, _>(stmt, role, |desc| {
                    assert!(
                        !desc.records.contains_key(&1),
                        "the {which} record survived the unbind"
                    );
                });
            }

            cleanup(env, conn, stmt);
        }
    }

    /// The spec runs the consistency check at bind, not only through the
    /// descriptor: "This check is always performed when SQLBindParameter or
    /// SQLBindCol is called". A binding that cannot be consistent is rejected
    /// before the statement runs rather than converted at execute time.
    #[test]
    fn bind_parameter_rejects_an_inconsistent_decimal_with_hy021() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 0;

            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SqlDataType::DECIMAL.0,
                5, // ColumnSize    -> SQL_DESC_PRECISION for an exact numeric
                9, // DecimalDigits -> SQL_DESC_SCALE
                std::ptr::from_mut(&mut val).cast::<c_void>(),
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);

            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                let record = handle
                    .diagnostics
                    .get(0)
                    .expect("no diagnostic was recorded for the inconsistent bind");
                assert_eq!(
                    record.sqlstate.as_str(),
                    crate::types::sql_state::INCONSISTENT_DESCRIPTOR_INFORMATION
                );
            });
            for role in [DescriptorRole::Apd, DescriptorRole::Ipd] {
                with_descriptor::<MockBackend, _>(stmt, role, |desc| {
                    assert!(
                        !desc.records.contains_key(&1),
                        "a rejected bind must leave neither descriptor written ({role:?})"
                    );
                });
            }

            cleanup(env, conn, stmt);
        }
    }

    /// Core converts SQL_C_BINARY only to the targets whose byte layout ODBC
    /// defines. The pairing is fixed at bind and needs no backend metadata, so
    /// the refusal is here rather than at execute time.
    #[test]
    fn bind_parameter_refuses_binary_to_decimal_with_07006() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i64 = 0;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::Binary as i16,
                SqlDataType::DECIMAL.0,
                19,
                2,
                &mut val as *mut i64 as *mut c_void,
                8,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn bind_parameter_refuses_binary_to_a_character_type_with_07006() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i64 = 0;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::Binary as i16,
                SqlDataType::VARCHAR.0,
                10,
                0,
                &mut val as *mut i64 as *mut c_void,
                8,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    /// The *C to SQL: Numeric* table's interval footnote: the conversion is
    /// supported "only for the exact numeric data types … not … for the
    /// approximate numeric data types (SQL_C_FLOAT or SQL_C_DOUBLE)".
    #[test]
    fn bind_parameter_refuses_an_approximate_source_to_an_interval_with_07006() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: f64 = 0.0;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::Double as i16,
                SQL_INTERVAL_YEAR.0,
                0,
                0,
                &mut val as *mut f64 as *mut c_void,
                8,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    /// The same footnote's other clause: exact numeric C types "cannot be
    /// converted to an interval SQL type whose interval precision is not a
    /// single field".
    #[test]
    fn bind_parameter_refuses_an_integer_to_a_multi_field_interval_with_07006() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 0;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SQL_INTERVAL_YEAR_TO_MONTH.0,
                0,
                0,
                &mut val as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    /// The pairing the footnote does permit, so the gate must not over-reach.
    #[test]
    fn bind_parameter_accepts_an_exact_source_to_a_single_field_interval() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 0;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SQL_INTERVAL_YEAR.0,
                0,
                0,
                &mut val as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    /// A numeric source paired with a target this table does not list at all —
    /// `SQL_GUID` is absent from every one of its six rows.
    #[test]
    fn bind_parameter_refuses_a_numeric_source_to_a_guid_with_07006() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 0;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::SLong as i16,
                SqlDataType::EXT_GUID.0,
                0,
                0,
                &mut val as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn bind_parameter_accepts_binary_to_integer() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 0;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::Binary as i16,
                SqlDataType::INTEGER.0,
                0,
                0,
                &mut val as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    /// The refusal is about the pairing, not about SQL_C_BINARY: a character C
    /// type bound to the same DECIMAL target is still accepted, because
    /// `param_convert` converts it.
    #[test]
    fn bind_parameter_still_accepts_char_to_decimal() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i64 = 0;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                ParamType::Input as i16,
                CDataType::Char as i16,
                SqlDataType::DECIMAL.0,
                19,
                2,
                &mut val as *mut i64 as *mut c_void,
                8,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn bind_parameter_zero_number_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 0;
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                0, // invalid — must be >= 1
                1,
                -16,
                4,
                10,
                0,
                &mut val as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn bind_parameter_null_both_ptrs_removes_binding() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut val: i32 = 1;
            // First bind param 1
            let _ = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                1,
                -16,
                4,
                10,
                0,
                &mut val as *mut i32 as *mut c_void,
                4,
                std::ptr::null_mut(),
            );
            // Now unbind with both null
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                1,
                -16,
                4,
                10,
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn bind_parameter_unknown_c_type_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = sql_bind_parameter::<MockBackend>(
                stmt,
                1,
                1,
                200, // unknown C type (99 is CDataType::Default which is valid)
                4,
                10,
                0,
                std::ptr::dangling_mut::<c_void>(),
                4,
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn num_params_without_prepare_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let mut count: i16 = 0;
            let ret = sql_num_params::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn num_params_after_prepare_returns_count() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);
            let ret = prepare_sql(stmt, "SELECT * FROM t WHERE id = ? AND name = ?");
            assert_eq!(ret, SqlReturn::SUCCESS);
            let mut count: i16 = 0;
            let ret = sql_num_params::<MockBackend>(stmt, &mut count);
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(count, 2);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn num_params_null_output_ptr_still_succeeds() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);
            let _ret = prepare_sql(stmt, "INSERT INTO t VALUES (?)");
            let ret = sql_num_params::<MockBackend>(stmt, std::ptr::null_mut());
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    /// The dialect the `count_params` tests scan with unless they are pinning
    /// dialect-specific quoting: `"`-delimited identifiers, as
    /// `Backend::escape_dialect` defaults to.
    fn ansi() -> crate::escape::EscapeDialect {
        crate::escape::EscapeDialect::ansi_default()
    }

    #[test]
    fn count_params_no_params() {
        assert_eq!(count_params("SELECT 1", &ansi()), 0);
    }

    #[test]
    fn count_params_single() {
        assert_eq!(count_params("SELECT * FROM t WHERE id = ?", &ansi()), 1);
    }

    #[test]
    fn count_params_multiple() {
        assert_eq!(
            count_params("INSERT INTO t (a, b, c) VALUES (?, ?, ?)", &ansi()),
            3
        );
    }

    #[test]
    fn count_params_ignores_question_mark_in_string_literal() {
        assert_eq!(count_params("SELECT '?' FROM t WHERE x = ?", &ansi()), 1);
    }

    #[test]
    fn count_params_handles_escaped_quote() {
        assert_eq!(
            count_params("SELECT 'it''s?' FROM t WHERE x = ?", &ansi()),
            1
        );
    }

    /// A `?` inside a delimited identifier is part of the column's name, not a
    /// parameter marker. Counting it makes `SQLNumParams` over-report and hands
    /// the backend a value list one longer than the statement has markers.
    #[test]
    fn count_params_ignores_question_mark_in_a_quoted_identifier() {
        assert_eq!(count_params(r#"SELECT "a?b" FROM t"#, &ansi()), 0);
    }

    /// A doubled `""` escapes the delimiter and keeps the identifier open, the
    /// same way `''` does inside a string literal.
    #[test]
    fn count_params_ignores_question_mark_after_a_doubled_quote_in_an_identifier() {
        assert_eq!(count_params(r#"SELECT "a""?b" FROM t"#, &ansi()), 0);
    }

    /// Which characters delimit an identifier is the backend's to state, so the
    /// scanner reads it from the dialect rather than assuming `"`.
    #[test]
    fn count_params_honours_the_dialects_bracket_identifiers() {
        let bracket = crate::escape::EscapeDialect::ansi_default()
            .with_identifier_quotes(&[('"', '"'), ('[', ']')]);
        assert_eq!(count_params("SELECT [a?b] FROM t WHERE x = ?", &bracket), 1);
    }

    #[test]
    fn count_params_ignores_question_mark_in_a_line_comment() {
        assert_eq!(count_params("SELECT 1 -- really?\nWHERE x = ?", &ansi()), 1);
    }

    /// An unterminated line comment runs to the end of the statement.
    #[test]
    fn count_params_ignores_question_mark_in_an_unterminated_line_comment() {
        assert_eq!(count_params("SELECT 1 -- really?", &ansi()), 0);
    }

    #[test]
    fn count_params_ignores_question_mark_in_a_block_comment() {
        assert_eq!(count_params("SELECT /* a? b */ 1 WHERE x = ?", &ansi()), 1);
    }

    #[test]
    fn count_params_ignores_question_mark_in_an_unterminated_block_comment() {
        assert_eq!(count_params("SELECT 1 /* a? b", &ansi()), 0);
    }

    #[test]
    fn describe_param_returns_generic_type_info() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);
            let ret = prepare_sql(stmt, "SELECT * FROM t WHERE a = ? AND b = ?");
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut data_type: i16 = 0;
            let mut param_size: ULen = 0;
            let mut decimal_digits: i16 = -1;
            let mut nullable: i16 = -1;
            let ret = sql_describe_param::<MockBackend>(
                stmt,
                1,
                &mut data_type,
                &mut param_size,
                &mut decimal_digits,
                &mut nullable,
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(data_type, SqlDataType::VARCHAR.0);
            assert_eq!(param_size, SQL_DEFAULT_PARAM_SIZE as ULen);
            assert_eq!(decimal_digits, 0);
            assert_eq!(nullable, crate::types::Nullable::SqlNullable as i16);
            cleanup(env, conn, stmt);
        }
    }

    /// The backend's answer must reach the application unchanged. Core reported
    /// a hard-wired `VARCHAR(4000)` for every parameter before
    /// `Backend::describe_param` existed, which a client that sizes its buffers
    /// from this turns into a number sent as text.
    #[test]
    fn describe_param_reports_what_the_backend_describes() {
        unsafe {
            let (env, conn, stmt) = alloc_long_data_env();

            let mut data_type: i16 = 0;
            let mut param_size: ULen = 0;
            let mut decimal_digits: i16 = -1;
            let mut nullable: i16 = -1;
            let ret = sql_describe_param::<MockLongDataBackend>(
                stmt,
                1,
                &mut data_type,
                &mut param_size,
                &mut decimal_digits,
                &mut nullable,
            );

            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(data_type, SqlDataType::DECIMAL.0);
            assert_eq!(param_size, 18 as ULen);
            assert_eq!(decimal_digits, 4);
            assert_eq!(nullable, crate::types::Nullable::SqlNoNulls as i16);

            cleanup_long_data_env(env, conn, stmt);
        }
    }

    /// A backend that declines to describe one parameter still gets core's
    /// generic answer for it, rather than an error — the call is supported
    /// either way, which is what `SQL_DESCRIBE_PARAMETER` = "Y" states.
    #[test]
    fn describe_param_falls_back_when_the_backend_declines() {
        unsafe {
            let (env, conn, stmt) = alloc_long_data_env();

            let mut data_type: i16 = 0;
            let mut param_size: ULen = 0;
            let mut decimal_digits: i16 = -1;
            let mut nullable: i16 = -1;
            let ret = sql_describe_param::<MockLongDataBackend>(
                stmt,
                2,
                &mut data_type,
                &mut param_size,
                &mut decimal_digits,
                &mut nullable,
            );

            assert_eq!(ret, SqlReturn::SUCCESS);
            assert_eq!(data_type, SqlDataType::VARCHAR.0);
            assert_eq!(param_size, SQL_DEFAULT_PARAM_SIZE as ULen);
            assert_eq!(nullable, crate::types::Nullable::SqlNullable as i16);

            cleanup_long_data_env(env, conn, stmt);
        }
    }

    /// Env + connection + statement prepared with two parameter markers, for
    /// [`MockLongDataBackend`], whose `describe_param` answers for the first.
    unsafe fn alloc_long_data_env() -> (*mut c_void, *mut c_void, *mut c_void) {
        let mut env: *mut c_void = std::ptr::null_mut();
        let mut conn: *mut c_void = std::ptr::null_mut();
        let mut stmt: *mut c_void = std::ptr::null_mut();
        unsafe {
            let _ = crate::ffi::handle::sql_alloc_handle::<MockLongDataBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env,
            );
            let _ = crate::ffi::handle::sql_alloc_handle::<MockLongDataBackend>(
                HandleType::Dbc as i16,
                env,
                &mut conn,
            );
            let wide: Vec<u16> = "DRIVER=mock;".encode_utf16().collect();
            let _ = crate::ffi::connect::sql_driver_connect_w::<MockLongDataBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            let _ = crate::ffi::handle::sql_alloc_handle::<MockLongDataBackend>(
                HandleType::Stmt as i16,
                conn,
                &mut stmt,
            );
            let sql = "SELECT * FROM t WHERE a = ? AND b = ?";
            let wide: Vec<u16> = sql.encode_utf16().collect();
            let ret = sql_prepare_w::<MockLongDataBackend>(stmt, wide.as_ptr(), wide.len() as i32);
            assert_eq!(ret, SqlReturn::SUCCESS, "precondition: prepare");
        }
        (env, conn, stmt)
    }

    unsafe fn cleanup_long_data_env(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
        unsafe {
            let _ = crate::ffi::handle::sql_free_handle::<MockLongDataBackend>(
                HandleType::Stmt as i16,
                stmt,
            );
            let _ = crate::ffi::connect::sql_disconnect::<MockLongDataBackend>(conn);
            let _ = crate::ffi::handle::sql_free_handle::<MockLongDataBackend>(
                HandleType::Dbc as i16,
                conn,
            );
            let _ = crate::ffi::handle::sql_free_handle::<MockLongDataBackend>(
                HandleType::Env as i16,
                env,
            );
        }
    }

    #[test]
    fn describe_param_out_of_range_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);
            let ret = prepare_sql(stmt, "SELECT * FROM t WHERE a = ?");
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut data_type: i16 = 0;
            let mut param_size: ULen = 0;
            let mut decimal_digits: i16 = 0;
            let mut nullable: i16 = 0;
            let ret = sql_describe_param::<MockBackend>(
                stmt,
                2, // only 1 param exists
                &mut data_type,
                &mut param_size,
                &mut decimal_digits,
                &mut nullable,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn describe_param_without_prepare_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let mut data_type: i16 = 0;
            let mut param_size: ULen = 0;
            let mut decimal_digits: i16 = 0;
            let mut nullable: i16 = 0;
            let ret = sql_describe_param::<MockBackend>(
                stmt,
                1,
                &mut data_type,
                &mut param_size,
                &mut decimal_digits,
                &mut nullable,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn describe_param_zero_returns_error() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);
            let ret = prepare_sql(stmt, "SELECT * FROM t WHERE a = ?");
            assert_eq!(ret, SqlReturn::SUCCESS);

            let mut data_type: i16 = 0;
            let mut param_size: ULen = 0;
            let mut decimal_digits: i16 = 0;
            let mut nullable: i16 = 0;
            let ret = sql_describe_param::<MockBackend>(
                stmt,
                0, // invalid
                &mut data_type,
                &mut param_size,
                &mut decimal_digits,
                &mut nullable,
            );
            assert_eq!(ret, SqlReturn::ERROR);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn describe_param_null_output_ptrs_succeeds() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = connect_handle(conn);
            assert_eq!(ret, SqlReturn::SUCCESS);
            let ret = prepare_sql(stmt, "SELECT * FROM t WHERE a = ?");
            assert_eq!(ret, SqlReturn::SUCCESS);

            let ret = sql_describe_param::<MockBackend>(
                stmt,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn read_param_value_null_data_indicator() {
        let mut indicator: isize = SQL_NULL_DATA;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::dangling_mut::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::Null);
    }

    #[test]
    fn read_param_value_slong() {
        let mut v: i32 = 42;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: &mut v as *mut i32 as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::I32(42));
    }

    /// An output-only parameter contributes no input value.
    ///
    /// The application binds a buffer for the *driver* to fill; it never had to
    /// put anything in it. Reading it is meaningless for `SQL_C_SLONG` and
    /// unsound for `SQL_C_CHAR` — see the next test.
    #[test]
    fn collect_params_does_not_read_an_output_only_binding() {
        let mut buf: i32 = 1234;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Output,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut buf).cast::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let mut bindings = BoundParams::new();
        bindings.insert(1u16, binding);

        let (params, _) = unsafe { collect_params(bindings.records(), 1) }.unwrap();

        assert_eq!(
            params,
            vec![ColumnValue::Null],
            "the output buffer's contents were sent to the backend as an input value"
        );
    }

    /// An `SQL_PARAM_INPUT_OUTPUT` binding *is* read: it carries a real input
    /// value, and `write_output_params` writes the result back through the same
    /// binding afterwards. The fix for output-only bindings must not catch this
    /// one too.
    #[test]
    fn collect_params_still_reads_an_input_output_binding() {
        let mut buf: i32 = 1234;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::InputOutput,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut buf).cast::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let mut bindings = BoundParams::new();
        bindings.insert(1u16, binding);

        let (params, _) = unsafe { collect_params(bindings.records(), 1) }.unwrap();

        assert_eq!(params, vec![ColumnValue::I32(1234)]);
    }

    /// Build an `SQL_C_CHAR` input binding over `text`.
    ///
    /// `col_size` is 0 — no declared size — because these tests are about the
    /// declared *type*. `crate::param_convert`'s own tests cover the declared
    /// size, and a `col_size` invented here would silently size-check them.
    fn char_binding(text: &'static [u8], sql_type: SqlDataType) -> BoundParam {
        BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::Char,
            sql_type,
            col_size: 0,
            decimal_digits: 0,
            value_ptr: text.as_ptr().cast_mut().cast::<c_void>(),
            buffer_length: text.len() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        }
    }

    /// The reported defect, at the boundary it is observed from: pyodbc binds
    /// a `Decimal` as `SQL_C_CHAR` + `SQL_NUMERIC`, and reading only the C type
    /// hands the backend a string. A backend that renders its parameters then
    /// emits `WHERE col_decimal = '12.34'`, which a typed data source rejects.
    #[test]
    fn read_param_value_converts_char_to_the_declared_decimal_type() {
        let binding = char_binding(b"12.34\0", SqlDataType::NUMERIC);

        let val = unsafe { read_bound_param(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::Decimal("12.34".to_string()));
    }

    #[test]
    fn read_param_value_converts_wchar_to_the_declared_decimal_type() {
        let units: Vec<u16> = "12.34".encode_utf16().collect();
        let mut indicator: isize = (units.len() * 2) as isize;
        let binding = BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::WChar,
            sql_type: SqlDataType::DECIMAL,
            col_size: 5,
            decimal_digits: 2,
            value_ptr: units.as_ptr().cast_mut().cast::<c_void>(),
            buffer_length: (units.len() * 2) as isize,
            str_len_or_ind_ptr: &mut indicator,
        };

        let val = unsafe { read_bound_param(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::Decimal("12.34".to_string()));
    }

    #[test]
    fn read_param_value_converts_char_to_the_declared_integer_type() {
        let binding = char_binding(b"42\0", SqlDataType::INTEGER);

        let val = unsafe { read_bound_param(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::I32(42));
    }

    /// A character SQL type is still a string. The declared type is consulted,
    /// not overridden.
    #[test]
    fn read_param_value_leaves_a_char_parameter_for_a_varchar_column_alone() {
        let binding = char_binding(b"hello\0", SqlDataType::VARCHAR);

        let val = unsafe { read_bound_param(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::String("hello".to_string()));
    }

    /// Text that is not a literal of the declared type is 22018, not something
    /// quietly forwarded for the data source to choke on.
    #[test]
    fn read_param_value_reports_22018_for_text_that_is_not_a_decimal() {
        let binding = char_binding(b"twelve\0", SqlDataType::DECIMAL);

        let err = unsafe { read_bound_param(&binding) }
            .expect_err("non-numeric text was accepted for a DECIMAL parameter");

        assert_eq!(err.sqlstate().as_str(), "22018");
    }

    /// **Inverted, deliberately.** This test used to assert that a binding
    /// whose C type already fixes the value's shape is untouched, on the
    /// premise that "the declared SQL type only decides how *text* is read".
    /// The *C to SQL: Numeric* table is exactly the repeal of that premise: its
    /// third row converts an integer source to a `DECIMAL` target, and the
    /// declared type reaches the backend as the value's shape rather than being
    /// discarded.
    ///
    /// Kept rather than deleted so the change of contract is visible in the
    /// history at the point it happened.
    #[test]
    fn read_param_value_converts_an_integer_to_the_declared_decimal_type() {
        let mut v: i32 = 42;
        let binding = BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::SLong,
            sql_type: SqlDataType::DECIMAL,
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut v).cast::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };

        let val = unsafe { read_bound_param(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::Decimal("42".to_owned()));
    }

    /// The declared type still has to be *honoured*, not merely applied: an
    /// integer target keeps its own width.
    #[test]
    fn read_param_value_keeps_an_integer_target_at_its_own_width() {
        let mut v: i32 = 42;
        let binding = BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::SLong,
            sql_type: SqlDataType::SMALLINT,
            col_size: 0,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut v).cast::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };

        assert_eq!(
            unsafe { read_bound_param(&binding) }.unwrap(),
            ColumnValue::I16(42)
        );
    }

    /// The sign-wrap this rework removes. `SQL_C_UBIGINT` was read as a `u64`
    /// and cast to `i64`, so every value above `i64::MAX` reached the data
    /// source negative. Reading through `i128` there is no cast that can wrap.
    #[test]
    fn a_large_unsigned_bigint_parameter_no_longer_wraps_negative() {
        let mut v: u64 = u64::MAX;
        let binding = BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::UBigInt,
            sql_type: SqlDataType::DECIMAL,
            col_size: 0,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut v).cast::<c_void>(),
            buffer_length: 8,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };

        assert_eq!(
            unsafe { read_bound_param(&binding) }.unwrap(),
            ColumnValue::Decimal(u64::MAX.to_string())
        );
    }

    /// The same wrap in the two narrower unsigned types.
    #[test]
    fn large_unsigned_short_and_tinyint_parameters_no_longer_wrap_negative() {
        let mut short: u16 = u16::MAX;
        let binding = BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::UShort,
            sql_type: SqlDataType::INTEGER,
            col_size: 0,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut short).cast::<c_void>(),
            buffer_length: 2,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        assert_eq!(
            unsafe { read_bound_param(&binding) }.unwrap(),
            ColumnValue::I32(65535)
        );

        let mut tiny: u8 = 200;
        let binding = BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::UTinyInt,
            sql_type: SqlDataType::SMALLINT,
            col_size: 0,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut tiny).cast::<c_void>(),
            buffer_length: 1,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        assert_eq!(
            unsafe { read_bound_param(&binding) }.unwrap(),
            ColumnValue::I16(200)
        );
    }

    /// Footnote [b] end to end through the read path: the value is truncated,
    /// sent, and accompanied by a warning rather than an error.
    #[test]
    fn read_param_value_reports_a_fractional_truncation_as_a_warning() {
        let mut v: f64 = 3.7;
        let binding = BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::Double,
            sql_type: SqlDataType::INTEGER,
            col_size: 0,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut v).cast::<c_void>(),
            buffer_length: 8,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };

        let read = unsafe { read_bound_param_full(&binding) }.unwrap();
        assert_eq!(read.value, ColumnValue::I32(3));
        assert_eq!(
            read.warning.expect("a warning").sqlstate().as_str(),
            "01S07"
        );
    }

    /// The interval row measures against the IPD's
    /// `SQL_DESC_DATETIME_INTERVAL_PRECISION`, so the read path has to hand
    /// that field over rather than a zero. Added because a mutation check found
    /// nothing pinned it: passing `0` here left every other test green, since
    /// they all reach `numeric_to_sql_type` directly.
    #[test]
    fn read_param_value_applies_the_declared_interval_precision() {
        let mut v: i32 = 100;
        // Built directly rather than through `BoundParam`, because
        // `SQL_DESC_DATETIME_INTERVAL_PRECISION` is an IPD field that only this
        // row reads and threading it through that helper would touch every
        // literal in the module for one test.
        let apd = DescriptorRecord {
            concise_type: CDataType::SLong as i16,
            verbose_type: CDataType::SLong as i16,
            data_ptr: std::ptr::from_mut(&mut v).cast::<c_void>(),
            octet_length: 4,
            indicator_ptr: std::ptr::null_mut(),
            ..Default::default()
        };
        let ipd_with = |precision: i32| DescriptorRecord {
            concise_type: SQL_INTERVAL_YEAR.0,
            verbose_type: SQL_INTERVAL_YEAR.0,
            parameter_type: ParamType::Input,
            datetime_interval_precision: precision,
            ..Default::default()
        };

        // A two-digit leading precision admits 0..=99, so 100 overflows it.
        let ipd = ipd_with(2);
        let err = unsafe {
            read_param_value(ParamRecord {
                apd: &apd,
                ipd: &ipd,
            })
        }
        .err()
        .expect("100 does not fit a two-digit leading precision");
        assert_eq!(err.sqlstate().as_str(), "22015");

        // Declaring none disables the check, and the same value converts.
        let ipd = ipd_with(0);
        let read = unsafe {
            read_param_value(ParamRecord {
                apd: &apd,
                ipd: &ipd,
            })
        }
        .expect("an undeclared precision checks nothing");
        assert_eq!(
            read.value,
            ColumnValue::IntervalYearMonth {
                years: 100,
                months: 0
            }
        );
    }

    /// And `collect_params` carries it out to the caller.
    #[test]
    fn collect_params_reports_a_fractional_truncation_as_a_warning() {
        let mut v: f64 = 3.7;
        let mut bindings = BoundParams::new();
        bindings.insert(
            1,
            BoundParam {
                input_output_type: ParamType::Input,
                c_type: CDataType::Double,
                sql_type: SqlDataType::INTEGER,
                col_size: 0,
                decimal_digits: 0,
                value_ptr: std::ptr::from_mut(&mut v).cast::<c_void>(),
                buffer_length: 8,
                str_len_or_ind_ptr: std::ptr::null_mut(),
            },
        );

        let (params, warnings) = unsafe { collect_params(bindings.records(), 1) }
            .expect("truncation is a warning, not an error");
        assert_eq!(params, vec![ColumnValue::I32(3)]);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].sqlstate().as_str(), "01S07");
    }

    /// Data-at-execution delivers the same text by another route, so it gets
    /// the same conversion — otherwise `SQLPutData` becomes a way to smuggle a
    /// decimal to the backend as a string.
    #[test]
    fn dae_buffer_to_value_converts_char_to_the_declared_decimal_type() {
        assert_eq!(
            dae_buffer_to_value(Some(CDataType::Char), SqlDataType::DECIMAL, 0, 0, b"12.34")
                .unwrap(),
            ColumnValue::Decimal("12.34".to_string())
        );
    }

    /// Binary data-at-execution is bytes on the wire; no text conversion
    /// applies to it whatever the declared type says.
    #[test]
    fn dae_buffer_to_value_leaves_binary_alone() {
        assert_eq!(
            dae_buffer_to_value(
                Some(CDataType::Binary),
                SqlDataType::EXT_BINARY,
                0,
                0,
                &[1, 2, 3]
            )
            .unwrap(),
            ColumnValue::Bytes(vec![1, 2, 3])
        );
    }

    /// Spec, `SQLExecute` / `SQLExecDirect` `07002`, first clause, carrying no
    /// `(DM)` marker: "The number of parameters specified in
    /// **SQLBindParameter** was less than the number of parameters in the SQL
    /// statement". Padding the gap with NULL instead runs a statement the
    /// application never asked for — `WHERE x = ?` with nothing bound silently
    /// becomes `WHERE x = NULL`, which matches no row and reports success.
    #[test]
    fn collect_params_rejects_a_marker_with_no_binding() {
        let bindings = BoundParams::new();

        let err = unsafe { collect_params(bindings.records(), 1) }
            .expect_err("an unbound parameter marker was padded with NULL");

        assert_eq!(err.sqlstate().as_str(), "07002");
    }

    /// The gap is reported even when it is not the last parameter, so the
    /// diagnostic names the marker the application actually missed.
    #[test]
    fn collect_params_rejects_a_gap_between_bound_markers() {
        let mut v: i32 = 7;
        let binding = BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::SLong,
            sql_type: SqlDataType::INTEGER,
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut v).cast::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let mut bindings = BoundParams::new();
        bindings.insert(1u16, binding);

        let err = unsafe { collect_params(bindings.records(), 2) }
            .expect_err("a gap after the last bound parameter was padded with NULL");

        assert_eq!(err.sqlstate().as_str(), "07002");
        assert!(
            err.to_string().contains('2'),
            "the diagnostic should name the unbound parameter number, got {err}"
        );
    }

    /// The data-at-execution route walks the same `1..=param_count` range and
    /// must reject the same gap, or it becomes a second way to reach the
    /// NULL padding this fix removes.
    #[test]
    fn find_data_at_exec_params_rejects_a_marker_with_no_binding() {
        let bindings = BoundParams::new();

        let err = unsafe { find_data_at_exec_params(bindings.records(), 1) }
            .expect_err("an unbound parameter marker was padded with NULL");

        assert_eq!(err.sqlstate().as_str(), "07002");
    }

    /// `collect_params` reports 07002 for a marker with no binding. A record
    /// with a null `SQL_DESC_DATA_PTR` is not a binding, so it must report the
    /// same thing rather than reading through the null pointer.
    #[test]
    fn collect_params_treats_a_null_data_pointer_as_unbound() {
        let mut records = BoundParams::new();
        records.insert(1, unbound_record());

        let err = unsafe { collect_params(records.records(), 1) }
            .expect_err("a record with a null data pointer was read as a binding");

        assert_eq!(err.sqlstate().as_str(), "07002");
    }

    /// A null `ParameterValuePtr` with an indicator of `SQL_NULL_DATA` is a
    /// binding, and the value it binds is NULL.
    ///
    /// Spec, `SQLBindParameter`'s *ParameterValuePtr* argument: "An application
    /// can set the *ParameterValuePtr* argument to a null pointer, as long as
    /// *StrLen_or_IndPtr is SQL_NULL_DATA or SQL_DATA_AT_EXEC." The Driver
    /// Manager's own `HY009` agrees, firing only when *both* pointers are null.
    ///
    /// This is how every client binds a NULL: pyodbc sends
    /// `value_ptr=NULL, ind=SQL_NULL_DATA` for `None`. Reporting `07002` here
    /// made `WHERE col = ?` with a NULL — an ordinary optional BI filter —
    /// impossible to express, and the diagnostic blamed the application for
    /// failing to bind a parameter it had bound.
    #[test]
    fn collect_params_accepts_a_null_data_pointer_with_a_null_data_indicator() {
        let mut indicator: isize = SQL_NULL_DATA;
        let mut records = BoundParams::new();
        records.insert(
            1,
            BoundParam {
                input_output_type: ParamType::Input,
                c_type: CDataType::SLong,
                sql_type: SqlDataType::INTEGER,
                col_size: 10,
                decimal_digits: 0,
                value_ptr: std::ptr::null_mut(),
                buffer_length: 4,
                str_len_or_ind_ptr: std::ptr::from_mut(&mut indicator),
            },
        );

        let (params, _) = unsafe { collect_params(records.records(), 1) }
            .expect("a NULL-valued parameter was reported as unbound");

        assert_eq!(params, vec![ColumnValue::Null]);
    }

    /// The data-at-execution route walks the same `1..=param_count` range, so
    /// it must agree that this is a binding — otherwise a NULL parameter is
    /// accepted by one path and rejected by the other.
    #[test]
    fn find_data_at_exec_params_accepts_a_null_data_pointer_with_an_indicator() {
        let mut indicator: isize = SQL_NULL_DATA;
        let mut records = BoundParams::new();
        records.insert(
            1,
            BoundParam {
                input_output_type: ParamType::Input,
                c_type: CDataType::SLong,
                sql_type: SqlDataType::INTEGER,
                col_size: 10,
                decimal_digits: 0,
                value_ptr: std::ptr::null_mut(),
                buffer_length: 4,
                str_len_or_ind_ptr: std::ptr::from_mut(&mut indicator),
            },
        );

        let (values, dae, _) = unsafe { find_data_at_exec_params(records.records(), 1) }
            .expect("a NULL-valued parameter was reported as unbound");

        assert!(
            dae.is_empty(),
            "SQL_NULL_DATA is not data-at-execution, got {dae:?}"
        );
        assert_eq!(values.get(&1), Some(&ColumnValue::Null));
    }

    /// The data-at-execution route walks the same range and must agree, or it
    /// becomes a second way past the check.
    #[test]
    fn find_data_at_exec_params_treats_a_null_data_pointer_as_unbound() {
        let mut records = BoundParams::new();
        records.insert(1, unbound_record());

        let err = unsafe { find_data_at_exec_params(records.records(), 1) }
            .expect_err("a record with a null data pointer was read as a binding");

        assert_eq!(err.sqlstate().as_str(), "07002");
    }

    /// The output direction: a backend must not write through a record the
    /// application never gave a buffer for.
    ///
    /// The indicator is what makes this observable. `write_column_value`
    /// declines to write through a null target pointer but writes the length
    /// indicator unconditionally, so a record wrongly treated as a binding
    /// reports a length for a value it did not store.
    #[test]
    fn write_output_params_skips_a_null_data_pointer() {
        // Sentinel: no ODBC length is negative, so any write is visible.
        let mut indicator: isize = -99;
        let mut records = BoundParams::new();
        records.insert(
            1,
            BoundParam {
                input_output_type: ParamType::Output,
                str_len_or_ind_ptr: std::ptr::from_mut(&mut indicator),
                ..unbound_record()
            },
        );
        let outputs = [crate::types::OutputParam::new(1, ColumnValue::I32(42))];

        unsafe { write_output_params(records.records(), &outputs) }
            .expect("writing an output value through a null data pointer");

        assert_eq!(
            indicator, -99,
            "an output value was written through a record that is not a binding"
        );
    }

    /// A record that exists but is not a binding: every field set as
    /// `SQLBindParameter` would set it, except the data pointer.
    fn unbound_record() -> BoundParam {
        BoundParam {
            input_output_type: ParamType::Input,
            c_type: CDataType::SLong,
            sql_type: SqlDataType::INTEGER,
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::null_mut(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        }
    }

    /// The unsound case, and the reason this is a fix rather than a tidy-up.
    ///
    /// An output-only `SQL_C_CHAR` buffer with no indicator is read as a
    /// null-terminated C string. The application never wrote a terminator —
    /// it bound the buffer for the driver to fill — so `CStr::from_ptr` walks
    /// off the end looking for one. Here the buffer holds no zero byte at all
    /// and is followed by a guard region that also holds none, so the scan must
    /// leave the allocation to terminate.
    ///
    /// Under Miri this test is the check: reading out of bounds is reported
    /// rather than merely producing a wrong string.
    #[test]
    fn collect_params_does_not_scan_an_uninitialised_output_char_buffer() {
        // No zero byte anywhere, so a terminator scan cannot stop inside it.
        let mut arena = vec![0xAAu8; 64];
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Output,
            c_type: odbc_sys::CDataType::Char,
            sql_type: SqlDataType(12),
            col_size: 8,
            decimal_digits: 0,
            value_ptr: arena.as_mut_ptr().cast::<c_void>(),
            buffer_length: 8,
            // Absent indicator: read_param_value falls back to CStr::from_ptr.
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let mut bindings = BoundParams::new();
        bindings.insert(1u16, binding);

        let (params, _) = unsafe { collect_params(bindings.records(), 1) }.unwrap();

        assert_eq!(params, vec![ColumnValue::Null]);
    }

    #[test]
    fn write_output_params_writes_value_into_bound_output_buffer() {
        // An OUTPUT-bound parameter must have the backend-produced value
        // marshalled back into the application's buffer, the symmetric
        // counterpart of reading input parameters out of it.
        let mut buf: i32 = 0;
        let mut indicator: isize = 0;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Output,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: &mut buf as *mut i32 as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        let mut bindings = BoundParams::new();
        bindings.insert(1u16, binding);
        let outputs = [crate::types::OutputParam::new(1, ColumnValue::I32(42))];

        unsafe { write_output_params(bindings.records(), &outputs).unwrap() };

        assert_eq!(buf, 42, "output value not written back to the bound buffer");
        assert_eq!(indicator, 4, "length indicator not set to the value size");
    }

    #[test]
    fn write_output_params_leaves_input_only_binding_untouched() {
        // A backend must not clobber a buffer the application bound as
        // input-only; write-back is gated on the binding direction.
        let mut buf: i32 = 7;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: &mut buf as *mut i32 as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let mut bindings = BoundParams::new();
        bindings.insert(1u16, binding);
        let outputs = [crate::types::OutputParam::new(1, ColumnValue::I32(42))];

        unsafe { write_output_params(bindings.records(), &outputs).unwrap() };

        assert_eq!(buf, 7, "input-only buffer was overwritten");
    }

    #[test]
    fn write_output_params_ignores_unbound_parameter_number() {
        // An output value for a parameter the application never bound must be
        // skipped, not panic or error.
        let bindings = BoundParams::new();
        let outputs = [crate::types::OutputParam::new(3, ColumnValue::I32(1))];
        unsafe { write_output_params(bindings.records(), &outputs).unwrap() };
    }

    #[test]
    fn read_param_value_double() {
        let mut v: f64 = 1.5_f64;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Double,
            sql_type: SqlDataType(8),
            col_size: 15,
            decimal_digits: 0,
            value_ptr: &mut v as *mut f64 as *mut c_void,
            buffer_length: 8,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert!(matches!(val, ColumnValue::F64(x) if (x - 1.5_f64).abs() < 1e-10));
    }

    #[test]
    fn read_param_value_char_nts() {
        let s = b"hello\0";
        let mut indicator: isize = SQL_NTS as isize;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Char,
            sql_type: SqlDataType(12),
            col_size: 5,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut c_void,
            buffer_length: 6,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hello".to_string()));
    }

    #[test]
    fn read_param_value_wchar() {
        let s: Vec<u16> = "world".encode_utf16().chain(std::iter::once(0)).collect();
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::WChar,
            sql_type: SqlDataType(12),
            col_size: 5,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut c_void,
            buffer_length: (s.len() * 2) as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("world".to_string()));
    }

    #[test]
    fn read_param_value_wchar_nts_indicator() {
        // Buffer: 'h', 'i', 0 (null terminator), 'X' — proves scan stops at null, not at length
        let s: Vec<u16> = vec!['h' as u16, 'i' as u16, 0u16, 'X' as u16];
        let mut indicator: isize = SQL_NTS as isize;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::WChar,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 2,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: (s.len() * 2) as isize,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hi".to_string()));
    }

    #[test]
    fn read_param_value_wchar_explicit_length() {
        // Buffer: 'a', 'b', 'c' — indicator says 4 bytes (2 code units = "ab")
        let s: Vec<u16> = vec!['a' as u16, 'b' as u16, 'c' as u16];
        let mut indicator: isize = 4; // 4 bytes = 2 u16 code units
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::WChar,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 3,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 6,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("ab".to_string()));
    }

    // -----------------------------------------------------------------------
    // The length indicator is application-supplied and may exceed the buffer
    // the application itself bound. `buffer_length` is recorded at
    // SQLBindParameter time and is the driver's own record of how much memory
    // exists, so it bounds the read: an indicator larger than it would build a
    // slice over memory past the buffer and hand it to the backend, which
    // sends it to the data source.
    // -----------------------------------------------------------------------

    #[test]
    fn read_param_value_char_clamps_an_indicator_larger_than_the_bound_buffer() {
        let s = b"hello";
        let mut indicator: isize = 65536;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Char,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 5,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 5,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hello".to_string()));
    }

    #[test]
    fn read_param_value_wchar_clamps_an_indicator_larger_than_the_bound_buffer() {
        let s: Vec<u16> = "hi".encode_utf16().collect();
        let mut indicator: isize = 65536;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::WChar,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 2,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 4, // two UTF-16 code units
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hi".to_string()));
    }

    #[test]
    fn read_param_value_binary_clamps_an_indicator_larger_than_the_bound_buffer() {
        let bytes: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut indicator: isize = 65536;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Binary,
            sql_type: odbc_sys::SqlDataType::EXT_BINARY,
            col_size: 4,
            decimal_digits: 0,
            value_ptr: bytes.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    }

    #[test]
    fn read_param_value_char_ignores_a_zero_buffer_length() {
        // Zero means the application declared no buffer size, so it carries no
        // bound. The indicator remains the only length available.
        let s = b"hello world";
        let mut indicator: isize = 5;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Char,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 11,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 0,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hello".to_string()));
    }

    #[test]
    fn read_param_value_char_explicit_length() {
        let s = b"hello world";
        let mut indicator: isize = 5; // only read "hello"
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Char,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 11,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 11,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hello".to_string()));
    }

    #[test]
    fn read_param_value_type_timestamp() {
        let mut ts = odbc_sys::Timestamp {
            year: 2024,
            month: 1,
            day: 2,
            hour: 10,
            minute: 30,
            second: 15,
            fraction: 123_000_000,
        };
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::TypeTimestamp,
            sql_type: SqlDataType(93),
            col_size: 23,
            decimal_digits: 9,
            value_ptr: &mut ts as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Timestamp>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 2024,
                month: 1,
                day: 2,
                hour: 10,
                minute: 30,
                second: 15,
                fraction: 123_000_000,
            }
        );
    }

    #[test]
    fn read_param_value_deprecated_timestamp() {
        // ODBC 2.x SQL_C_TIMESTAMP (11) uses the same struct as SQL_C_TYPE_TIMESTAMP.
        let mut ts = odbc_sys::Timestamp {
            year: 1999,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            second: 59,
            fraction: 0,
        };
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::TimeStamp,
            sql_type: SqlDataType(93),
            col_size: 23,
            decimal_digits: 0,
            value_ptr: &mut ts as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Timestamp>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(
            val,
            ColumnValue::Timestamp {
                year: 1999,
                month: 12,
                day: 31,
                hour: 23,
                minute: 59,
                second: 59,
                fraction: 0,
            }
        );
    }

    #[test]
    fn read_param_value_type_date() {
        let mut d = odbc_sys::Date {
            year: 2024,
            month: 3,
            day: 15,
        };
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::TypeDate,
            sql_type: SqlDataType(91),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: &mut d as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Date>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(
            val,
            ColumnValue::Date {
                year: 2024,
                month: 3,
                day: 15,
            }
        );
    }

    #[test]
    fn read_param_value_type_time() {
        // SQL_TIME_STRUCT has no fraction field; the fraction is reported as 0.
        let mut t = odbc_sys::Time {
            hour: 14,
            minute: 30,
            second: 5,
        };
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::TypeTime,
            sql_type: SqlDataType(92),
            col_size: 8,
            decimal_digits: 0,
            value_ptr: &mut t as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Time>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(
            val,
            ColumnValue::Time {
                hour: 14,
                minute: 30,
                second: 5,
                fraction: 0,
            }
        );
    }

    #[test]
    fn read_param_value_numeric_negative() {
        // -123.45 with precision 5, scale 2 is stored as the unsigned mantissa
        // 12345 (little-endian) with sign 0 (negative).
        let mut val_bytes = [0u8; 16];
        val_bytes[..16].copy_from_slice(&12_345u128.to_le_bytes());
        let mut num = odbc_sys::Numeric {
            precision: 5,
            scale: 2,
            sign: 0,
            val: val_bytes,
        };
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Numeric,
            sql_type: SqlDataType(2),
            col_size: 5,
            decimal_digits: 2,
            value_ptr: &mut num as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Numeric>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::Decimal("-123.45".to_string()));
    }

    #[test]
    fn read_param_value_numeric_positive_scale_zero() {
        let mut val_bytes = [0u8; 16];
        val_bytes[..16].copy_from_slice(&42u128.to_le_bytes());
        let mut num = odbc_sys::Numeric {
            precision: 2,
            scale: 0,
            sign: 1,
            val: val_bytes,
        };
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Numeric,
            sql_type: SqlDataType(2),
            col_size: 2,
            decimal_digits: 0,
            value_ptr: &mut num as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Numeric>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::Decimal("42".to_string()));
    }

    #[test]
    fn read_param_value_numeric_leading_zero_fraction() {
        // 0.05: mantissa 5, scale 2 -> must zero-pad to "0.05", not ".05".
        let mut val_bytes = [0u8; 16];
        val_bytes[..16].copy_from_slice(&5u128.to_le_bytes());
        let mut num = odbc_sys::Numeric {
            precision: 1,
            scale: 2,
            sign: 1,
            val: val_bytes,
        };
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Numeric,
            sql_type: SqlDataType(2),
            col_size: 1,
            decimal_digits: 2,
            value_ptr: &mut num as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Numeric>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::Decimal("0.05".to_string()));
    }

    #[test]
    fn read_param_value_guid() {
        // Struct fields map to the canonical string-order [u8; 16]:
        // d1/d2/d3 big-endian, d4 verbatim.
        let mut guid = odbc_sys::Guid {
            d1: 0x1234_5678,
            d2: 0x9abc,
            d3: 0xdef0,
            d4: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        };
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Guid,
            sql_type: SqlDataType(-11),
            col_size: 36,
            decimal_digits: 0,
            value_ptr: &mut guid as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Guid>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(
            val,
            ColumnValue::Guid([
                0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                0x77, 0x88,
            ])
        );
    }

    #[test]
    fn is_data_at_exec_recognizes_sql_data_at_exec() {
        assert!(is_data_at_exec(SQL_DATA_AT_EXEC));
    }

    #[test]
    fn is_data_at_exec_recognizes_len_data_at_exec() {
        // SQL_LEN_DATA_AT_EXEC(100) = -(100) + (-100) = -200
        assert!(is_data_at_exec(-200));
        // SQL_LEN_DATA_AT_EXEC(0) = 0 + (-100) = -100
        assert!(is_data_at_exec(SQL_LEN_DATA_AT_EXEC_OFFSET));
    }

    #[test]
    fn is_data_at_exec_rejects_normal_indicators() {
        assert!(!is_data_at_exec(0));
        assert!(!is_data_at_exec(100));
        assert!(!is_data_at_exec(SQL_NULL_DATA)); // -1
        assert!(!is_data_at_exec(SQL_NTS as isize));
    }

    #[test]
    fn param_data_no_dae_state_returns_error() {
        let (env, conn, stmt) = unsafe { alloc_env_conn_stmt() };
        let mut value_ptr: *mut c_void = std::ptr::null_mut();
        let ret = unsafe { sql_param_data::<MockBackend>(stmt, &mut value_ptr) };
        assert_eq!(ret, SqlReturn::ERROR); // HY010
        unsafe { cleanup(env, conn, stmt) };
    }

    /// A stale record from a failed iteration must not be reported by the next
    /// successful one. `SQLParamData` is the data-at-execution loop, so an
    /// uncleared queue is re-reported on every subsequent call.
    #[test]
    fn param_data_clears_diagnostics_from_an_earlier_call() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            // A call with no data-at-execution in progress fails with HY010 and
            // leaves a record.
            let mut ptr: *mut c_void = std::ptr::null_mut();
            let ret = sql_param_data::<MockBackend>(stmt, &mut ptr);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                assert_eq!(h.diagnostics.len(), 1, "precondition: a record is queued");
            });

            // The next call must start from an empty queue rather than appending.
            let ret = sql_param_data::<MockBackend>(stmt, &mut ptr);
            assert_eq!(ret, SqlReturn::ERROR);
            with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |h| {
                assert_eq!(
                    h.diagnostics.len(),
                    1,
                    "the queue must be cleared at entry, not appended to"
                );
            });

            cleanup(env, conn, stmt);
        }
    }

    #[test]
    fn put_data_no_dae_state_returns_error() {
        let (env, conn, stmt) = unsafe { alloc_env_conn_stmt() };
        let data: i32 = 42;
        let ret = unsafe {
            sql_put_data::<MockBackend>(
                stmt,
                &data as *const i32 as *mut c_void,
                std::mem::size_of::<i32>() as isize,
            )
        };
        assert_eq!(ret, SqlReturn::ERROR); // HY010
        unsafe { cleanup(env, conn, stmt) };
    }

    #[test]
    fn put_data_null_handle() {
        let ret =
            unsafe { sql_put_data::<MockBackend>(std::ptr::null_mut(), std::ptr::null_mut(), 0) };
        assert_eq!(ret, SqlReturn::INVALID_HANDLE);
    }

    #[test]
    fn param_data_null_handle() {
        let mut value_ptr: *mut c_void = std::ptr::null_mut();
        let ret = unsafe { sql_param_data::<MockBackend>(std::ptr::null_mut(), &mut value_ptr) };
        assert_eq!(ret, SqlReturn::INVALID_HANDLE);
    }

    #[test]
    fn read_param_value_binary() {
        let bytes: [u8; 4] = [0xDE, 0xAD, 0x00, 0xBE];
        let mut indicator: isize = 4;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Binary,
            sql_type: SqlDataType::EXT_BINARY,
            col_size: 4,
            decimal_digits: 0,
            value_ptr: bytes.as_ptr() as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_bound_param(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::Bytes(vec![0xDE, 0xAD, 0x00, 0xBE]));
    }

    /// "C to SQL: Binary"'s binary row: "Length of data > column length" is
    /// 22001. `read_param_value_binary` above is the accepting half — four
    /// bytes into a `col_size` of 4.
    #[test]
    fn read_param_value_binary_over_the_declared_size_is_22001() {
        let bytes: [u8; 5] = [0xDE, 0xAD, 0x00, 0xBE, 0xEF];
        let mut indicator: isize = 5;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Binary,
            sql_type: SqlDataType::EXT_VAR_BINARY,
            col_size: 4,
            decimal_digits: 0,
            value_ptr: bytes.as_ptr() as *mut c_void,
            buffer_length: 5,
            str_len_or_ind_ptr: &mut indicator,
        };
        let err =
            unsafe { read_bound_param(&binding) }.expect_err("five bytes exceed VARBINARY(4)");
        assert_eq!(err.sqlstate().as_str(), "22001");
    }

    /// Was `read_param_value_binary_ignores_the_declared_size_for_a_non_binary_target`,
    /// which pinned the deferral this change closes: a non-binary target used
    /// to skip the size check and pass raw bytes through. It now converts, so
    /// what is worth pinning is that `ColumnSize` is not consulted for a
    /// non-binary target — its width comes from the SQL type, not the
    /// application's declaration.
    #[test]
    fn read_param_value_binary_takes_its_width_from_the_sql_type_not_column_size() {
        let bytes = 1_i32.to_ne_bytes();
        let mut indicator: isize = 4;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Binary,
            sql_type: SqlDataType::INTEGER,
            col_size: 1,
            decimal_digits: 0,
            value_ptr: bytes.as_ptr() as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        assert_eq!(
            unsafe { read_bound_param(&binding) }.expect("ColumnSize is irrelevant here"),
            ColumnValue::I32(1)
        );
    }

    /// The defect this module's new sibling exists to fix: a SQL_C_BINARY
    /// parameter declared SQL_INTEGER used to reach the backend as raw bytes.
    #[test]
    fn read_param_value_binary_converts_to_the_declared_integer_type() {
        let bytes = (-123_456_i32).to_ne_bytes();
        let mut indicator: isize = 4;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Binary,
            sql_type: SqlDataType::INTEGER,
            col_size: 0,
            decimal_digits: 0,
            value_ptr: bytes.as_ptr() as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        assert_eq!(
            unsafe { read_bound_param(&binding) }.expect("four bytes fit an INTEGER"),
            ColumnValue::I32(-123_456)
        );
    }

    #[test]
    fn read_param_value_binary_at_the_wrong_width_is_22003() {
        let bytes: [u8; 5] = [0; 5];
        let mut indicator: isize = 5;
        let binding = BoundParam {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Binary,
            sql_type: SqlDataType::INTEGER,
            col_size: 0,
            decimal_digits: 0,
            value_ptr: bytes.as_ptr() as *mut c_void,
            buffer_length: 5,
            str_len_or_ind_ptr: &mut indicator,
        };
        let err = unsafe { read_bound_param(&binding) }.expect_err("five bytes are not an INTEGER");
        assert_eq!(err.sqlstate().as_str(), "22003");
    }

    #[test]
    fn dae_buffer_binary_is_bytes() {
        let buf = [0x00u8, 0xFF, 0x10];
        assert_eq!(
            dae_buffer_to_value(
                Some(odbc_sys::CDataType::Binary),
                SqlDataType::EXT_BINARY,
                0,
                0,
                &buf
            )
            .unwrap(),
            ColumnValue::Bytes(vec![0x00, 0xFF, 0x10])
        );
    }

    /// `SQLPutData` is only a different way to hand over the same parameter,
    /// so it must not be a way to reach the backend with the declared size
    /// discarded.
    #[test]
    fn dae_buffer_binary_over_the_declared_size_is_22001() {
        let buf = [0x00u8, 0xFF, 0x10];
        let err = dae_buffer_to_value(
            Some(odbc_sys::CDataType::Binary),
            SqlDataType::EXT_VAR_BINARY,
            2,
            0,
            &buf,
        )
        .expect_err("three bytes exceed VARBINARY(2)");
        assert_eq!(err.sqlstate().as_str(), "22001");
    }

    /// `SQLPutData` is only a different way to hand over the same parameter, so
    /// it must not be a way to reach the backend with the declared type
    /// discarded.
    #[test]
    fn dae_buffer_binary_converts_to_the_declared_integer_type() {
        let buf = 7_i32.to_ne_bytes();
        assert_eq!(
            dae_buffer_to_value(
                Some(odbc_sys::CDataType::Binary),
                SqlDataType::INTEGER,
                0,
                0,
                &buf
            )
            .expect("four bytes fit an INTEGER"),
            ColumnValue::I32(7)
        );
    }

    #[test]
    fn dae_buffer_binary_at_the_declared_size_is_accepted() {
        let buf = [0x00u8, 0xFF, 0x10];
        assert_eq!(
            dae_buffer_to_value(
                Some(odbc_sys::CDataType::Binary),
                SqlDataType::EXT_VAR_BINARY,
                3,
                0,
                &buf
            )
            .expect("three bytes fit VARBINARY(3)"),
            ColumnValue::Bytes(vec![0x00, 0xFF, 0x10])
        );
    }

    #[test]
    fn dae_buffer_wchar_decodes_utf16() {
        let units: Vec<u16> = "hi".encode_utf16().collect();
        let mut buf = Vec::new();
        for u in units {
            buf.extend_from_slice(&u.to_ne_bytes());
        }
        assert_eq!(
            dae_buffer_to_value(
                Some(odbc_sys::CDataType::WChar),
                SqlDataType::VARCHAR,
                0,
                0,
                &buf
            )
            .unwrap(),
            ColumnValue::String("hi".to_string())
        );
    }

    #[test]
    fn dae_buffer_char_is_text() {
        assert_eq!(
            dae_buffer_to_value(
                Some(odbc_sys::CDataType::Char),
                SqlDataType::VARCHAR,
                0,
                0,
                b"abc"
            )
            .unwrap(),
            ColumnValue::String("abc".to_string())
        );
    }
}
