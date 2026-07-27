//! ODBC handle types (environment, connection, statement), their allocation
//! and free routines, and handle validation at the FFI boundary.
//!
//! # Why handles are not pointers
//!
//! A handle arrives from the C boundary as an untrusted `*mut c_void`. The
//! obvious validation — store a magic tag in the allocation and compare it —
//! cannot work, because reading that tag *is* a dereference of the untrusted
//! value. It catches a live handle of the wrong type and nothing else: a freed
//! handle is a use-after-free read, and a value that was never a pointer is an
//! immediate segfault.
//!
//! So a handle is not an address. It is an opaque token pairing a slot index
//! with a generation counter, and validation is a bounds check plus two integer
//! comparisons against a table this crate owns — with no access to
//! application-supplied memory at all. Nothing in ODBC requires otherwise: a
//! `SQLHANDLE` is `void*` to the application, and the Driver Manager only ever
//! hands it back.
//!
//! Freeing bumps the slot's generation, so every outstanding token for that
//! slot is permanently rejected, including after the slot is reused.
//!
//! The one case no scheme can defend is an application freeing a handle on one
//! thread while another is mid-call on it. ODBC forbids that and the Driver
//! Manager serialises calls per handle.

use std::ffi::c_void;

use odbc_sys::AttrOdbcVersion;

use crate::backend::{Backend, StatementBackend};
use crate::diagnostics::DiagnosticQueue;
use crate::errors::{IntoOdbc, OdbcError};
use crate::sync::Arc;
use crate::types::{ColumnDescriptor, ColumnValue, ConnectParams, FetchResult, SqlReturn, ULen};
use odbc_sys::{CDataType, ParamType, SqlDataType};

pub(crate) mod registry;

use registry::{GroupLock, HandleKind, encode_token, registry};

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

impl HasKind for DescriptorHandle {
    const KIND: HandleKind = HandleKind::Desc;
}

impl DescriptorHandle {
    /// The token `SQLGetStmtAttrW` hands to the application for this
    /// descriptor.
    ///
    /// Handing out its address instead would give the application a value that
    /// could never be validated, and that would dangle the moment the owning
    /// statement was freed.
    pub fn token(&self) -> *mut c_void {
        self.header.token()
    }
}

/// First field of every handle struct. The `tag` is a magic constant that
/// identifies the handle type and is checked by [`as_handle_ref`] before
/// casting a raw `*mut c_void` to a typed reference.
///
/// Must be `#[repr(C)]` and the first field so that we can read it from an
/// untyped pointer without knowing the concrete handle type.
/// First field of every handle struct.
///
/// Records where the handle sits in the registry so that freeing it does not
/// need an address-to-slot search. It is *not* used to validate anything: the
/// registry is the sole authority, and it is consulted without touching the
/// caller's value. See the module docs.
#[repr(C)]
pub struct HandleHeader {
    slot: u32,
    generation: u32,
}

impl HandleHeader {
    /// Written into a handle before it is registered. Generation 0 is never
    /// issued, so a handle still carrying this has no valid token.
    const PLACEHOLDER: Self = Self {
        slot: 0,
        generation: 0,
    };

    /// The token that was handed to the application for this handle.
    fn token(&self) -> *mut c_void {
        encode_token(self.slot as usize, self.generation)
    }
}

/// A handle type that can be looked up in the registry.
pub trait HasKind {
    /// Which kind of ODBC handle this type is.
    const KIND: HandleKind;
}

