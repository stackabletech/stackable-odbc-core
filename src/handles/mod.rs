//! ODBC handle types (environment, connection, statement, descriptor), their
//! allocation and free routines, and handle validation at the FFI boundary.
//!
//! A descriptor is a registered allocation like the other three rather than a field of
//! a statement: an implicit one is parented to its statement and an explicit one to a
//! connection, and all of them join the connection's lock group.
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
//! An application freeing a handle on one thread while another is mid-call on
//! it is exactly the case the per-connection group lock defends: `SQLFreeHandle`
//! holds the group for the whole free, so a concurrent call on the same
//! connection either completes first or blocks until the free is done. See
//! `registry.rs` for the lock group itself.

use std::any::Any;
use std::ffi::c_void;
// Deliberately `std::sync::Arc`, not `crate::sync::Arc`: this feeds the same
// `Arc<dyn Any + Send + Sync>` erasure `Slot::cancel` stores, and that
// coercion does not exist for loom's `Arc`. See `registry.rs`'s note on
// `Slot::cancel` for the full reason.
use std::sync::Arc as StdArc;

use odbc_sys::Desc;

use crate::backend::{Backend, StatementBackend};
use crate::cancel::CancelState;
use crate::descriptor::{BindOffset, DescriptorRecord, DescriptorRole};
use crate::diagnostics::DiagnosticQueue;
use crate::errors::{IntoOdbc, OdbcError};
use crate::sync::Arc;
use crate::types::{ColumnDescriptor, ColumnValue, ConnectParams, FetchResult, SqlReturn};

pub(crate) mod registry;
pub(crate) mod scope;

use registry::{GroupLock, HandleKind, encode_token, registry};
use scope::HandleScope;

/// SQL_ATTR_NOSCAN = SQL_NOSCAN_ON: the application asks the driver not to scan
/// SQL for escape sequences.
const SQL_NOSCAN_ON: usize = 1;

/// Whether a descriptor was allocated implicitly with its statement or
/// explicitly by the application.
///
/// `SQL_DESC_ALLOC_TYPE`, which is read-only on every role and is the one field
/// `SQLCopyDesc` never copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocType {
    /// `SQL_DESC_ALLOC_AUTO` — one of the four allocated with a statement.
    Auto,
    /// `SQL_DESC_ALLOC_USER` — allocated by `SQLAllocHandle(SQL_HANDLE_DESC)`.
    User,
}

impl AllocType {
    /// The `SQL_DESC_ALLOC_TYPE` value `SQLGetDescField` reports.
    pub fn as_sql(self) -> isize {
        match self {
            Self::Auto => crate::types::SQL_DESC_ALLOC_AUTO,
            Self::User => crate::types::SQL_DESC_ALLOC_USER,
        }
    }
}

/// One ODBC descriptor (`SQL_HANDLE_DESC`).
///
/// A statement has four of these — the ARD, APD, IRD and IPD — and the ODBC
/// spec makes them the *definition* of a binding rather than a copy of one:
/// "when `SQLBindCol` is called, the driver sets fields in the ARD". So a bound
/// column is a record in the statement's ARD, and a bound parameter is a record
/// in the APD together with one in the IPD. There is one storage, so a binding
/// and its descriptor cannot disagree.
///
/// [`Self::role`] is what tells the four apart, and it is a field rather than a
/// type parameter because ODBC has one record shape and four *readings* of it:
/// `SQLSetDescField` accepts any field identifier against any descriptor and
/// decides validity from the role. Core stores no records for the IRD; reads
/// there are computed from `ColumnDescriptor`, which is how `SQLColAttributeW`
/// and `SQLGetDescField` stay one answer rather than two — inventing a second
/// source of truth for column metadata is the exact failure this type exists to
/// prevent.
///
/// The Windows Driver Manager queries `SQLGetStmtAttrW` for descriptor handle
/// attributes (10010–10013) immediately after allocating a statement. If the
/// driver returns NULL or ERROR, the DM's CLI dispatch table pointer stays
/// NULL and all subsequent application-facing calls crash. These handles
/// satisfy that requirement.
///
/// # Why there *is* a `HasKind` impl
///
/// It deliberately did not, and both facts that refusal depended on are now
/// false. The argument was
/// that [`HandleScope::get`] dispatches on [`HandleKind`] alone, and all four of
/// a statement's descriptors register as `HandleKind::Desc` — so
/// `get::<Descriptor>` would resolve any one of the four as any other and pass
/// every check the registry can make. That held only while the four were `Box`
/// fields of one allocation: a token then named the *statement*, and which field
/// it meant had to come from somewhere else.
///
/// Each descriptor is now its own `Box::into_raw` with its own registry slot and
/// its own [`Self::role`] field. A token therefore names exactly one descriptor,
/// and the struct at that address says which of the four it is — so `get` needs
/// nothing the registry cannot check, and the role needs no owner to supply it.
///
/// The same change is what makes [`HandleScope::stmt_with_desc`] sound: a
/// descriptor is no longer reachable through [`StatementHandle`]'s `&mut`, which
/// is the one thing that would have made that combinator alias.
///
/// The rule that remains is that a `Descriptor` is never reached by casting an
/// address — only through the registry, as every other handle kind is.
///
/// [`HandleScope::get`]: crate::handles::scope::HandleScope::get
/// [`HandleScope::stmt_with_desc`]:
///     crate::handles::scope::HandleScope::stmt_with_desc
#[repr(C)]
pub struct Descriptor {
    header: HandleHeader,
    /// This descriptor's own diagnostic queue.
    ///
    /// `SQLGetDescField`, `SQLSetDescField` and `SQLSetDescRec` all say their
    /// SQLSTATE "can be obtained by calling **SQLGetDiagRec** with a
    /// *HandleType* of SQL_HANDLE_DESC and a *Handle* of *DescriptorHandle*",
    /// so a descriptor that carried no queue could report a failure and nothing
    /// about it.
    pub diagnostics: DiagnosticQueue,
    /// The descriptor's records, keyed by the 1-based column or parameter
    /// number — record 0, the bookmark record, is not supported.
    ///
    /// `SQL_DESC_COUNT` is derived from this map rather than stored beside it,
    /// so the two cannot disagree.
    pub records: std::collections::HashMap<u16, DescriptorRecord>,
    /// The descriptor's header fields, keyed by `SQL_DESC_*` field identifier
    /// (`field as u16`).
    ///
    /// Eight `SQL_ATTR_*` statement attributes **are** descriptor header
    /// fields, per `SQLSetStmtAttr`'s own mapping table, which states that
    /// setting one sets the other. They live here and not in
    /// [`StatementHandle::attrs`] for the same reason records do: one storage
    /// cannot disagree with itself.
    ///
    /// Keyed by the field rather than by the attribute that names it, because
    /// the mapping is not one-to-one: `SQL_DESC_ARRAY_SIZE` is
    /// `SQL_ATTR_ROW_ARRAY_SIZE` on an ARD and `SQL_ATTR_PARAMSET_SIZE` on an
    /// APD, and one descriptor may be the ARD of one statement and the APD of
    /// another. Two keys for one field would be two values for one field —
    /// exactly the disagreement single storage exists to prevent. `Desc` has no
    /// `Hash` impl in `odbc-sys`, so the discriminant is the key.
    pub attrs: std::collections::HashMap<u16, usize>,
    /// Which of the four this descriptor is.
    ///
    /// Fixed at allocation for an implicit descriptor. It is what `HY091` is
    /// decided from: a field defined for an ARD may be undefined on an IPD, and
    /// `SQL_DESC_CONCISE_TYPE` names a C type on one and a SQL type on another.
    pub role: DescriptorRole,
    /// Whether the application allocated this descriptor or a statement did.
    ///
    /// `SQL_DESC_ALLOC_TYPE` reads it. It is deliberately *not* what routes
    /// `SQLFreeHandle`: that decides from the registry's parentage, so a wrong
    /// value here cannot make the two disagree about which descriptors this
    /// function allocated.
    pub alloc_type: AllocType,
}

impl HasKind for Descriptor {
    const KIND: HandleKind = HandleKind::Desc;
}

impl Descriptor {
    /// An empty descriptor, before it is registered.
    fn new(role: DescriptorRole, alloc_type: AllocType) -> Self {
        Self {
            header: HandleHeader::PLACEHOLDER,
            diagnostics: DiagnosticQueue::new(),
            records: std::collections::HashMap::new(),
            attrs: std::collections::HashMap::new(),
            role,
            alloc_type,
        }
    }

    /// The token `SQLGetStmtAttrW` hands to the application for this
    /// descriptor.
    ///
    /// Handing out its address instead would give the application a value that
    /// could never be validated, and that would dangle the moment the owning
    /// statement was freed.
    pub fn token(&self) -> *mut c_void {
        self.header.token()
    }

    /// This descriptor's `SQL_DESC_BIND_OFFSET_PTR`, resolved to a byte offset.
    ///
    /// The one reader of that header field, for both of the statement attributes
    /// that name it: `SQL_ATTR_ROW_BIND_OFFSET_PTR` on the ARD, which `SQLFetch`
    /// applies to its column bindings, and `SQL_ATTR_PARAM_BIND_OFFSET_PTR` on
    /// the APD, which an execution applies to its parameter bindings. One field,
    /// one reader — two copies of the lookup would be two chances to key it
    /// wrongly or to skip the null check in [`BindOffset::apply`].
    ///
    /// # Safety
    ///
    /// As [`BindOffset::read`]: the stored value must be null or a pointer to a
    /// valid `SQLULEN`.
    pub unsafe fn bind_offset(&self) -> BindOffset {
        // SAFETY: forwarded from this function's own contract.
        unsafe { BindOffset::read(&self.attrs) }
    }
}

