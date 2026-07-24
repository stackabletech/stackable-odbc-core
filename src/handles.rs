//! ODBC handle types (environment, connection, statement), their allocation
//! and free routines, and tag validation at the FFI boundary.

use std::ffi::c_void;

use odbc_sys::AttrOdbcVersion;

use crate::backend::{Backend, StatementBackend};
use crate::diagnostics::DiagnosticQueue;
use crate::errors::OdbcError;
use crate::types::{ColumnDescriptor, ColumnValue, ConnectParams, FetchResult, SqlReturn, ULen};
use odbc_sys::{CDataType, ParamType, SqlDataType};

const ENV_TAG: u32 = 0x4F44_4245; // "ODBE"
const DBC_TAG: u32 = 0x4F44_4243; // "ODBC"
const STMT_TAG: u32 = 0x4F44_4253; // "ODBS"
const DESC_TAG: u32 = 0x4F44_4244; // "ODBD"

/// SQL_ATTR_NOSCAN = SQL_NOSCAN_ON: the application asks the driver not to scan
/// SQL for escape sequences.
const SQL_NOSCAN_ON: usize = 1;

/// Minimal descriptor handle.
///
/// The Windows Driver Manager queries `SQLGetStmtAttrW` for descriptor handle
/// attributes (10010–10013) immediately after allocating a statement. If the
/// driver returns NULL or ERROR, the DM's CLI dispatch table pointer stays
/// NULL and all subsequent application-facing calls crash. These handles
/// satisfy that requirement.
#[repr(C)]
pub struct DescriptorHandle {
    header: HandleHeader,
}

impl HasTag for DescriptorHandle {
    const TAG: u32 = DESC_TAG;
    fn invalidate_tag(&mut self) {
        self.header.invalidate();
    }
}

/// First field of every handle struct. The `tag` is a magic constant that
/// identifies the handle type and is checked by [`as_handle_ref`] before
/// casting a raw `*mut c_void` to a typed reference.
///
/// Must be `#[repr(C)]` and the first field so that we can read it from an
/// untyped pointer without knowing the concrete handle type.
#[repr(C)]
pub struct HandleHeader {
    /// Identifies the handle type; zeroed on free to catch use-after-free.
    pub tag: u32,
}

/// A handle type that carries a compile-time tag for FFI validation.
///
/// Every handle stores a [`HandleHeader`] tag as its first field, and
/// `as_handle_ref` checks it against `TAG` before dereferencing a raw pointer
/// from the C boundary.
pub trait HasTag {
    /// The tag value stamped into this handle type's header.
    const TAG: u32;

    /// Zero the handle tag, making subsequent `as_handle_ref` calls return
    /// `INVALID_HANDLE`. Used when freeing handles to prevent use-after-free
    /// if the application holds a stale pointer.
    fn invalidate_tag(&mut self);
}

impl HandleHeader {
    fn invalidate(&mut self) {
        self.tag = 0;
    }
}

/// Top-level ODBC environment handle (`SQL_HANDLE_ENV`).
///
/// Models the ODBC state machine: allocated → version set → connections created.
/// The Driver Manager calls `SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION)` before
/// allocating any connections.
///
/// `connections` tracks child connection handles as raw pointers because ODBC
/// controls the lifecycle (not Rust ownership). The Driver Manager holds these
/// pointers externally and passes them back to us on subsequent calls.
///
/// Must be `#[repr(C)]` so that `HandleHeader` is at offset 0 for tag validation.
#[repr(C)]
pub struct EnvironmentHandle<B: Backend> {
    header: HandleHeader,
    pub odbc_version: AttrOdbcVersion,
    pub connections: Vec<*mut ConnectionHandle<B>>,
    pub diagnostics: DiagnosticQueue,
}

impl<B: Backend> HasTag for EnvironmentHandle<B> {
    const TAG: u32 = ENV_TAG;
    fn invalidate_tag(&mut self) {
        self.header.invalidate();
    }
}

