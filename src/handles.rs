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
use std::sync::RwLock;

use odbc_sys::AttrOdbcVersion;

use crate::backend::{Backend, StatementBackend};
use crate::diagnostics::DiagnosticQueue;
use crate::errors::OdbcError;
use crate::types::{ColumnDescriptor, ColumnValue, ConnectParams, FetchResult, SqlReturn, ULen};
use odbc_sys::{CDataType, ParamType, SqlDataType};

/// Which kind of ODBC handle a registry slot holds.
///
/// Replaces the old magic tags. The kind lives in the registry rather than in
/// the allocation, so checking it never touches the caller's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    /// `SQL_HANDLE_ENV`
    Env,
    /// `SQL_HANDLE_DBC`
    Dbc,
    /// `SQL_HANDLE_STMT`
    Stmt,
    /// `SQL_HANDLE_DESC`
    Desc,
}

/// Bits of a token given to the slot index; the rest hold the generation.
///
/// On a 64-bit target this is a 32/32 split: four billion concurrent handles
/// and four billion reuses of each. On 32-bit — which ODBC very much still has,
/// since Excel and Access are 32-bit on Windows — it is 16/16, so 65 535
/// concurrent handles and 65 535 reuses per slot. A slot whose generation would
/// wrap is retired rather than reused, which keeps the scheme sound at the cost
/// of the table growing slowly under extreme churn.
const TOKEN_INDEX_BITS: u32 = usize::BITS / 2;

/// Largest slot index a token can encode.
const MAX_SLOT_INDEX: usize = (1usize << TOKEN_INDEX_BITS) - 1;

/// Largest generation a token can encode. A slot reaching this is retired.
const MAX_GENERATION: u32 = MAX_SLOT_INDEX as u32;

/// One entry in the handle registry.
struct Slot {
    /// Incremented on every free. A token carrying a different value is stale.
    generation: u32,
    /// `None` when the slot is free and available for reuse.
    kind: Option<HandleKind>,
    /// Address of the `Box`-allocated handle this crate owns.
    ///
    /// Stored as `usize` rather than a raw pointer so `Slot` stays `Send`;
    /// it is only ever produced by `Box::into_raw` in an `alloc_*` function.
    addr: usize,
}

/// The live-handle table.
///
/// Read on every FFI call and written only when a handle is allocated or freed,
/// so it is read-mostly by a wide margin.
static REGISTRY: RwLock<Vec<Slot>> = RwLock::new(Vec::new());

/// Encode a slot index and generation as the opaque value handed to the
/// application.
///
/// Generations start at 1, so a valid token is never zero and stays
/// distinguishable from `SQL_NULL_HANDLE`.
fn encode_token(index: usize, generation: u32) -> *mut c_void {
    (((generation as usize) << TOKEN_INDEX_BITS) | index) as *mut c_void
}

/// Split a token back into its slot index and generation.
fn decode_token(token: *mut c_void) -> (usize, u32) {
    let raw = token as usize;
    (raw & MAX_SLOT_INDEX, (raw >> TOKEN_INDEX_BITS) as u32)
}

/// Recover the registry after a panic poisoned it.
///
/// The table is a plain `Vec` of integers; a panic mid-update cannot leave it
/// in a state that makes the checks below unsound, and refusing every handle
/// for the life of the process because one call panicked would be far worse
/// than the alternative.
macro_rules! registry {
    (read) => {
        REGISTRY.read().unwrap_or_else(|e| e.into_inner())
    };
    (write) => {
        REGISTRY.write().unwrap_or_else(|e| e.into_inner())
    };
}

/// Resolve a token to the address of a live handle of the expected kind.
///
/// Returns `None` for a token that was never issued, was issued for a different
/// kind, or has been freed — **without dereferencing `token`**.
fn resolve(token: *mut c_void, expected: HandleKind) -> Option<usize> {
    if token.is_null() {
        return None;
    }
    let (index, generation) = decode_token(token);
    let registry = registry!(read);
    let slot = registry.get(index)?;
    if slot.generation != generation || slot.kind != Some(expected) {
        return None;
    }
    Some(slot.addr)
}