/// Top-level ODBC environment handle (`SQL_HANDLE_ENV`).
///
/// Models the ODBC state machine: allocated → version set → connections created.
/// The Driver Manager calls `SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION)` before
/// allocating any connections.
///
/// Child connections are not tracked here: the registry records each
/// connection's parent at `register` time, so `Registry::children_of` answers
/// "which connections belong to this environment" without this type carrying
/// a list of its own. A caller that needs the list gets an owned snapshot from
/// the registry, which is what keeps `SQLEndTran(SQL_HANDLE_ENV)` from ever
/// needing to hold this handle's lock and a child connection's at once.
///
/// Must be `#[repr(C)]` so that `HandleHeader` is at offset 0 for tag validation.
#[repr(C)]
pub struct EnvironmentHandle<B: Backend> {
    header: HandleHeader,
    pub odbc_version: AttrOdbcVersion,
    /// No field names `B`, since child handles are tokens rather than typed
    /// pointers, but the struct must stay generic: `as_handle_ref::<T>` keys on
    /// the concrete type, and an environment allocated for one backend must not
    /// resolve as another's.
    _backend: std::marker::PhantomData<fn() -> B>,
    pub diagnostics: DiagnosticQueue,
}

impl<B: Backend> HasKind for EnvironmentHandle<B> {
    const KIND: HandleKind = HandleKind::Env;
}

/// ODBC connection handle (`SQL_HANDLE_DBC`).
///
/// Models the connection state machine: allocated → connected → statements created.
/// `connection` is `None` until `SQLDriverConnectW` succeeds, then `Some(B::Connection)`.
/// `SQLDisconnect` sets it back to `None` and frees all child statements.
///
/// `env` is a raw pointer back to the parent environment. Raw because ODBC
/// controls the lifecycle: the environment outlives all its connections.
///
/// Child statements are not tracked here, for the same reason
/// `EnvironmentHandle` carries no connection list: see its doc comment.
///
/// Must be `#[repr(C)]` so that `HandleHeader` is at offset 0 for tag validation.
#[repr(C)]
pub struct ConnectionHandle<B: Backend> {
    header: HandleHeader,
    pub env: *mut c_void,
    pub connection: Option<B::Connection>,
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

impl<B: Backend> HasKind for ConnectionHandle<B> {
    const KIND: HandleKind = HandleKind::Dbc;
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
    /// `OdbcError`, because this is the point where two different error types
    /// meet: the backend arm carries `<B::Statement as StatementBackend>::Error`
    /// and the synthetic arm carries `OdbcError`. Normalising here is what lets
    /// core hold either kind of statement behind one handle, and costs nothing —
    /// the backend's error converts through the `Into<OdbcError>` bound its own
    /// associated type already carries.
    type Error = OdbcError;

    fn fetch(&mut self) -> Result<FetchResult, OdbcError> {
        match self {
            StatementData::Backend(s) => s.fetch().into_odbc(),
            StatementData::Synthetic(s) => s.fetch(),
        }
    }

    fn get_data(
        &mut self,
        col: u16,
        target_type: odbc_sys::CDataType,
    ) -> Result<std::borrow::Cow<'_, ColumnValue>, OdbcError> {
        match self {
            StatementData::Backend(s) => s.get_data(col, target_type).into_odbc(),
            StatementData::Synthetic(s) => s.get_data(col, target_type),
        }
    }

    fn column_count(&self) -> i16 {
        match self {
            StatementData::Backend(s) => s.column_count(),
            StatementData::Synthetic(s) => s.column_count(),
        }
    }