/// ODBC connection handle (`SQL_HANDLE_DBC`).
///
/// Models the connection state machine: allocated → connected → statements created.
/// `connection` is `None` until `SQLDriverConnectW` succeeds, then `Some(B::Connection)`.
/// `SQLDisconnect` sets it back to `None` and frees all child statements.
///
/// `env` is a raw pointer back to the parent environment. Raw because ODBC
/// controls the lifecycle: the environment outlives all its connections.
/// `statements` tracks child statement handles the same way.
///
/// Must be `#[repr(C)]` so that `HandleHeader` is at offset 0 for tag validation.
#[repr(C)]
pub struct ConnectionHandle<B: Backend> {
    header: HandleHeader,
    pub env: *mut EnvironmentHandle<B>,
    pub connection: Option<B::Connection>,
    pub statements: Vec<*mut StatementHandle<B>>,
    pub diagnostics: DiagnosticQueue,
    /// Integer/pointer-valued connection attributes set via `SQLSetConnectAttr`.
    /// Values are stored as `usize` (pointer-sized). Defaults are applied at read time.
    pub attrs: std::collections::HashMap<i32, usize>,
    /// String-valued connection attributes (e.g. `SQL_ATTR_CURRENT_CATALOG`).
    pub attr_strings: std::collections::HashMap<i32, String>,
    /// Accumulated connection string attributes from iterative
    /// `SQLBrowseConnectW` calls. Reset on successful connect or `SQLDisconnect`.
    pub browse_request: Option<ConnectParams>,
}

impl<B: Backend> HasTag for ConnectionHandle<B> {
    const TAG: u32 = DBC_TAG;
    fn invalidate_tag(&mut self) {
        self.header.invalidate();
    }
}

/// Wraps either a real backend statement or a driver-synthesized in-memory
/// result set (e.g. from `SQLGetTypeInfo`). Implements [`StatementBackend`] by
/// delegating to the inner variant.
pub enum StatementData<B: Backend> {
    /// A statement produced by the backend (database query).
    Backend(B::Statement),
    /// A driver-synthesized in-memory result set.
    Synthetic(crate::synthetic::SyntheticStatement),
}

impl<B: Backend> StatementBackend for StatementData<B> {
    fn fetch(&mut self) -> Result<FetchResult, OdbcError> {
        match self {
            StatementData::Backend(s) => s.fetch(),
            StatementData::Synthetic(s) => s.fetch(),
        }
    }

    fn get_data(
        &mut self,
        col: u16,
        target_type: odbc_sys::CDataType,
    ) -> Result<std::borrow::Cow<'_, ColumnValue>, OdbcError> {
        match self {
            StatementData::Backend(s) => s.get_data(col, target_type),
            StatementData::Synthetic(s) => s.get_data(col, target_type),
        }
    }

    fn column_count(&self) -> u16 {
        match self {
            StatementData::Backend(s) => s.column_count(),
            StatementData::Synthetic(s) => s.column_count(),
        }
    }

    fn describe_col(&self, col: u16) -> Result<ColumnDescriptor, OdbcError> {
        match self {
            StatementData::Backend(s) => s.describe_col(col),
            StatementData::Synthetic(s) => s.describe_col(col),
        }
    }

    fn row_count(&self) -> Option<usize> {
        match self {
            StatementData::Backend(s) => s.row_count(),
            StatementData::Synthetic(s) => s.row_count(),
        }
    }

    fn close_cursor(&mut self) {
        match self {
            StatementData::Backend(s) => s.close_cursor(),
            StatementData::Synthetic(s) => s.close_cursor(),
        }
    }
}

/// Column binding information stored by `SQLBindCol`.
#[derive(Debug)]
pub struct ColumnBinding {
    /// Target C data type requested by the application.
    pub target_type: CDataType,
    /// Pointer to the application's data buffer.
    pub target_value_ptr: *mut c_void,
    /// Size of the application's data buffer in bytes.
    pub buffer_length: isize,
    /// Pointer to the length/indicator value.
    pub str_len_or_ind_ptr: *mut isize,
}

// SAFETY: ColumnBinding holds raw pointers that point to application-owned buffers.
// The ODBC contract guarantees these buffers remain valid until the binding is
// changed or the statement is freed.
unsafe impl Send for ColumnBinding {}
unsafe impl Sync for ColumnBinding {}

