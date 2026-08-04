//! Access to handle contents, gated on holding the owning connection's lock.
//!
//! A `HandleScope` is the only way to obtain `&mut` to a handle. The only way
//! to obtain one is through the four callers of the `pub(crate)`
//! `HandleScope::new` in this crate: [`panic_safe`], which builds the
//! outermost scope for an FFI call; [`HandleScope::with_child_group`], which
//! builds a nested scope for the one legitimate case of holding two groups at
//! once; `sql_cancel`, which builds one only on the branch where its own
//! `try_lock` succeeded; and `sql_copy_desc`, whose phase one holds the
//! *source* descriptor's group while every diagnostic belongs to the target.
//! All four lock the group immediately before constructing the scope and tie its
//! lifetime to that lock (see [`HandleScope::new`]), which is what makes "the
//! group lock is held" a fact the compiler checks rather than a rule a comment
//! states.
//!
//! [`panic_safe`]: crate::panic::panic_safe

use std::ffi::c_void;
use std::marker::PhantomData;

use crate::backend::{Backend, StatementBackend};
use crate::descriptor::{DescriptorRole, DescriptorSnapshot};
use crate::diagnostics::DiagnosticQueue;
use crate::errors::OdbcError;
use crate::handles::registry::{GroupLock, HandleKind, Registry, registry};
use crate::handles::{
    ConnectionHandle, Descriptor, EnvironmentHandle, HasKind, HeaderOwner, ParamRecords,
    StatementHandle,
};
use crate::sync::{Arc, MutexGuard};
use crate::types::statement_attribute_from_raw;
use odbc_sys::Desc;

/// Proof that the caller holds one lock group, and the gateway to the handles
/// inside it.
///
/// `HandleScope::new` is `pub(crate)`, with exactly four callers in this
/// crate: [`panic_safe`], which builds the outermost scope for an FFI call;
/// [`Self::with_child_group`], which builds a nested scope for the one
/// legitimate case of holding two groups at once; `sql_cancel`
/// (`ffi::cursor`), which builds one only on the branch where its own
/// `try_lock` succeeded, never on the branch where another thread holds the
/// group; and `sql_copy_desc` (`ffi::desc`), which cannot use `panic_safe`
/// because the lock it needs is the **source** descriptor's while every
/// diagnostic it posts belongs to the target. All four lock the group
/// immediately before constructing the scope and pass a borrow of that lock as
/// `new`'s `guard` parameter, which is what ties the lifetime `'a` to it: a
/// `HandleScope<'a>` cannot be constructed, returned, or used once its
/// originating guard is gone, so a live `HandleScope` always corresponds to a
/// held group lock, or, for a null handle, to nothing needing one.
///
/// [`panic_safe`]: crate::panic::panic_safe
pub struct HandleScope<'a> {
    /// The group whose lock the caller holds, or `None` for a call that
    /// arrived with `SQL_NULL_HANDLE` and so has nothing to protect.
    group: Option<Arc<GroupLock>>,
    /// Ties `'a` to the guard borrowed in [`Self::new`], and makes the scope
    /// `!Send`.
    ///
    /// `*const ()` rather than `&'a ()` for the `!Send`: a `MutexGuard` is
    /// itself `!Send` because releasing a lock on a thread other than the one
    /// that took it is undefined for the underlying primitive, and a scope is
    /// only valid while that guard is held. Leaving the scope `Send` would let a
    /// scoped thread receive one whose guard is held elsewhere, and reach handle
    /// contents while claiming a lock it does not hold.
    ///
    /// No in-crate closure that receives a scope spawns a thread, so this
    /// closes the hole rather than fixing a live bug. It costs nothing: the
    /// lifetime is still tied, and `PhantomData<*const ()>` carries no variance
    /// the scope relies on.
    _guard: PhantomData<*const &'a ()>,
}

impl<'a> HandleScope<'a> {
    /// Construct a scope for a held group.
    ///
    /// `guard` is a borrow of the `MutexGuard` the caller is already holding
    /// for `group` (or `None`, for the null-handle case with nothing locked);
    /// its lifetime is what `'a` on the returned scope is unified with, so the
    /// borrow checker, and not just a doc comment, refuses a `HandleScope` that
    /// outlives the lock it claims to hold. `guard`'s value is never read:
    /// this scope reaches handles through the registry, not through the
    /// guard, so the parameter exists purely to carry the lifetime.
    ///
    /// `pub(crate)` so that only this crate's four callers can claim to hold
    /// a lock.
    pub(crate) fn new(
        group: Option<Arc<GroupLock>>,
        guard: Option<&'a MutexGuard<'_, ()>>,
    ) -> Self {
        let _ = guard;
        Self {
            group,
            _guard: PhantomData,
        }
    }

    /// True when `token` belongs to the group this scope holds.
    ///
    /// Only [`Self::with_child_group_in`] needs this as a separate question:
    /// [`Self::get`] and [`Self::diagnostics`] fold it into their single
    /// registry pass. See that method for why the registry is a parameter.
    fn holds_in(&self, reg: &Registry, token: *mut c_void) -> bool {
        match (&self.group, reg.group_of(token)) {
            (Some(held), Some(theirs)) => Arc::ptr_eq(held, &theirs),
            _ => false,
        }
    }