/// Resolve a token to `(kind, address)` without knowing its kind in advance.
///
/// Used by the diagnostic-queue lookup, which accepts any handle type.
fn resolve_any(token: *mut c_void) -> Option<(HandleKind, usize)> {
    if token.is_null() {
        return None;
    }
    let (index, generation) = decode_token(token);
    let registry = registry!(read);
    let slot = registry.get(index)?;
    if slot.generation != generation {
        return None;
    }
    Some((slot.kind?, slot.addr))
}

/// Register a freshly allocated handle and return its token.
///
/// Reuses a free slot when one is available, otherwise appends. Returns `None`
/// if the table is exhausted, which the caller reports as an allocation
/// failure rather than handing back an ambiguous token.
fn register(kind: HandleKind, addr: usize) -> Option<(*mut c_void, u32, u32)> {
    let mut registry = registry!(write);

    if let Some((index, slot)) = registry
        .iter_mut()
        .enumerate()
        .find(|(_, slot)| slot.kind.is_none() && slot.generation < MAX_GENERATION)
    {
        slot.generation += 1;
        slot.kind = Some(kind);
        slot.addr = addr;
        let generation = slot.generation;
        return Some((encode_token(index, generation), index as u32, generation));
    }

    let index = registry.len();
    if index > MAX_SLOT_INDEX {
        tracing::error!("handle registry exhausted at {index} slots");
        return None;
    }
    registry.push(Slot {
        generation: 1,
        kind: Some(kind),
        addr,
    });
    Some((encode_token(index, 1), index as u32, 1))
}

/// Retire a handle's slot, so every outstanding token for it is rejected.
///
/// Returns the address that was registered, or `None` if the token was already
/// stale — which is what makes a double free a refusal rather than a second
/// deallocation.
fn unregister(token: *mut c_void, expected: HandleKind) -> Option<usize> {
    if token.is_null() {
        return None;
    }
    let (index, generation) = decode_token(token);
    let mut registry = registry!(write);
    let slot = registry.get_mut(index)?;
    if slot.generation != generation || slot.kind != Some(expected) {
        return None;
    }
    slot.kind = None;
    // Bumped here as well as on reuse so that the token is dead the instant the
    // handle is freed, not merely once the slot is handed out again.
    slot.generation = slot.generation.saturating_add(1);
    Some(slot.addr)
}

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
    fn header(&self) -> &HandleHeader {
        &self.header
    }
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

    /// Access to the header, so `free_*` can find the handle's slot.
    fn header(&self) -> &HandleHeader;
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
    /// Tokens of child connections, not addresses: everything outside this
    /// module deals in tokens, so they can be revalidated after any free.
    pub connections: Vec<*mut c_void>,
    /// `EnvironmentHandle` no longer names `B` in any field now that child
    /// handles are tokens, but it must stay generic: `as_handle_ref::<T>` keys
    /// on the concrete type, and an environment allocated for one backend must
    /// not resolve as another's.
    _backend: std::marker::PhantomData<fn() -> B>,
    pub diagnostics: DiagnosticQueue,
}