/// Allocate and register a descriptor, returning its token.
///
/// `parent` is the statement for one of the four implicit descriptors and the
/// connection for an explicit one; `group` is the connection's in both cases,
/// which every statement on that connection already shares — so a descriptor
/// adds no lock and no ordering rule.
///
/// Returns `None` if the registry is exhausted, having freed the allocation
/// rather than leaking it.
pub(crate) fn alloc_descriptor(
    role: DescriptorRole,
    alloc_type: AllocType,
    group: &Arc<GroupLock>,
    parent: *mut c_void,
) -> Option<*mut c_void> {
    let ptr = Box::into_raw(Box::new(Descriptor::new(role, alloc_type)));
    let Some((token, slot, generation)) = registry().register(
        HandleKind::Desc,
        ptr as usize,
        Arc::clone(group),
        Some(parent as usize),
    ) else {
        // SAFETY: `ptr` came from `Box::into_raw` immediately above and was
        // never registered, so nothing else can hold it.
        drop(unsafe { Box::from_raw(ptr) });
        return None;
    };
    // SAFETY: `ptr` is live and registered nowhere else; this is the only write
    // to its header.
    unsafe { (*ptr).header = HandleHeader { slot, generation } };
    Some(token)
}

/// Retire a descriptor's slot and drop its allocation.
///
/// A stale or non-descriptor token is ignored, which is what makes a double free
/// a refusal rather than a second deallocation.
pub(crate) fn free_descriptor(token: *mut c_void) {
    if let Some(addr) = registry().unregister(token, HandleKind::Desc) {
        // SAFETY: `unregister` returned the address `alloc_descriptor`
        // registered and retired the slot, so no other caller can reach it.
        drop(unsafe { Box::from_raw(addr as *mut Descriptor) });
    }
}

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
/// Must be `#[repr(C)]`: validation never reads this struct's memory at all
/// (the registry validates a token by slot index and generation, not by
/// dereferencing it — see the module's top-level docs), so no field's offset
/// is load-bearing for that. `#[repr(C)]` is kept anyway for a defined,
/// non-reordered layout on a type that is heap-allocated via `Box::into_raw`
/// and later reclaimed via `Box::from_raw` at that same raw address.
#[repr(C)]
pub struct EnvironmentHandle<B: Backend> {
    header: HandleHeader,
    pub odbc_version: crate::types::DeclaredOdbcVersion,
    /// No field names `B`, since child handles are tokens rather than typed
    /// pointers, but the struct must stay generic: the registry resolves a
    /// token against the concrete `HasKind` type a caller asks for, and an
    /// environment allocated for one backend must not resolve as another's.
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
/// Must be `#[repr(C)]`: validation never reads this struct's memory at all
/// (the registry validates a token by slot index and generation, not by
/// dereferencing it — see the module's top-level docs), so no field's offset
/// is load-bearing for that. `#[repr(C)]` is kept anyway for a defined,
/// non-reordered layout on a type that is heap-allocated via `Box::into_raw`
/// and later reclaimed via `Box::from_raw` at that same raw address.
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
    /// Whether work has been done on this connection under manual commit since
    /// the last `SQLEndTran` — that is, whether a transaction is open.
    ///
    /// Backs the spec's `HY011` row for `SQLSetConnectAttr`: "The *Attribute*
    /// argument was SQL_ATTR_TXN_ISOLATION, and a transaction was open." The
    /// attribute table says the same thing twice over — "an application must
    /// call `SQLEndTran` to commit or roll back all open transactions on a
    /// connection, before calling `SQLSetConnectAttr` with this option", and
    /// footnote [3], "SQL_ATTR_TXN_ISOLATION can be set only if there are no
    /// open transactions on the connection".
    ///
    /// **Not the same state as an open cursor.** `SQLSetConnectAttr`'s
    /// neighbouring `24000` row is about a pending *result set*, which is
    /// [`StatementHandle::cursor_open`] on one of this connection's statements.
    /// A `SELECT` under autocommit leaves a cursor open with no transaction,
    /// and a rolled-back transaction may have no cursor, so the two conditions
    /// cannot substitute for each other.
    ///
    /// Deliberately conservative: set *before* the backend call rather than
    /// after it succeeds, because a call that fails partway may still have
    /// opened a transaction, and the spec's requirement is to refuse an
    /// isolation change while one might be open.
    pub txn_dirty: bool,
}

impl<B: Backend> HasKind for ConnectionHandle<B> {
    const KIND: HandleKind = HandleKind::Dbc;
}

impl<B: Backend> ConnectionHandle<B> {
    /// Whether this connection is in manual-commit mode.
    ///
    /// `SQL_ATTR_AUTOCOMMIT` defaults to `SQL_AUTOCOMMIT_ON` — the spec's "this
    /// is the default" — so an attribute that was never set means autocommit,
    /// and only an explicit `SQL_AUTOCOMMIT_OFF` puts the connection into the
    /// mode where work opens a transaction.
    pub fn in_manual_commit(&self) -> bool {
        self.attrs
            .get(&odbc_sys::ConnectionAttribute::AUTOCOMMIT.0)
            .is_some_and(|&v| v == crate::types::SQL_AUTOCOMMIT_OFF)
    }

    /// Record that work is about to run on this connection, opening a
    /// transaction if it is in manual-commit mode.
    ///
    /// Called by every statement-producing entry point. A no-op under
    /// autocommit, where each statement commits as it completes and no
    /// transaction outlives the call.
    pub fn note_work_started(&mut self) {
        if self.in_manual_commit() {
            self.txn_dirty = true;
        }
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

    /// Forwards to the backend's [`StatementBackend::take_value_warning`].
    ///
    /// A synthetic result set has none: core builds those rows itself, from
    /// values it already holds, so there is no conversion in which precision
    /// could have been lost before core saw them.
    fn take_value_warning(&mut self) -> Option<crate::types::ValueWarning> {
        match self {
            StatementData::Backend(s) => s.take_value_warning(),
            StatementData::Synthetic(_) => None,
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

/// Both halves of one bound parameter, borrowed together.
///
/// What the readers of a parameter binding want: they need the buffer from the
/// APD and the declared type from the IPD, and a value assembled from two
/// different parameters' records would be nonsense. Constructed only by
/// [`ParamRecords::get`], which will not pair records under different keys.
///
/// Both halves are [`DescriptorRecord`]s, and which of its fields each half
/// speaks for is the split itself:
///
/// - The **APD** half carries `SQL_DESC_CONCISE_TYPE` read as a C type, plus
///   `SQL_DESC_DATA_PTR`, `SQL_DESC_OCTET_LENGTH` and
///   `SQL_DESC_INDICATOR_PTR` — what `SQLBindParameter`'s page maps onto
///   application parameter descriptor fields. It describes the *buffer* the
///   application supplied and says nothing about the parameter's type at the
///   data source.
/// - The **IPD** half carries `SQL_DESC_CONCISE_TYPE` read as a SQL type —
///   `SQLBindParameter`'s `ParameterType`, the type the value is converted to
///   before it reaches the backend. For every C type but the two character ones
///   that conversion is a no-op, because the APD's C type already fixes the
///   value's shape; for `SQL_C_CHAR` and `SQL_C_WCHAR` it is the only statement
///   of what the text *is*. Its `length` and `scale` are `ColumnSize` and
///   `DecimalDigits`, which [`crate::param_convert`] enforces at execute time
///   and `SQLDescribeParam` reports back.
#[derive(Clone, Copy)]
pub(crate) struct ParamRecord<'a> {
    /// The APD half: where the value is and how it is laid out.
    ///
    /// Its two pointers are the *unoffset* `SQL_DESC_DATA_PTR` and
    /// `SQL_DESC_INDICATOR_PTR`, as `SQLBindParameter` stored them. Read them
    /// through [`Self::data_ptr`] and [`Self::indicator_ptr`], which apply
    /// `SQL_ATTR_PARAM_BIND_OFFSET_PTR`; the field is public only because the
    /// other half of it — the C type, the buffer length — carries no offset.
    pub apd: &'a DescriptorRecord,
    /// The IPD half: what the value is declared to be.
    pub ipd: &'a DescriptorRecord,
    /// The APD header's `SQL_DESC_BIND_OFFSET_PTR`, resolved for this call.
    ///
    /// Carried on the record rather than passed beside it so that a reader
    /// cannot hold a binding without also holding the offset that binding is
    /// to be read at. The bug this closes was the absence of exactly that: the
    /// attribute was stored, readable and never applied.
    ///
    /// Never applied by hand — [`Self::data_ptr`] and [`Self::indicator_ptr`]
    /// are the readers, so the null rule is in one place.
    pub bind_offset: BindOffset,
}

impl ParamRecord<'_> {
    /// Where this parameter's value is, with
    /// `SQL_ATTR_PARAM_BIND_OFFSET_PTR` applied.
    ///
    /// Null when the application bound none — the offset never resurrects a
    /// null pointer. See [`BindOffset::apply`].
    pub(crate) fn data_ptr(&self) -> *mut c_void {
        self.bind_offset.apply(self.apd.data_ptr)
    }

    /// Where this parameter's length or indicator is, with
    /// `SQL_ATTR_PARAM_BIND_OFFSET_PTR` applied. Null when absent.
    pub(crate) fn indicator_ptr(&self) -> *mut isize {
        self.bind_offset.apply(self.apd.indicator_ptr)
    }
}

/// The two parameter descriptors' record maps, borrowed together.
///
/// The free functions in [`crate::ffi::params`] took one `HashMap` before the
/// split and take this after, so they still borrow one field-path of the
/// statement rather than the whole handle.
#[derive(Clone, Copy)]
pub(crate) struct ParamRecords<'a> {
    /// The APD's records.
    pub apd: &'a std::collections::HashMap<u16, DescriptorRecord>,
    /// The IPD's records.
    pub ipd: &'a std::collections::HashMap<u16, DescriptorRecord>,
    /// The APD header's `SQL_DESC_BIND_OFFSET_PTR`, resolved once for this call
    /// and handed to every [`ParamRecord`] this yields.
    ///
    /// Resolved once rather than per record because the spec fixes the
    /// dereference at execution time (see [`BindOffset`]), and because two reads
    /// of a value the application can change at will could shift one parameter
    /// by one offset and the next by another.
    pub bind_offset: BindOffset,
}

impl<'a> ParamRecords<'a> {
    /// Both halves of parameter `number`, or `None` if it is not bound.
    ///
    /// "Not bound" covers two shapes: no record at all, and a record carrying
    /// neither a data pointer nor an indicator pointer. The second exists
    /// because `SQLSetDescField` can create a record by setting any single
    /// field, so presence in the map does not by itself mean a binding.
    ///
    /// A null data pointer alone is **not** the test, which is the one thing
    /// to be careful of here. `SQLBindParameter`'s *ParameterValuePtr* section:
    /// "An application can set the *ParameterValuePtr* argument to a null
    /// pointer, as long as *StrLen_or_IndPtr is SQL_NULL_DATA or
    /// SQL_DATA_AT_EXEC." The Driver Manager agrees — its `HY009` fires only
    /// when *both* pointers are null — and so does `sql_bind_parameter`, which
    /// removes a binding on that same pair. Every client binds a NULL this
    /// way: pyodbc sends `value_ptr=NULL, *ind=SQL_NULL_DATA` for `None`.
    ///
    /// Testing the data pointer alone therefore reported `07002` — "the number
    /// of parameters specified in SQLBindParameter was less than the number of
    /// parameters in the SQL statement" — for a parameter the application had
    /// bound, making `WHERE col = ?` with a NULL inexpressible.
    ///
    /// This is a parameter-side rule, and [`DescriptorRecord::is_bound`] is
    /// deliberately left alone: it answers "is there a data buffer", which is
    /// still exactly what a writer of column data needs to know. `SQLBindCol`
    /// draws the same distinction on the column side — a null `TargetValuePtr`
    /// unbinds the *data buffer* and keeps a live `StrLen_or_IndPtr` bound — so
    /// both sides ask two questions of a record rather than one.
    ///
    /// `Err` is reserved for the one case that is neither: a parameter present
    /// in one descriptor and absent from the other. `SQLBindParameter` writes
    /// and removes both under the same key, so that state is unreachable — but
    /// it is unreachable by construction rather than by type, and the
    /// alternative to reporting it is an `unwrap` on a path that marshals
    /// application pointers.
    pub(crate) fn get(&self, number: u16) -> Result<Option<ParamRecord<'a>>, OdbcError> {
        match (self.apd.get(&number), self.ipd.get(&number)) {
            // A record carrying neither pointer exists but is not a binding.
            // `SQLSetDescField` can create one by setting any single field, so
            // presence in the map stopped answering this question.
            (Some(apd), Some(_)) if !apd.is_bound() && apd.indicator_ptr.is_null() => Ok(None),
            (Some(apd), Some(ipd)) => Ok(Some(ParamRecord {
                apd,
                ipd,
                bind_offset: self.bind_offset,
            })),
            (None, None) => Ok(None),
            (apd, _) => Err(OdbcError::general(
                format!(
                    "internal: parameter {number} is bound in the {} but not the {}",
                    if apd.is_some() { "APD" } else { "IPD" },
                    if apd.is_some() { "IPD" } else { "APD" },
                ),
                crate::types::SqlState::general_error(),
            )),
        }
    }
}