    /// Borrow a handle from the locked group.
    ///
    /// Returns [`OdbcError::InvalidHandle`] for a stale token, a token of the
    /// wrong kind, **or a token belonging to another group**, the last case
    /// being what stops a caller reaching a handle this scope does not protect.
    ///
    /// The returned lifetime is tied to `&mut self`, so two handles cannot be
    /// held at once. Use [`Self::stmt_with_parent`] when both a statement and
    /// its connection are needed.
    /// One registry pass answers all three questions, live, right kind and
    /// right group, because this is the hottest lookup in the crate: it is on
    /// every FFI entry point. Splitting it into a [`Self::holds`] and a
    /// `Registry::resolve` would take the lock and decode the token twice, plus
    /// an `Arc` clone `holds` makes only to compare and drop.
    pub fn get<T: HasKind>(&mut self, token: *mut c_void) -> Result<&mut T, OdbcError> {
        // `None` is the null-handle case, where nothing is locked and so no
        // handle is reachable.
        let held = self.group.as_ref().ok_or(OdbcError::InvalidHandle)?;
        let addr = registry()
            .resolve_in_group(token, T::KIND, held)
            .ok_or(OdbcError::InvalidHandle)?;
        // SAFETY: the registry produced `addr`, so it came from `Box::into_raw`
        // in an `alloc_*` function for a handle of exactly `T::KIND` and has not
        // been freed. The same lookup established that the slot's group is the
        // one this scope holds the lock for, so no other thread can hold a
        // reference to the same handle.
        Ok(unsafe { &mut *(addr as *mut T) })
    }

    /// Borrow a statement and its parent connection together.
    ///
    /// Sound because **neither handle is reachable from the other**: `conn` is
    /// an opaque token (`*mut c_void`), not a typed pointer the compiler could
    /// follow from `&mut StatementHandle` to reborrow the connection, and
    /// `ConnectionHandle` holds no field pointing at its statements at all,
    /// parentage living in the registry (`Registry::children_of`) rather than in
    /// either handle struct. Different [`HandleKind`]s alone would not be
    /// enough to justify this. A typed `Box<Descriptor>` field on
    /// `StatementHandle` would be reachable through that handle's own `&mut`,
    /// so a `stmt_with_desc` built over one would alias under Stacked/Tree
    /// Borrows despite the two addresses differing and `debug_assert_ne!` seeing
    /// nothing wrong. Each descriptor being its own registered allocation is
    /// what avoids that, and [`Self::stmt_with_desc`] is the second combinator
    /// of this shape. Both exist only for pairs that are actually mutually
    /// unreachable.
    ///
    /// They share one group, so this needs no second acquisition.
    ///
    /// [`HandleKind`]: crate::handles::registry::HandleKind
    pub fn stmt_with_parent<B: Backend>(
        &mut self,
        token: *mut c_void,
    ) -> Result<(&mut StatementHandle<B>, &mut ConnectionHandle<B>), OdbcError> {
        let stmt_addr = {
            let stmt: &mut StatementHandle<B> = self.get(token)?;
            std::ptr::from_mut(stmt)
        };
        // SAFETY: `get` validated the statement above, and `stmt_addr` was
        // produced from that validated reference, so reading its `conn` field
        // is just reading our own already-validated allocation.
        let conn_token = unsafe { (*stmt_addr).conn };
        let conn_addr = {
            let conn: &mut ConnectionHandle<B> = self.get(conn_token)?;
            std::ptr::from_mut(conn)
        };
        // A statement and a connection are different `HandleKind`s, hence
        // different `Box` allocations from different `alloc_*` calls, so these
        // addresses can never be equal. This only pins the weaker fact that
        // they are literally different allocations. The reason the two
        // references cannot alias is that neither handle is reachable from the
        // other (see the doc comment above), which distinct addresses alone
        // would not establish.
        debug_assert_ne!(stmt_addr as usize, conn_addr as usize);
        // SAFETY: both addresses came from `get`, which validated each token
        // against the registry and confirmed it belongs to the group this
        // scope holds, so neither is stale or foreign. The second `get` call
        // takes `&mut self`, but `self` is just `{ group, _guard: PhantomData }`
        // and holds no pointer into either handle's memory, so reborrowing
        // it to make that call touches nothing `stmt_addr` points at.
        // `stmt_addr` itself was produced by casting `addr as *mut T` inside
        // the *first* `get` call, never derived from `self`, so it carries no
        // provenance tying it to `self` for a later reborrow of `self` to
        // invalidate. Combined with neither handle being reachable from the
        // other, the two `&mut`s below cannot alias.
        Ok(unsafe { (&mut *stmt_addr, &mut *conn_addr) })
    }