/// Parameter binding information stored by `SQLBindParameter`.
#[derive(Debug)]
pub struct ParameterBinding {
    /// Whether this is an input, output, or input/output parameter.
    pub input_output_type: ParamType,
    /// The C data type of the value buffer.
    pub c_type: CDataType,
    /// The SQL data type of the parameter.
    pub sql_type: SqlDataType,
    /// Column size (precision for numerics, length for strings).
    pub col_size: ULen,
    /// Decimal digits (scale for numerics).
    pub decimal_digits: i16,
    /// Pointer to the value buffer (may be null for output-only params).
    pub value_ptr: *mut c_void,
    /// Size of the value buffer in bytes.
    pub buffer_length: isize,
    /// Pointer to the length/indicator value. `SQL_NULL_DATA` (-1) signals NULL.
    pub str_len_or_ind_ptr: *mut isize,
}

// SAFETY: ParameterBinding holds raw pointers to application-owned buffers.
// The ODBC contract guarantees these buffers remain valid until the binding is
// changed or the statement is freed.
unsafe impl Send for ParameterBinding {}
unsafe impl Sync for ParameterBinding {}

/// Tracks the state machine for data-at-execution parameter streaming
/// (SQLParamData / SQLPutData).
///
/// Created when SQLExecute/SQLExecDirectW detects parameters with
/// `SQL_DATA_AT_EXEC` indicators. Consumed when all parameters have been
/// supplied and the statement is executed.
pub struct DataAtExecState {
    /// 1-based parameter numbers that still need data, in order.
    pub pending_params: std::collections::VecDeque<u16>,
    /// The parameter currently receiving data via SQLPutData.
    /// `None` before the first SQLParamData call.
    pub current_param: Option<u16>,
    /// Accumulated data chunks for the current parameter.
    pub buffer: Vec<u8>,
    /// Already-collected parameter values (both DAE and non-DAE).
    /// Key is 1-based parameter number.
    pub collected_values: std::collections::HashMap<u16, ColumnValue>,
    /// The SQL text to execute once all DAE params are supplied.
    /// Needed because SQLExecDirectW doesn't store prepared_sql.
    pub sql: String,
}

// SAFETY: DataAtExecState contains no raw pointers — all data is owned.
unsafe impl Send for DataAtExecState {}
unsafe impl Sync for DataAtExecState {}

/// ODBC statement handle (`SQL_HANDLE_STMT`).
///
/// Models the statement state machine: allocated → executed → cursor open → fetching.
/// `statement` is `None` until a query is executed (e.g. `SQLExecDirectW`), then
/// `Some(StatementData)` which implements [`StatementBackend`] for row iteration.
///
/// `conn` is a raw pointer back to the parent connection, used to remove this
/// statement from the parent's list on free.
///
/// Must be `#[repr(C)]` so that `HandleHeader` is at offset 0 for tag validation.
///
/// [`StatementBackend`]: crate::backend::StatementBackend
#[repr(C)]
pub struct StatementHandle<B: Backend> {
    header: HandleHeader,
    pub conn: *mut ConnectionHandle<B>,
    pub statement: Option<StatementData<B>>,
    /// SQL text stored by `SQLPrepareW`, executed by `SQLExecute`.
    pub prepared_sql: Option<String>,
    /// Number of `?` parameter markers counted in `prepared_sql` by `SQLPrepareW`.
    pub param_count: Option<u16>,
    /// Column bindings set by `SQLBindCol`. Key is 1-based column number.
    pub bindings: std::collections::HashMap<u16, ColumnBinding>,
    /// Parameter bindings set by `SQLBindParameter`. Key is 1-based parameter number.
    pub param_bindings: std::collections::HashMap<u16, ParameterBinding>,
    /// Cursor name set by SQLSetCursorNameW or auto-generated by SQLGetCursorNameW.
    pub cursor_name: Option<String>,
    /// Data-at-execution state for SQLParamData/SQLPutData.
    /// `Some` when SQLExecute/SQLExecDirectW returned SQL_NEED_DATA.
    pub data_at_exec: Option<DataAtExecState>,
    pub diagnostics: DiagnosticQueue,
    /// Integer/pointer-valued statement attributes set via `SQLSetStmtAttr`.
    /// Values are stored as `usize` (pointer-sized). Defaults are applied at read time.
    pub attrs: std::collections::HashMap<i32, usize>,
    /// Descriptor handles required by the Windows Driver Manager.
    /// The DM queries these via SQLGetStmtAttrW(10010–10013) after statement
    /// allocation. Without valid handles, the DM crashes.
    pub app_row_desc: Box<DescriptorHandle>,
    pub app_param_desc: Box<DescriptorHandle>,
    pub imp_row_desc: Box<DescriptorHandle>,
    pub imp_param_desc: Box<DescriptorHandle>,
}