/// How far `SQLGetData` has read into a single column of the current row, so
/// that a repeated call for the same column returns the *next* part.
///
/// See [`StatementHandle::get_data_cursor`] for why only one column is tracked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetDataCursor {
    /// The 1-based column this position refers to. A call for any other column
    /// discards this cursor and starts that column from zero.
    pub column: u16,
    /// Units already delivered — UTF-16 code units for `SQL_C_WCHAR`, bytes for
    /// `SQL_C_CHAR` and `SQL_C_BINARY`.
    ///
    /// The unit follows the C type the application asked for, and an
    /// application that changes C type mid-column would reinterpret it. That
    /// costs nothing to allow and nothing to police: the spec gives no meaning
    /// to switching target type between parts, and the resulting offset is no
    /// less defined than the partial value the application would be assembling.
    pub delivered: usize,
    /// Set once the whole value has been handed over, so the next call for this
    /// same column returns `SQL_NO_DATA` rather than restarting it. Also set
    /// immediately for a fixed-width target, which cannot be read in parts.
    pub done: bool,
    /// The converted value being drained, and the C type it was converted for,
    /// materialised on the first call for this column.
    ///
    /// This is what stops a chunked read costing O(N²/K) — see
    /// [`crate::column_value::CachedChunkSource`]. `None` means every call
    /// re-asks the backend, which is still the path for a target the cache does
    /// not cover (a fixed-width one, or a value that has to be *rendered* rather
    /// than borrowed).
    ///
    /// The C type is part of the key because an application may legally change
    /// target type between parts, which invalidates the conversion and not
    /// merely the offset.
    pub(crate) cached: Option<(
        crate::types::CDataType,
        crate::column_value::CachedChunkSource,
    )>,
}

/// What `SQLPutData` has delivered so far for the parameter
/// [`DataAtExecState::current_param`] names.
///
/// Three states rather than a `bool`, because three separate rules read it and
/// no two of them ask the same question: `SQLPutData` needs "has a NULL already
/// been sent" for its `HY020`, `SQLParamData` needs "was `SQLPutData` called at
/// all" for its `HY010`, and the finaliser needs "NULL, or a value" — an empty
/// buffer is a zero-length value when data was put and nothing at all when it
/// was not, and those are two different parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutDataState {
    /// `SQLParamData` has named a parameter and `SQLPutData` has not been
    /// called for it yet.
    NotCalled,
    /// At least one `SQLPutData` delivered data for it.
    Data,
    /// A `SQLPutData` delivered `SQL_NULL_DATA` for it.
    Null,
}

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
    /// What `SQLPutData` has delivered for [`Self::current_param`]. Reset to
    /// [`PutDataState::NotCalled`] each time `SQLParamData` names a new one.
    pub put_state: PutDataState,
    /// Already-collected parameter values (both DAE and non-DAE).
    /// Key is 1-based parameter number.
    pub collected_values: std::collections::HashMap<u16, ColumnValue>,
    /// The SQL text to execute once all DAE params are supplied.
    /// Needed because SQLExecDirectW doesn't store prepared_sql.
    pub sql: String,
    /// Warnings raised converting the parameters that were readable at the
    /// call which returned `SQL_NEED_DATA`.
    ///
    /// Carried rather than posted there: `SQL_NEED_DATA` is not a completion,
    /// an application that receives it does not call `SQLGetDiagRec`, and the
    /// diagnostic belongs with the call that actually sends the value. They are
    /// posted by the `SQLParamData` that completes the execution, which then
    /// answers `SQL_SUCCESS_WITH_INFO`.
    pub warnings: Vec<crate::errors::OdbcError>,
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
/// `conn` is the token of the parent connection, not an address: it is
/// resolved through the registry like any other handle, via
/// [`HandleScope::get`](crate::handles::scope::HandleScope::get), which is why
/// treating it as a raw pointer anywhere would be unsound. Parentage itself
/// lives in the registry, not in a list on the connection.
///
/// Must be `#[repr(C)]`: validation never reads this struct's memory at all
/// (the registry validates a token by slot index and generation, not by
/// dereferencing it — see the module's top-level docs), so no field's offset
/// is load-bearing for that. `#[repr(C)]` is kept anyway for a defined,
/// non-reordered layout on a type that is heap-allocated via `Box::into_raw`
/// and later reclaimed via `Box::from_raw` at that same raw address.
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
    /// Whether this statement has been **executed** (ODBC states S4-S7), as
    /// opposed to merely prepared (S2/S3).
    ///
    /// A third fact, because neither of the other two answers the question.
    /// `statement.is_some()` is true from `SQLPrepare` onwards, and
    /// `cursor_open` is false for an `UPDATE` that executed perfectly well —
    /// so a check written against either one gets state S4 wrong in one
    /// direction or the other. Appendix B separates `S2-S3 Prepared` from
    /// `S4 Executed` in almost every row, and `SQLSetCursorName`'s is the row
    /// where the two answers differ: `--` for prepared, `24000` for executed.
    pub executed: bool,
    /// SQL text stored by `SQLPrepareW`, executed by `SQLExecute`.
    pub prepared_sql: Option<String>,
    /// Number of `?` parameter markers counted in `prepared_sql` by `SQLPrepareW`.
    pub param_count: Option<u16>,
    /// Cursor name set by SQLSetCursorNameW or auto-generated by SQLGetCursorNameW.
    pub cursor_name: Option<String>,
    /// Data-at-execution state for SQLParamData/SQLPutData.
    /// `Some` when SQLExecute/SQLExecDirectW returned SQL_NEED_DATA.
    pub data_at_exec: Option<DataAtExecState>,
    /// How far `SQLGetData` has read into one column of the current row.
    ///
    /// One slot, not one per column, because that is exactly what the spec
    /// mandates: "Successive calls to `SQLGetData` will retrieve data from the
    /// last column requested; prior offsets become invalid" — so
    /// `SQLGetData(n)`, `SQLGetData(m)`, `SQLGetData(n)` restarts column `n`
    /// from the beginning. Keeping a position per column would *preserve* an
    /// offset the spec says is invalid.
    ///
    /// Cleared whenever the cursor moves or the result set goes away, since a
    /// position into the previous row's value means nothing in the next one.
    pub get_data_cursor: Option<GetDataCursor>,
    pub diagnostics: DiagnosticQueue,
    /// Integer/pointer-valued statement attributes set via `SQLSetStmtAttr`.
    /// Values are stored as `usize` (pointer-sized). Defaults are applied at read time.
    pub attrs: std::collections::HashMap<i32, usize>,
    /// Seconds of `SQL_ATTR_QUERY_TIMEOUT` that **core** must enforce, if any.
    ///
    /// Set only when [`Backend::set_query_timeout`] answered
    /// [`QueryTimeout::CoreCancels`], so it is deliberately narrower than the
    /// attribute in `attrs`: a timeout the *data source* enforces is stored
    /// there and absent here, because core arming a second timer for it would
    /// cancel a statement the server was already managing.
    ///
    /// [`Backend::set_query_timeout`]: crate::backend::Backend::set_query_timeout
    /// [`QueryTimeout::CoreCancels`]: crate::types::QueryTimeout::CoreCancels
    pub core_query_timeout: Option<usize>,
    /// Tokens for the four descriptors allocated with this statement, in
    /// [`DescriptorRole`] order: ARD, APD, IRD, IPD.
    ///
    /// Tokens rather than `Box` fields because a descriptor may be shared: one
    /// explicit descriptor can stand in for the ARD of several statements, which
    /// no owned field can express. It is also what makes
    /// [`HandleScope::stmt_with_desc`] sound — a descriptor is no longer
    /// reachable through this struct's `&mut`.
    ///
    /// The IRD's descriptor stores no records: reads there are computed from the
    /// current result set's `ColumnDescriptor`s. It exists as a handle because
    /// the Windows Driver Manager queries `SQLGetStmtAttrW(10010–10013)` after
    /// statement allocation and crashes without a valid handle for each.
    ///
    /// **These are not reclaimed by `Drop`.** [`free_statement_allocation`]
    /// frees them explicitly; Miri's leak check is what catches a teardown path
    /// that forgets.
    ///
    /// [`HandleScope::stmt_with_desc`]:
    ///     crate::handles::scope::HandleScope::stmt_with_desc
    implicit_desc: [*mut c_void; 4],
    /// `SQL_ATTR_APP_ROW_DESC`, when the application has supplied its own.
    /// `None` means the implicit ARD, which is what `SQL_NULL_DESC` restores.
    ard_override: Option<*mut c_void>,
    /// `SQL_ATTR_APP_PARAM_DESC`, likewise. The implementation descriptors have
    /// no override: "the application cannot specify alternate implementation
    /// descriptors".
    apd_override: Option<*mut c_void>,
}