    fn describe_col(&self, col: u16) -> Result<std::borrow::Cow<'_, ColumnDescriptor>, OdbcError> {
        match self {
            StatementData::Backend(s) => s.describe_col(col).into_odbc(),
            StatementData::Synthetic(s) => s.describe_col(col),
        }
    }

    fn row_count(&self) -> Option<i64> {
        match self {
            StatementData::Backend(s) => s.row_count(),
            StatementData::Synthetic(s) => s.row_count(),
        }
    }

    fn close_cursor(&mut self) -> Result<(), OdbcError> {
        match self {
            StatementData::Backend(s) => s.close_cursor().into_odbc(),
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
///
/// `sql_type`, `col_size` and `decimal_digits` are recorded but not yet read.
/// They are exactly what `SQLDescribeParam` has to report back, and dropping
/// them would mean `SQLBindParameter` discarding the only copy of what the
/// application declared. Kept deliberately, not by oversight.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "recorded for SQLDescribeParam, which does not read them yet"
)]
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
/// `statement` is `None` until the backend produces one — `SQLPrepareW` already
/// stores one, `SQLExecDirectW` and the catalog functions replace it — and then
/// `Some(StatementData)`, which implements [`StatementBackend`] for row iteration.
///
/// `statement` answers "is there a backend statement to operate on?", never "is
/// a cursor open?": a prepared-but-unexecuted statement (ODBC state S2) has a
/// `statement` and no cursor, and `SQLEndTran` under `SQL_CB_CLOSE` closes the
/// cursor while deliberately keeping the statement. [`Self::cursor_open`] is the
/// answer to the second question and is what every `24000` guard tests.
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
    pub conn: *mut c_void,
    pub statement: Option<StatementData<B>>,
    /// Whether a cursor is currently open on this statement (ODBC states
    /// S5-S7). Set when an execution produces a result set, cleared when that
    /// cursor is closed or discarded.
    ///
    /// Distinct from `statement.is_some()`: `SQLPrepareW` stores a backend
    /// statement without opening a cursor, an `UPDATE` executes without
    /// producing a result set, and `SQLEndTran` under `SQL_CB_CLOSE` closes the
    /// cursor but keeps the statement. Every `24000` "cursor already open" /
    /// "no cursor open" check reads this field.
    pub cursor_open: bool,
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

impl<B: Backend> HasKind for StatementHandle<B> {
    const KIND: HandleKind = HandleKind::Stmt;
}

impl<B: Backend> StatementHandle<B> {
    /// Store the result of an execution and open a cursor over it if it has
    /// columns.
    ///
    /// A statement that produced no result set — an `UPDATE`, say — reports
    /// zero columns and leaves the cursor closed, which is exactly the ODBC
    /// distinction between state S4 (executed, no cursor) and S5 (cursor open).
    /// The backend must therefore report [`StatementBackend::column_count`]
    /// accurately as soon as `execute`/`exec_direct` returns; `SQLNumResultCols`
    /// reads the same value at the same point, so this adds no new requirement.
    pub fn set_result_set(&mut self, data: StatementData<B>) {
        self.cursor_open = data.column_count() > 0;
        self.statement = Some(data);
    }

    /// Store a prepared-but-unexecuted backend statement (`SQLPrepareW`, or a
    /// re-prepare before `SQLExecute`). No cursor is open in the prepared
    /// states S2/S3.
    pub fn set_prepared_statement(&mut self, data: StatementData<B>) {
        self.statement = Some(data);
        self.cursor_open = false;
    }