    /// Run `f` while additionally holding `token`'s group.
    ///
    /// The crate's one lock-ordering rule is **environment before
    /// connection**, and `SQLEndTran(SQL_HANDLE_ENV)` is its only site: it
    /// holds the environment's group while walking that environment's
    /// connections. Do not call this in the other direction.
    ///
    /// Closure-shaped so the child's guard lives in this function's frame; a
    /// version returning a `HandleScope` would have to own its guard alongside
    /// the `Arc` it borrows from, which is not expressible without `unsafe`.
    ///
    /// `token` must name a group *different* from the one this scope already
    /// holds: `crate::sync::Mutex` is not reentrant, so locking the same group
    /// twice deadlocks the calling thread forever, with no diagnostic and no
    /// `SqlReturn`. A token from the already-held group (e.g. the scope's own
    /// token, or a statement belonging to a connection whose group this scope
    /// holds) is treated as a no-op instead: re-entering a group one already
    /// holds needs no second acquisition, so `f` runs directly against this
    /// scope and a [`tracing::warn!`] records the deviation. This runs
    /// identically in every build profile, because a debug-only guard such as
    /// `debug_assert!` would leave the one branch that actually prevents the
    /// deadlock uncovered by every test this crate runs in debug, which is
    /// all of them.
    pub fn with_child_group<R>(
        &mut self,
        token: *mut c_void,
        f: impl FnOnce(&mut HandleScope<'_>) -> R,
    ) -> Result<R, OdbcError> {
        // Re-entering a group this scope already holds is a no-op, not a
        // nested acquisition: the lock is not reentrant, so taking it again
        // would hang the application thread with no diagnostic and no
        // SqlReturn. The one legitimate nesting is environment-then-
        // connection, where the groups differ.
        self.with_child_group_in(registry(), token, f)
    }

    /// The body of [`Self::with_child_group`], against an explicit registry.
    ///
    /// Split out so the loom model can drive the crate's **real** nesting path
    /// rather than a hand-written imitation of it. `registry()` panics outside
    /// an active `loom::model` and cannot be called from inside one either (a
    /// `static` runs its initializer once, while loom replays the closure many
    /// times), so a model restricted to `with_child_group` could only lock two
    /// `GroupLock`s of its own in the right order, which proves the ordering
    /// rule is safe to follow, not that this function follows it. Taking the
    /// registry as a parameter is what closes that gap: a regression reversing
    /// the acquisition order here now fails
    /// `env_before_connection_cannot_deadlock`.
    pub(crate) fn with_child_group_in<R>(
        &mut self,
        reg: &Registry,
        token: *mut c_void,
        f: impl FnOnce(&mut HandleScope<'_>) -> R,
    ) -> Result<R, OdbcError> {
        if self.holds_in(reg, token) {
            tracing::warn!(
                "with_child_group: token is already in the held group; not re-acquiring"
            );
            return Ok(f(self));
        }
        let group = reg.group_of(token).ok_or(OdbcError::InvalidHandle)?;
        let guard = group.lock();
        let mut child = HandleScope::new(Some(Arc::clone(&group)), Some(&guard));
        let result = f(&mut child);
        drop(guard);
        Ok(result)
    }

    /// Run `f` holding `token`'s group, releasing it before returning.
    ///
    /// `SQLCopyDesc`'s phase one, and the crate's only acquisition outside
    /// [`panic_safe`] that is not the called handle's own group: the lock it needs
    /// belongs to the *source* descriptor while every diagnostic it posts belongs
    /// to the target, so `panic_safe`, which locks the handle it is given, is
    /// the wrong tool. Phase two is an ordinary `panic_safe` on the target.
    ///
    /// `None` when `token` is not a live handle of `kind`, which is
    /// `SQLCopyDesc`'s `SQL_INVALID_HANDLE`-with-no-SQLSTATE case: nothing has
    /// been resolved yet, so there is no queue to post to.
    ///
    /// The return type carries no guard. That is what makes "phase one does not
    /// retain the lock" a fact this signature states rather than a comment, and
    /// it is what the loom model relies on, since a version that handed the guard
    /// back would not compile against it.
    ///
    /// [`panic_safe`]: crate::panic::panic_safe
    pub(crate) fn with_group<R>(
        token: *mut c_void,
        kind: HandleKind,
        f: impl FnOnce(&mut HandleScope<'_>) -> R,
    ) -> Option<R> {
        Self::with_group_in(registry(), token, kind, f)
    }

    /// The body of [`Self::with_group`], against an explicit registry.
    ///
    /// Split out for the same reason [`Self::with_child_group_in`] is: a loom
    /// model cannot reach `registry()`, so a model restricted to the wrapper
    /// could only lock `GroupLock`s of its own and would prove a property of
    /// itself rather than of this function. `opposite_direction_copies_cannot_deadlock`
    /// drives this.
    pub(crate) fn with_group_in<R>(
        reg: &Registry,
        token: *mut c_void,
        kind: HandleKind,
        f: impl FnOnce(&mut HandleScope<'_>) -> R,
    ) -> Option<R> {
        let group = reg.group_of_kind(token, kind)?;
        let guard = group.lock();
        let mut scope = HandleScope::new(Some(Arc::clone(&group)), Some(&guard));
        let result = f(&mut scope);
        drop(guard);
        Some(result)
    }

    /// Push a diagnostic onto whichever handle `token` names.
    ///
    /// Used by `panic_safe` on the error path. Silently does nothing for
    /// a token outside the held group or of a kind that carries no queue
    /// (descriptors), because there is no better handle to report against.
    /// Expressed through [`Self::diagnostics`] because that method already
    /// answers the same question, which queue (if any) this token names, and
    /// answers it in one registry pass. Spelling the dispatch out a second time
    /// here would cost several lookups on a path that runs on **every** error:
    /// a `holds`, then up to three `get`s each doing a `holds` of its own.
    pub fn push_diagnostic<B: Backend>(&mut self, token: *mut c_void, err: &OdbcError) {
        if let Some(queue) = self.diagnostics::<B>(token) {
            queue.push(err);
        }
    }

    /// Borrow whichever handle `token` names, for its diagnostic queue only.
    ///
    /// `SQLGetDiagRecW`/`SQLGetDiagFieldW` are the spec's own exception to
    /// clearing a handle's diagnostics at the start of a call: they read the
    /// queue and must leave it untouched, so this hands back a plain
    /// `&mut DiagnosticQueue` rather than the whole handle. Like
    /// [`Self::get`] and [`Self::push_diagnostic`], it refuses a token outside
    /// the held group. A descriptor is dispatched to
    /// [`Self::descriptor_diagnostics`], which needs no backend type.
    pub fn diagnostics<B: Backend>(&mut self, token: *mut c_void) -> Option<&mut DiagnosticQueue> {
        let (kind, addr) = {
            let held = self.group.as_ref()?;
            let (kind, addr, _parent) = registry().resolve_any_in_group(token, held)?;
            (kind, addr)
        };
        // SAFETY: the lookup confirmed this scope owns the lock guarding
        // `token`'s group, and the registry produced `addr` for exactly `kind`,
        // so the cast below matches what the corresponding `alloc_*` function
        // allocated (same reasoning as [`Self::get`]).
        match kind {
            HandleKind::Env => {
                Some(unsafe { &mut (*(addr as *mut EnvironmentHandle<B>)).diagnostics })
            }
            HandleKind::Dbc => {
                Some(unsafe { &mut (*(addr as *mut ConnectionHandle<B>)).diagnostics })
            }
            HandleKind::Stmt => {
                Some(unsafe { &mut (*(addr as *mut StatementHandle<B>)).diagnostics })
            }
            HandleKind::Desc => self.descriptor_diagnostics(token),
        }
    }

    /// Borrow a descriptor's diagnostic queue.
    ///
    /// `SQLGetDescField`, `SQLSetDescField` and `SQLSetDescRec` all say their
    /// SQLSTATE "can be obtained by calling **SQLGetDiagRec** with a *HandleType*
    /// of SQL_HANDLE_DESC", so each descriptor carries a queue of its own.
    ///
    /// Not generic in `B`, unlike its three siblings in [`Self::diagnostics`]:
    /// resolving a descriptor needs no backend type, because a `Descriptor` is
    /// not parameterised by one.
    pub fn descriptor_diagnostics(&mut self, token: *mut c_void) -> Option<&mut DiagnosticQueue> {
        Some(&mut self.descriptor(token).ok()?.diagnostics)
    }

    /// One of a statement's descriptors, by role.
    ///
    /// The single door onto descriptor storage. It exists so that no call site
    /// names a field of [`StatementHandle`]: which allocation a role resolves to
    /// is this function's business: an application-supplied descriptor for the
    /// ARD or APD when one has been set, the implicit one otherwise.
    ///
    /// Returns only the descriptor. A caller that also needs the statement, such
    /// as the IRD's computed view or `SQL_DESC_COUNT`, wants
    /// [`Self::stmt_with_desc`].
    pub fn desc_of<B: Backend>(
        &mut self,
        stmt_token: *mut c_void,
        role: DescriptorRole,
    ) -> Result<&mut Descriptor, OdbcError> {
        let token = self
            .get::<StatementHandle<B>>(stmt_token)?
            .descriptor_token(role);
        self.descriptor(token)
    }

    /// A descriptor, by its own token.
    ///
    /// Sound as a plain [`Self::get`] because every descriptor is its own
    /// allocation carrying its own role; see [`Descriptor`]'s doc comment for
    /// why that is enough for the registry to check.
    pub fn descriptor(&mut self, token: *mut c_void) -> Result<&mut Descriptor, OdbcError> {
        self.get::<Descriptor>(token)
    }

    /// The statement a descriptor was allocated with, or `None` when it has
    /// none.
    ///
    /// Read from `Slot::parent`, which `alloc_descriptor` records. An
    /// application-allocated descriptor is parented to a *connection* instead, so
    /// this answers `None` for one, which is what the IRD paths use to tell "no
    /// column metadata is reachable from here" from "the statement has none yet".
    pub fn descriptor_stmt(&mut self, token: *mut c_void) -> Option<*mut c_void> {
        let held = self.group.as_ref()?;
        let (kind, _addr, parent) = registry().resolve_any_in_group(token, held)?;
        if kind != HandleKind::Desc {
            return None;
        }
        let parent = parent?;
        let (parent_kind, _, _) = registry().resolve_any_in_group(parent, held)?;
        (parent_kind == HandleKind::Stmt).then_some(parent)
    }

    /// Borrow a statement and one of its descriptors together.
    ///
    /// Sound for exactly the reason [`Self::stmt_with_parent`] is: neither is
    /// reachable from the other. The statement holds descriptor **tokens**
    /// (`*mut c_void`), not typed pointers the compiler could follow, and a
    /// [`Descriptor`] holds no back-pointer to any statement. This combinator was
    /// forbidden while the four descriptors were `Box` fields of the statement and
    /// therefore *were* reachable through its `&mut`; making each its own
    /// registered allocation is what removed that.
    ///
    /// Needed by the IRD, whose fields are computed from the statement's column
    /// metadata, and by `SQL_DESC_COUNT`.
    pub fn stmt_with_desc<B: Backend>(
        &mut self,
        stmt_token: *mut c_void,
        desc_token: *mut c_void,
    ) -> Result<(&mut StatementHandle<B>, &mut Descriptor), OdbcError> {
        let stmt_addr = {
            let stmt: &mut StatementHandle<B> = self.get(stmt_token)?;
            std::ptr::from_mut(stmt)
        };
        let desc_addr = {
            let desc: &mut Descriptor = self.get(desc_token)?;
            std::ptr::from_mut(desc)
        };
        // As in `stmt_with_parent`: this pins only the weaker fact that the two
        // are different allocations. The reason the references cannot alias is
        // that neither handle is reachable from the other.
        debug_assert_ne!(stmt_addr as usize, desc_addr as usize);
        // SAFETY: both addresses came from `get`, which validated each token
        // against the registry and confirmed it belongs to the group this scope
        // holds. Neither handle is reachable from the other, so the two `&mut`s
        // cannot alias, which is the same argument `stmt_with_parent` makes; see
        // its comment for why reborrowing `self` between the two lookups touches
        // nothing either pointer refers to.
        Ok(unsafe { (&mut *stmt_addr, &mut *desc_addr) })
    }

    /// Phase one of `SQLCopyDesc`: materialise a descriptor's contents.
    ///
    /// The caller holds only this descriptor's group and releases it before
    /// touching the target. An IRD is materialised from the owning statement's
    /// column metadata, which is why this resolves the parent; a source IRD on a
    /// statement that has not been prepared or executed is `HY007`, per
    /// `SQLCopyDesc`'s own row for it.
    pub fn snapshot_descriptor<B: Backend>(
        &mut self,
        token: *mut c_void,
    ) -> Result<DescriptorSnapshot, OdbcError> {
        let role = self.descriptor(token)?.role;
        let mut snapshot = if role == DescriptorRole::Ird {
            self.snapshot_ird::<B>(token)?
        } else {
            let desc = self.descriptor(token)?;
            DescriptorSnapshot {
                records: desc.records.clone(),
                attrs: desc.attrs.clone(),
            }
        };
        self.merge_statement_header_fields::<B>(token, role, &mut snapshot.attrs)?;
        Ok(snapshot)
    }

    /// Fold the two header fields an IRD or IPD keeps on its owning
    /// **statement** into a snapshot's `attrs`.
    ///
    /// `SQL_DESC_ARRAY_STATUS_PTR` and `SQL_DESC_ROWS_PROCESSED_PTR` are
    /// `SQL_ATTR_ROW_STATUS_PTR` / `SQL_ATTR_ROWS_FETCHED_PTR` on an IRD and
    /// `SQL_ATTR_PARAM_STATUS_PTR` / `SQL_ATTR_PARAMS_PROCESSED_PTR` on an IPD,
    /// and those four deliberately stay in [`StatementHandle::attrs`] rather
    /// than on a descriptor header; see [`HeaderOwner`]. A snapshot built from
    /// `Descriptor::attrs` alone therefore drops them, and `SQLCopyDesc` is
    /// explicit that it must not: "All fields of the descriptor, except
    /// SQL_DESC_ALLOC_TYPE ..., are copied, whether or not the field is defined
    /// for the destination descriptor."
    ///
    /// Keyed by the `SQL_DESC_*` field, as [`Descriptor::attrs`] is, so the
    /// target side routes them by **its own** role rather than by the source's,
    /// which is what lets an IRD's status pointer land on an APD's header and
    /// an ARD's on an IPD's statement.
    ///
    /// No extra lock: a descriptor and its statement always share one group,
    /// and the caller already holds it.
    fn merge_statement_header_fields<B: Backend>(
        &mut self,
        token: *mut c_void,
        role: DescriptorRole,
        attrs: &mut std::collections::HashMap<u16, usize>,
    ) -> Result<(), OdbcError> {
        if !matches!(role, DescriptorRole::Ird | DescriptorRole::Ipd) {
            return Ok(());
        }
        let Some(stmt_token) = self.descriptor_stmt(token) else {
            return Ok(());
        };
        let stmt: &mut StatementHandle<B> = self.get(stmt_token)?;
        for field in [Desc::ArrayStatusPtr, Desc::RowsProcessedPtr] {
            let Some(attr) = crate::descriptor::header_attribute(role, field) else {
                continue;
            };
            if let Some(value) = stmt.plain_attr_get(attr as i32) {
                attrs.insert(field as u16, value);
            }
        }
        Ok(())
    }

    /// [`Self::snapshot_descriptor`] for an IRD, whose records are computed
    /// rather than stored.
    ///
    /// Built from `col_attr::get_column_attribute`, the same function
    /// `SQLColAttributeW` and `SQLGetDescField`'s IRD path use, so the three
    /// cannot disagree about one column.
    fn snapshot_ird<B: Backend>(
        &mut self,
        token: *mut c_void,
    ) -> Result<DescriptorSnapshot, OdbcError> {
        let attrs = self.descriptor(token)?.attrs.clone();
        // An IRD is always one of a statement's four, so a missing statement is
        // unreachable rather than a case to answer for.
        let Some(stmt_token) = self.descriptor_stmt(token) else {
            return Err(OdbcError::general(
                "An implementation row descriptor has no owning statement",
                crate::types::SqlState::general_error(),
            ));
        };
        let stmt: &mut StatementHandle<B> = self.get(stmt_token)?;
        // Spec HY007, the same wording `read_ird_field` uses: "The fields of an
        // IRD have a default value only after the statement has been prepared or
        // executed and the IRD has been populated ... Until the IRD has been
        // populated, any attempt to gain access to a field of an IRD will return
        // an error."
        let Some(statement) = stmt.statement.as_mut().filter(|s| s.column_count() > 0) else {
            return Err(OdbcError::general(
                "The IRD is not populated: the statement has not been prepared or executed",
                crate::types::SqlState::associated_statement_not_prepared(),
            ));
        };
        let column_count = statement.column_count();
        let mut records = std::collections::HashMap::new();
        for column in 1..=u16::try_from(column_count).unwrap_or(0) {
            let described = statement.describe_col(column)?;
            records.insert(
                column,
                crate::types::col_attr::record_from_column(&described, column_count)?,
            );
        }
        Ok(DescriptorSnapshot { records, attrs })
    }

    /// A descriptor header field's stored value on one of a statement's
    /// descriptors, or `None` if never set.
    ///
    /// Keyed by the `SQL_DESC_*` field rather than by the statement attribute
    /// that names it; see [`Descriptor::attrs`] for why.
    pub fn header_field_get<B: Backend>(
        &mut self,
        stmt_token: *mut c_void,
        owner: HeaderOwner,
        field: Desc,
    ) -> Result<Option<usize>, OdbcError> {
        let role = match owner {
            HeaderOwner::Ard => DescriptorRole::Ard,
            HeaderOwner::Apd => DescriptorRole::Apd,
        };
        Ok(self
            .desc_of::<B>(stmt_token, role)?
            .attrs
            .get(&(field as u16))
            .copied())
    }

    /// [`Self::header_field_get`], for writing.
    pub fn header_field_set<B: Backend>(
        &mut self,
        stmt_token: *mut c_void,
        owner: HeaderOwner,
        field: Desc,
        value: usize,
    ) -> Result<(), OdbcError> {
        let role = match owner {
            HeaderOwner::Ard => DescriptorRole::Ard,
            HeaderOwner::Apd => DescriptorRole::Apd,
        };
        self.desc_of::<B>(stmt_token, role)?
            .attrs
            .insert(field as u16, value);
        Ok(())
    }

    /// Where the value of the statement attribute `attribute` is stored: a
    /// descriptor's header when ODBC defines the attribute as one, the
    /// statement's own bag otherwise.
    ///
    /// Both the set and the get path route through [`HeaderOwner::of`] here, so
    /// the two cannot disagree about which map to look in. It lives on the scope
    /// rather than on [`StatementHandle`] because a descriptor is not a field of
    /// a statement: resolving one is a registry lookup.
    pub fn attr_get<B: Backend>(
        &mut self,
        stmt_token: *mut c_void,
        attribute: i32,
    ) -> Result<Option<usize>, OdbcError> {
        match HeaderOwner::of(statement_attribute_from_raw(attribute)) {
            Some((owner, field)) => self.header_field_get::<B>(stmt_token, owner, field),
            None => Ok(self
                .get::<StatementHandle<B>>(stmt_token)?
                .plain_attr_get(attribute)),
        }
    }

    /// [`Self::attr_get`], for writing.
    pub fn attr_set<B: Backend>(
        &mut self,
        stmt_token: *mut c_void,
        attribute: i32,
        value: usize,
    ) -> Result<(), OdbcError> {
        match HeaderOwner::of(statement_attribute_from_raw(attribute)) {
            Some((owner, field)) => self.header_field_set::<B>(stmt_token, owner, field, value),
            None => {
                self.get::<StatementHandle<B>>(stmt_token)?
                    .plain_attr_set(attribute, value);
                Ok(())
            }
        }
    }

    /// Borrow a statement, its connection, and its two parameter descriptors'
    /// records, all at once.
    ///
    /// `SQLExecute`, `SQLExecDirect` and `SQLParamData` each need all four in one
    /// call: the connection to reach the backend, the statement for its
    /// parameter count and data-at-execution state, and the two descriptors for
    /// the bindings. Resolving them one at a time is not possible while any is
    /// borrowed, and cloning the record maps per execution would make a second
    /// copy of the storage that is deliberately single.
    ///
    /// Sound for the same reason [`Self::stmt_with_parent`] and
    /// [`Self::stmt_with_desc`] are, applied to four allocations rather than two:
    /// each is a separate registry slot, and none is reachable from any other. A
    /// statement holds only opaque tokens; a `ConnectionHandle` holds no list of
    /// its statements; a [`Descriptor`] holds no back-pointer at all.
    ///
    /// # Safety
    ///
    /// The APD's `SQL_DESC_BIND_OFFSET_PTR` must be null or point to a valid
    /// `SQLULEN`, which is the application's undertaking when it set
    /// `SQL_ATTR_PARAM_BIND_OFFSET_PTR`. This function dereferences it, because
    /// the offset belongs to the call rather than to any one binding and the
    /// spec resolves it once, at execution time (see
    /// [`crate::descriptor::BindOffset`]). Every caller is already an FFI entry
    /// point marshalling application pointers under that same contract.
    pub unsafe fn stmt_with_parent_and_params<B: Backend>(
        &mut self,
        stmt_token: *mut c_void,
    ) -> Result<
        (
            &mut StatementHandle<B>,
            &mut ConnectionHandle<B>,
            ParamRecords<'_>,
        ),
        OdbcError,
    > {
        let (apd_token, ipd_token) = {
            let stmt: &mut StatementHandle<B> = self.get(stmt_token)?;
            (
                stmt.descriptor_token(DescriptorRole::Apd),
                stmt.descriptor_token(DescriptorRole::Ipd),
            )
        };
        let apd = std::ptr::from_mut(self.descriptor(apd_token)?);
        // SAFETY: forwarded from this function's own contract. Read here rather
        // than per record so one call applies one offset to every parameter.
        let bind_offset = unsafe { (*apd).bind_offset() };
        let ipd = std::ptr::from_mut(self.descriptor(ipd_token)?);
        let (stmt, conn) = self.stmt_with_parent::<B>(stmt_token)?;
        let stmt_addr = std::ptr::from_mut(stmt);
        let conn_addr = std::ptr::from_mut(conn);
        // As in the two-way combinators, this pins only the weaker fact that the
        // four are distinct allocations.
        debug_assert_ne!(apd as usize, ipd as usize);
        debug_assert_ne!(stmt_addr as usize, conn_addr as usize);
        // SAFETY: every address came from a validated registry lookup in the group
        // this scope holds, and no two of the four handles are reachable from each
        // other; see this function's doc comment. `self` holds no pointer into
        // any of them, so the reborrows between the lookups touch nothing they
        // refer to (the argument spelled out in `stmt_with_parent`).
        Ok(unsafe {
            (
                &mut *stmt_addr,
                &mut *conn_addr,
                ParamRecords {
                    apd: &(*apd).records,
                    ipd: &(*ipd).records,
                    bind_offset,
                },
            )
        })
    }

    /// Remove every parameter binding, from both descriptors.
    ///
    /// `SQLFreeStmt(SQL_RESET_PARAMS)`. Clearing one map and not the other leaves
    /// exactly the split state [`ParamRecords::get`] reports as an internal
    /// error, so the two clears stay in one place.
    pub fn clear_param_records<B: Backend>(
        &mut self,
        stmt_token: *mut c_void,
    ) -> Result<(), OdbcError> {
        self.desc_of::<B>(stmt_token, DescriptorRole::Apd)?
            .records
            .clear();
        self.desc_of::<B>(stmt_token, DescriptorRole::Ipd)?
            .records
            .clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::DescriptorRecord;
    use crate::panic::panic_safe;
    use crate::test_utils::{MockBackend, alloc_env_conn_stmt, cleanup_env_conn_stmt, with_handle};
    use crate::types::SqlReturn;

    /// The check that makes the whole scheme real: a token from a *different*
    /// group must be refused, because the scope does not hold that group's
    /// lock. Without this, `scope.get` would validate a token's kind and
    /// liveness but not its group, reaching a handle no lock protects it
    /// from, which is the exact unguarded access this type exists to close off.
    #[test]
    fn a_token_outside_the_locked_group_is_refused() {
        unsafe {
            let (env_a, conn_a, stmt_a) = alloc_env_conn_stmt();
            let (env_b, conn_b, stmt_b) = alloc_env_conn_stmt();

            let ret = panic_safe::<MockBackend, _>(conn_a, |scope| {
                // Its own group: fine.
                scope.get::<ConnectionHandle<MockBackend>>(conn_a)?;
                // Another connection's group, whose lock this scope does not
                // hold.
                let other = scope.get::<ConnectionHandle<MockBackend>>(conn_b);
                assert!(
                    matches!(other, Err(OdbcError::InvalidHandle)),
                    "a handle from an unlocked group must not be reachable"
                );
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);

            cleanup_env_conn_stmt(env_a, conn_a, stmt_a);
            cleanup_env_conn_stmt(env_b, conn_b, stmt_b);
        }
    }

    /// Each of a statement's four descriptor tokens resolves to a descriptor
    /// carrying that role, and to the statement it was allocated with. The role
    /// is what `HY091` and the IRD's read-only rule are decided from, so a token
    /// resolving as the wrong role would answer the wrong SQLSTATE for every
    /// field.
    #[test]
    fn each_descriptor_token_resolves_to_its_statement_and_role() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let tokens =
                with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                    [
                        (
                            handle.descriptor_token(DescriptorRole::Ard),
                            DescriptorRole::Ard,
                        ),
                        (
                            handle.descriptor_token(DescriptorRole::Apd),
                            DescriptorRole::Apd,
                        ),
                        (
                            handle.descriptor_token(DescriptorRole::Ird),
                            DescriptorRole::Ird,
                        ),
                        (
                            handle.descriptor_token(DescriptorRole::Ipd),
                            DescriptorRole::Ipd,
                        ),
                    ]
                });

            for (token, expected_role) in tokens {
                let ret = panic_safe::<MockBackend, _>(token, |scope| {
                    assert_eq!(scope.descriptor(token)?.role, expected_role);
                    // Every descriptor of one statement is parented to the same
                    // statement, whichever token was used.
                    assert_eq!(scope.descriptor_stmt(token), Some(stmt));
                    Ok(SqlReturn::SUCCESS)
                });
                assert_eq!(ret, SqlReturn::SUCCESS, "{expected_role:?} did not resolve");
            }

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A statement token is not a descriptor token, even though a descriptor is
    /// now resolved by the same [`HandleScope::get`] every other kind uses: the
    /// registry's kind compare is what refuses it.
    #[test]
    fn descriptor_refuses_a_non_descriptor_token() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();

            let ret = panic_safe::<MockBackend, _>(stmt, |scope| {
                let result = scope.descriptor(stmt);
                assert!(
                    matches!(result, Err(OdbcError::InvalidHandle)),
                    "a statement token resolved as a descriptor"
                );
                assert_eq!(
                    scope.descriptor_stmt(stmt),
                    None,
                    "a statement token has no owning descriptor to report"
                );
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);

            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A statement and its parent connection share a group, so both are
    /// reachable from one acquisition, as the `ffi/metadata.rs` sites need.
    #[test]
    fn a_statement_and_its_parent_come_from_one_acquisition() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            // `alloc_env_conn_stmt` deliberately leaves the connection
            // unconnected (`ffi/execute.rs`'s
            // `exec_direct_not_connected_returns_error` relies on exactly
            // that), so connect it here to exercise `c.connection.is_some()`
            // below.
            let input = "Host=localhost;Port=8080;Database=test;User=me";
            let wide: Vec<u16> = input.encode_utf16().collect();
            let connect_ret = crate::ffi::connect::sql_driver_connect_w::<MockBackend>(
                conn,
                std::ptr::null_mut(),
                wide.as_ptr(),
                wide.len() as i16,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(connect_ret, SqlReturn::SUCCESS);

            let ret = panic_safe::<MockBackend, _>(stmt, |scope| {
                let (s, c) = scope.stmt_with_parent::<MockBackend>(stmt)?;
                assert!(s.statement.is_none());
                assert!(c.connection.is_some());
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);

            // `SQLDisconnect` frees every statement on the connection as part
            // of tearing it down, so `stmt` is already gone (a stale token, not
            // a double-free, since `unregister` returns `None` before any
            // `Box::from_raw`) by the time `cleanup_env_conn_stmt` reaches it;
            // pass null in its place rather than the now-stale `stmt` value, so
            // this call site does not contradict `cleanup_env_conn_stmt`'s
            // documented "must be live" precondition. `free_connection` would
            // otherwise refuse to free a still-connected connection (HY010),
            // leaking its registry slot, so assert the disconnect actually
            // succeeded rather than silently leaking on a future regression.
            let disconnect_ret = crate::ffi::connect::sql_disconnect::<MockBackend>(conn);
            assert_eq!(disconnect_ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, std::ptr::null_mut());
        }
    }

    /// `SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, ..)` has no handle to
    /// lock, and must still run.
    #[test]
    fn a_null_handle_yields_a_scope_holding_no_lock() {
        let ret = unsafe {
            panic_safe::<MockBackend, _>(std::ptr::null_mut(), |_scope| Ok(SqlReturn::SUCCESS))
        };
        assert_eq!(ret, SqlReturn::SUCCESS);
    }

    /// The other half of the null-handle case: a scope holding no group must
    /// still refuse a token from a real, live group: `holds` must treat "no
    /// group held" as "nothing is reachable," not as "anything goes."
    #[test]
    fn a_null_handle_scope_still_refuses_a_live_token() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe::<MockBackend, _>(std::ptr::null_mut(), |scope| {
                let result = scope.get::<ConnectionHandle<MockBackend>>(conn);
                assert!(
                    matches!(result, Err(OdbcError::InvalidHandle)),
                    "a scope holding no group must not reach any live handle"
                );
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A panic inside the closure releases the group rather than wedging every
    /// later call on that connection.
    #[test]
    fn a_panic_releases_the_group() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe::<MockBackend, _>(conn, |_scope| {
                panic!("test panic");
            });
            assert_eq!(ret, SqlReturn::ERROR);

            // The group is free: a second call on the same connection works.
            let ret = panic_safe::<MockBackend, _>(conn, |scope| {
                scope.get::<ConnectionHandle<MockBackend>>(conn)?;
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `with_child_group`'s primary use (`SQLEndTran(SQL_HANDLE_ENV)`): nest a
    /// second, distinct group's lock inside a scope that already holds a
    /// different one, and reach the child group's own handle through the
    /// nested scope. This is the interface's only correctness test; the test
    /// below covers the deadlock guard.
    #[test]
    fn with_child_group_locks_a_distinct_group_and_reaches_its_handle() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe::<MockBackend, _>(env, |scope| {
                let reached = scope.with_child_group(conn, |child| {
                    child.get::<ConnectionHandle<MockBackend>>(conn).is_ok()
                })?;
                assert!(
                    reached,
                    "the nested scope must be able to reach the child group's own handle"
                );
                // The outer scope's own group is still held and usable
                // afterward: with_child_group must not have released it.
                scope.get::<EnvironmentHandle<MockBackend>>(env)?;
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `crate::sync::Mutex` is not reentrant, so passing a token from the
    /// group this scope already holds would deadlock the calling thread
    /// forever, with no diagnostic and no `SqlReturn`, which is the hazard
    /// `with_child_group`'s guard exists to close, and identically in every
    /// build profile: removing the early return here hangs this test rather
    /// than failing it, which is exactly why the guard runs unconditionally
    /// instead of only under `debug_assertions`.
    #[test]
    fn with_child_group_on_the_scope_s_own_group_is_a_no_op_not_a_deadlock() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe::<MockBackend, _>(env, |scope| {
                // `env` belongs to the group `scope` already holds.
                let ran = scope.with_child_group(env, |_| true)?;
                assert!(
                    ran,
                    "with_child_group must still run f, against this scope, rather than \
                     silently dropping the call"
                );
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(
                ret,
                SqlReturn::SUCCESS,
                "re-entering the already-held group must be a no-op, not an error or a hang"
            );
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// A snapshot is an owned copy: the lock can be released before it is used,
    /// which is what lets `SQLCopyDesc` span two connections without ever holding
    /// two group locks.
    #[test]
    fn a_snapshot_outlives_the_lock_it_was_taken_under() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ard = with_handle::<MockBackend, StatementHandle<MockBackend>, _>(stmt, |handle| {
                handle.descriptor_token(DescriptorRole::Ard)
            });
            let snapshot = returning(stmt, |scope| {
                scope
                    .desc_of::<MockBackend>(stmt, DescriptorRole::Ard)?
                    .records
                    .insert(1, DescriptorRecord::default());
                scope.snapshot_descriptor::<MockBackend>(ard)
            })
            .expect("the ARD is live");
            // The lock is gone; the snapshot is still readable.
            assert_eq!(snapshot.records.len(), 1);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// Run `f` under `token`'s group and hand back its value.
    ///
    /// [`panic_safe`] returns a `SqlReturn`, which is the wrong shape for a test
    /// whose subject is the value a scope method produced. Modelled on
    /// `panic_safe`'s first three lines.
    ///
    /// # Safety
    ///
    /// `token` must be a live handle.
    unsafe fn returning<R>(
        token: *mut c_void,
        f: impl FnOnce(&mut HandleScope<'_>) -> Result<R, OdbcError>,
    ) -> Result<R, OdbcError> {
        let group = registry().group_of(token);
        let guard = group.as_ref().map(|g| g.lock());
        let mut scope = HandleScope::new(group.clone(), guard.as_ref());
        f(&mut scope)
    }

    /// `desc_of` reaches the same storage the statement's own field does.
    ///
    /// The point of the indirection: once a descriptor is its own allocation the
    /// field is gone, and every caller must already be going through this.
    #[test]
    fn desc_of_reaches_a_statements_own_descriptor() {
        unsafe {
            let (env, conn, stmt) = alloc_env_conn_stmt();
            let ret = panic_safe::<MockBackend, _>(stmt, |scope| {
                let ard = scope.desc_of::<MockBackend>(stmt, DescriptorRole::Ard)?;
                ard.records.insert(1, DescriptorRecord::default());
                let again = scope.desc_of::<MockBackend>(stmt, DescriptorRole::Ard)?;
                assert!(again.records.contains_key(&1));
                let apd = scope.desc_of::<MockBackend>(stmt, DescriptorRole::Apd)?;
                assert!(
                    !apd.records.contains_key(&1),
                    "the APD is a different descriptor from the ARD"
                );
                Ok(SqlReturn::SUCCESS)
            });
            assert_eq!(ret, SqlReturn::SUCCESS);
            cleanup_env_conn_stmt(env, conn, stmt);
        }
    }

    /// `HandleScope` must not be `Send`.
    ///
    /// It is only valid while the group's `MutexGuard` is held, and a guard is
    /// itself `!Send` because releasing a lock from a thread other than the one
    /// that took it is undefined for the underlying primitive. A `Send` scope
    /// could be handed to a scoped thread, which would then reach handle
    /// contents while claiming a lock held on another thread.
    ///
    /// This is a compile-time assertion, not a runtime one: it fails to build if
    /// `_guard` ever goes back to a `PhantomData` that carries `Send`.
    const _: () = {
        trait AmbiguousIfSend<A> {
            fn some_item() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

        // The type parameter must be an inference variable: that is what makes
        // both impls candidates. Resolution succeeds only because exactly one
        // applies, i.e. `HandleScope` is not `Send`. If it became `Send` the
        // second impl would apply too and this fails to compile as ambiguous.
        // Naming `AmbiguousIfSend<()>` explicitly here would pick that impl
        // directly and assert nothing.
        let _ = <HandleScope<'static> as AmbiguousIfSend<_>>::some_item;
    };
}
