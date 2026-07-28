//! Generic implementations of SQLBindParameter and SQLNumParams.

#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;

use odbc_sys::SqlDataType;

use crate::{
    backend::{Backend, StatementBackend},
    cancel::{reclassify_cancelled, reclassify_cancelled_opt},
    errors::OdbcError,
    handles::{ParameterBinding, StatementHandle},
    panic::panic_safe,
    types::{
        ColumnValue, ParamDescriptor, SQL_DATA_AT_EXEC, SQL_DEFAULT_PARAM_SIZE,
        SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_NTS, SQL_NULL_DATA, SqlReturn, SqlState, ULen,
        c_data_type_from_raw, param_type_from_raw,
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
/// - `01000` General warning — (driver-manager-handled; not returned here)
/// - `07006` Restricted data type attribute violation — detected at execute time by the data
///   source; the binding is stored without validating type compatibility at bind time
/// - `07009` Invalid descriptor index — returned when `parameter_number == 0` (bookmark
///   parameters are not supported)
/// - `HY000` General error — returned for unexpected failures
/// - `HY001` Memory allocation error — (driver-manager-handled; not returned here)
/// - `HY003` Invalid application buffer type — returned when `value_type` is not a valid
///   C data type (`c_data_type_from_raw` returns `None`)
/// - `HY004` Invalid SQL data type — a driver-returned code (the spec does not mark it (DM)):
///   the driver is responsible for rejecting a `parameter_type` that is neither a valid ODBC
///   SQL type nor a driver-specific type it supports. Here `parameter_type` is accepted as-is
///   and any incompatibility surfaces at execute time (`07006`), because the backend exposes no
///   bind-time SQL-type metadata to validate against. Validation is intentionally deferred.
/// - `HY009` Invalid argument value — (driver-manager-handled; not returned here)
/// - `HY010` Function sequence error — (driver-manager-handled; not returned here)
/// - `HY013` Memory management error — (driver-manager-handled; not returned here)
/// - `HY021` Inconsistent descriptor information — (driver-manager-handled; not returned here)
/// - `HY090` Invalid string or buffer length — (driver-manager-handled; not returned here)
/// - `HY104` Invalid precision or scale value — a driver-returned code (the spec does not mark it
///   (DM)): `column_size` and `decimal_digits` are stored verbatim without range validation.
///   Data-source-specific range checking would require backend metadata not available at bind
///   time, so validation is intentionally deferred to execute time.
/// - `HY105` Invalid parameter type — (DM) the spec marks HY105 as DM-only. When the driver
///   receives an unrecognised `input_output_type`, it returns `HY024` (invalid attribute value)
///   instead, since the DM should have rejected it first.
/// - `HY117` Connection is suspended — (driver-manager-handled; not returned here)
/// - `HYC00` Optional feature not implemented — (driver-manager-handled; not returned here)
/// - `HYT01` Connection timeout expired — (driver-manager-handled; not returned here)
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

            // Null value pointer AND null indicator removes the binding.
            if parameter_value_ptr.is_null() && str_len_or_ind_ptr.is_null() {
                stmt.param_bindings.remove(&parameter_number);
            } else {
                stmt.param_bindings.insert(
                    parameter_number,
                    ParameterBinding {
                        input_output_type: io_type,
                        c_type: c_data_type,
                        sql_type,
                        col_size: column_size,
                        decimal_digits,
                        value_ptr: parameter_value_ptr,
                        buffer_length,
                        str_len_or_ind_ptr,
                    },
                );
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
/// - `01000` General warning — (driver-manager-handled; not returned here)
/// - `08S01` Communication link failure — not applicable; parameter count is evaluated
///   locally without a round-trip to the data source
/// - `HY000` General error — returned for unexpected failures
/// - `HY001` Memory allocation error — (driver-manager-handled; not returned here)
/// - HY008: Operation canceled; not returned here. This call makes no fallible backend call —
///   `SQLNumParams` reads the parameter count cached at prepare time — so there is no error for a
///   cancellation to be reported through. The asynchronous clause is inapplicable: core never
///   returns `SQL_STILL_EXECUTING`.
/// - `HY010` Function sequence error — returned when `sql_num_params` is called before
///   `SQLPrepare` or `SQLExecDirect` (i.e., `stmt.param_count` is `None`)
/// - `HY013` Memory management error — (driver-manager-handled; not returned here)
/// - `HY117` Connection is suspended — (driver-manager-handled; not returned here)
/// - `HYT01` Connection timeout expired — (driver-manager-handled; not returned here)
/// - `IM001` Driver does not support this function — (driver-manager-handled; not returned
///   here)
/// - `IM017` Polling disabled in async notification mode — (driver-manager-handled; not
///   returned here)
/// - `IM018` SQLCompleteAsync not called — (driver-manager-handled; not returned here)
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
/// - 01000: General warning — (driver-manager-handled; not returned here).
/// - 07009: Invalid descriptor index — returned when `parameter_number` is 0 or exceeds
///   the number of parameter markers in the prepared statement.
/// - 08S01: Communication link failure — not applicable (no backend query).
/// - HY000: General error — returned for unexpected failures.
/// - HY001: Memory allocation error — not applicable; Rust allocation panics are caught by `panic_safe`.
/// - HY008: Operation canceled. The row's first clause — asynchronous processing, then the
///   function called again — is not applicable: core implements no asynchronous execution and
///   never returns `SQL_STILL_EXECUTING`. The second clause, `SQLCancel` called on the
///   statement "from a different thread in a multithread application", **is returned by this
///   driver**: the row carries no `(DM)` marker, and when a backend call fails with
///   `Backend::is_cancelled` reporting its token signalled, core reports `HY008` in place of
///   the backend's own SQLSTATE.
/// - HY010: Function sequence error — returned when no SQL has been prepared.
/// - HY013: Memory management error — not applicable.
/// - HY117: Connection suspended — (DM) (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired — not applicable (no backend query).
/// - IM001: Driver does not support this function — (DM) (driver-manager-handled; not returned here).
/// - IM017: Polling disabled — (DM) (driver-manager-handled; not returned here).
/// - IM018: SQLCompleteAsync not called — (DM) (driver-manager-handled; not returned here).
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

/// # Safety
///
/// `binding.value_ptr` and `binding.str_len_or_ind_ptr` must point to valid
/// memory of the appropriate type and size.
pub(crate) unsafe fn read_param_value(
    binding: &ParameterBinding,
) -> Result<ColumnValue, OdbcError> {
    use odbc_sys::CDataType;

    // Check indicator for NULL.
    if !binding.str_len_or_ind_ptr.is_null() {
        // SAFETY: str_len_or_ind_ptr is non-null and the caller guarantees it points to a valid isize.
        let indicator = unsafe { std::ptr::read_unaligned(binding.str_len_or_ind_ptr) };
        if indicator == SQL_NULL_DATA {
            return Ok(ColumnValue::Null);
        }
    }

    if binding.value_ptr.is_null() {
        return Ok(ColumnValue::Null);
    }

    Ok(match binding.c_type {
        // SAFETY for all integer/float reads below: value_ptr is non-null (guarded above) and the
        // caller guarantees it points to a valid value of the appropriate C type provided by the
        // ODBC caller via SQLBindParameter. `read_unaligned` tolerates the arbitrary offsets
        // row-wise binding can place an application buffer at, where a plain dereference of a
        // multi-byte type would be UB.
        CDataType::SLong => {
            ColumnValue::I32(unsafe { std::ptr::read_unaligned(binding.value_ptr as *const i32) })
        }
        CDataType::SShort => {
            ColumnValue::I16(unsafe { std::ptr::read_unaligned(binding.value_ptr as *const i16) })
        }
        CDataType::STinyInt => {
            ColumnValue::I8(unsafe { std::ptr::read_unaligned(binding.value_ptr as *const i8) })
        }
        CDataType::SBigInt => {
            ColumnValue::I64(unsafe { std::ptr::read_unaligned(binding.value_ptr as *const i64) })
        }
        CDataType::ULong => {
            ColumnValue::I64(
                unsafe { std::ptr::read_unaligned(binding.value_ptr as *const u32) } as i64,
            )
        }
        CDataType::UShort => {
            ColumnValue::I16(
                unsafe { std::ptr::read_unaligned(binding.value_ptr as *const u16) } as i16,
            )
        }
        CDataType::UTinyInt => {
            ColumnValue::I8(
                unsafe { std::ptr::read_unaligned(binding.value_ptr as *const u8) } as i8,
            )
        }
        CDataType::UBigInt => {
            ColumnValue::I64(
                unsafe { std::ptr::read_unaligned(binding.value_ptr as *const u64) } as i64,
            )
        }
        CDataType::Double => {
            ColumnValue::F64(unsafe { std::ptr::read_unaligned(binding.value_ptr as *const f64) })
        }
        CDataType::Float => {
            ColumnValue::F32(unsafe { std::ptr::read_unaligned(binding.value_ptr as *const f32) })
        }
        CDataType::Bit => ColumnValue::Bool(
            unsafe { std::ptr::read_unaligned(binding.value_ptr as *const u8) } != 0,
        ),
        CDataType::Char => {
            let ptr = binding.value_ptr as *const u8;
            let byte_len = if binding.str_len_or_ind_ptr.is_null() {
                None
            } else {
                // SAFETY: str_len_or_ind_ptr is non-null and the caller guarantees it points
                // to a valid isize provided by the ODBC caller.
                let l = unsafe { std::ptr::read_unaligned(binding.str_len_or_ind_ptr) };
                if l == SQL_NTS as isize || l < 0 {
                    None
                } else {
                    Some(clamp_to_bound_buffer(l as usize, binding.buffer_length))
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
                binding.sql_type,
            );
        }
        CDataType::WChar => {
            let ptr = binding.value_ptr as *const u16;
            let code_units = if binding.str_len_or_ind_ptr.is_null() {
                // Indicator pointer absent: treat as null-terminated (SQL_NTS).
                // Use utf16_to_string which bounds the scan to MAX_NTS_SCAN code units.
                // SAFETY: caller guarantees ptr is a valid, null-terminated UTF-16 string.
                // value_ptr null case is excluded by the guard above; unwrap_or_default is unreachable.
                debug_assert!(!ptr.is_null(), "value_ptr null case excluded above");
                return crate::param_convert::text_to_sql_type(
                    &unsafe { utf16_to_string(ptr, SQL_NTS) }.unwrap_or_default(),
                    binding.sql_type,
                );
            } else {
                // SAFETY: str_len_or_ind_ptr is non-null and the caller guarantees it points
                // to a valid isize provided by the ODBC caller.
                let l = unsafe { std::ptr::read_unaligned(binding.str_len_or_ind_ptr) };
                if l == SQL_NTS as isize || l < 0 {
                    // Null-terminated: delegate to bounded NTS scan helper.
                    // SAFETY: caller guarantees ptr is a valid, null-terminated UTF-16 string.
                    // value_ptr null case is excluded by the guard above; unwrap_or_default is unreachable.
                    debug_assert!(!ptr.is_null(), "value_ptr null case excluded above");
                    return crate::param_convert::text_to_sql_type(
                        &unsafe { utf16_to_string(ptr, SQL_NTS) }.unwrap_or_default(),
                        binding.sql_type,
                    );
                } else {
                    // Explicit byte length: ODBC reports lengths in bytes for WChar.
                    // Clamp before halving, because buffer_length is in bytes too.
                    // Divide by 2 because UTF-16 encodes each code unit as exactly 2 bytes.
                    clamp_to_bound_buffer(l as usize, binding.buffer_length) / 2
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
                binding.sql_type,
            );
        }
        CDataType::Binary => {
            let ptr = binding.value_ptr as *const u8;
            let byte_len = if binding.str_len_or_ind_ptr.is_null() {
                None
            } else {
                // SAFETY: str_len_or_ind_ptr is non-null and the caller guarantees it points
                // to a valid isize provided by the ODBC caller.
                let l = unsafe { std::ptr::read_unaligned(binding.str_len_or_ind_ptr) };
                if l < 0 {
                    None
                } else {
                    Some(clamp_to_bound_buffer(l as usize, binding.buffer_length))
                }
            };
            match byte_len {
                Some(n) => {
                    // SAFETY: value_ptr is non-null (guarded above) and the caller guarantees
                    // it points to at least `n` valid bytes as indicated by str_len_or_ind_ptr.
                    let bytes = unsafe { std::slice::from_raw_parts(ptr, n) };
                    ColumnValue::Bytes(bytes.to_vec())
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
            let ts = unsafe {
                std::ptr::read_unaligned(binding.value_ptr as *const odbc_sys::Timestamp)
            };
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
            let d = unsafe { std::ptr::read_unaligned(binding.value_ptr as *const odbc_sys::Date) };
            ColumnValue::Date {
                year: d.year,
                month: d.month,
                day: d.day,
            }
        }
        CDataType::TypeTime | CDataType::Time => {
            // SQL_TIME_STRUCT carries no fractional seconds; report 0.
            let t = unsafe { std::ptr::read_unaligned(binding.value_ptr as *const odbc_sys::Time) };
            ColumnValue::Time {
                hour: t.hour,
                minute: t.minute,
                second: t.second,
                fraction: 0,
            }
        }
        CDataType::Numeric => {
            let n =
                unsafe { std::ptr::read_unaligned(binding.value_ptr as *const odbc_sys::Numeric) };
            ColumnValue::Decimal(numeric_struct_to_decimal_string(&n))
        }
        CDataType::Guid => {
            let g = unsafe { std::ptr::read_unaligned(binding.value_ptr as *const odbc_sys::Guid) };
            ColumnValue::Guid(guid_struct_to_bytes(&g))
        }
        // Interval and SQL Server extended C types are not marshalled. Emitting
        // NULL loses data silently, so warn rather than accept in silence.
        _ => {
            tracing::warn!(
                c_type = ?binding.c_type,
                "read_param_value: unsupported C data type for input parameter; treating as NULL"
            );
            ColumnValue::Null
        }
    })
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
/// All `ParameterBinding` value and indicator pointers must point to valid memory.
pub(crate) unsafe fn collect_params(
    bindings: &std::collections::HashMap<u16, ParameterBinding>,
    param_count: u16,
) -> Result<Vec<ColumnValue>, OdbcError> {
    use odbc_sys::ParamType;

    let mut params = Vec::with_capacity(param_count as usize);
    for i in 1..=param_count {
        match bindings.get(&i) {
            // `InputOutput` is read: it carries an input value the application
            // did initialise, and `write_output_params` writes the result back
            // through the same binding afterwards.
            Some(binding) if binding.input_output_type != ParamType::Output => {
                // SAFETY: the caller guarantees all ParameterBinding value and indicator
                // pointers in `bindings` point to valid memory of the appropriate type.
                params.push(unsafe { read_param_value(binding) }?);
            }
            Some(_) => params.push(ColumnValue::Null),
            None => return Err(unbound_parameter(i)),
        }
    }
    Ok(params)
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
pub(crate) unsafe fn write_output_params(
    bindings: &std::collections::HashMap<u16, ParameterBinding>,
    output_params: &[crate::types::OutputParam],
) -> Result<(), OdbcError> {
    use odbc_sys::ParamType;

    for out in output_params {
        let Some(binding) = bindings.get(&out.parameter_number) else {
            // The application never bound this parameter; nothing to write into.
            continue;
        };
        if !matches!(
            binding.input_output_type,
            ParamType::Output | ParamType::InputOutput
        ) {
            // Input-only binding: never write back through it.
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
                binding.c_type,
                binding.value_ptr,
                binding.buffer_length,
                binding.str_len_or_ind_ptr,
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
/// is treated as text. The buffer is assumed non-empty (the caller maps an empty
/// buffer to `ColumnValue::Null`).
///
/// Text is then converted to `sql_type` by [`crate::param_convert::text_to_sql_type`],
/// the same way [`read_param_value`] converts a value delivered in one piece.
/// `SQLPutData` is only a different way to hand over the same parameter, so it
/// must not be a way to reach the backend with the declared type discarded.
fn dae_buffer_to_value(
    c_type: Option<odbc_sys::CDataType>,
    sql_type: SqlDataType,
    buffer: &[u8],
) -> Result<ColumnValue, OdbcError> {
    use odbc_sys::CDataType;
    let text = match c_type {
        Some(CDataType::Binary) => return Ok(ColumnValue::Bytes(buffer.to_vec())),
        Some(CDataType::WChar) => {
            let units: Vec<u16> = buffer
                .chunks_exact(2)
                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(buffer).into_owned(),
    };
    crate::param_convert::text_to_sql_type(&text, sql_type)
}

/// Scan bound parameters for data-at-execution indicators.
///
/// Returns `(non_dae_values, dae_param_numbers)`:
/// - `non_dae_values`: HashMap mapping 1-based param number to ColumnValue
///   for parameters that are NOT data-at-execution.
/// - `dae_param_numbers`: Ordered list of 1-based param numbers that ARE
///   data-at-execution, in ascending order.
///
/// # Safety
///
/// All `ParameterBinding` value and indicator pointers must point to valid memory.
pub(crate) unsafe fn find_data_at_exec_params(
    bindings: &std::collections::HashMap<u16, crate::handles::ParameterBinding>,
    param_count: u16,
) -> Result<
    (
        std::collections::HashMap<u16, crate::types::ColumnValue>,
        Vec<u16>,
    ),
    OdbcError,
> {
    let mut non_dae = std::collections::HashMap::new();
    let mut dae_params = Vec::new();

    for i in 1..=param_count {
        if let Some(binding) = bindings.get(&i) {
            let is_dae = if !binding.str_len_or_ind_ptr.is_null() {
                // SAFETY: caller guarantees str_len_or_ind_ptr points to valid memory.
                let indicator = unsafe { std::ptr::read_unaligned(binding.str_len_or_ind_ptr) };
                is_data_at_exec(indicator)
            } else {
                false
            };

            if is_dae {
                dae_params.push(i);
            } else {
                // SAFETY: caller guarantees all binding pointers are valid.
                non_dae.insert(i, unsafe { read_param_value(binding) }?);
            }
        } else {
            // The same 07002 `collect_params` reports. This is the other route
            // to the identical gap, and letting it pad with NULL would make
            // data-at-execution a way around the check.
            return Err(unbound_parameter(i));
        }
    }

    Ok((non_dae, dae_params))
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
/// - 01000: General warning — (driver-manager-handled; not returned here).
/// - 01004: String data, right truncated — not applicable; data is accumulated without
///   truncation.
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
/// - HY009: Invalid use of null pointer — returned when `data_ptr` is null but
///   `str_len_or_ind` is not `SQL_NULL_DATA`.
/// - HY010: Function sequence error — returned when no data-at-execution is in progress
///   (no prior `SQL_NEED_DATA` from `SQLExecute`/`SQLExecDirectW`), or when
///   `SQLParamData` has not yet been called to identify the current parameter.
///   (DM cases for async/NEED_DATA: driver-manager-handled; not returned here.)
/// - HY013: Memory management error — not applicable.
/// - HY019: Non-character and non-binary data sent in pieces — not applicable; we accept
///   all data types in pieces.
/// - HY020: Attempt to concatenate a null value — not applicable; NULL is handled via
///   `SQL_NULL_DATA` indicator.
/// - HY090: Invalid string or buffer length — returned when `str_len_or_ind` is negative
///   and not `SQL_NTS` or `SQL_NULL_DATA`.
/// - HY117: Connection suspended — (driver-manager-handled; not returned here).
/// - HYT01: Connection timeout expired — (driver-manager-handled; not returned here).
/// - IM001: Driver does not support this function — (driver-manager-handled; not returned
///   here).
/// - IM017: Polling disabled in async notification mode — (driver-manager-handled; not
///   returned here).
/// - IM018: SQLCompleteAsync not called — (driver-manager-handled; not returned here).
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
    tracing::debug!(
        "SQLPutData(stmt={:?}, data={:?}, len={})",
        statement_handle,
        data_ptr,
        str_len_or_ind
    );
    // SAFETY: statement_handle is null or a valid StatementHandle<B>; kind and group
    // validated by scope.get inside the closure. data_ptr is checked for null
    // before use and is valid for the specified length per the caller's contract.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
            stmt.diagnostics.clear();

            // Spec HY010: must be in DAE state.
            let dae = stmt.data_at_exec.as_mut().ok_or_else(|| {
                OdbcError::general(
                    "No data-at-execution operation in progress",
                    SqlState::function_sequence_error(),
                )
            })?;

            // Spec HY010: SQLParamData must have been called first to set current_param.
            if dae.current_param.is_none() {
                return Err(OdbcError::general(
                    "SQLParamData must be called before SQLPutData to identify the current parameter",
                    SqlState::function_sequence_error(),
                ));
            }

            // SQL_NULL_DATA: set param to NULL by clearing the buffer.
            if str_len_or_ind == SQL_NULL_DATA {
                dae.buffer.clear();
                return Ok(SqlReturn::SUCCESS);
            }

            // Spec HY009: data_ptr must not be null (unless SQL_NULL_DATA, handled above).
            if data_ptr.is_null() {
                return Err(OdbcError::general(
                    "DataPtr is null",
                    SqlState::invalid_use_of_null_pointer(),
                ));
            }

            // Determine byte count.
            let byte_count = if str_len_or_ind == SQL_NTS as isize {
                // Null-terminated string: scan for null byte.
                // SAFETY: caller guarantees data_ptr is a valid null-terminated C string.
                let cstr = std::ffi::CStr::from_ptr(data_ptr as *const std::ffi::c_char);
                cstr.to_bytes().len()
            } else if str_len_or_ind < 0 {
                // Spec HY090: negative length that's not SQL_NTS or SQL_NULL_DATA.
                return Err(OdbcError::general(
                    format!("Invalid string or buffer length: {str_len_or_ind}"),
                    SqlState::invalid_string_or_buffer_length(),
                ));
            } else {
                str_len_or_ind as usize
            };

            // SAFETY: caller guarantees data_ptr is valid for byte_count bytes.
            let data = std::slice::from_raw_parts(data_ptr as *const u8, byte_count);
            dae.buffer.extend_from_slice(data);

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
/// - 01000: General warning — propagated from backend (during final execution).
/// - 01004: String data, right truncated — propagated from backend.
/// - 07006: Restricted data type attribute violation — propagated from backend.
/// - 08S01: Communication link failure — propagated from backend.
/// - 22001: String data, right truncation — returned here when the accumulated
///   data-at-execution text would be truncated by conversion to the declared SQL type, the
///   same check `SQLExecute` applies to a value delivered in one piece
///   (`crate::param_convert`). Also propagated from backend.
/// - 22003: Numeric value out of range — returned here when that text falls outside the
///   range of the declared numeric type (`crate::param_convert`). Also propagated from
///   backend.
/// - 22007: Invalid datetime format — returned here for a datetime literal with an
///   out-of-range field (`crate::param_convert`). Also propagated from backend.
/// - 22008: Datetime field overflow — returned here when that text carries a datetime
///   component the declared type cannot hold (`crate::param_convert`). Also propagated from
///   backend.
/// - 22012: Division by zero — propagated from backend.
/// - 22015: Interval field overflow — propagated from backend.
/// - 22018: Invalid character value for cast specification — returned here when the
///   accumulated data-at-execution text is not a valid literal of the SQL type declared for
///   the parameter at `SQLBindParameter` (`crate::param_convert`). Also propagated from
///   backend.
/// - 23000: Integrity constraint violation — propagated from backend.
/// - 24000: Invalid cursor state — propagated from backend.
/// - 40001: Serialization failure — propagated from backend.
/// - 40003: Statement completion unknown — propagated from backend.
/// - 42000: Syntax error or access violation — propagated from backend.
/// - 44000: WITH CHECK OPTION violation — propagated from backend.
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
///   progress. (DM cases for async: driver-manager-handled; not returned here.)
/// - HY013: Memory management error — not applicable.
/// - HY090: Invalid string or buffer length — propagated from backend.
/// - HY105: Invalid parameter type — propagated from backend.
/// - HY117: Connection suspended — (driver-manager-handled; not returned here).
/// - HYC00: Optional feature not implemented — propagated from backend.
/// - HYT00: Timeout expired — propagated from backend.
/// - HYT01: Connection timeout expired — propagated from backend.
/// - IM001: Driver does not support this function — (driver-manager-handled; not returned
///   here).
/// - IM017: Polling disabled in async notification mode — (driver-manager-handled; not
///   returned here).
/// - IM018: SQLCompleteAsync not called — (driver-manager-handled; not returned here).
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
    // null before write. Bound parameter buffer pointers in param_bindings were registered
    // via SQLBindParameter under the caller's guarantee that they remain valid.
    let ret = unsafe {
        panic_safe::<B, _>(statement_handle, |scope| {
            let (stmt, conn) = scope.stmt_with_parent::<B>(statement_handle)?;
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

            // If there's a current param being filled, finalize it.
            if let Some(param_num) = dae.current_param.take() {
                let value = if dae.buffer.is_empty() {
                    ColumnValue::Null
                } else {
                    let binding = stmt.param_bindings.get(&param_num);
                    let c_type = binding.map(|b| b.c_type);
                    // An absent binding cannot reach here — `SQLParamData` only
                    // offers a parameter that `find_data_at_exec_params` found
                    // a data-at-execution indicator on, which requires one.
                    let sql_type = binding.map_or(SqlDataType::UNKNOWN_TYPE, |b| b.sql_type);
                    dae_buffer_to_value(c_type, sql_type, &dae.buffer)?
                };
                dae.collected_values.insert(param_num, value);
                dae.buffer.clear();
            }

            // Check if there's another pending parameter.
            if let Some(next_param) = dae.pending_params.pop_front() {
                dae.current_param = Some(next_param);

                // Write the value_ptr from the binding to *value_ptr_ptr so the app
                // can identify which parameter is being requested.
                if !value_ptr_ptr.is_null() {
                    if let Some(binding) = stmt.param_bindings.get(&next_param) {
                        std::ptr::write_unaligned(value_ptr_ptr, binding.value_ptr);
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

            // If statement was closed (e.g. SQLFreeStmt(SQL_CLOSE)), re-prepare.
            if stmt.statement.is_none() {
                let prepared =
                    reclassify_cancelled::<B, _, _>(B::prepare(connection, cancel, &sql), cancel)?;
                stmt.set_prepared_statement(crate::handles::StatementData::Backend(prepared));
            }

            let stmt_data = stmt.statement.as_mut().ok_or_else(|| {
                OdbcError::general("No prepared statement", SqlState::function_sequence_error())
            })?;

            match stmt_data {
                crate::handles::StatementData::Backend(backend_stmt) => {
                    reclassify_cancelled::<B, _, _>(
                        B::execute(connection, cancel, backend_stmt, &params),
                        cancel,
                    )?;
                }
                crate::handles::StatementData::Synthetic(_) => {
                    return Err(OdbcError::general(
                        "Cannot execute a synthetic statement",
                        SqlState::general_error(),
                    ));
                }
            }

            // A cursor is open only if the execution produced columns.
            stmt.cursor_open = stmt
                .statement
                .as_ref()
                .is_some_and(|s| s.column_count() > 0);

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLParamData -> {:?}", ret);
    ret
}

#[cfg(test)]
mod tests {
    use odbc_sys::HandleType;

    use super::*;
    use crate::{
        ffi::{execute::sql_prepare_w, handle::sql_free_handle},
        handles::ConnectionHandle,
        test_utils::{
            MockBackend, MockCancelAwareBackend, MockConnection, MockLongDataBackend,
            alloc_env_conn_stmt, with_handle,
        },
        types::{CDataType, ParamType},
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::dangling_mut::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::Null);
    }

    #[test]
    fn read_param_value_slong() {
        let mut v: i32 = 42;
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: &mut v as *mut i32 as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Output,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut buf).cast::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let mut bindings = std::collections::HashMap::new();
        bindings.insert(1u16, binding);

        let params = unsafe { collect_params(&bindings, 1) }.unwrap();

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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::InputOutput,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut buf).cast::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let mut bindings = std::collections::HashMap::new();
        bindings.insert(1u16, binding);

        let params = unsafe { collect_params(&bindings, 1) }.unwrap();

        assert_eq!(params, vec![ColumnValue::I32(1234)]);
    }

    /// Build an `SQL_C_CHAR` input binding over `text` with an explicit length.
    fn char_binding(text: &'static [u8], sql_type: SqlDataType) -> ParameterBinding {
        ParameterBinding {
            input_output_type: ParamType::Input,
            c_type: CDataType::Char,
            sql_type,
            col_size: text.len() as ULen,
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

        let val = unsafe { read_param_value(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::Decimal("12.34".to_string()));
    }

    #[test]
    fn read_param_value_converts_wchar_to_the_declared_decimal_type() {
        let units: Vec<u16> = "12.34".encode_utf16().collect();
        let mut indicator: isize = (units.len() * 2) as isize;
        let binding = ParameterBinding {
            input_output_type: ParamType::Input,
            c_type: CDataType::WChar,
            sql_type: SqlDataType::DECIMAL,
            col_size: 5,
            decimal_digits: 2,
            value_ptr: units.as_ptr().cast_mut().cast::<c_void>(),
            buffer_length: (units.len() * 2) as isize,
            str_len_or_ind_ptr: &mut indicator,
        };

        let val = unsafe { read_param_value(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::Decimal("12.34".to_string()));
    }

    #[test]
    fn read_param_value_converts_char_to_the_declared_integer_type() {
        let binding = char_binding(b"42\0", SqlDataType::INTEGER);

        let val = unsafe { read_param_value(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::I32(42));
    }

    /// A character SQL type is still a string. The declared type is consulted,
    /// not overridden.
    #[test]
    fn read_param_value_leaves_a_char_parameter_for_a_varchar_column_alone() {
        let binding = char_binding(b"hello\0", SqlDataType::VARCHAR);

        let val = unsafe { read_param_value(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::String("hello".to_string()));
    }

    /// Text that is not a literal of the declared type is 22018, not something
    /// quietly forwarded for the data source to choke on.
    #[test]
    fn read_param_value_reports_22018_for_text_that_is_not_a_decimal() {
        let binding = char_binding(b"twelve\0", SqlDataType::DECIMAL);

        let err = unsafe { read_param_value(&binding) }
            .expect_err("non-numeric text was accepted for a DECIMAL parameter");

        assert_eq!(err.sqlstate().as_str(), "22018");
    }

    /// A binding whose C type already fixes the value's shape is untouched:
    /// the declared SQL type only decides how *text* is read.
    #[test]
    fn read_param_value_ignores_the_declared_type_for_a_non_character_binding() {
        let mut v: i32 = 42;
        let binding = ParameterBinding {
            input_output_type: ParamType::Input,
            c_type: CDataType::SLong,
            sql_type: SqlDataType::DECIMAL,
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut v).cast::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };

        let val = unsafe { read_param_value(&binding) }.unwrap();

        assert_eq!(val, ColumnValue::I32(42));
    }

    /// Data-at-execution delivers the same text by another route, so it gets
    /// the same conversion — otherwise `SQLPutData` becomes a way to smuggle a
    /// decimal to the backend as a string.
    #[test]
    fn dae_buffer_to_value_converts_char_to_the_declared_decimal_type() {
        assert_eq!(
            dae_buffer_to_value(Some(CDataType::Char), SqlDataType::DECIMAL, b"12.34").unwrap(),
            ColumnValue::Decimal("12.34".to_string())
        );
    }

    /// Binary data-at-execution is bytes on the wire; no text conversion
    /// applies to it whatever the declared type says.
    #[test]
    fn dae_buffer_to_value_leaves_binary_alone() {
        assert_eq!(
            dae_buffer_to_value(Some(CDataType::Binary), SqlDataType::EXT_BINARY, &[1, 2, 3])
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
        let bindings = std::collections::HashMap::new();

        let err = unsafe { collect_params(&bindings, 1) }
            .expect_err("an unbound parameter marker was padded with NULL");

        assert_eq!(err.sqlstate().as_str(), "07002");
    }

    /// The gap is reported even when it is not the last parameter, so the
    /// diagnostic names the marker the application actually missed.
    #[test]
    fn collect_params_rejects_a_gap_between_bound_markers() {
        let mut v: i32 = 7;
        let binding = ParameterBinding {
            input_output_type: ParamType::Input,
            c_type: CDataType::SLong,
            sql_type: SqlDataType::INTEGER,
            col_size: 10,
            decimal_digits: 0,
            value_ptr: std::ptr::from_mut(&mut v).cast::<c_void>(),
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let mut bindings = std::collections::HashMap::new();
        bindings.insert(1u16, binding);

        let err = unsafe { collect_params(&bindings, 2) }
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
        let bindings = std::collections::HashMap::new();

        let err = unsafe { find_data_at_exec_params(&bindings, 1) }
            .expect_err("an unbound parameter marker was padded with NULL");

        assert_eq!(err.sqlstate().as_str(), "07002");
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
        let binding = ParameterBinding {
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
        let mut bindings = std::collections::HashMap::new();
        bindings.insert(1u16, binding);

        let params = unsafe { collect_params(&bindings, 1) }.unwrap();

        assert_eq!(params, vec![ColumnValue::Null]);
    }

    #[test]
    fn write_output_params_writes_value_into_bound_output_buffer() {
        // An OUTPUT-bound parameter must have the backend-produced value
        // marshalled back into the application's buffer, the symmetric
        // counterpart of reading input parameters out of it.
        let mut buf: i32 = 0;
        let mut indicator: isize = 0;
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Output,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: &mut buf as *mut i32 as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        let mut bindings = std::collections::HashMap::new();
        bindings.insert(1u16, binding);
        let outputs = [crate::types::OutputParam::new(1, ColumnValue::I32(42))];

        unsafe { write_output_params(&bindings, &outputs).unwrap() };

        assert_eq!(buf, 42, "output value not written back to the bound buffer");
        assert_eq!(indicator, 4, "length indicator not set to the value size");
    }

    #[test]
    fn write_output_params_leaves_input_only_binding_untouched() {
        // A backend must not clobber a buffer the application bound as
        // input-only; write-back is gated on the binding direction.
        let mut buf: i32 = 7;
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::SLong,
            sql_type: SqlDataType(4),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: &mut buf as *mut i32 as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let mut bindings = std::collections::HashMap::new();
        bindings.insert(1u16, binding);
        let outputs = [crate::types::OutputParam::new(1, ColumnValue::I32(42))];

        unsafe { write_output_params(&bindings, &outputs).unwrap() };

        assert_eq!(buf, 7, "input-only buffer was overwritten");
    }

    #[test]
    fn write_output_params_ignores_unbound_parameter_number() {
        // An output value for a parameter the application never bound must be
        // skipped, not panic or error.
        let bindings: std::collections::HashMap<u16, ParameterBinding> =
            std::collections::HashMap::new();
        let outputs = [crate::types::OutputParam::new(3, ColumnValue::I32(1))];
        unsafe { write_output_params(&bindings, &outputs).unwrap() };
    }

    #[test]
    fn read_param_value_double() {
        let mut v: f64 = 1.5_f64;
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Double,
            sql_type: SqlDataType(8),
            col_size: 15,
            decimal_digits: 0,
            value_ptr: &mut v as *mut f64 as *mut c_void,
            buffer_length: 8,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert!(matches!(val, ColumnValue::F64(x) if (x - 1.5_f64).abs() < 1e-10));
    }

    #[test]
    fn read_param_value_char_nts() {
        let s = b"hello\0";
        let mut indicator: isize = SQL_NTS as isize;
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Char,
            sql_type: SqlDataType(12),
            col_size: 5,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut c_void,
            buffer_length: 6,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hello".to_string()));
    }

    #[test]
    fn read_param_value_wchar() {
        let s: Vec<u16> = "world".encode_utf16().chain(std::iter::once(0)).collect();
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::WChar,
            sql_type: SqlDataType(12),
            col_size: 5,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut c_void,
            buffer_length: (s.len() * 2) as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("world".to_string()));
    }

    #[test]
    fn read_param_value_wchar_nts_indicator() {
        // Buffer: 'h', 'i', 0 (null terminator), 'X' — proves scan stops at null, not at length
        let s: Vec<u16> = vec!['h' as u16, 'i' as u16, 0u16, 'X' as u16];
        let mut indicator: isize = SQL_NTS as isize;
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::WChar,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 2,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: (s.len() * 2) as isize,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hi".to_string()));
    }

    #[test]
    fn read_param_value_wchar_explicit_length() {
        // Buffer: 'a', 'b', 'c' — indicator says 4 bytes (2 code units = "ab")
        let s: Vec<u16> = vec!['a' as u16, 'b' as u16, 'c' as u16];
        let mut indicator: isize = 4; // 4 bytes = 2 u16 code units
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::WChar,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 3,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 6,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Char,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 5,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 5,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hello".to_string()));
    }

    #[test]
    fn read_param_value_wchar_clamps_an_indicator_larger_than_the_bound_buffer() {
        let s: Vec<u16> = "hi".encode_utf16().collect();
        let mut indicator: isize = 65536;
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::WChar,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 2,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 4, // two UTF-16 code units
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hi".to_string()));
    }

    #[test]
    fn read_param_value_binary_clamps_an_indicator_larger_than_the_bound_buffer() {
        let bytes: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut indicator: isize = 65536;
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Binary,
            sql_type: odbc_sys::SqlDataType::EXT_BINARY,
            col_size: 4,
            decimal_digits: 0,
            value_ptr: bytes.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    }

    #[test]
    fn read_param_value_char_ignores_a_zero_buffer_length() {
        // Zero means the application declared no buffer size, so it carries no
        // bound. The indicator remains the only length available.
        let s = b"hello world";
        let mut indicator: isize = 5;
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Char,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 11,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 0,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::String("hello".to_string()));
    }

    #[test]
    fn read_param_value_char_explicit_length() {
        let s = b"hello world";
        let mut indicator: isize = 5; // only read "hello"
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Char,
            sql_type: odbc_sys::SqlDataType(12),
            col_size: 11,
            decimal_digits: 0,
            value_ptr: s.as_ptr() as *mut std::ffi::c_void,
            buffer_length: 11,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::TypeTimestamp,
            sql_type: SqlDataType(93),
            col_size: 23,
            decimal_digits: 9,
            value_ptr: &mut ts as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Timestamp>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::TimeStamp,
            sql_type: SqlDataType(93),
            col_size: 23,
            decimal_digits: 0,
            value_ptr: &mut ts as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Timestamp>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::TypeDate,
            sql_type: SqlDataType(91),
            col_size: 10,
            decimal_digits: 0,
            value_ptr: &mut d as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Date>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::TypeTime,
            sql_type: SqlDataType(92),
            col_size: 8,
            decimal_digits: 0,
            value_ptr: &mut t as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Time>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Numeric,
            sql_type: SqlDataType(2),
            col_size: 5,
            decimal_digits: 2,
            value_ptr: &mut num as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Numeric>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Numeric,
            sql_type: SqlDataType(2),
            col_size: 2,
            decimal_digits: 0,
            value_ptr: &mut num as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Numeric>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Numeric,
            sql_type: SqlDataType(2),
            col_size: 1,
            decimal_digits: 2,
            value_ptr: &mut num as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Numeric>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Guid,
            sql_type: SqlDataType(-11),
            col_size: 36,
            decimal_digits: 0,
            value_ptr: &mut guid as *mut _ as *mut c_void,
            buffer_length: std::mem::size_of::<odbc_sys::Guid>() as isize,
            str_len_or_ind_ptr: std::ptr::null_mut(),
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
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
        let binding = ParameterBinding {
            input_output_type: odbc_sys::ParamType::Input,
            c_type: odbc_sys::CDataType::Binary,
            sql_type: SqlDataType::EXT_BINARY,
            col_size: 4,
            decimal_digits: 0,
            value_ptr: bytes.as_ptr() as *mut c_void,
            buffer_length: 4,
            str_len_or_ind_ptr: &mut indicator,
        };
        let val = unsafe { read_param_value(&binding) }.unwrap();
        assert_eq!(val, ColumnValue::Bytes(vec![0xDE, 0xAD, 0x00, 0xBE]));
    }

    #[test]
    fn dae_buffer_binary_is_bytes() {
        let buf = [0x00u8, 0xFF, 0x10];
        assert_eq!(
            dae_buffer_to_value(
                Some(odbc_sys::CDataType::Binary),
                SqlDataType::EXT_BINARY,
                &buf
            )
            .unwrap(),
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
            dae_buffer_to_value(Some(odbc_sys::CDataType::WChar), SqlDataType::VARCHAR, &buf)
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
                b"abc"
            )
            .unwrap(),
            ColumnValue::String("abc".to_string())
        );
    }
}