    /// Discard the result set and close the cursor (`SQLCloseCursor`,
    /// `SQLFreeStmt(SQL_CLOSE)`, `SQLEndTran` under `SQL_CB_DELETE`).
    pub fn discard_result_set(&mut self) {
        self.statement = None;
        self.cursor_open = false;
    }

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
pub unsafe fn as_handle_ref<T: HasKind>(token: *mut c_void) -> Result<&'static mut T, OdbcError> {
    let addr = registry()
        .resolve(token, T::KIND)
        .ok_or(OdbcError::InvalidHandle)?;
    // SAFETY: `addr` came out of the registry, so it was produced by
    // `Box::into_raw` in an `alloc_*` function for a handle of exactly `T::KIND`
    // and has not been freed — freeing clears the slot, and the generation
    // check above rejects any token issued before that.
    Ok(unsafe { &mut *(addr as *mut T) })
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
        header: HandleHeader::PLACEHOLDER,
        odbc_version: AttrOdbcVersion::Odbc3,
        diagnostics: DiagnosticQueue::new(),
        _backend: std::marker::PhantomData,
    });
    let ptr = Box::into_raw(handle);
    // SAFETY: `ptr` came from `Box::into_raw` just above and has not been
    // shared, so both the reclaim on failure and the header write are sound.
    match registry().register(HandleKind::Env, ptr as usize, GroupLock::new(), None) {
        Some((token, slot, generation)) => unsafe {
            (*ptr).header = HandleHeader { slot, generation };
            std::ptr::write_unaligned(output, token);
            SqlReturn::SUCCESS
        },
        None => {
            drop(unsafe { Box::from_raw(ptr) });
            SqlReturn::ERROR
        }
    }
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
    // Validates that the environment is live without dereferencing it. There
    // is no list on the environment to push this connection's token onto:
    // `register` below records the parentage the registry needs.
    if registry().group_of(env_ptr).is_none() {
        return SqlReturn::INVALID_HANDLE;
    }
    let handle = Box::new(ConnectionHandle::<B> {
        header: HandleHeader::PLACEHOLDER,
        env: env_ptr,
        connection: None,
        diagnostics: DiagnosticQueue::new(),
        attrs: std::collections::HashMap::new(),
        attr_strings: std::collections::HashMap::new(),
        browse_request: None,
    });
    let ptr = Box::into_raw(handle);
    // SAFETY: as in `alloc_environment`.
    match registry().register(
        HandleKind::Dbc,
        ptr as usize,
        GroupLock::new(),
        Some(env_ptr as usize),
    ) {
        Some((token, slot, generation)) => unsafe {
            (*ptr).header = HandleHeader { slot, generation };
            std::ptr::write_unaligned(output, token);
            SqlReturn::SUCCESS
        },
        None => {
            drop(unsafe { Box::from_raw(ptr) });
            SqlReturn::ERROR
        }
    }
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
    // Statements and their descriptors share the connection's lock. One
    // acquisition then covers a call that touches a statement and its parent.
    // `group_of` both validates that the connection is live and hands back
    // the group to join, without dereferencing the parent.
    let group = match registry().group_of(conn_ptr) {
        Some(g) => g,
        None => return SqlReturn::INVALID_HANDLE,
    };
    // Owned by the statement: dropping the StatementHandle frees them, so no
    // teardown path can forget to -- do not add a manual free in
    // disconnect/free-handle paths.
    // Each descriptor gets its own registry slot: `SQLGetStmtAttrW` hands
    // these out to the application, so they need tokens of their own or the
    // application would receive a raw address it could not be validated from.
    let alloc_desc = || {
        Box::new(DescriptorHandle {
            header: HandleHeader::PLACEHOLDER,
        })
    };
    let handle = Box::new(StatementHandle::<B> {
        header: HandleHeader::PLACEHOLDER,
        conn: conn_ptr,
        statement: None,
        cursor_open: false,
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
    // SAFETY: as in `alloc_environment`.
    let Some((token, slot, generation)) = registry().register(
        HandleKind::Stmt,
        ptr as usize,
        Arc::clone(&group),
        Some(conn_ptr as usize),
    ) else {
        drop(unsafe { Box::from_raw(ptr) });
        return SqlReturn::ERROR;
    };
    unsafe {
        (*ptr).header = HandleHeader { slot, generation };
        // Register the four descriptors now that the statement owns them.
        // Each shares the statement's group and records the statement as its
        // parent.
        for desc in [
            std::ptr::from_mut(&mut *(*ptr).app_row_desc),
            std::ptr::from_mut(&mut *(*ptr).app_param_desc),
            std::ptr::from_mut(&mut *(*ptr).imp_row_desc),
            std::ptr::from_mut(&mut *(*ptr).imp_param_desc),
        ] {
            if let Some((_, dslot, dgen)) = registry().register(
                HandleKind::Desc,
                desc as usize,
                Arc::clone(&group),
                Some(token as usize),
            ) {
                (*desc).header = HandleHeader {
                    slot: dslot,
                    generation: dgen,
                };
            }
        }
        std::ptr::write_unaligned(output, token);
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
    // Spec HY010: "cannot free an environment with active connections." The
    // registry, not a field on this handle, is the source of truth for
    // whether any connection still names it as parent.
    if !registry().children_of(handle).is_empty() {
        env.diagnostics.push(&OdbcError::general(
            "Cannot free environment with active connections",
            crate::types::SqlState::function_sequence_error(),
        ));
        return SqlReturn::ERROR;
    }
    let Some(addr) = registry().unregister(handle, HandleKind::Env) else {
        return SqlReturn::INVALID_HANDLE;
    };
    // SAFETY: `unregister` returned the address this crate registered in
    // `alloc_environment`, and retired the slot, so no other call can obtain it.
    drop(unsafe { Box::from_raw(addr as *mut EnvironmentHandle<B>) });
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
    // Spec HY010: all statements must be freed first. Reading this from the
    // registry rather than a field of this handle means freeing a connection
    // never needs to reach its parent environment at all — the only place
    // in the crate allowed to acquire a connection's lock and an
    // environment's is `SQLEndTran(SQL_HANDLE_ENV)`, and only in that order.
    if !registry().children_of(handle).is_empty() {
        conn.diagnostics.push(&OdbcError::general(
            "Cannot free connection with active statements",
            crate::types::SqlState::function_sequence_error(),
        ));
        return SqlReturn::ERROR;
    }
    let Some(addr) = registry().unregister(handle, HandleKind::Dbc) else {
        return SqlReturn::INVALID_HANDLE;
    };
    // SAFETY: as in `free_environment`.
    drop(unsafe { Box::from_raw(addr as *mut ConnectionHandle<B>) });
    SqlReturn::SUCCESS
}

/// Free a statement handle.
///
/// # Safety
///
/// `handle` must point to a valid `StatementHandle<B>` previously allocated
/// by [`alloc_statement`].
pub unsafe fn free_statement<B: Backend>(handle: *mut c_void) -> SqlReturn {
    // `free_statement_allocation` retires this slot, which is the whole job:
    // parentage lives in the registry, not in a list on the parent
    // connection, so there is nothing else to remove the statement from.
    unsafe { free_statement_allocation::<B>(handle) }
}

/// Retire a statement's registry slot and those of its four descriptors, then
/// drop the allocation.
///
/// Split out so that `SQLDisconnect`, which frees a connection's statements
/// without going through `free_statement`, cannot forget the descriptor slots.
/// Leaking them would be invisible until the registry grew unboundedly.
///
/// # Safety
///
/// `token` must be a live statement handle.
pub(crate) unsafe fn free_statement_allocation<B: Backend>(token: *mut c_void) -> SqlReturn {
    let Some(addr) = registry().unregister(token, HandleKind::Stmt) else {
        return SqlReturn::INVALID_HANDLE;
    };
    // SAFETY: `unregister` returned the address registered in
    // `alloc_statement` and retired the slot.
    let stmt = unsafe { Box::from_raw(addr as *mut StatementHandle<B>) };
    for desc in [
        &stmt.app_row_desc,
        &stmt.app_param_desc,
        &stmt.imp_row_desc,
        &stmt.imp_param_desc,
    ] {
        registry().unregister(desc.header.token(), HandleKind::Desc);
    }
    // `stmt` drops here, taking the descriptor allocations with it.
    SqlReturn::SUCCESS
}

/// Free every statement allocated on a connection, retiring their registry
/// slots and their descriptors'.
///
/// `SQLDisconnect` must free the connection's statements per the spec. This
/// lives here so no caller has to reach into a handle's header to reconstruct
/// its token, and so the descriptor slots cannot be forgotten on that path.
///
/// Takes the connection's token rather than `&mut ConnectionHandle<B>`: the
/// statement list is an owned snapshot from the registry, not a field to
/// drain, so nothing here needs a mutable borrow of the connection at all.
///
/// # Safety
///
/// `conn_token` must be a live connection handle.
pub(crate) unsafe fn free_connection_statements<B: Backend>(conn_token: *mut c_void) {
    for token in registry().children_of(conn_token) {
        // SAFETY: `children_of` returns live tokens registered by
        // `alloc_statement`; a token freed between the snapshot and here is
        // rejected by `unregister` rather than freed twice.
        let _ = unsafe { free_statement_allocation::<B>(token) };
    }
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
    let (kind, addr) = registry().resolve_any(handle).ok_or(())?;
    // SAFETY: the registry produced both the kind and the address, so the cast
    // matches what `alloc_*` allocated and the handle is live.
    match kind {
        HandleKind::Env => Ok(unsafe { &mut (*(addr as *mut EnvironmentHandle<B>)).diagnostics }),
        HandleKind::Dbc => Ok(unsafe { &mut (*(addr as *mut ConnectionHandle<B>)).diagnostics }),
        HandleKind::Stmt => Ok(unsafe { &mut (*(addr as *mut StatementHandle<B>)).diagnostics }),
        // Descriptors carry no diagnostic queue of their own.
        HandleKind::Desc => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockBackend, MockConnection};

    // -----------------------------------------------------------------------
    // Handle validation
    //
    // A handle arrives from the C boundary as an untrusted value. Deciding
    // whether it is valid by *dereferencing* it — which is what reading a tag
    // out of its header does — can only ever catch a wrong-typed pointer that
    // is still live. It cannot catch a freed one, and on a value that was never
    // a pointer at all it is an immediate segfault.
    //
    // These tests state the requirement: validation must reach a verdict
    // without touching the address.
    // -----------------------------------------------------------------------

    #[test]
    fn a_value_that_was_never_a_handle_is_rejected() {
        // Not a pointer at all. Reading a header from it must never be
        // attempted.
        let bogus = 0x1234_usize as *mut c_void;
        let result = unsafe { as_handle_ref::<EnvironmentHandle<MockBackend>>(bogus) };
        assert!(result.is_err(), "a non-handle value must be rejected");
    }

    #[test]
    fn a_freed_handle_is_rejected() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            assert_eq!(free_environment::<MockBackend>(env_ptr), SqlReturn::SUCCESS);

            // The application still holds the old value.
            let result = as_handle_ref::<EnvironmentHandle<MockBackend>>(env_ptr);
            assert!(result.is_err(), "a freed handle must be rejected");
        }
    }

    #[test]
    fn a_freed_handle_is_rejected_even_after_its_slot_is_reused() {
        unsafe {
            let mut first: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut first as *mut _);
            let _ = free_environment::<MockBackend>(first);

            // The next allocation is very likely to take the slot just freed.
            let mut second: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut second as *mut _);

            let stale = as_handle_ref::<EnvironmentHandle<MockBackend>>(first);
            assert!(
                stale.is_err(),
                "the old handle must not be revived by a reused slot"
            );
            assert!(as_handle_ref::<EnvironmentHandle<MockBackend>>(second).is_ok());

            let _ = free_environment::<MockBackend>(second);
        }
    }

    #[test]
    fn freeing_a_handle_twice_is_rejected_the_second_time() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            assert_eq!(free_environment::<MockBackend>(env_ptr), SqlReturn::SUCCESS);
            assert_eq!(
                free_environment::<MockBackend>(env_ptr),
                SqlReturn::INVALID_HANDLE,
                "a double free must be refused, not performed"
            );
        }
    }

    #[test]
    fn a_statement_is_rejected_after_its_connection_frees_it() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            let mut conn_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_connection::<MockBackend>(env_ptr, &mut conn_ptr as *mut _);
            let mut stmt_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_statement::<MockBackend>(conn_ptr, &mut stmt_ptr as *mut _);

            assert!(as_handle_ref::<StatementHandle<MockBackend>>(stmt_ptr).is_ok());

            let _ = free_statement::<MockBackend>(stmt_ptr);
            assert!(
                as_handle_ref::<StatementHandle<MockBackend>>(stmt_ptr).is_err(),
                "a freed statement must be rejected"
            );

            let _ = free_connection::<MockBackend>(conn_ptr);
            let _ = free_environment::<MockBackend>(env_ptr);
        }
    }

    #[test]
    fn a_handle_of_the_wrong_type_is_rejected() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            // A real, live handle — but asked for as the wrong type.
            assert!(as_handle_ref::<ConnectionHandle<MockBackend>>(env_ptr).is_err());
            assert!(as_handle_ref::<StatementHandle<MockBackend>>(env_ptr).is_err());
            assert!(as_handle_ref::<EnvironmentHandle<MockBackend>>(env_ptr).is_ok());

            let _ = free_environment::<MockBackend>(env_ptr);
        }
    }

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

    /// A statement and its four descriptors share the connection's lock group
    /// so that one acquisition covers a call touching either — but a
    /// connection must not share its environment's group, since
    /// `SQLEndTran(SQL_HANDLE_ENV)` is the only place lock nesting is allowed
    /// to happen at all.
    #[test]
    fn alloc_wires_the_group_hierarchy_correctly() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            let mut conn_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_connection::<MockBackend>(env_ptr, &mut conn_ptr as *mut _);
            let mut stmt_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_statement::<MockBackend>(conn_ptr, &mut stmt_ptr as *mut _);

            let env_group = registry().group_of(env_ptr).expect("live");
            let conn_group = registry().group_of(conn_ptr).expect("live");
            let stmt_group = registry().group_of(stmt_ptr).expect("live");

            assert!(
                !Arc::ptr_eq(&env_group, &conn_group),
                "a connection must not share its environment's lock group"
            );
            assert!(
                Arc::ptr_eq(&conn_group, &stmt_group),
                "a statement must share its connection's lock group"
            );

            let stmt = as_handle_ref::<StatementHandle<MockBackend>>(stmt_ptr).expect("live");
            for desc_token in [
                stmt.app_row_desc.token(),
                stmt.app_param_desc.token(),
                stmt.imp_row_desc.token(),
                stmt.imp_param_desc.token(),
            ] {
                let desc_group = registry().group_of(desc_token).expect("live");
                assert!(
                    Arc::ptr_eq(&desc_group, &stmt_group),
                    "each descriptor must share its statement's lock group"
                );
            }

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

    /// Allocates an environment, a connection on it, and a statement on the
    /// connection, wiring all three token levels together for tests that
    /// need the full hierarchy.
    unsafe fn alloc_env_conn_stmt() -> (*mut c_void, *mut c_void, *mut c_void) {
        unsafe {
            let mut env: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env as *mut _);
            let mut conn: *mut c_void = std::ptr::null_mut();
            let _ = alloc_connection::<MockBackend>(env, &mut conn as *mut _);
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let _ = alloc_statement::<MockBackend>(conn, &mut stmt as *mut _);
            (env, conn, stmt)
        }
    }

    /// `SQLEndTran` walks an owned snapshot, so a statement freed mid-walk cannot
    /// shift the sequence under it. When the list was a field of the handle, a
    /// `push` that reallocated or a `retain` that shifted did exactly that.
    #[test]
    fn a_statement_freed_during_iteration_cannot_disturb_the_walk() {
        unsafe {
            let (env, conn, stmt_a) = alloc_env_conn_stmt();
            let mut out: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                alloc_statement::<MockBackend>(conn, &mut out),
                SqlReturn::SUCCESS
            );
            let stmt_b = out;

            let snapshot = registry::registry().children_of(conn);
            assert_eq!(snapshot.len(), 2);

            // Free one while "iterating" the snapshot.
            assert_eq!(free_statement::<MockBackend>(stmt_a), SqlReturn::SUCCESS);

            // The snapshot is unchanged; the registry has moved on.
            assert_eq!(snapshot.len(), 2);
            assert_eq!(registry::registry().children_of(conn), vec![stmt_b]);

            assert_eq!(free_statement::<MockBackend>(stmt_b), SqlReturn::SUCCESS);
            assert_eq!(free_connection::<MockBackend>(conn), SqlReturn::SUCCESS);
            assert_eq!(free_environment::<MockBackend>(env), SqlReturn::SUCCESS);
        }
    }
}