impl<B: Backend> HasTag for StatementHandle<B> {
    const TAG: u32 = STMT_TAG;
    fn invalidate_tag(&mut self) {
        self.header.invalidate();
    }
}

impl<B: Backend> StatementHandle<B> {
    /// True when `SQL_ATTR_NOSCAN` is `SQL_NOSCAN_ON` (escape scanning disabled).
    pub fn noscan_enabled(&self) -> bool {
        self.attrs
            .get(&(odbc_sys::StatementAttribute::NoScan as i32))
            .copied()
            .unwrap_or(0)
            == SQL_NOSCAN_ON
    }
}

/// Validate a raw pointer and cast it to a mutable reference of the expected handle type.
///
/// Checks for null and verifies the tag matches before casting.
///
/// # Lifetime
///
/// Returns `&'static mut T` because the true lifetime (until `free_*` is called)
/// cannot be expressed in Rust's type system here: the allocation is managed by
/// raw pointers, not by Rust ownership. Callers must NOT cache or store the
/// returned reference beyond the current FFI function call.
///
/// # Safety
///
/// The caller must ensure the pointer was originally created from a `Box<T>` of the
/// same type and that no other mutable references to the handle exist.
pub unsafe fn as_handle_ref<T: HasTag>(ptr: *mut c_void) -> Result<&'static mut T, OdbcError> {
    if ptr.is_null() {
        return Err(OdbcError::InvalidHandle);
    }
    let header = unsafe { &*(ptr as *const HandleHeader) };
    if header.tag != T::TAG {
        return Err(OdbcError::InvalidHandle);
    }
    Ok(unsafe { &mut *(ptr as *mut T) })
}

/// Allocate a new environment handle and write it to `output`.
///
/// # Safety
///
/// `output` must be a valid, non-null pointer to a `*mut c_void`.
/// The caller (`sql_alloc_handle`) is responsible for validating that `output`
/// is non-null before calling this function.
pub unsafe fn alloc_environment<B: Backend>(output: *mut *mut c_void) -> SqlReturn {
    let handle = Box::new(EnvironmentHandle::<B> {
        header: HandleHeader { tag: ENV_TAG },
        odbc_version: AttrOdbcVersion::Odbc3,
        connections: Vec::new(),
        diagnostics: DiagnosticQueue::new(),
    });
    let ptr = Box::into_raw(handle);
    unsafe {
        *output = ptr as *mut c_void;
    }
    SqlReturn::SUCCESS
}

/// Allocate a new connection handle, register it with the parent environment,
/// and write it to `output`.
///
/// # Safety
///
/// `env_ptr` must point to a valid `EnvironmentHandle<B>`. `output` must be a
/// valid, non-null pointer to a `*mut c_void`.
/// The caller (`sql_alloc_handle`) is responsible for validating that `output`
/// is non-null before calling this function.
pub unsafe fn alloc_connection<B: Backend>(
    env_ptr: *mut c_void,
    output: *mut *mut c_void,
) -> SqlReturn {
    let env = match unsafe { as_handle_ref::<EnvironmentHandle<B>>(env_ptr) } {
        Ok(e) => e,
        Err(_) => return SqlReturn::INVALID_HANDLE,
    };
    let handle = Box::new(ConnectionHandle::<B> {
        header: HandleHeader { tag: DBC_TAG },
        env: env_ptr as *mut EnvironmentHandle<B>,
        connection: None,
        statements: Vec::new(),
        diagnostics: DiagnosticQueue::new(),
        attrs: std::collections::HashMap::new(),
        attr_strings: std::collections::HashMap::new(),
        browse_request: None,
    });
    let ptr = Box::into_raw(handle);
    env.connections.push(ptr);
    unsafe {
        *output = ptr as *mut c_void;
    }
    SqlReturn::SUCCESS
}