impl<B: Backend> HasKind for EnvironmentHandle<B> {
    const KIND: HandleKind = HandleKind::Env;
    fn header(&self) -> &HandleHeader {
        &self.header
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
    pub env: *mut c_void,
    pub connection: Option<B::Connection>,
    /// Tokens of child statements. See `EnvironmentHandle::connections`.
    pub statements: Vec<*mut c_void>,
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
    fn header(&self) -> &HandleHeader {
        &self.header
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
    fn header(&self) -> &HandleHeader {
        &self.header
    }
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
    let addr = resolve(token, T::KIND).ok_or(OdbcError::InvalidHandle)?;
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
        connections: Vec::new(),
        diagnostics: DiagnosticQueue::new(),
        _backend: std::marker::PhantomData,
    });
    let ptr = Box::into_raw(handle);
    // SAFETY: `ptr` came from `Box::into_raw` just above and has not been
    // shared, so both the reclaim on failure and the header write are sound.
    match register(HandleKind::Env, ptr as usize) {
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
    let env = match unsafe { as_handle_ref::<EnvironmentHandle<B>>(env_ptr) } {
        Ok(e) => e,
        Err(_) => return SqlReturn::INVALID_HANDLE,
    };
    let handle = Box::new(ConnectionHandle::<B> {
        header: HandleHeader::PLACEHOLDER,
        env: env_ptr,
        connection: None,
        statements: Vec::new(),
        diagnostics: DiagnosticQueue::new(),
        attrs: std::collections::HashMap::new(),
        attr_strings: std::collections::HashMap::new(),
        browse_request: None,
    });
    let ptr = Box::into_raw(handle);
    // SAFETY: as in `alloc_environment`.
    match register(HandleKind::Dbc, ptr as usize) {
        Some((token, slot, generation)) => unsafe {
            (*ptr).header = HandleHeader { slot, generation };
            env.connections.push(token);
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
    let conn = match unsafe { as_handle_ref::<ConnectionHandle<B>>(conn_ptr) } {
        Ok(c) => c,
        Err(_) => return SqlReturn::INVALID_HANDLE,
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
    let Some((token, slot, generation)) = register(HandleKind::Stmt, ptr as usize) else {
        drop(unsafe { Box::from_raw(ptr) });
        return SqlReturn::ERROR;
    };
    unsafe {
        (*ptr).header = HandleHeader { slot, generation };
        // Register the four descriptors now that the statement owns them.
        for desc in [
            std::ptr::from_mut(&mut *(*ptr).app_row_desc),
            std::ptr::from_mut(&mut *(*ptr).app_param_desc),
            std::ptr::from_mut(&mut *(*ptr).imp_row_desc),
            std::ptr::from_mut(&mut *(*ptr).imp_param_desc),
        ] {
            if let Some((_, dslot, dgen)) = register(HandleKind::Desc, desc as usize) {
                (*desc).header = HandleHeader {
                    slot: dslot,
                    generation: dgen,
                };
            }
        }
        conn.statements.push(token);
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
    if !env.connections.is_empty() {
        env.diagnostics.push(&OdbcError::general(
            "Cannot free environment with active connections",
            crate::types::SqlState::function_sequence_error(),
        ));
        return SqlReturn::ERROR;
    }
    let Some(addr) = unregister(handle, HandleKind::Env) else {
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
    let env_token = conn.env;
    if let Ok(env) = unsafe { as_handle_ref::<EnvironmentHandle<B>>(env_token) } {
        env.connections.retain(|&p| p != handle);
    }
    let Some(addr) = unregister(handle, HandleKind::Dbc) else {
        return SqlReturn::INVALID_HANDLE;
    };
    // SAFETY: as in `free_environment`.
    drop(unsafe { Box::from_raw(addr as *mut ConnectionHandle<B>) });
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
    let conn_token = stmt.conn;
    if let Ok(conn) = unsafe { as_handle_ref::<ConnectionHandle<B>>(conn_token) } {
        conn.statements.retain(|&p| p != handle);
    }
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
    let Some(addr) = unregister(token, HandleKind::Stmt) else {
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
        unregister(desc.header.token(), HandleKind::Desc);
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
/// # Safety
///
/// `conn.statements` must hold addresses registered by [`alloc_statement`].
pub(crate) unsafe fn free_connection_statements<B: Backend>(conn: &mut ConnectionHandle<B>) {
    for token in std::mem::take(&mut conn.statements) {
        // SAFETY: the caller guarantees these are live statement handles.
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
    let (kind, addr) = resolve_any(handle).ok_or(())?;
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
}