impl<B: Backend> HasKind for StatementHandle<B> {
    const KIND: HandleKind = HandleKind::Stmt;
}

/// The descriptor whose header field a statement attribute aliases.
///
/// `SQLSetStmtAttr`'s own page carries a table mapping statement attributes
/// onto descriptor **header** fields and states: "When a descriptor field that
/// is also a statement attribute is set by a call to **SQLSetDescField**, the
/// corresponding statement attribute is set." Two copies of the value is how
/// those two views come to disagree, so the descriptor's header is the only
/// copy and [`StatementHandle::attrs`] no longer holds these keys at all.
///
/// | Statement attribute | Header field | Descriptor |
/// |---|---|---|
/// | `SQL_ATTR_ROW_ARRAY_SIZE` | `SQL_DESC_ARRAY_SIZE` | ARD |
/// | `SQL_ATTR_ROW_BIND_TYPE` | `SQL_DESC_BIND_TYPE` | ARD |
/// | `SQL_ATTR_ROW_BIND_OFFSET_PTR` | `SQL_DESC_BIND_OFFSET_PTR` | ARD |
/// | `SQL_ATTR_ROW_OPERATION_PTR` | `SQL_DESC_ARRAY_STATUS_PTR` | ARD |
/// | `SQL_ATTR_PARAMSET_SIZE` | `SQL_DESC_ARRAY_SIZE` | APD |
/// | `SQL_ATTR_PARAM_BIND_TYPE` | `SQL_DESC_BIND_TYPE` | APD |
/// | `SQL_ATTR_PARAM_BIND_OFFSET_PTR` | `SQL_DESC_BIND_OFFSET_PTR` | APD |
/// | `SQL_ATTR_PARAM_OPERATION_PTR` | `SQL_DESC_ARRAY_STATUS_PTR` | APD |
///
/// The IRD- and IPD-side pairs are absent deliberately, not by oversight:
/// `SQL_ATTR_ROW_STATUS_PTR` and `SQL_ATTR_ROWS_FETCHED_PTR` are
/// `SQL_DESC_ARRAY_STATUS_PTR` and `SQL_DESC_ROWS_PROCESSED_PTR` on the **IRD**,
/// which [`Descriptor`] does not back, and `SQL_ATTR_PARAM_STATUS_PTR` and
/// `SQL_ATTR_PARAMS_PROCESSED_PTR` are the same two fields on the IPD, whose
/// header defines neither. They stay in [`StatementHandle::attrs`].
///
/// Note the third column of that table is *not* one-to-one with the second:
/// `SQL_DESC_ARRAY_SIZE` appears twice. So [`Self::of`] answers with the
/// descriptor **and** the field, and the field is what keys the storage — one
/// explicit descriptor may be the ARD of one statement and the APD of another,
/// and keyed by attribute that one field would become two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderOwner {
    /// The application row descriptor.
    Ard,
    /// The application parameter descriptor.
    Apd,
}

impl HeaderOwner {
    /// The descriptor and the header field `attr` names, or `None` if it is an
    /// ordinary statement attribute.
    ///
    /// The field, not the attribute, is what keys
    /// [`Descriptor::attrs`]: two attributes name `SQL_DESC_ARRAY_SIZE`, and
    /// once a descriptor can be one statement's ARD and another's APD, two keys
    /// for one field is two values for one field.
    ///
    /// `SQL_ATTR_PARAM_OPERATION_PTR` is spelled
    /// `StatementAttribute::ParamOpterationPtr` in `odbc-sys` — transposed
    /// letters, upstream. A grep for the correct spelling finds nothing here
    /// and reads as "core does not implement it", which is false.
    pub(crate) fn of(attr: Option<odbc_sys::StatementAttribute>) -> Option<(Self, Desc)> {
        use odbc_sys::StatementAttribute as A;
        Some(match attr? {
            A::RowArraySize => (Self::Ard, Desc::ArraySize),
            A::RowBindType => (Self::Ard, Desc::BindType),
            A::RowBindOffsetPtr => (Self::Ard, Desc::BindOffsetPtr),
            A::RowOperationPtr => (Self::Ard, Desc::ArrayStatusPtr),
            A::ParamsetSize => (Self::Apd, Desc::ArraySize),
            A::ParamBindType => (Self::Apd, Desc::BindType),
            A::ParamBindOffsetPtr => (Self::Apd, Desc::BindOffsetPtr),
            A::ParamOpterationPtr => (Self::Apd, Desc::ArrayStatusPtr),
            _ => return None,
        })
    }
}

impl<B: Backend> StatementHandle<B> {
    /// A statement attribute that is *not* a descriptor header field.
    ///
    /// The four IRD- and IPD-side pairs (`SQL_ATTR_ROW_STATUS_PTR`,
    /// `SQL_ATTR_ROWS_FETCHED_PTR`, `SQL_ATTR_PARAM_STATUS_PTR`,
    /// `SQL_ATTR_PARAMS_PROCESSED_PTR`) live here with the ordinary attributes,
    /// as they did before the header fields were re-keyed.
    ///
    /// A header-field attribute is on a descriptor, which is not a field of this
    /// struct — [`HandleScope::attr_get`] is the door that reaches either.
    ///
    /// [`HandleScope::attr_get`]: crate::handles::scope::HandleScope::attr_get
    pub(crate) fn plain_attr_get(&self, attribute: i32) -> Option<usize> {
        self.attrs.get(&attribute).copied()
    }

    /// [`Self::plain_attr_get`], for writing.
    pub(crate) fn plain_attr_set(&mut self, attribute: i32, value: usize) {
        self.attrs.insert(attribute, value);
    }

    /// The token for this statement's descriptor in `role`, honouring an
    /// application-supplied override for the two application descriptors.
    ///
    /// The one place the override is applied, so no call site can read the
    /// implicit descriptor while the application believes its own is in use.
    pub(crate) fn descriptor_token(&self, role: DescriptorRole) -> *mut c_void {
        match role {
            DescriptorRole::Ard => self.ard_override.unwrap_or(self.implicit_desc[0]),
            DescriptorRole::Apd => self.apd_override.unwrap_or(self.implicit_desc[1]),
            DescriptorRole::Ird => self.implicit_desc[2],
            DescriptorRole::Ipd => self.implicit_desc[3],
            // `App` is what an *explicit* descriptor answers for itself, so no
            // statement has one under that role. A caller asking for it wants
            // whichever application descriptor is in use, and the ARD is the
            // arbitrary half of a question that should not have been asked —
            // so say so rather than answer it.
            DescriptorRole::App => {
                tracing::error!(
                    "descriptor_token(App): a statement has no descriptor under the \
                     not-yet-known role; returning the ARD"
                );
                self.descriptor_token(DescriptorRole::Ard)
            }
        }
    }

    /// Point one of the two application descriptors at an explicit descriptor, or
    /// back at the implicit one with `None`.
    ///
    /// `SQL_ATTR_APP_ROW_DESC` / `SQL_ATTR_APP_PARAM_DESC`. The implementation
    /// descriptors have no counterpart: "the application cannot specify alternate
    /// implementation descriptors".
    ///
    /// A token equal to this statement's own implicit descriptor is stored as
    /// `None` rather than as an override of itself, so
    /// [`Self::descriptor_token`] answers the same either way and a later
    /// `SQLFreeHandle` on some *other* descriptor cannot mistake this statement
    /// for one that needs reverting.
    pub(crate) fn set_app_descriptor(&mut self, role: DescriptorRole, token: Option<*mut c_void>) {
        let token = match token {
            Some(t) if t == self.implicit_descriptor_token(role) => None,
            other => other,
        };
        match role {
            DescriptorRole::Ard => self.ard_override = token,
            DescriptorRole::Apd => self.apd_override = token,
            _ => tracing::error!("set_app_descriptor called for {role:?}, which has no override"),
        }
    }