/// Allocate a new statement handle, register it with the parent connection,
/// and write it to `output`.
///
/// # Safety
///
/// `conn_ptr` must point to a valid `ConnectionHandle<B>`. `output` must be a
/// valid, non-null pointer to a `*mut c_void`.
/// The caller (`sql_alloc_handle`) is responsible for validating that `output`
/// is non-null before calling this function.
pub unsafe fn alloc_statement<B: Backend>(
    conn_ptr: *mut c_void,
    output: *mut *mut c_void,
) -> SqlReturn {
    let conn = match unsafe { as_handle_ref::<ConnectionHandle<B>>(conn_ptr) } {
        Ok(c) => c,
        Err(_) => return SqlReturn::INVALID_HANDLE,
    };
    // Owned by the statement: dropping the StatementHandle frees them, so no
    // teardown path can forget to -- do not add a manual free in
    // disconnect/free-handle paths.
    let alloc_desc = || {
        Box::new(DescriptorHandle {
            header: HandleHeader { tag: DESC_TAG },
        })
    };
    let handle = Box::new(StatementHandle::<B> {
        header: HandleHeader { tag: STMT_TAG },
        conn: conn_ptr as *mut ConnectionHandle<B>,
        statement: None,
        prepared_sql: None,
        param_count: None,
        bindings: std::collections::HashMap::new(),
        param_bindings: std::collections::HashMap::new(),
        cursor_name: None,
        data_at_exec: None,
        diagnostics: DiagnosticQueue::new(),
        attrs: std::collections::HashMap::new(),
        app_row_desc: alloc_desc(),
        app_param_desc: alloc_desc(),
        imp_row_desc: alloc_desc(),
        imp_param_desc: alloc_desc(),
    });
    let ptr = Box::into_raw(handle);
    conn.statements.push(ptr);
    unsafe {
        *output = ptr as *mut c_void;
    }
    SqlReturn::SUCCESS
}

/// Free an environment handle. Fails with `SqlReturn::ERROR` if there are
/// still active connections.
///
/// # Safety
///
/// `handle` must point to a valid `EnvironmentHandle<B>` previously allocated
/// by [`alloc_environment`].
pub unsafe fn free_environment<B: Backend>(handle: *mut c_void) -> SqlReturn {
    let env = match unsafe { as_handle_ref::<EnvironmentHandle<B>>(handle) } {
        Ok(e) => e,
        Err(_) => return SqlReturn::INVALID_HANDLE,
    };
    if !env.connections.is_empty() {
        env.diagnostics.push(&OdbcError::general(
            "Cannot free environment with active connections",
            crate::types::SqlState::function_sequence_error(),
        ));
        return SqlReturn::ERROR;
    }
    env.invalidate_tag();
    let _ = unsafe { Box::from_raw(handle as *mut EnvironmentHandle<B>) };
    SqlReturn::SUCCESS
}

/// Free a connection handle.
///
/// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlfreehandle-function>
///
/// Fails with `SqlReturn::ERROR` if:
/// - The connection is still open (HY010: SQLDisconnect must be called first)
/// - There are still active statements (HY010)
///
/// Removes itself from the parent environment's connection list on success.
///
/// # Safety
///
/// `handle` must point to a valid `ConnectionHandle<B>` previously allocated
/// by [`alloc_connection`].
pub unsafe fn free_connection<B: Backend>(handle: *mut c_void) -> SqlReturn {
    let conn = match unsafe { as_handle_ref::<ConnectionHandle<B>>(handle) } {
        Ok(c) => c,
        Err(_) => return SqlReturn::INVALID_HANDLE,
    };
    // Spec HY010: SQLDisconnect must be called before freeing a connection handle.
    if conn.connection.is_some() {
        conn.diagnostics.push(&OdbcError::general(
            "Cannot free connection: still open. Call SQLDisconnect first",
            crate::types::SqlState::function_sequence_error(),
        ));
        return SqlReturn::ERROR;
    }
    // Spec HY010: All statements must be freed first.
    if !conn.statements.is_empty() {
        conn.diagnostics.push(&OdbcError::general(
            "Cannot free connection with active statements",
            crate::types::SqlState::function_sequence_error(),
        ));
        return SqlReturn::ERROR;
    }
    // Remove from parent environment's connection list.
    // Use as_handle_ref for tag validation: if the parent was already freed,
    // we skip removal rather than dereferencing freed memory.
    let env_ptr = conn.env as *mut c_void;
    if let Ok(env) = unsafe { as_handle_ref::<EnvironmentHandle<B>>(env_ptr) } {
        let conn_typed = handle as *mut ConnectionHandle<B>;
        env.connections.retain(|&p| p != conn_typed);
    }
    conn.invalidate_tag();
    let _ = unsafe { Box::from_raw(handle as *mut ConnectionHandle<B>) };
    SqlReturn::SUCCESS
}

