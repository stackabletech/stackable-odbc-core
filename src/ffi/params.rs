//! Generic implementations of SQLBindParameter and SQLNumParams.

#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;

use odbc_sys::SqlDataType;

use crate::{
    backend::{Backend, StatementBackend},
    errors::{IntoOdbc, OdbcError},
    handles::{ParameterBinding, StatementHandle},
    panic::panic_safe,
    types::{
        ColumnValue, Nullable, SQL_DATA_AT_EXEC, SQL_DEFAULT_PARAM_SIZE,
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
/// - `HY008` Operation canceled — (driver-manager-handled; not returned here)
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
/// - HY008: Operation canceled — (driver-manager-handled; not returned here).
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
            let stmt = scope.get::<StatementHandle<B>>(statement_handle)?;
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

            // Return generic SQL_VARCHAR type info.
            if !data_type_ptr.is_null() {
                std::ptr::write_unaligned(data_type_ptr, SqlDataType::VARCHAR.0);
            }
            if !parameter_size_ptr.is_null() {
                std::ptr::write_unaligned(parameter_size_ptr, SQL_DEFAULT_PARAM_SIZE as ULen);
            }
            if !decimal_digits_ptr.is_null() {
                std::ptr::write_unaligned(decimal_digits_ptr, 0_i16);
            }
            if !nullable_ptr.is_null() {
                std::ptr::write_unaligned(nullable_ptr, Nullable::SqlNullable as i16);
            }

            Ok(SqlReturn::SUCCESS)
        })
    };
    tracing::debug!("SQLDescribeParam -> {:?}", ret);
    ret
}