    /// The token for the descriptor implicitly allocated with this statement,
    /// ignoring any override. Two callers: [`Self::set_app_descriptor`], which stores a
    /// token equal to this one as `None` rather than as an override of itself, and
    /// `free_statement_allocation`, which frees all four at teardown.
    ///
    /// A `match` rather than `implicit_desc[role as usize]`: [`DescriptorRole`]
    /// has a fifth variant, `App`, for an explicitly allocated descriptor whose
    /// role is not yet known, and an index would run off the end of a
    /// four-element array rather than being refused.
    pub(crate) fn implicit_descriptor_token(&self, role: DescriptorRole) -> *mut c_void {
        match role {
            DescriptorRole::Ard => self.implicit_desc[0],
            DescriptorRole::Apd => self.implicit_desc[1],
            DescriptorRole::Ird => self.implicit_desc[2],
            DescriptorRole::Ipd => self.implicit_desc[3],
            // No statement allocates a descriptor under the not-yet-known role;
            // see `descriptor_token`.
            DescriptorRole::App => {
                tracing::error!(
                    "implicit_descriptor_token(App): a statement allocates no descriptor \
                     under the not-yet-known role; returning the ARD"
                );
                self.implicit_desc[0]
            }
        }
    }

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
        self.executed = true;
        self.get_data_cursor = None;
    }

    /// Store a prepared-but-unexecuted backend statement (`SQLPrepareW`, or a
    /// re-prepare before `SQLExecute`). No cursor is open in the prepared
    /// states S2/S3.
    pub fn set_prepared_statement(&mut self, data: StatementData<B>) {
        self.statement = Some(data);
        self.cursor_open = false;
        self.executed = false;
        self.get_data_cursor = None;
    }

    /// Discard the result set and close the cursor (`SQLCloseCursor`,
    /// `SQLFreeStmt(SQL_CLOSE)`, `SQLEndTran` under `SQL_CB_DELETE`).
    pub fn discard_result_set(&mut self) {
        self.statement = None;
        self.cursor_open = false;
        self.executed = false;
        self.get_data_cursor = None;
    }

    /// Record that the backend statement already held has now been executed,
    /// opening a cursor over it if it produced columns.
    ///
    /// `SQLExecute` and the `SQLParamData` that completes a data-at-execution
    /// execution both land here: `SQLPrepare` already stored the backend
    /// statement, so there is nothing to store — only the S2/S3 -> S4/S5
    /// transition to record. Both used to assign `cursor_open` inline, which
    /// left `executed` behind the moment it existed.
    pub fn note_executed(&mut self) {
        self.executed = true;
        self.cursor_open = self
            .statement
            .as_ref()
            .is_some_and(|s| s.column_count() > 0);
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

/// Allocate a new environment handle and write it to `output`.
///
/// # Safety
///
/// `output` must be a valid, non-null pointer to a `*mut c_void`.
/// The caller (`sql_alloc_handle`) is responsible for validating that `output`
/// is non-null before calling this function.
/// Why an `alloc_*` function produced no handle.
///
/// A distinct type rather than a bare [`SqlReturn`] so that the exhaustion arm
/// cannot be confused with any other failure: `SQLAllocHandle` answers `HY014`
/// for that one and nothing else, and a future error path added to one of these
/// functions has to say which it is rather than inheriting a SQLSTATE by
/// accident.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AllocFailure {
    /// The parent token did not name a live handle of the required kind.
    InvalidHandle,
    /// The registry has no slot left — `SQLAllocHandle`'s `HY014`.
    RegistryExhausted,
}