/// Free a statement handle. Removes itself from the parent connection's
/// statement list.
///
/// # Safety
///
/// `handle` must point to a valid `StatementHandle<B>` previously allocated
/// by [`alloc_statement`].
pub unsafe fn free_statement<B: Backend>(handle: *mut c_void) -> SqlReturn {
    let stmt = match unsafe { as_handle_ref::<StatementHandle<B>>(handle) } {
        Ok(s) => s,
        Err(_) => return SqlReturn::INVALID_HANDLE,
    };
    // Remove from parent connection's statement list.
    // Use as_handle_ref for tag validation: if the parent was already freed,
    // we skip removal rather than dereferencing freed memory.
    let conn_ptr = stmt.conn as *mut c_void;
    if let Ok(conn) = unsafe { as_handle_ref::<ConnectionHandle<B>>(conn_ptr) } {
        let stmt_typed = handle as *mut StatementHandle<B>;
        conn.statements.retain(|&p| p != stmt_typed);
    }
    // The descriptor handles are owned Boxes; dropping the StatementHandle
    // below frees them.
    stmt.invalidate_tag();
    let _ = unsafe { Box::from_raw(handle as *mut StatementHandle<B>) };
    SqlReturn::SUCCESS
}

/// Try to obtain a mutable reference to the diagnostic queue of any handle type.
///
/// Reads the tag from the header to determine the handle type, then returns
/// the `diagnostics` field. Returns `Err(())` if the pointer is null or the
/// tag is unrecognized.
///
/// This function is generic over `B: Backend` because the connection and
/// statement handle layouts depend on the backend's associated types.
///
/// # Lifetime
///
/// Same caveat as [`as_handle_ref`]: returns `&'static mut` because the true
/// lifetime cannot be expressed. Callers must NOT store the returned reference
/// beyond the current function call.
///
/// # Safety
///
/// `handle` must point to a valid handle previously allocated by one of the
/// `alloc_*` functions.
#[allow(clippy::result_unit_err)]
pub unsafe fn try_get_diagnostic_queue<B: Backend>(
    handle: *mut c_void,
) -> Result<&'static mut DiagnosticQueue, ()> {
    if handle.is_null() {
        return Err(());
    }
    let header = unsafe { &*(handle as *const HandleHeader) };
    match header.tag {
        ENV_TAG => {
            let env = unsafe { &mut *(handle as *mut EnvironmentHandle<B>) };
            Ok(&mut env.diagnostics)
        }
        DBC_TAG => {
            let conn = unsafe { &mut *(handle as *mut ConnectionHandle<B>) };
            Ok(&mut conn.diagnostics)
        }
        STMT_TAG => {
            let stmt = unsafe { &mut *(handle as *mut StatementHandle<B>) };
            Ok(&mut stmt.diagnostics)
        }
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockBackend, MockConnection};

    #[test]
    fn alloc_and_free_environment() {
        unsafe {
            let mut output: *mut c_void = std::ptr::null_mut();
            let result = alloc_environment::<MockBackend>(&mut output as *mut *mut c_void);
            assert_eq!(result, SqlReturn::SUCCESS);
            assert!(!output.is_null());
            let result = free_environment::<MockBackend>(output);
            assert_eq!(result, SqlReturn::SUCCESS);
        }
    }

    #[test]
    fn alloc_connection_requires_environment() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            let mut conn_ptr: *mut c_void = std::ptr::null_mut();
            let result = alloc_connection::<MockBackend>(env_ptr, &mut conn_ptr as *mut _);
            assert_eq!(result, SqlReturn::SUCCESS);
            assert!(!conn_ptr.is_null());

            let _ = free_connection::<MockBackend>(conn_ptr);
            let _ = free_environment::<MockBackend>(env_ptr);
        }
    }

    #[test]
    fn free_env_with_active_connections_fails() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            let mut conn_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_connection::<MockBackend>(env_ptr, &mut conn_ptr as *mut _);

            let result = free_environment::<MockBackend>(env_ptr);
            assert_eq!(result, SqlReturn::ERROR);

            let _ = free_connection::<MockBackend>(conn_ptr);
            let _ = free_environment::<MockBackend>(env_ptr);
        }
    }

    #[test]
    fn null_handle_returns_invalid() {
        let result =
            unsafe { as_handle_ref::<EnvironmentHandle<MockBackend>>(std::ptr::null_mut()) };
        assert!(result.is_err());
    }

    #[test]
    fn alloc_and_free_statement() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            let mut conn_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_connection::<MockBackend>(env_ptr, &mut conn_ptr as *mut _);
            let mut stmt_ptr: *mut c_void = std::ptr::null_mut();
            let result = alloc_statement::<MockBackend>(conn_ptr, &mut stmt_ptr as *mut _);
            assert_eq!(result, SqlReturn::SUCCESS);

            let _ = free_statement::<MockBackend>(stmt_ptr);
            let _ = free_connection::<MockBackend>(conn_ptr);
            let _ = free_environment::<MockBackend>(env_ptr);
        }
    }

    #[test]
    fn free_connection_with_active_statements_fails() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            let mut conn_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_connection::<MockBackend>(env_ptr, &mut conn_ptr as *mut _);
            let mut stmt_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_statement::<MockBackend>(conn_ptr, &mut stmt_ptr as *mut _);

            let result = free_connection::<MockBackend>(conn_ptr);
            assert_eq!(result, SqlReturn::ERROR);

            let _ = free_statement::<MockBackend>(stmt_ptr);
            let _ = free_connection::<MockBackend>(conn_ptr);
            let _ = free_environment::<MockBackend>(env_ptr);
        }
    }

    #[test]
    fn free_connection_while_connected_fails() {
        // Spec HY010: SQLDisconnect must be called before freeing.
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            let mut conn_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_connection::<MockBackend>(env_ptr, &mut conn_ptr as *mut _);

            // Simulate an active connection by setting connection to Some.
            let conn_handle = as_handle_ref::<ConnectionHandle<MockBackend>>(conn_ptr).unwrap();
            conn_handle.connection = Some(MockConnection);

            // Should fail because connection is still open.
            let result = free_connection::<MockBackend>(conn_ptr);
            assert_eq!(result, SqlReturn::ERROR);

            // Clean up: remove connection, then free.
            let conn_handle = as_handle_ref::<ConnectionHandle<MockBackend>>(conn_ptr).unwrap();
            conn_handle.connection = None;
            let _ = free_connection::<MockBackend>(conn_ptr);
            let _ = free_environment::<MockBackend>(env_ptr);
        }
    }

    #[test]
    fn as_handle_ref_rejects_wrong_handle_type() {
        // A valid environment handle presented where a statement is expected
        // must be rejected by the tag check, not silently reinterpreted.
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            let wrong = as_handle_ref::<StatementHandle<MockBackend>>(env_ptr);
            assert!(matches!(wrong, Err(OdbcError::InvalidHandle)));

            let _ = free_environment::<MockBackend>(env_ptr);
        }
    }

    #[test]
    fn as_handle_ref_rejects_invalidated_tag() {
        // Freeing a handle zeroes its tag (see `invalidate_tag`), so a stale
        // pointer to it is rejected rather than dereferenced as a live handle.
        // We invalidate the tag directly to exercise that check without a real
        // use-after-free (which would be undefined behaviour).
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            let env = as_handle_ref::<EnvironmentHandle<MockBackend>>(env_ptr).unwrap();
            env.invalidate_tag();

            let stale = as_handle_ref::<EnvironmentHandle<MockBackend>>(env_ptr);
            assert!(matches!(stale, Err(OdbcError::InvalidHandle)));

            // The tag is now 0, so `free_environment` would also reject it;
            // reclaim the still-live allocation directly to avoid a leak.
            drop(Box::from_raw(
                env_ptr as *mut EnvironmentHandle<MockBackend>,
            ));
        }
    }

    #[test]
    fn free_handle_rejects_wrong_handle_type() {
        // Calling the wrong `free_*` for a handle must not free it: the tag
        // check fails, `INVALID_HANDLE` is returned, and the handle stays valid.
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            // Treat the environment as if it were a statement.
            let result = free_statement::<MockBackend>(env_ptr);
            assert_eq!(result, SqlReturn::INVALID_HANDLE);

            // The environment is untouched and still frees cleanly.
            assert_eq!(free_environment::<MockBackend>(env_ptr), SqlReturn::SUCCESS);
        }
    }
}