/// Count `?` parameter markers in an SQL string, ignoring occurrences inside
/// single-quoted string literals.
///
/// Escaped single quotes (`''`) inside a string literal are handled correctly.
pub(crate) fn count_params(sql: &str) -> u16 {
    let mut count = 0u16;
    let mut in_string = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, in_string) {
            ('\'', false) => in_string = true,
            ('\'', true) => {
                // Escaped quote '' stays inside the string; otherwise end of string.
                if chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    in_string = false;
                }
            }
            ('?', false) => count += 1,
            _ => {}
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
pub(crate) unsafe fn read_param_value(binding: &ParameterBinding) -> ColumnValue {
    use odbc_sys::CDataType;

    // Check indicator for NULL.
    if !binding.str_len_or_ind_ptr.is_null() {
        // SAFETY: str_len_or_ind_ptr is non-null and the caller guarantees it points to a valid isize.
        let indicator = unsafe { std::ptr::read_unaligned(binding.str_len_or_ind_ptr) };
        if indicator == SQL_NULL_DATA {
            return ColumnValue::Null;
        }
    }

    if binding.value_ptr.is_null() {
        return ColumnValue::Null;
    }

    match binding.c_type {
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
            ColumnValue::String(String::from_utf8_lossy(bytes).into_owned())
        }
        CDataType::WChar => {
            let ptr = binding.value_ptr as *const u16;
            let code_units = if binding.str_len_or_ind_ptr.is_null() {
                // Indicator pointer absent: treat as null-terminated (SQL_NTS).
                // Use utf16_to_string which bounds the scan to MAX_NTS_SCAN code units.
                // SAFETY: caller guarantees ptr is a valid, null-terminated UTF-16 string.
                // value_ptr null case is excluded by the guard above; unwrap_or_default is unreachable.
                debug_assert!(!ptr.is_null(), "value_ptr null case excluded above");
                return ColumnValue::String(
                    unsafe { utf16_to_string(ptr, SQL_NTS) }.unwrap_or_default(),
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
                    return ColumnValue::String(
                        unsafe { utf16_to_string(ptr, SQL_NTS) }.unwrap_or_default(),
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
            ColumnValue::String(String::from_utf16_lossy(&units))
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
    }
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

/// Collect bound parameter values in order 1..=param_count.
///
/// Unbound parameters are emitted as `ColumnValue::Null`.
///
/// # Safety
///
/// All `ParameterBinding` value and indicator pointers must point to valid memory.
pub(crate) unsafe fn collect_params(
    bindings: &std::collections::HashMap<u16, ParameterBinding>,
    param_count: u16,
) -> Result<Vec<ColumnValue>, OdbcError> {
    let mut params = Vec::with_capacity(param_count as usize);
    for i in 1..=param_count {
        if let Some(binding) = bindings.get(&i) {
            // SAFETY: the caller guarantees all ParameterBinding value and indicator
            // pointers in `bindings` point to valid memory of the appropriate type.
            params.push(unsafe { read_param_value(binding) });
        } else {
            params.push(ColumnValue::Null);
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
/// reads input values *out* of the same bindings.
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
fn dae_buffer_to_value(c_type: Option<odbc_sys::CDataType>, buffer: &[u8]) -> ColumnValue {
    use odbc_sys::CDataType;
    match c_type {
        Some(CDataType::Binary) => ColumnValue::Bytes(buffer.to_vec()),
        Some(CDataType::WChar) => {
            let units: Vec<u16> = buffer
                .chunks_exact(2)
                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                .collect();
            ColumnValue::String(String::from_utf16_lossy(&units))
        }
        _ => ColumnValue::String(String::from_utf8_lossy(buffer).into_owned()),
    }
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
) -> (
    std::collections::HashMap<u16, crate::types::ColumnValue>,
    Vec<u16>,
) {
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
                non_dae.insert(i, unsafe { read_param_value(binding) });
            }
        } else {
            non_dae.insert(i, crate::types::ColumnValue::Null);
        }
    }

    (non_dae, dae_params)
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
/// - 22001: String data, right truncation — propagated from backend.
/// - 22003: Numeric value out of range — propagated from backend.
/// - 22007: Invalid datetime format — propagated from backend.
/// - 22008: Datetime field overflow — propagated from backend.
/// - 22012: Division by zero — propagated from backend.
/// - 22015: Interval field overflow — propagated from backend.
/// - 22018: Invalid character value for cast specification — propagated from backend.
/// - 23000: Integrity constraint violation — propagated from backend.
/// - 24000: Invalid cursor state — propagated from backend.
/// - 40001: Serialization failure — propagated from backend.
/// - 40003: Statement completion unknown — propagated from backend.
/// - 42000: Syntax error or access violation — propagated from backend.
/// - 44000: WITH CHECK OPTION violation — propagated from backend.
/// - HY000: General error — propagated from backend.
/// - HY001: Memory allocation error — not applicable; Rust allocation panics are caught by
///   `panic_safe`.
/// - HY008: Operation canceled — (driver-manager-handled; not returned here).
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
            // Spec: do NOT clear diagnostics; SQLParamData can return diagnostics from the
            // eventual execution.

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
                    let c_type = stmt.param_bindings.get(&param_num).map(|b| b.c_type);
                    dae_buffer_to_value(c_type, &dae.buffer)
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

            // If statement was closed (e.g. SQLFreeStmt(SQL_CLOSE)), re-prepare.
            if stmt.statement.is_none() {
                let prepared = B::prepare(connection, &sql).into_odbc()?;
                stmt.set_prepared_statement(crate::handles::StatementData::Backend(prepared));
            }

            let stmt_data = stmt.statement.as_mut().ok_or_else(|| {
                OdbcError::general("No prepared statement", SqlState::function_sequence_error())
            })?;

            match stmt_data {
                crate::handles::StatementData::Backend(backend_stmt) => {
                    B::execute(connection, backend_stmt, &params).into_odbc()?;
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
        test_utils::{MockBackend, alloc_env_conn_stmt},
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

    #[test]
    fn count_params_no_params() {
        assert_eq!(count_params("SELECT 1"), 0);
    }

    #[test]
    fn count_params_single() {
        assert_eq!(count_params("SELECT * FROM t WHERE id = ?"), 1);
    }

    #[test]
    fn count_params_multiple() {
        assert_eq!(count_params("INSERT INTO t (a, b, c) VALUES (?, ?, ?)"), 3);
    }

    #[test]
    fn count_params_ignores_question_mark_in_string_literal() {
        assert_eq!(count_params("SELECT '?' FROM t WHERE x = ?"), 1);
    }

    #[test]
    fn count_params_handles_escaped_quote() {
        assert_eq!(count_params("SELECT 'it''s?' FROM t WHERE x = ?"), 1);
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
            assert_eq!(nullable, Nullable::SqlNullable as i16);
            cleanup(env, conn, stmt);
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
        assert_eq!(val, ColumnValue::I32(42));
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
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
        let val = unsafe { read_param_value(&binding) };
        assert_eq!(val, ColumnValue::Bytes(vec![0xDE, 0xAD, 0x00, 0xBE]));
    }

    #[test]
    fn dae_buffer_binary_is_bytes() {
        let buf = [0x00u8, 0xFF, 0x10];
        assert_eq!(
            dae_buffer_to_value(Some(odbc_sys::CDataType::Binary), &buf),
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
            dae_buffer_to_value(Some(odbc_sys::CDataType::WChar), &buf),
            ColumnValue::String("hi".to_string())
        );
    }

    #[test]
    fn dae_buffer_char_is_text() {
        assert_eq!(
            dae_buffer_to_value(Some(odbc_sys::CDataType::Char), b"abc"),
            ColumnValue::String("abc".to_string())
        );
    }
}