pub unsafe fn alloc_environment<B: Backend>(output: *mut *mut c_void) -> Result<(), AllocFailure> {
    let handle = Box::new(EnvironmentHandle::<B> {
        header: HandleHeader::PLACEHOLDER,
        odbc_version: crate::types::DeclaredOdbcVersion::Odbc3,
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
            Ok(())
        },
        None => {
            drop(unsafe { Box::from_raw(ptr) });
            Err(AllocFailure::RegistryExhausted)
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
) -> Result<(), AllocFailure> {
    // Validates that the environment is live and really is an environment,
    // without dereferencing it. There is no list on the environment to push
    // this connection's token onto: `register` below records the parentage
    // the registry needs.
    if registry().group_of_kind(env_ptr, HandleKind::Env).is_none() {
        return Err(AllocFailure::InvalidHandle);
    }
    let handle = Box::new(ConnectionHandle::<B> {
        header: HandleHeader::PLACEHOLDER,
        env: env_ptr,
        connection: None,
        diagnostics: DiagnosticQueue::new(),
        attrs: std::collections::HashMap::new(),
        attr_strings: std::collections::HashMap::new(),
        browse_request: None,
        // A fresh connection has done no work, so no transaction is open
        // whatever commit mode it is later put into.
        txn_dirty: false,
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
            Ok(())
        },
        None => {
            drop(unsafe { Box::from_raw(ptr) });
            Err(AllocFailure::RegistryExhausted)
        }
    }
}

/// Allocate a new statement handle, register it with the parent connection,
/// and write it to `output`.
///
/// `inherited_metadata_id` is the parent connection's `SQL_ATTR_METADATA_ID`,
/// or `None` if it was never set there. `SQLSetStmtAttr`'s Comments make this
/// attribute one of exactly two that may be set at the connection level —
/// "ODBC 3.x statement attributes cannot be set at the connection level, with
/// the exception of the SQL_ATTR_METADATA_ID and SQL_ATTR_ASYNC_ENABLE
/// attributes" — and the connection-level value is the default for statements
/// allocated afterwards. It is seeded here, at the one site that decides a
/// statement's initial state, rather than in the caller, so a future
/// allocation path cannot forget it. A later `SQLSetStmtAttr` overwrites the
/// seeded entry like any other.
///
/// The other of the two, `SQL_ATTR_ASYNC_ENABLE`, is deliberately not
/// inherited: core reports `SQL_AM_NONE` for `SQL_ASYNC_MODE`, so the only
/// value a connection can hold is `SQL_ASYNC_ENABLE_OFF`, which is already the
/// statement default.
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
    inherited_metadata_id: Option<usize>,
) -> Result<(), AllocFailure> {
    // Statements and their descriptors share the connection's lock. One
    // acquisition then covers a call that touches a statement and its parent.
    // `group_of_kind` validates that `conn_ptr` is live and really is a
    // connection, and hands back the group to join, without dereferencing
    // the parent.
    let group = match registry().group_of_kind(conn_ptr, HandleKind::Dbc) {
        Some(g) => g,
        None => return Err(AllocFailure::InvalidHandle),
    };
    // Each descriptor is its own allocation with its own registry slot:
    // `SQLGetStmtAttrW` hands these out to the application, so they need tokens
    // of their own or the application would receive a raw address it could not be
    // validated from — and a descriptor may be shared with another statement,
    // which no owned field can express.
    //
    // **Not** freed by `Drop`. `free_statement_allocation` frees them, and it is
    // the only thing that does; Miri's leak check is what catches a teardown path
    // that forgets.
    let handle = Box::new(StatementHandle::<B> {
        header: HandleHeader::PLACEHOLDER,
        conn: conn_ptr,
        statement: None,
        cursor_open: false,
        executed: false,
        prepared_sql: None,
        param_count: None,
        cursor_name: None,
        data_at_exec: None,
        get_data_cursor: None,
        diagnostics: DiagnosticQueue::new(),
        // Seeded from the connection; see this function's doc comment. The two
        // identifiers are the same number (`SQL_ATTR_METADATA_ID` is 10014 at
        // both levels), but the statement's own name is written here because
        // this map is read with it.
        attrs: match inherited_metadata_id {
            Some(value) => std::collections::HashMap::from([(
                odbc_sys::StatementAttribute::MetadataId as i32,
                value,
            )]),
            None => std::collections::HashMap::new(),
        },
        // Not inherited from the connection: `SQL_ATTR_QUERY_TIMEOUT` is a
        // statement attribute, so a fresh statement starts with no deadline
        // until something sets one on it.
        core_query_timeout: None,
        // Filled in below, once the statement has a token to be their parent.
        implicit_desc: [std::ptr::null_mut(); 4],
        ard_override: None,
        apd_override: None,
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
        return Err(AllocFailure::RegistryExhausted);
    };
    unsafe {
        (*ptr).header = HandleHeader { slot, generation };
    }
    // Allocate the four descriptors now that the statement has a token to be
    // their parent. Each shares the connection's group, which the statement
    // already joined, so a descriptor adds no lock.
    //
    // A loop rather than four calls: the four are four elements of one array
    // rather than four distinct fields.
    let mut implicit = [std::ptr::null_mut(); 4];
    for (index, role) in [
        DescriptorRole::Ard,
        DescriptorRole::Apd,
        DescriptorRole::Ird,
        DescriptorRole::Ipd,
    ]
    .into_iter()
    .enumerate()
    {
        let Some(desc) = alloc_descriptor(role, AllocType::Auto, &group, token) else {
            // Registry exhausted part-way: retire what this call created,
            // including the statement, so no half-built handle escapes.
            for created in implicit.iter().take(index) {
                free_descriptor(*created);
            }
            registry().unregister(token, HandleKind::Stmt);
            // SAFETY: `ptr` came from `Box::into_raw` above and its slot has
            // just been retired, so no token can reach it any more.
            drop(unsafe { Box::from_raw(ptr) });
            return Err(AllocFailure::RegistryExhausted);
        };
        implicit[index] = desc;
    }
    unsafe {
        (*ptr).implicit_desc = implicit;
        std::ptr::write_unaligned(output, token);
    }
    Ok(())
}

/// Resolve the cancel token for `stmt_token`, creating it on first use.
///
/// `Backend::cancel_token` cannot run at `SQLAllocHandle(SQL_HANDLE_STMT)`
/// (see that method's doc comment): a statement can be allocated on a
/// connection that is not yet open, and there is no `&B::Connection` to hand
/// it until one exists. Instead, core calls this from the first
/// statement-producing call that has a connection in hand — `exec_direct`,
/// `prepare`, `tables`, and the rest of the ten callers all check
/// `conn.connection.is_some()` and return HY010 before reaching here, so
/// `connection` is always real by the time this runs.
///
/// Mint per execution, replacing any previous token. One token therefore spans
/// exactly one execution plus the cursor it opened, which is the unit an
/// application means by "cancel this".
///
/// This replaced an earlier create-once-never-replace rule, whose stated
/// reasoning was that a `SQLCancel` holding a token from a finished execution
/// would "silently cancel nothing". The spec makes that outcome *required*, not
/// a bug: "In ODBC 3.5, a call to SQLCancel when no processing is being done on
/// the statement ... has is [sic] no effect at all." Doing nothing to a run
/// that already completed is correct; reaching into the unrelated run that
/// replaced it is not.
///
/// Create-once also broke a second spec rule outright. `Backend::cancel` marks
/// the token, so a reused token stays marked and every later call on that
/// statement observes a cancellation that is not its own — while the spec says
/// "After the statement has been canceled, the application can call SQLExecute
/// or SQLExecDirect again."
///
/// `Backend::cancel_token` still runs eagerly, with a real `&B::Connection` in
/// hand; its ODBC-401 rationale is about *laziness*, not identity, so minting
/// more often does not weaken it.
///
/// # Locking
///
/// The caller must already hold `stmt_token`'s connection group lock — every
/// statement-producing FFI entry point does, via `panic_safe`/`HandleScope`,
/// for the whole duration of the backend call this feeds. That is what makes
/// the check-then-set below race-free with no synchronisation of its own: two
/// threads can never be inside this function for the same statement at the
/// same time. This is prose rather than a `&HandleScope` witness parameter
/// only because the rest of the crate already makes it hard to violate: the
/// `connection: &B::Connection` argument itself is obtainable in production
/// only through `HandleScope::get`/`stmt_with_parent`, both of which require
/// `&mut HandleScope`, and a `HandleScope` is only ever constructed while its
/// group lock is held (`HandleScope::new` is `pub(crate)` with exactly four
/// production callers — `panic_safe`, `HandleScope::with_child_group_in`,
/// `HandleScope::with_group` and `sql_cancel` — all four of which lock first).
/// This function is never reached through `sql_cancel`'s own scope: that scope
/// only ever calls `scope.get::<StatementHandle<B>>`, never obtains a
/// `&B::Connection`, so it is irrelevant to the argument here beyond being one
/// more site that upholds the same "lock before scope" precondition the other
/// three do. `with_group` is `SQLCopyDesc` phase one's, which takes the
/// *source*'s group and materialises an owned snapshot.
///
/// What is stored is a [`CancelState<B::CancelToken>`], not the bare
/// `B::CancelToken`: the wrapper adds core's own record of *why* the token was
/// signalled, which the query timer sets and `QueryTimer::reclassify` reads.
/// See that type's doc comment for why the fact belongs in this allocation and
/// not on the statement or in the registry slot. Nothing outside
/// [`cancel_as`], `QueryTimer` and `sql_cancel` needs to know: `cancel_as`
/// hands back the `&B::CancelToken` inside it, so every backend call site is
/// unaffected.
pub(crate) fn mint_cancel_token<B: Backend>(
    stmt_token: *mut c_void,
    connection: &B::Connection,
) -> StdArc<dyn Any + Send + Sync> {
    let created: StdArc<dyn Any + Send + Sync> =
        StdArc::new(CancelState::new(B::cancel_token(connection)));
    registry().set_cancel(stmt_token, StdArc::clone(&created));
    created
}

/// Read the current execution's cancel token without minting one.
///
/// For calls that consume a cursor some earlier call produced — `SQLFetch`,
/// `SQLGetData`, `SQLDescribeCol` and their neighbours. They belong to the
/// execution that opened the cursor and must observe *its* token, not a fresh
/// one that nothing has ever signalled.
///
/// `None` means no backend call has run on this statement yet, which is
/// indistinguishable from "nothing to cancel" and is handled as such.
pub(crate) fn current_cancel_token(
    stmt_token: *mut c_void,
) -> Option<StdArc<dyn Any + Send + Sync>> {
    registry().cancel_of(stmt_token)
}

/// Downcast a type-erased cancel token back to the concrete `B::CancelToken`
/// a statement-producing call needs to pass through.
///
/// The stored value is a [`CancelState<B::CancelToken>`] (see
/// [`mint_cancel_token`]); this returns the backend's half of it, borrowed
/// from inside the `Arc`, so a call site passing a token to a `Backend` method
/// sees exactly what it always did.
///
/// The `Err` arm is unreachable in practice: every token this crate stores
/// was built by `mint_cancel_token::<B>` for this exact `B`, so the type
/// always matches. It exists anyway because nothing makes that statically
/// provable across the `dyn Any` erasure, and this crate denies
/// `unwrap`/`expect` outside tests.
pub(crate) fn cancel_as<B: Backend>(
    token: &StdArc<dyn Any + Send + Sync>,
) -> Result<&B::CancelToken, OdbcError> {
    cancel_state_as::<B>(token).map(CancelState::token)
}

/// [`cancel_as`], keeping core's own cancellation record rather than
/// discarding it.
///
/// Three callers. [`cancel_as`] above, which throws the record away; and the
/// two in [`crate::query_timer`] that need to know *why* the token was
/// signalled rather than only that it was — `QueryTimer::timed_out`, whose
/// timeout pass reads [`CancelState::timed_out`], and the timer thread, which
/// sets it.
///
/// Those two spelled the `downcast_ref` out by hand until review caught it.
/// The type named in a `downcast` is not checked against the type
/// `mint_cancel_token` stores — a mismatch compiles and then silently misses
/// at run time — so every site that names it is a site that can drift. This
/// function is where it is named once.
pub(crate) fn cancel_state_as<B: Backend>(
    token: &StdArc<dyn Any + Send + Sync>,
) -> Result<&CancelState<B::CancelToken>, OdbcError> {
    token
        .downcast_ref::<CancelState<B::CancelToken>>()
        .ok_or_else(|| {
            OdbcError::general(
                "Statement's cancel token is not this backend's CancelToken type",
                crate::types::SqlState::general_error(),
            )
        })
}

/// Free an environment handle. Fails with `SqlReturn::ERROR` if there are
/// still active connections.
///
/// `scope` must hold `handle`'s own lock group, obtained by the caller passing
/// `handle` itself to [`crate::panic::panic_safe`] — freeing an environment
/// reaches its diagnostics and, on the HY010 path, pushes to them, so this
/// goes through the scope like every other handle mutation rather than
/// resolving the token off to one side of it.
///
/// # Safety
///
/// `handle` must point to a valid `EnvironmentHandle<B>` previously allocated
/// by [`alloc_environment`].
pub unsafe fn free_environment<B: Backend>(
    handle: *mut c_void,
    scope: &mut HandleScope<'_>,
) -> SqlReturn {
    let env = match scope.get::<EnvironmentHandle<B>>(handle) {
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
/// - There are still handles allocated under it — statements or explicitly
///   allocated descriptors (HY010)
///
/// `scope` must hold `handle`'s own lock group, for the same reason as
/// [`free_environment`].
///
/// # Safety
///
/// `handle` must point to a valid `ConnectionHandle<B>` previously allocated
/// by [`alloc_connection`].
pub unsafe fn free_connection<B: Backend>(
    handle: *mut c_void,
    scope: &mut HandleScope<'_>,
) -> SqlReturn {
    let conn = match scope.get::<ConnectionHandle<B>>(handle) {
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
    // in the crate allowed to acquire both an environment's lock and a
    // connection's is `SQLEndTran(SQL_HANDLE_ENV)`, and only environment
    // before connection.
    if !registry().children_of(handle).is_empty() {
        conn.diagnostics.push(&OdbcError::general(
            "Cannot free connection with handles still allocated under it",
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
    // Not `Drop`'s job any more: the four are separate allocations, so this is
    // the only thing that reclaims them. An override is *not* freed here — an
    // explicit descriptor outlives every statement that referenced it, until
    // `SQLFreeHandle` or `SQLDisconnect` takes it.
    for role in [
        DescriptorRole::Ard,
        DescriptorRole::Apd,
        DescriptorRole::Ird,
        DescriptorRole::Ipd,
    ] {
        free_descriptor(stmt.implicit_descriptor_token(role));
    }
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
/// Point every statement on `conn_token` that used `desc_token` back at its own
/// implicit descriptor.
///
/// The spec: "When an explicitly allocated descriptor is freed, all statement
/// handles to which the freed descriptor applied automatically revert to the
/// descriptors implicitly allocated for them."
///
/// Called *before* the descriptor is freed. Afterwards would work equally well —
/// nothing here reads the descriptor — but the token is what identifies it, and
/// comparing against a token whose slot has been retired invites the next reader
/// to wonder whether it still resolves.
///
/// # Safety
///
/// The caller must hold the connection's group lock, which `scope` is the proof
/// of, and `conn_token` must be live.
pub(crate) unsafe fn revert_statements_using<B: Backend>(
    scope: &mut HandleScope<'_>,
    conn_token: *mut c_void,
    desc_token: *mut c_void,
) {
    for stmt_token in registry().children_of(conn_token) {
        let Ok(stmt) = scope.get::<StatementHandle<B>>(stmt_token) else {
            // Not a statement — a sibling descriptor on the same connection.
            continue;
        };
        for role in [DescriptorRole::Ard, DescriptorRole::Apd] {
            if stmt.descriptor_token(role) == desc_token {
                tracing::debug!(
                    "SQLFreeHandle: statement {:?} reverts its {:?} to the implicit descriptor",
                    stmt_token,
                    role
                );
                stmt.set_app_descriptor(role, None);
            }
        }
    }
}

/// Free every explicit descriptor allocated on a connection.
///
/// `SQLDisconnect` "drops any statements or descriptors open on the connection".
/// Called alongside [`free_connection_statements`]; a descriptor whose parent is
/// the connection is explicit by construction — an implicit one is parented to
/// its statement — so no alloc type is inspected here either.
///
/// No statement is reverted first: `SQLDisconnect` frees the connection's
/// statements too, so there is nothing left holding an override.
///
/// # Safety
///
/// `conn_token` must be a live connection handle.
pub(crate) unsafe fn free_connection_descriptors(conn_token: *mut c_void) {
    let mut freed = 0usize;
    for token in registry().children_of(conn_token) {
        // `parent_of` rather than `group_of_kind`: the only question here is
        // whether this child is a descriptor, and the caller already holds the
        // group — asking for it again would clone an `Arc` to drop it, and would
        // read as a lock-acquisition site to the guard test that counts them.
        if registry().parent_of(token, HandleKind::Desc).is_some() {
            free_descriptor(token);
            freed += 1;
        }
    }
    if freed > 0 {
        tracing::debug!("SQLDisconnect: freed {freed} explicit descriptor(s)");
    }
}

pub(crate) unsafe fn free_connection_statements<B: Backend>(conn_token: *mut c_void) {
    for token in registry().children_of(conn_token) {
        // SAFETY: `children_of` returns live tokens registered by
        // `alloc_statement`; a token freed between the snapshot and here is
        // rejected by `unregister` rather than freed twice.
        let _ = unsafe { free_statement_allocation::<B>(token) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockBackend, MockConnection, with_handle};

    /// Free an environment through the same lock-gated path `SQLFreeHandle`
    /// uses: `free_environment` takes a scope rather than resolving its own
    /// handle, so a test calling it directly must acquire one first.
    fn free_env(env: *mut c_void) -> SqlReturn {
        unsafe {
            crate::panic::panic_safe::<MockBackend, _>(env, |scope| {
                Ok(free_environment::<MockBackend>(env, scope))
            })
        }
    }

    /// As [`free_env`], for `free_connection`.
    fn free_conn(conn: *mut c_void) -> SqlReturn {
        unsafe {
            crate::panic::panic_safe::<MockBackend, _>(conn, |scope| {
                Ok(free_connection::<MockBackend>(conn, scope))
            })
        }
    }

    /// Whether `token` resolves as a live `T` through the scope, for tests
    /// pinning the *rejection* itself. [`with_handle`] is for the
    /// complementary case: reading or mutating through a token already known
    /// to be valid.
    fn resolves_as<T: HasKind>(token: *mut c_void) -> bool {
        let ret = unsafe {
            crate::panic::panic_safe::<MockBackend, _>(token, |scope| {
                Ok(if scope.get::<T>(token).is_ok() {
                    SqlReturn::SUCCESS
                } else {
                    SqlReturn::INVALID_HANDLE
                })
            })
        };
        ret == SqlReturn::SUCCESS
    }

    /// Like [`resolves_as`], for a test whose subject is *which* error comes
    /// back rather than a bare pass/fail.
    fn resolve_error<T: HasKind>(token: *mut c_void) -> Option<OdbcError> {
        let mut error = None;
        let ret = unsafe {
            crate::panic::panic_safe::<MockBackend, _>(token, |scope| {
                if let Err(err) = scope.get::<T>(token) {
                    error = Some(err);
                }
                Ok(SqlReturn::SUCCESS)
            })
        };
        // `assert_eq!`, not `debug_assert_eq!`: this is test code, and a
        // `debug_assert` is compiled out of a `--release` test build, which is
        // precisely the build where a `panic_safe` returning non-`SUCCESS` for
        // some reason other than the closure — a caught panic — would otherwise
        // pass unnoticed and leave `error` silently `None`.
        assert_eq!(
            ret,
            SqlReturn::SUCCESS,
            "the closure above never returns Err"
        );
        error
    }

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
        assert!(
            !resolves_as::<EnvironmentHandle<MockBackend>>(bogus),
            "a non-handle value must be rejected"
        );
    }

    #[test]
    fn a_freed_handle_is_rejected() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            assert_eq!(free_env(env_ptr), SqlReturn::SUCCESS);

            // The application still holds the old value.
            assert!(
                !resolves_as::<EnvironmentHandle<MockBackend>>(env_ptr),
                "a freed handle must be rejected"
            );
        }
    }

    #[test]
    fn a_freed_handle_is_rejected_even_after_its_slot_is_reused() {
        unsafe {
            let mut first: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut first as *mut _);
            let _ = free_env(first);

            // The next allocation is very likely to take the slot just freed.
            let mut second: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut second as *mut _);

            assert!(
                !resolves_as::<EnvironmentHandle<MockBackend>>(first),
                "the old handle must not be revived by a reused slot"
            );
            assert!(resolves_as::<EnvironmentHandle<MockBackend>>(second));

            let _ = free_env(second);
        }
    }

    #[test]
    fn freeing_a_handle_twice_is_rejected_the_second_time() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            assert_eq!(free_env(env_ptr), SqlReturn::SUCCESS);
            assert_eq!(
                free_env(env_ptr),
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
            let _ = alloc_statement::<MockBackend>(conn_ptr, &mut stmt_ptr as *mut _, None);

            assert!(resolves_as::<StatementHandle<MockBackend>>(stmt_ptr));

            let _ = free_statement::<MockBackend>(stmt_ptr);
            assert!(
                !resolves_as::<StatementHandle<MockBackend>>(stmt_ptr),
                "a freed statement must be rejected"
            );

            let _ = free_conn(conn_ptr);
            let _ = free_env(env_ptr);
        }
    }

    #[test]
    fn a_handle_of_the_wrong_type_is_rejected() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            // A real, live handle — but asked for as the wrong type.
            assert!(!resolves_as::<ConnectionHandle<MockBackend>>(env_ptr));
            assert!(!resolves_as::<StatementHandle<MockBackend>>(env_ptr));
            assert!(resolves_as::<EnvironmentHandle<MockBackend>>(env_ptr));

            let _ = free_env(env_ptr);
        }
    }

    #[test]
    fn alloc_and_free_environment() {
        unsafe {
            let mut output: *mut c_void = std::ptr::null_mut();
            let result = alloc_environment::<MockBackend>(&mut output as *mut *mut c_void);
            assert_eq!(result, Ok(()));
            assert!(!output.is_null());
            let result = free_env(output);
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
            assert_eq!(result, Ok(()));
            assert!(!conn_ptr.is_null());

            let _ = free_conn(conn_ptr);
            let _ = free_env(env_ptr);
        }
    }

    /// A connection's parent must be an environment. Passing a live handle of
    /// any other kind must be rejected, or the new connection joins a lock
    /// group it has no relationship to and becomes invisible to
    /// `children_of` of any real environment.
    #[test]
    fn alloc_connection_rejects_a_statement_as_parent() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            // A statement already has children of its own: its four
            // descriptors, whose parent token is the statement. Compare
            // before/after rather than asserting emptiness, so the test does
            // not confuse those pre-existing children with a wrongly
            // registered connection.
            let children_before = registry::registry().children_of(stmt);

            let mut out: *mut c_void = std::ptr::null_mut();
            let result = alloc_connection::<MockBackend>(stmt, &mut out as *mut _);
            assert_eq!(
                result,
                Err(AllocFailure::InvalidHandle),
                "a statement token must not be accepted as a connection's parent environment"
            );
            assert!(out.is_null(), "no connection should have been allocated");
            assert_eq!(
                registry::registry().children_of(stmt),
                children_before,
                "no connection should have been registered under the statement"
            );

            let _ = free_statement::<MockBackend>(stmt);
            let _ = free_conn(conn);
            let _ = free_env(env);
        }
    }

    /// A statement's parent must be a connection. Passing a live handle of
    /// any other kind must be rejected, or the new statement joins the wrong
    /// lock group — exactly the "a connection and its statements share one
    /// lock" invariant the rest of the lock discipline depends on.
    #[test]
    fn alloc_statement_rejects_an_environment_as_parent() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            let mut out: *mut c_void = std::ptr::null_mut();
            let result = alloc_statement::<MockBackend>(env_ptr, &mut out as *mut _, None);
            assert_eq!(
                result,
                Err(AllocFailure::InvalidHandle),
                "an environment token must not be accepted as a statement's parent connection"
            );
            assert!(out.is_null(), "no statement should have been allocated");
            assert!(
                registry::registry().children_of(env_ptr).is_empty(),
                "no statement should have been registered under the environment"
            );

            let _ = free_env(env_ptr);
        }
    }

    /// Allocation alone must not create a cancel token (see
    /// `mint_cancel_token`'s doc comment for why it cannot run at
    /// `SQLAllocHandle` time), and two calls standing in for two different
    /// statement-producing FFI entry points on the same statement must each
    /// get their **own** `Arc`.
    ///
    /// This asserted the opposite once. See `mint_cancel_token`'s doc
    /// comment for the two spec sentences that overturned it — in short, a
    /// reused token stays signalled after `SQLCancel`, and the spec requires
    /// the next `SQLExecute` on that statement to work.
    #[test]
    fn mint_cancel_token_replaces_the_previous_token() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |c| {
                c.connection = Some(MockConnection);
            });

            assert!(
                registry::registry().cancel_of(stmt).is_none(),
                "allocation alone must not create a cancel token"
            );
            assert!(
                current_cancel_token(stmt).is_none(),
                "before any backend call there is nothing that could have been cancelled"
            );

            let first = with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |c| {
                let connection = c.connection.as_ref().expect("connected above");
                mint_cancel_token::<MockBackend>(stmt, connection)
            });
            let second = with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |c| {
                let connection = c.connection.as_ref().expect("connected above");
                mint_cancel_token::<MockBackend>(stmt, connection)
            });

            assert!(
                !StdArc::ptr_eq(&first, &second),
                "each statement-producing call owns its own token, so a cancel aimed at one \
                 execution cannot leak into the next"
            );
            assert!(
                registry::registry()
                    .cancel_of(stmt)
                    .is_some_and(|current| StdArc::ptr_eq(&current, &second)),
                "the registry must hold the most recent token, not the first"
            );
            // `current_cancel_token` is what the cursor-consuming entry points
            // read, and it must see the same most-recent token: a `SQLFetch`
            // draining the cursor `second` opened has to observe `second`.
            assert!(
                current_cancel_token(stmt).is_some_and(|current| StdArc::ptr_eq(&current, &second)),
                "a cursor-consuming call must observe the execution's own token"
            );
            assert!(
                cancel_as::<MockBackend>(&first).is_ok(),
                "the stored token must be the backend's own type"
            );

            let _ = free_statement::<MockBackend>(stmt);
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn, |c| {
                c.connection = None;
            });
            let _ = free_conn(conn);
            let _ = free_env(env);
        }
    }

    #[test]
    fn free_env_with_active_connections_fails() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            let mut conn_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_connection::<MockBackend>(env_ptr, &mut conn_ptr as *mut _);

            let result = free_env(env_ptr);
            assert_eq!(result, SqlReturn::ERROR);

            let _ = free_conn(conn_ptr);
            let _ = free_env(env_ptr);
        }
    }

    #[test]
    fn null_handle_yields_no_group_to_hold() {
        // `resolves_as` routes through `panic_safe`/`HandleScope`, so a null
        // token exercises `holds()` finding no group to hold (the scope
        // built for it is `HandleScope::new(None, None)`), not the
        // registry's own null-token rejection in `resolve`/`resolve_any`,
        // `scope.rs`'s `a_null_handle_scope_still_refuses_a_live_token` names
        // that mechanism directly.
        assert!(!resolves_as::<EnvironmentHandle<MockBackend>>(
            std::ptr::null_mut()
        ));
    }

    #[test]
    fn alloc_and_free_statement() {
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);
            let mut conn_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_connection::<MockBackend>(env_ptr, &mut conn_ptr as *mut _);
            let mut stmt_ptr: *mut c_void = std::ptr::null_mut();
            let result = alloc_statement::<MockBackend>(conn_ptr, &mut stmt_ptr as *mut _, None);
            assert_eq!(result, Ok(()));

            let _ = free_statement::<MockBackend>(stmt_ptr);
            let _ = free_conn(conn_ptr);
            let _ = free_env(env_ptr);
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
            let _ = alloc_statement::<MockBackend>(conn_ptr, &mut stmt_ptr as *mut _, None);

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

            let desc_tokens =
                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt_ptr, |stmt| {
                    [
                        stmt.descriptor_token(DescriptorRole::Ard),
                        stmt.descriptor_token(DescriptorRole::Apd),
                        stmt.descriptor_token(DescriptorRole::Ird),
                        stmt.descriptor_token(DescriptorRole::Ipd),
                    ]
                });
            for desc_token in desc_tokens {
                let desc_group = registry().group_of(desc_token).expect("live");
                assert!(
                    Arc::ptr_eq(&desc_group, &stmt_group),
                    "each descriptor must share its statement's lock group"
                );
            }

            let _ = free_statement::<MockBackend>(stmt_ptr);
            let _ = free_conn(conn_ptr);
            let _ = free_env(env_ptr);
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
            let _ = alloc_statement::<MockBackend>(conn_ptr, &mut stmt_ptr as *mut _, None);

            let result = free_conn(conn_ptr);
            assert_eq!(result, SqlReturn::ERROR);

            let _ = free_statement::<MockBackend>(stmt_ptr);
            let _ = free_conn(conn_ptr);
            let _ = free_env(env_ptr);
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
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn_ptr, |conn| {
                conn.connection = Some(MockConnection);
            });

            // Should fail because connection is still open.
            let result = free_conn(conn_ptr);
            assert_eq!(result, SqlReturn::ERROR);

            // Clean up: remove connection, then free.
            with_handle::<MockBackend, ConnectionHandle<MockBackend>, _>(conn_ptr, |conn| {
                conn.connection = None;
            });
            let _ = free_conn(conn_ptr);
            let _ = free_env(env_ptr);
        }
    }

    #[test]
    fn wrong_handle_type_is_rejected_as_invalid_handle() {
        // A valid environment handle presented where a statement is expected
        // must be rejected by the registry's kind check, not silently
        // reinterpreted, and specifically with `InvalidHandle`, not merely
        // some error or other.
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            let wrong = resolve_error::<StatementHandle<MockBackend>>(env_ptr);
            assert!(matches!(wrong, Some(OdbcError::InvalidHandle)));

            let _ = free_env(env_ptr);
        }
    }

    #[test]
    fn free_handle_rejects_wrong_handle_type() {
        // Calling the wrong `free_*` for a handle must not free it: the kind
        // compare fails, `INVALID_HANDLE` is returned, and the handle stays valid.
        unsafe {
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            let _ = alloc_environment::<MockBackend>(&mut env_ptr as *mut _);

            // Treat the environment as if it were a statement.
            let result = free_statement::<MockBackend>(env_ptr);
            assert_eq!(result, SqlReturn::INVALID_HANDLE);

            // The environment is untouched and still frees cleanly.
            assert_eq!(free_env(env_ptr), SqlReturn::SUCCESS);
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
            let _ = alloc_statement::<MockBackend>(conn, &mut stmt as *mut _, None);
            (env, conn, stmt)
        }
    }

    /// A snapshot already taken from `children_of` is unaffected by a later
    /// free — trivially true of any by-value `Vec` return, not a guard
    /// against a `push` that reallocates or a `retain` that shifts, since
    /// neither exists once the list is not a field to begin with. What can
    /// still be gotten wrong is the *next* snapshot: this also checks that
    /// freeing a statement removes it from a subsequent `children_of` call,
    /// which is the half of "`SQLEndTran` walks an owned snapshot" that is
    /// not free by construction.
    #[test]
    fn a_statement_freed_during_iteration_cannot_disturb_the_walk() {
        unsafe {
            let (env, conn, stmt_a) = alloc_env_conn_stmt();
            let mut out: *mut c_void = std::ptr::null_mut();
            assert_eq!(alloc_statement::<MockBackend>(conn, &mut out, None), Ok(()));
            let stmt_b = out;

            let snapshot = registry::registry().children_of(conn);
            assert_eq!(snapshot.len(), 2);

            // Free one while "iterating" the snapshot.
            assert_eq!(free_statement::<MockBackend>(stmt_a), SqlReturn::SUCCESS);

            // The snapshot is unchanged; the registry has moved on.
            assert_eq!(snapshot.len(), 2);
            assert_eq!(registry::registry().children_of(conn), vec![stmt_b]);

            assert_eq!(free_statement::<MockBackend>(stmt_b), SqlReturn::SUCCESS);
            assert_eq!(free_conn(conn), SqlReturn::SUCCESS);
            assert_eq!(free_env(env), SqlReturn::SUCCESS);
        }
    }

    /// A statement's descriptors are separate allocations, reclaimed when the
    /// statement is freed.
    ///
    /// `Drop` used to cover this because they were `Box` fields. It does not any
    /// more, so a missed `free_descriptor` here is a leak Miri reports and
    /// nothing else does.
    #[test]
    fn freeing_a_statement_retires_its_four_descriptor_slots() {
        unsafe {
            let (env, conn, stmt) = crate::test_utils::alloc_env_conn_stmt();
            let tokens = with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |s| {
                [
                    s.descriptor_token(DescriptorRole::Ard),
                    s.descriptor_token(DescriptorRole::Apd),
                    s.descriptor_token(DescriptorRole::Ird),
                    s.descriptor_token(DescriptorRole::Ipd),
                ]
            });
            assert_eq!(free_statement::<MockBackend>(stmt), SqlReturn::SUCCESS);
            for token in tokens {
                assert!(
                    registry().group_of(token).is_none(),
                    "every descriptor slot must be retired with its statement"
                );
            }
            assert_eq!(free_conn(conn), SqlReturn::SUCCESS);
            assert_eq!(free_env(env), SqlReturn::SUCCESS);
        }
    }

    /// One header field, one storage — whichever statement attribute names it.
    ///
    /// `SQL_DESC_ARRAY_SIZE` is `SQL_ATTR_ROW_ARRAY_SIZE` on an ARD and
    /// `SQL_ATTR_PARAMSET_SIZE` on an APD. One explicit descriptor can be both at
    /// once, so the two names must reach one value.
    #[test]
    fn a_header_field_has_one_key_whichever_attribute_names_it() {
        use odbc_sys::StatementAttribute as A;
        let (ard_owner, ard_field) = HeaderOwner::of(Some(A::RowArraySize))
            .expect("SQL_ATTR_ROW_ARRAY_SIZE is a header field");
        let (apd_owner, apd_field) = HeaderOwner::of(Some(A::ParamsetSize))
            .expect("SQL_ATTR_PARAMSET_SIZE is a header field");
        assert_eq!(ard_owner, HeaderOwner::Ard);
        assert_eq!(apd_owner, HeaderOwner::Apd);
        assert_eq!(
            ard_field, apd_field,
            "both name SQL_DESC_ARRAY_SIZE, so they must key the same storage"
        );
        assert_eq!(ard_field as u16, odbc_sys::Desc::ArraySize as u16);
    }
}
