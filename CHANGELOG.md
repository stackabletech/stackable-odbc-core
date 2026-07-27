# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `OdbcError::with_native_error` and `OdbcError::with_source`, plus the
  `OdbcError::native_error` and `OdbcError::cause` accessors. A driver holding
  its data source's own error code and the failure that caused it can now carry
  both across the FFI boundary instead of flattening them into a message string.
  `SQLGetDiagRec` reports the code verbatim through `NativeErrorPtr`, where
  every driver's native code previously reached the application as `0`, and the
  diagnostic message now includes the whole causal chain rather than only its
  outermost link.
- `FunctionId` gains the 16 `SQL_API_*` ids it was missing (`SQLError`,
  `SQLFreeConnect`, `SQLFreeEnv`, `SQLSetParam`, `SQLTransact`,
  `SQLGetConnectOption`, `SQLGetStmtOption`, `SQLSetConnectOption`,
  `SQLSetStmtOption`, `SQLExtendedFetch`, `SQLParamOptions`,
  `SQLSetScrollOptions`, `SQLDrivers`, `SQLAllocHandleStd`, `SQLBindParam`,
  `SQLCopyDesc`) and is now `#[non_exhaustive]`. A test pins every value against
  `sql.h`/`sqlext.h` and round-trips `function_id_from_raw` — every other enum in
  the crate had such a test; this one did not.
- `test-support`, a default-off feature gating the `conformance` module. It is
  test code that was compiled into every driver's production binary, and it
  reaches an `unreachable!()` through a public `unsafe fn` taking a
  caller-supplied `u16`. Driver test suites enable it under `[dev-dependencies]`.
- `pub use odbc_sys;`. `odbc-sys` is a public dependency appearing in trait
  signatures, but was not re-exported, so a driver declared its own with nothing
  pinning it to core's version — and two versions of a `#[repr(C)]` enum are two
  different types to the compiler.
- `Backend::sensitive_connect_keywords`, naming the connection-string keywords
  whose values must never be logged, plus
  `ConnectParams::declare_sensitive_keywords` which the generic FFI entry points
  use to apply it. The backend owns its connection-string vocabulary, so it is
  the only party that can identify its own secrets: core sees `WalletLocation`,
  `OAuthAssertion` or `KeyStorePin` as ordinary keywords. Because the list is
  attached to the `ConnectParams` itself rather than applied at each log site, a
  driver's own `{:?}` on the params it receives in `connect` redacts too.
  Defaulted to empty rather than required, because core's substring heuristic
  stays in force underneath: declaring nothing still covers the common shapes,
  so the default understates rather than leaks, and a declaration can only ever
  add redaction.
- Initial extraction of `stackable-odbc-core` into its own repository.
  Provides the database-independent ODBC framework:
  protocol logic, handle allocation and tag validation, UTF-16 marshalling,
  diagnostics, panic safety, and the generic implementations of the ODBC FFI
  entry points, exported for a concrete backend via the `forward_ffi!` macro.
- `CursorBehavior` and the `SQL_CB_DELETE` / `SQL_CB_CLOSE` / `SQL_CB_PRESERVE`
  constants, modelling what `SQLEndTran` does to open cursors.
- `Backend::cursor_commit_behavior` and `Backend::cursor_rollback_behavior`.
  Both default to `CursorBehavior::Preserve`. A backend that supports
  transactions should declare its data source's actual behaviour here; the
  value drives both `SQLGetInfoW` and what `SQLEndTran` does to statements.
- `SQL_CURSOR_COMMIT_BEHAVIOR` constant (23), derived from
  `odbc_sys::InfoType::CursorCommitBehaviour`.
- The `SQL_AT_*` bitmask constants for the `SQL_ALTER_TABLE` (86) info type, so
  a driver can describe its `ALTER TABLE` support by name instead of by raw
  literal. The set is now complete: `SQL_AT_ADD_COLUMN`, `SQL_AT_DROP_COLUMN`
  and `SQL_AT_ADD_CONSTRAINT` (defined in `sql.h`) and the four
  `SQL_AT_CONSTRAINT_INITIALLY_*` / `_DEFERRABLE` / `_NON_DEFERRABLE`
  deferrability bits were missing, so a driver still needed raw literals for
  them.
- The `SQL_NC_START` / `SQL_NC_END`, `SQL_CN_*`, `SQL_NNC_*`, `SQL_GB_COLLATE`,
  `SQL_SC_*` and `SQL_FN_TSI_*` constants, plus the `SQL_ROW_UPDATES` (11) and
  `SQL_PROCEDURES` (21) info type numbers, all asserted against
  `/usr/include/sql.h` and `sqlext.h` by
  `info_type_value_constants_match_sql_headers`. These are values where a typo
  cannot look empty, because zero is itself a valid claim for several of them.
- A test that enforces the backend/`SQLGetInfo` split instead of leaving it to
  review. `default_get_info_answers_are_backend_derived_or_declared_core_facts`
  evaluates `default_get_info` for two mock backends sharing no capability
  declaration; any info type answering identically for both is one core
  decided, and must be listed with the reason core is entitled to decide it —
  a fact about core's own implementation, a limit where the spec defines `0`
  as "no limit or unknown", or driver identity. Hard-coding a claim about the
  data source now fails a test naming the info type. Every item fixed in this
  release was found by hand; this is what stops the next one needing that.
- `EscapeDialect::rewrite_scalar_fn`, which receives a whole
  `{fn NAME(args)}` escape and returns the replacement text.
  `remap_scalar_fn` only swaps the identifier in front of the parentheses and
  never sees the arguments, so any scalar function whose ODBC form differs
  from the target dialect in *argument* syntax was untranslatable —
  `{fn LOCATE('b','ab')}` → `position('b' IN 'ab')`,
  `{fn TIMESTAMPADD(SQL_TSI_DAY, 1, t)}` → `date_add('day', 1, t)`, or a
  zero-argument call that must become a bare keyword with no trailing `()`
  such as `{fn CURDATE()}` → `current_date`. Because the `SQL_*_FUNCTIONS`
  bitmaps are defined in terms of the `{fn}` escape, a driver that could not
  translate one could not honestly advertise it.
  `args` arrives already escape-translated, so a nested `{fn}` or `{ts}` is
  resolved before the dialect sees it, and with string literals, quoted
  identifiers, comments and nested parentheses intact — core does not split
  on commas, which would corrupt `{fn LOCATE(',', x)}`. Splitting arguments is
  the dialect's job. `remap_scalar_fn` stays as the cheap path, and a dialect
  setting only it is unaffected.
- `Backend::set_txn_isolation`, for a backend that can switch isolation
  levels. Defaulted: a data source with exactly one level in
  `txn_isolation_options` needs no implementation, while one declaring several
  and not overriding this reports `NotImplemented` rather than accepting a
  level it would silently fail to apply.
- `StatementHandle::cursor_open`, plus the `set_result_set`,
  `set_prepared_statement` and `discard_result_set` helpers that maintain it.
  `StatementHandle::statement` no longer doubles as the answer to "is a cursor
  open?" — see the `Fixed` entry below.
- `types::ODBC_RESERVED_KEYWORDS`, the reserved-word list from Appendix C of
  the ODBC specification. `SQL_KEYWORDS` is defined as the data source's own
  keywords *excluding* these, so the list and the subtraction now live in core
  once instead of being transcribed into each driver.

### Changed

- `SQLGetInfoW` answers ten more info types instead of letting them reach the
  shape-aware default. `SQL_GETDATA_EXTENSIONS` reports
  `SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER | SQL_GD_BOUND`, which is what core's
  fetch actually supports; `SQL_LIKE_ESCAPE_CLAUSE` reports `"Y"`, which is what
  `escape.rs`'s `{escape}` translation implements; `SQL_OUTER_JOINS` is derived
  from `Backend::outer_join_capabilities` so the two cannot disagree; and the
  batch, parameter-array, async and pooling group all report what core's
  synchronous, single-parameter-set implementation does. `""` and `0` were not
  legal values for several of these.
- The `default_get_info` guard test also checks info types with **no** arm.
  Previously it compared only the answers that existed, so a type answered
  nowhere reached the shape-aware default (`0` or `""`) with nothing naming that
  as intended — bypassing the "a claim about the data source must be declared"
  design entirely. The 24 that legitimately take the default are now listed in
  `SHAPE_DEFAULT_IS_THE_ANSWER` with the reason, and adding an info type without
  either an arm or an entry fails a test that names it.
- `SQL_PARC_*`, `SQL_PAS_*` and `SQL_ASYNC_NOTIFICATION_*` constants.
- `CORE_EXPORTED_FUNCTIONS` and `CORE_UNEXPORTED_FUNCTIONS`, partitioning
  `FunctionId` by whether `forward_ffi!` generates a C entry point for it. Nine
  ids name real ODBC functions core does not export (`SQLGetDescRec`,
  `SQLCancelHandle`, `SQLDataSources` and six others), and `SQLGetFunctions` is
  what the Windows Driver Manager builds its dispatch table from — reporting one
  of them supported hands the DM a null pointer to call. A driver's
  `get_functions` should be built from `CORE_EXPORTED_FUNCTIONS` rather than
  hand-listed. A test in the `forward_ffi!` expansion module takes the address
  of every entry, so listing a function without a matching macro arm is a
  compile error.
- **Breaking:** `Backend::identifier_case`, a required capability method for
  `SQL_IDENTIFIER_CASE`. Required rather than defaulted because no default can
  be legal: the spec defines `SQL_IC_UPPER` (1) through `SQL_IC_MIXED` (4), and
  the shape-aware fallback produced `0`. Unlike the capability methods where
  zero is a substantive claim core could understate, every possible value here
  is a different assertion about how the data source folds unquoted
  identifiers, which is what an application reads to decide how to quote the
  SQL it generates.
- `default_get_info` takes only the `InfoType`; the catalog column widths come
  from `Backend::catalog_result_column_widths` on the type parameter it already
  has. Both call sites in the sibling drivers passed exactly
  `B::catalog_result_column_widths()`, and taking the widths separately allowed
  a caller to supply values disagreeing with what the same backend reports
  everywhere else — the `SQL_MAX_*_NAME_LEN` group is derived from them.
- `SQLGetInfoW` consults `default_get_info` as part of its own fallback chain,
  after `Backend::get_info_raw` and `common_get_info_raw` and before the
  shape-aware default. It matters most before a connection exists, where there
  is no `get_info_raw` to consult: the Windows Driver Manager queries
  `SQL_DRIVER_ODBC_VER` ahead of `SQLDriverConnectW`, and reaching the `String`
  shape default of `""` there marks the driver as ODBC 2.x, which blocks 3.x
  features such as `SQL_C_SBIGINT`.
- **Breaking:** `SqlState`'s byte array is private, and the type derives
  `Clone, Copy, PartialEq, Eq, Hash`. The public field froze `[u8; 5]` as API and
  let a caller build a state that was not five ASCII characters, which made
  `as_str`'s otherwise-unreachable `"?????"` fallback reachable. `TryFrom<&str>`
  is the checked constructor for a value not known at compile time. The missing
  `PartialEq` was why no driver could write
  `assert_eq!(err.sqlstate(), SqlState::general_error())`.
- **Breaking:** `ColumnDescriptor::nullable` is a `Nullable`, not a `bool`. The
  spec defines three values and the third is not expressible as a boolean:
  `SQL_NULLABLE_UNKNOWN` is what a driver must report for a computed or
  outer-joined column whose nullability it cannot determine, and `SQLDescribeCol`
  was reporting those as `SQL_NO_NULLS` — telling an application it could skip a
  NULL check it needs.
- **Breaking:** `ColumnDescriptor` is `#[non_exhaustive]` and gains `searchable`,
  `literal_prefix`, `literal_suffix`, `table_name`, `schema_name` and
  `catalog_name`. `SQLColAttribute` hard-coded all six; a backend that tracks a
  column's origin or its type's literal form can now report it, and the previous
  values remain the defaults. Build descriptors with `ColumnDescriptor::new` and
  the `with_*` builders, which stay source-compatible as fields are added.
- **Breaking:** `EscapeDialect`, `TypeInfoRow`, `CatalogResultColumnWidths` and
  `FunctionId` are `#[non_exhaustive]`, with `with_*` builders on the first
  three. `EscapeDialect` is the cautionary case: adding `rewrite_scalar_fn` to
  it was already a silent breaking change (commit `886007b`, labelled `feat:`),
  which cost nothing only because nothing was released.
- **Breaking:** the `handles`, `panic` and `diagnostics` modules are
  `pub(crate)`. Nothing outside the crate needs them — `forward_ffi!` references
  only `$crate::ffi` and `$crate::types` — and leaving them public froze
  `StatementHandle`'s 17 fields as API for nothing. Making them private also
  surfaced genuinely dead code that being `pub` had masked: `HasKind::header`,
  implemented four times and called zero, is removed.
- **Breaking:** `Backend::Error` is now bounded by
  `Into<OdbcError> + From<OdbcError> + std::error::Error + Send + Sync + 'static`,
  and *every* `Backend` method returns `Result<_, Self::Error>`. Previously
  `connect`, `disconnect`, `exec_direct`, `prepare`, `execute`, `get_info`,
  `tables` and `columns` returned `Self::Error` while `set_autocommit`,
  `get_info_pre_connect`, `primary_keys`, `foreign_keys`, `statistics`,
  `special_columns`, `cancel`, `end_tran` and `set_txn_isolation` returned
  `OdbcError`, which forced a driver to double-convert at every call site in the
  second group. A driver adds one `impl From<OdbcError> for ItsError`; the
  `From<OdbcError>` direction is what lets a defaulted trait body construct an
  error and still name `Self::Error`, and `Into<OdbcError>` is what core uses to
  build the diagnostic.
- **Breaking:** `StatementBackend` gains its own associated `Error` with the same
  bounds; `fetch`, `get_data` and `describe_col` return it instead of
  `OdbcError`. The fetch path is the hottest error path in the crate, and it was
  the one place a driver still had to flatten its error into a string. A driver
  may use one type for both traits.
- **Breaking:** `StatementBackend::close_cursor` returns
  `Result<(), Self::Error>` instead of `()`. For a networked data source this is
  a round trip that can fail, and under `SQL_CB_CLOSE` it is the *only* thing
  that closes the cursor during `SQLEndTran` — which previously reported
  `SQL_SUCCESS` whatever happened. A failure is now recorded on the statement's
  own diagnostic queue, where the spec tells the application to look, and
  `SQLEndTran` reports it. Every statement is still visited, so one failure does
  not strand the rest.
- **Breaking:** `StatementBackend::column_count` returns `i16` instead of `u16`,
  matching the `SQLSMALLINT *` that `SQLNumResultCols` writes through. Core no
  longer clamps a value it cannot interpret; a backend that cannot express its
  count says so where it knows the real number.
- **Breaking:** `StatementBackend::row_count` returns `Option<i64>` instead of
  `Option<usize>`. `SQLRowCount` writes through a signed `SQLLEN *`, and the
  signedness is load-bearing: `SQL_NO_TOTAL` (`-1`, "cannot be determined") is a
  different answer from `None` ("not applicable to this statement"), and
  `usize` could express neither.
- **Breaking:** `StatementBackend::describe_col` returns
  `Cow<'_, ColumnDescriptor>` instead of `ColumnDescriptor`, matching `get_data`.
  A backend holding its descriptors in memory no longer clones one and its two
  `String`s on every call, which `SQLColAttribute` makes once per column per
  attribute.
- A `disconnect` that fails while unwinding a half-open connection is now logged
  rather than silently discarded. Core could not report it before, because
  `Backend::Error` carried no `Debug` or `Display` bound. A failure here can
  leave a session on the data source that the application will never reclaim,
  since it never received a connection to disconnect.
- **Breaking:** `OdbcError::General` gains `native_error` and `cause` fields and
  is now `#[non_exhaustive]` at the variant level, so it can no longer be built
  with struct-literal syntax from outside the crate. Use `OdbcError::general`
  with the `with_native_error` / `with_source` builders instead, which stay
  source-compatible as further fields are added. The enum-level
  `#[non_exhaustive]` already stopped drivers matching it exhaustively but did
  nothing to stop them constructing it, which made every future field on this
  variant a breaking change.
- **Breaking:** the `handles` module's tag-based validation is replaced by a
  generational handle registry (see `Fixed`). `HasTag` becomes `HasKind`, with
  `const KIND: HandleKind` in place of `const TAG: u32` and a `header()`
  accessor in place of `invalidate_tag()`; `HandleHeader` now records a slot and
  generation rather than a tag. `ConnectionHandle::env`, `StatementHandle::conn`,
  `EnvironmentHandle::connections` and `ConnectionHandle::statements` hold
  tokens rather than addresses.
  No application or Driver Manager sees any difference — `SQLHANDLE` is opaque
  to both — and `forward_ffi!`'s exported signatures are unchanged, so a driver
  crate needs no edit unless it reached into these internals, which nothing in
  the FFI surface requires. The module is slated to become `pub(crate)`, which
  would make this representation private for good.
- `SQL_CURSOR_COMMIT_BEHAVIOR` and `SQL_CURSOR_ROLLBACK_BEHAVIOR` now report
  `SQL_CB_PRESERVE` instead of `SQL_CB_DELETE`, and are derived from
  `Backend::cursor_commit_behavior` / `Backend::cursor_rollback_behavior`
  rather than hard-coded. Core previously advertised `SQL_CB_DELETE` while
  `SQLEndTran` did nothing to cursors at all.
- The derivation now also covers backends that answer these two info types
  *nowhere*: `SQLGetInfoW`'s Driver-Manager-safe fallback reports the hooks
  instead of the generic shape default. Previously such a backend got
  `U16(0)` for `SQL_CURSOR_COMMIT_BEHAVIOR` and `U32(0)` for
  `SQL_CURSOR_ROLLBACK_BEHAVIOR` — both `SQL_CB_DELETE`, the second in the
  wrong shape — while `SQLEndTran` applied the declared behaviour.
- Under `SQL_CB_CLOSE`, `SQLEndTran` now closes each statement's cursor via
  `StatementBackend::close_cursor` and keeps the statement itself, instead of
  dropping it. The `SQLEndTran` statement transition table's footnote `[2]`
  leaves the prepared states S2/S3 unchanged; dropping the statement sent a
  prepared-but-never-executed statement back to the allocated state, so a
  subsequent `SQLNumResultCols` failed with `HY010` where the spec allows it.
  A backend declaring `CursorBehavior::Close` must implement `close_cursor`,
  which defaults to a no-op.
- Under `SQL_CB_DELETE`, `SQLEndTran` now also clears a pending
  data-at-execution sequence. It already cleared `param_count`, which
  `SQLParamData` uses to size its parameter vector, so a surviving sequence
  would have executed with zero parameters and silently discarded everything
  the application streamed via `SQLPutData`.
- **Breaking:** `default_get_info` and `common_get_info_raw` are now generic
  over the backend. Call them as `default_get_info::<Self>(info_type, widths)`
  and `common_get_info_raw::<Self>(info_type)` from a `Backend` impl.
- **Breaking:** new **required** `Backend` methods —
  `supports_catalogs`, `supports_schemas`, `alter_table_support`,
  `outer_join_capabilities`, `default_txn_isolation`,
  `txn_isolation_options`, `group_by`, `null_collation`, `correlation_name`,
  `non_nullable_columns`, `expressions_in_order_by`, `sql_conformance`,
  `timedate_add_intervals`, `timedate_diff_intervals`, `subqueries`,
  `column_alias`, `concat_null_behavior`, `union_support`,
  `convert_functions`, `order_by_columns_in_select`, `accessible_tables`,
  `data_source_read_only`, `search_pattern_escape` and `keywords`.
  They are required rather than defaulted on purpose:
  each states a *capability*, where a defaulted value is a claim the backend
  author never made and is unlikely to notice. A defaulted `0` understates and
  a defaulted `true` overstates; the compiler asking is what makes the fact
  explicit. Every one of them replaces a value core previously invented (see
  `Fixed`).
- **Breaking:** `SQL_KEYWORDS` (89) now comes from the new required
  `Backend::keywords`, filtered against `ODBC_RESERVED_KEYWORDS`, sorted and
  comma-separated. **Every backend must implement it** — there is no default,
  because an empty list is the claim that the data source reserves nothing
  beyond ODBC. A backend returns its raw reserved words in any order and any
  case; core applies the spec's subtraction. It does so on every call rather
  than caching (a `static` cannot be generic over the backend); a backend whose
  list is expensive to produce should cache on its own side. Returning `&[]`
  reproduces the value core previously gave everyone.
- `SQL_ALTER_TABLE` and `SQL_OJ_CAPABILITIES` now come from
  `Backend::alter_table_support` / `Backend::outer_join_capabilities` instead
  of defaulting to `0`. `0` remains the shared default for the surrounding
  *limits* (`SQL_MAX_INDEX_SIZE`, `SQL_MAX_ROW_SIZE`, `SQL_MAX_STATEMENT_LEN`,
  the `SQL_MAX_COLUMNS_IN_*` group), where the spec defines it as "no
  specified limit or the limit is unknown".
- `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` now validates its value and
  applies it, where it previously stored any `usize` and echoed it back. A
  value that does not name exactly one isolation level, or names one outside
  `Backend::txn_isolation_options`, is rejected with `HY024` — the spec assigns
  this check to the driver, since the Driver Manager only validates attributes
  "that accept a discrete set of values". An accepted level is passed to the
  new `Backend::set_txn_isolation`, including one set before connecting, which
  is applied when the connection opens (the same deferral
  `SQL_ATTR_AUTOCOMMIT` already had).

### Fixed

- Six functions post the `01004` diagnostic record that their
  `SQL_SUCCESS_WITH_INFO` refers to when they truncate a string:
  `SQLGetInfoW`, `SQLGetConnectAttrW`, `SQLDescribeColW`, `SQLColAttributeW`,
  `SQLGetCursorNameW` and `SQLNativeSqlW`. An application that sees
  `SQL_SUCCESS_WITH_INFO` calls `SQLGetDiagRec` to learn why, and an empty queue
  left it unable to distinguish truncation from any other informational
  condition. `SQLGetData` and `SQLFetch` already did this.

  Two paths deliberately still do not. `SQLGetDiagRecW` / `SQLGetDiagFieldW`
  truncate their own messages, but posting a record about reading a record would
  recurse and overwrite the queue the application is reading. `SQLDriverConnectW`
  and `SQLBrowseConnectW` do not report a truncated output connection string at
  all, because `SQL_SUCCESS_WITH_INFO` there sends the Windows Driver Manager
  down a diagnostic-retrieval path that crashes; the required length still
  reaches the application so it can retry with a larger buffer. That reasoning is
  now written at the browse site too, not only at `SQLDriverConnectW`.
- `SQLGetTypeInfo` orders its result set by `DATA_TYPE`, then `TYPE_NAME`. The
  spec requires ordering by `DATA_TYPE` and then by how closely each type maps to
  the ODBC type; core cannot rank closeness, so it uses `TYPE_NAME` as a stable,
  total second key. Ordering was previously whatever the backend declared, which
  matters because an application picking "the first row for this `DATA_TYPE`"
  reads it as the preferred type. The sort is stable, so a backend's own
  preference between same-named rows survives.
- `SQLColAttributeW` reports `SQL_DESC_TYPE` as the *verbose* type. The spec
  splits it from `SQL_DESC_CONCISE_TYPE` for the datetime and interval families:
  a `SQL_TYPE_TIMESTAMP` column reports `SQL_DATETIME` for the former, `93` for
  the latter, and `93` again for `SQL_DESC_DATETIME_INTERVAL_CODE`, which is now
  answered too. Both previously returned the concise type. Only the ODBC 3.x
  concise codes (91–95, 101–113) are mapped — the 2.x `SQL_DATE` (9) and
  `SQL_TIME` (10) spellings are the verbose values themselves, so treating them
  as concise types would be ambiguous.
- `SQLColAttributeW` answers `SQL_DESC_BASE_TABLE_NAME` from the column
  descriptor instead of reporting `HYC00`. ODBC 3.x requires a value for every
  descriptor field, and an application asking about a column's provenance was
  seeing the whole call fail; it now returns the backend's table name, or an
  empty string when the backend does not track one.
- `SQL_DATETIME` and `SQL_INTERVAL` constants.
- `SyntheticStatement::new` checks that every row carries one value per declared
  column. The descriptors and the row values are built by separate functions for
  each catalog result set — `type_info_columns` against
  `TypeInfoRow::to_column_values`, and each `*ResultCol` enum against its
  `all_descriptors` — and nothing paired them up, so a short or long row
  surfaced later as `SQLGetData` returning a neighbouring column's value rather
  than as an error. A direct test also cross-checks the 19 `SQLGetTypeInfo`
  columns, including that a column declared character is not filled with a
  number.
- `SQLSetStmtAttrW` substitutes and reports `01S02` for an unsupported
  `SQL_ATTR_CURSOR_TYPE` or `SQL_ATTR_CURSOR_SCROLLABLE`, instead of refusing
  them with `HYC00`. The spec defines 01S02 as "the driver did not support the
  value specified and substituted a similar value", and `SQLGetStmtAttr` now
  reports the substituted value back, which is how the application learns what
  it was given. `SQL_ATTR_ROW_ARRAY_SIZE` in the same function already behaved
  this way.

- `SQLGetFunctions`' ODBC 2.x `SQL_API_ALL_FUNCTIONS` array recorded
  `SQLGetConnectOption` at index 30 instead of its spec value 42. Slot 30 is not
  an assigned `SQL_API_*` id, so the Windows Driver Manager — which builds its
  dispatch table from this array — was told the driver did not support a
  function it exports, while a meaningless slot was marked present. The array is
  now filled from named `FunctionId` values rather than constants transcribed by
  hand at the call site, which is how the wrong number got in. No test covered
  this: `MockBackend::get_functions` returns an empty slice, so every existing
  `SQLGetFunctions` test walked a loop with no iterations.

- **Security.** A handle is no longer a pointer, so validating one no longer
  dereferences an untrusted value. Handles were validated by reading a magic tag
  out of the allocation they pointed at, which can only ever catch a live handle
  of the *wrong type*: a freed handle was a use-after-free read, and a value
  that was never a pointer was an immediate segfault — a test passing `0x1234`
  as a handle crashed the process with SIGSEGV. Zeroing the tag before
  `Box::from_raw` on `SQLDisconnect` did not help, because the note was written
  into memory that was then handed back to the allocator; it appeared to work
  only because allocators do not usually reuse those bytes at once.
  A handle is now an opaque token pairing a slot index with a generation
  counter, and validation is a bounds check plus two integer comparisons against
  a table this crate owns, touching no application memory at all. Nothing in
  ODBC requires a handle to be an address: it is `void*` to the application and
  the Driver Manager only hands it back. Freeing bumps the slot's generation, so
  every outstanding token is permanently rejected — including after the slot is
  reused, which closes the recycled-address double free as well. A double free
  is now refused rather than performed.
  Descriptor handles handed out by `SQLGetStmtAttrW` get their own slots, so the
  addresses previously returned — which dangled the moment the owning statement
  was freed — are gone. This was latent only because nothing yet accepts
  `SQL_HANDLE_DESC`.
  Measured cost of the added `RwLock` read: **3.4 ns** per validation, or about
  34 ms across a 1M-row by 10-column fetch. The existing fetch benchmark could
  not measure this, since it drives `SyntheticStatement` directly and never
  crosses the FFI boundary.
  The one case no scheme can defend is an application freeing a handle on one
  thread while another is mid-call on it; ODBC forbids that and the Driver
  Manager serialises calls per handle.
- **Security.** Writes and reads through application-supplied pointers are now
  unaligned throughout. ODBC applications using row-wise binding pass pointers
  at arbitrary offsets into a packed buffer, so alignment is never guaranteed —
  `ffi/metadata.rs` says exactly this and uses unaligned writes for the reason,
  but 27 other sites did not: `write_utf16` (the shared helper behind most
  string output), `SQLNativeSqlW`, `SQLNumResultCols`, `SQLRowCount`,
  `SQLNumParams`, `SQLDescribeParam`, `SQLParamData`, `SQLGetDiagRecW`,
  `SQLGetDiagFieldW`, `SQLGetConnectAttrW`, `SQLGetEnvAttr`, `SQLGetFunctions`,
  `SQLAllocHandle`, `SQLAllocConnect`, `SQLAllocStmt`, `SQLNativeSqlW`'s length
  output, `ConfigDSNW`, and the five parameter length-indicator reads.
  This was not merely theoretical UB: the standard library's own precondition
  check fires on a misaligned `copy_nonoverlapping` or `ptr::write`, and it
  raises a **non-unwinding** panic, which `panic_safe`'s `catch_unwind` cannot
  contain. Any debug build of a driver would abort the host process. Two of the
  affected calls were `copy_nonoverlapping` over `u16`, which requires
  alignment; those now copy byte-wise, where alignment is 1.
  A second, larger class was `slice::from_raw_parts`, which requires the
  pointer to be aligned for its pointee type — so building a `&[u16]` over an
  application buffer was undefined behaviour before a single element was read.
  Six such sites are fixed: `utf16_to_string` (both the `SQL_NTS` and
  explicit-length paths), `SQLGetDiagRecW`'s SQLSTATE output, `SQLGetFunctions`'
  3.x bitmap and 2.x array, `ConfigDSNW`'s attribute parser, and the `SQL_C_WCHAR`
  parameter read. Each now reads element-wise or assembles into a local buffer
  and copies out byte-wise. The `u8` cases (`SQL_C_CHAR`, `SQL_C_BINARY`,
  `write_char`, `write_binary`) were always sound, since `u8` has alignment 1.
  `SQLGetDiagRecW` additionally no longer panics on a SQLSTATE longer than five
  characters, where the padding range would previously have been reversed.
  Existing tests missed all of it because every one passed a naturally aligned
  `Vec<u16>`. The new tests offset one byte into a `u16`-aligned allocation, so
  the misalignment is guaranteed on every platform — offsetting into a `Vec<u8>`
  is not enough, since a byte allocation may already start on an odd address.
- `parse_attributes_w` (`ConfigDSNW`) bounds its scan for a segment terminator
  instead of walking memory until it faults, matching `utf16_to_string`'s
  existing `SQL_NTS` bound.
- DSN lookup no longer trusts the installer library's returned length.
  `read_dsn_keys` sliced its buffers at the value `SQLGetPrivateProfileStringW`
  returned, checking only that it was positive. unixODBC and odbccp32 have both
  been observed returning the length *required* rather than the length *copied*
  when a buffer is too small, which would panic — and the panic would cross the
  C ABI boundary from `SQLConnectW`, `SQLDriverConnectW` or `SQLBrowseConnectW`.
  Both lengths are now clamped to the buffer.
- **Security.** Credentials no longer reach the log file by four separate
  routes. `SQLBrowseConnectW` logged the raw incoming connection string,
  `PWD=` included — every other connect path was already careful — and now logs
  the parsed `ConnectParams`, whose `Debug` redacts. `ConfigDSNW` logged the
  whole parsed DSN attribute list, which routinely carries `PWD=`, and now logs
  only the keyword names. `ConnectParams`' redaction covered exactly `password`
  and `pwd`, so a backend-defined `token`, `apikey`, `clientsecret` or
  `sslkeypassword` printed in clear; it now matches a set of substrings covering
  the realistic shapes. And the log file is created `0600` on Unix instead of
  inheriting the umask, which is commonly `0644`.
  That substring list is a safety net rather than the primary mechanism: a
  backend names its own secret keywords through
  `Backend::sensitive_connect_keywords` (see `Added`).
- **Security.** A parameter's length indicator is no longer trusted over the
  buffer the application bound. For `SQL_C_CHAR`, `SQL_C_WCHAR` and
  `SQL_C_BINARY`, `read_param_value` took the byte count solely from
  `*StrLen_or_IndPtr` and never consulted `BufferLength`, which it already
  records at `SQLBindParameter` time. An indicator larger than the buffer built
  a slice over memory past the end of it, and the backend then sent that to the
  data source — so adjacent process memory could be read back out of a table.
  The indicator is now clamped to `BufferLength` and the truncation is logged.
  A non-positive `BufferLength` carries no bound and is left alone, since zero
  is how an application declares "not applicable".
- **Security.** Deeply nested escape sequences killed the host process.
  `translate_slice` and `translate_escape` are mutually recursive, one level per
  nested escape, over SQL the application supplies, and had no depth bound —
  so `{oj {oj {oj …}}}` recursed until the stack was exhausted. A stack overflow
  is a guard-page abort rather than a panic, so `panic_safe`'s `catch_unwind`
  could not contain it: the application hosting the driver died. Reachable from
  `SQLExecDirectW`, `SQLPrepareW` and `SQLNativeSqlW`. Nesting is now capped at
  `MAX_ESCAPE_DEPTH` (64, far above anything real SQL produces) and deeper input
  returns `42000`. The margin mattered: at roughly 330 bytes per level the old
  behaviour aborted at about 25 000 levels on Linux's 8 MiB main stack, but only
  about 3 000 — some 12 KB of SQL — on a 1 MiB Windows thread stack.
- **Security.** `SQL_C_DEFAULT` could overrun the application's buffer.
  `write_column_value` ignored `BufferLength` for every fixed-width C type,
  which is correct when the application *names* the type — naming it states the
  buffer's size — but not for `SQL_C_DEFAULT`, where the driver chooses. Core
  chose from the runtime `ColumnValue` variant rather than from the `sql_type`
  `SQLDescribeCol` reported and the application sized its buffer against, and
  nothing cross-checked the two: a column described as `SQL_INTEGER` and bound
  with four bytes received a 16-byte write if the backend produced a
  `Timestamp`, and `SQL_SUCCESS` with it. A positive `BufferLength` is now
  honoured on the `SQL_C_DEFAULT` path and a value too wide for it returns
  `07006`. `BufferLength = 0` remains exempt: for a fixed C type it is the
  idiomatic "not applicable" and carries no size information. The residual is
  closed properly by deriving the default C type from the column's `sql_type`,
  which needs a signature change and is deferred.
  The `column_value` fuzz target allocated a blanket 256 bytes for every
  non-character target, including `SQL_C_DEFAULT`, which is what hid this; it
  now allocates exactly `BufferLength` for that case.
- **Security.** A DSN entry in `odbc.ini` could override the connection string
  the application supplied, and could inject additional keywords. Both are
  fixed, and `merge_dsn_params` now has tests, which it had none of.
  `ConnectParams::parse` is first-occurrence-wins, but `merge_dsn_params`
  placed the DSN file's keys *first* and appended the connection string last,
  under a comment asserting the opposite — so the file won every conflict. A
  DSN could therefore redirect a connection to another host and substitute the
  `UID`/`PWD` an application had passed explicitly. Separately, DSN values were
  rendered back into `Key=Value;` form and re-parsed without quoting, so a `;`
  in a value injected further keywords. The merge is now performed on parsed
  values via `ConnectParams::merge`, which does not overwrite existing keys;
  no DSN value is ever re-parsed. Quoting alone would not have been enough,
  because a `}` in a value ends the quoted run early and re-opens the same
  hole.
- Every `SQLGetInfo` type the spec declares as a character string but which
  `odbc_sys::InfoType` has no variant for is answered as a string, instead of
  falling through to the unnamed-raw default `U32(0)`. An application reading
  one into a character buffer got four bytes of binary zero with
  `StringLength = 4`. Found by sweeping every info-type number in
  `sql.h`/`sqlext.h` against `info_type_from_raw`, rather than one at a time:
  `SQL_ROW_UPDATES` (11) `"N"`, `SQL_PROCEDURES` (21) `"N"`,
  `SQL_MULTIPLE_ACTIVE_TXN` (37) `"N"`, `SQL_DATABASE_NAME` (16) `""`,
  `SQL_PROCEDURE_TERM` (40) `""` (consistent with `SQL_PROCEDURES = "N"`),
  `SQL_TABLE_TERM` (45) `"table"`, and `SQL_KEYWORDS` (89) — the last of these
  now carrying the backend's own list rather than `""`, see below.
  All come from `common_get_info_raw`, and core's own fallback consults that
  helper, so a backend whose `get_info_raw` does not delegate to it gets the
  same answer. That last change also fixes `SQL_QUOTED_IDENTIFIER_CASE` for
  such a backend, which previously defaulted to `U16(0)` — not one of the four
  `SQL_IC_*` values.
- `SQL_MULT_RESULT_SETS`, `SQL_MAX_ROW_SIZE_INCLUDES_LONG` and
  `SQL_NEED_LONG_DATA_LEN` now default to `"N"` instead of `""`. They had no
  arm in `default_get_info`, so the shape-aware fallback gave them the empty
  string: the right shape, but not a value in any of their value lists.
- `SQL_GROUP_BY`, `SQL_NULL_COLLATION`, `SQL_CORRELATION_NAME` and
  `SQL_NON_NULLABLE_COLUMNS` are stated by the backend rather than invented by
  core. For all four, zero is a *substantive answer* — `SQL_GB_NOT_SUPPORTED`,
  `SQL_NC_HIGH`, `SQL_CN_NONE`, `SQL_NNC_NULL` — so the shape default was
  handing every backend a specific spec claim it never made. `SQL_GROUP_BY`
  was worse than a default: core actively reported `SQL_GB_NO_RELATION`.
- `SQL_SQL_CONFORMANCE` comes from the backend instead of a hard-coded
  `SQL_SC_SQL92_ENTRY`. Core claimed entry level while separately supplying
  `SQL_CORRELATION_NAME`, `SQL_NON_NULLABLE_COLUMNS` and `SQL_GROUP_BY` values
  the spec says an entry-level driver never returns, so every backend built on
  core inherited a contradiction it could not see.
- `SQL_SUBQUERIES`, `SQL_COLUMN_ALIAS` and `SQL_CONCAT_NULL_BEHAVIOR` come
  from the backend too — the other half of that same contradiction. The spec
  names all three as values a SQL-92 Entry level-conformant driver returns,
  and core hard-coded exactly those, so a backend declaring *no* conformance
  level was still reported as supporting correlated subqueries, quantified
  predicates and column aliases. `SQL_SUBQUERIES` is the one an application
  acts on: claiming `SQL_SQ_CORRELATED_SUBQUERIES` for a source without them
  is how a BI tool comes to push down SQL the server rejects.
  `SQL_CONCAT_NULL_BEHAVIOR` was additionally a bare `0` literal for a
  spec-named constant; `SQL_CB_NULL` and `SQL_CB_NON_NULL` now exist.
- `SQL_UNION`, `SQL_CONVERT_FUNCTIONS`, `SQL_ORDER_BY_COLUMNS_IN_SELECT`,
  `SQL_ACCESSIBLE_TABLES` and `SQL_DATA_SOURCE_READ_ONLY` come from the
  backend. Each was a statement about the data source that core had no way to
  know. `SQL_ACCESSIBLE_TABLES` was the sharpest: `"Y"` guarantees the
  connected user has `SELECT` on every table `SQLTables` returns, which
  depends on the principal, not the driver.
- `SQL_IDENTIFIER_QUOTE_CHAR` is derived from
  `EscapeDialect::identifier_quotes` rather than hard-coded to `"`. The escape
  translator already consulted the dialect, so a backend quoting identifiers
  with a backtick or brackets had core telling applications something the
  translator contradicted. No new hook: the fact was already declared.
- `SQL_SEARCH_PATTERN_ESCAPE` comes from the backend. It describes what
  escapes `%` and `_` in catalog-function pattern arguments, which the backend
  interprets.
- `SQL_KEYWORDS` is stated by the backend. Core answered it with `""` for
  everyone, which is not an absence of an answer but the claim that the data
  source reserves nothing beyond ODBC — and applications read it to decide
  which identifiers need quoting, so a wrong empty list is how a generated
  identifier goes unquoted and the statement fails to parse. Every real
  backend has keywords of its own.
- `SQL_EXPRESSIONS_IN_ORDERBY` is stated by the backend. It previously fell to
  `""`, which reads as "no" to a tool deciding whether to push an expression
  into `ORDER BY`.
- `SQL_TIMEDATE_ADD_INTERVALS` and `SQL_TIMEDATE_DIFF_INTERVALS` are stated by
  the backend instead of defaulting to `0`. The spec defines them as the units
  `TIMESTAMPADD` / `TIMESTAMPDIFF` accept, so `0` alongside a
  `SQL_FN_TD_TIMESTAMPADD` claim in `SQL_TIMEDATE_FUNCTIONS` says the function
  exists but takes no units.
- `SQL_CATALOG_TERM`, `SQL_SCHEMA_TERM` and `SQL_CATALOG_NAME_SEPARATOR` no
  longer name catalogs and schemas that the data source does not have. The
  `SQLGetInfo` spec defines the whole group — those three plus
  `SQL_CATALOG_NAME`, `SQL_CATALOG_LOCATION`, `SQL_CATALOG_USAGE` and
  `SQL_SCHEMA_USAGE` — in terms of whether catalogs (resp. schemas) exist at
  all, and mandates an empty string or zero when they do not. Core hard-coded
  `"catalog"`, `"schema"` and `"."`, so a backend reporting
  `SQL_CATALOG_NAME = "N"` and letting the rest fall through contradicted
  itself. All seven are now derived from `Backend::supports_catalogs` /
  `Backend::supports_schemas`. Where the spec only mandates the *zero*, core
  asserts only that: `SQL_CATALOG_LOCATION`, `SQL_CATALOG_USAGE` and
  `SQL_SCHEMA_USAGE` are left to the backend once catalogs or schemas exist,
  rather than core inventing a value it cannot know.
- `SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION)` on a connection where the
  application has not set the attribute now reports
  `Backend::default_txn_isolation` instead of a hard-coded
  `SQL_TXN_READ_COMMITTED`. `SQL_DEFAULT_TXN_ISOLATION` is derived from the
  same hook, so the info type and the connection attribute can no longer report
  two different levels for one connection — a driver declaring
  `SQL_TXN_SERIALIZABLE` previously disagreed with itself.
- Cursor state is now tracked explicitly, by `StatementHandle::cursor_open`,
  instead of being inferred from `StatementHandle::statement`. Every `24000`
  check reads it: `SQLExecDirect`, `SQLGetData`, `SQLCloseCursor`, all ten
  catalog functions and `SQLSetStmtAttr` for
  `SQL_ATTR_CURSOR_TYPE` / `SQL_ATTR_CONCURRENCY` / `SQL_ATTR_SIMULATE_CURSOR` /
  `SQL_ATTR_USE_BOOKMARKS` / `SQL_ATTR_ROW_NUMBER`. Three behaviour changes
  follow:
  - After `SQLEndTran` under `SQL_CB_CLOSE`, `SQLExecDirect`, the catalog
    functions and those `SQLSetStmtAttr` attributes are accepted instead of
    returning `24000`. The statement transition table makes them legal
    (S4→S2, S5-S7→S3), but the statement is kept under `SQL_CB_CLOSE` and the
    old `statement.is_some()` guard read that as an open cursor.
  - `SQLCloseCursor` and `SQLGetData` now return `24000` on a statement that is
    only prepared, that executed without producing a result set, or whose
    cursor `SQLEndTran` closed under `SQL_CB_CLOSE`. They previously accepted
    all three, `SQLCloseCursor` reporting success with no cursor to close.
  - An execution that produces no result set (an `UPDATE`, say) no longer
    counts as an open cursor. A backend must report
    `StatementBackend::column_count` accurately as soon as
    `execute`/`exec_direct` returns — the same point `SQLNumResultCols` already
    reads it.
- `SQLEndTran` now applies the cursor behaviour the driver advertises. It
  previously reported `SQL_CB_DELETE` and left every cursor and prepared
  statement untouched, so an application that trusted the reported value
  operated on cursors it had been told were gone.
- `SQLEndTran` with `SQL_HANDLE_ENV` now attempts every connected connection
  on the environment instead of stopping at the first failure, and records a
  diagnostic on each failing connection so `SQLGetDiagRec` can identify it.
  A failure on one connection previously left every later connection holding
  an open transaction the application had asked to commit or roll back.
- `SQLEndTran` now clears diagnostics at entry — on the handle it was given,
  and on each connection it visits in the `SQL_HANDLE_ENV` loop. Without this,
  a record left by an earlier failed call on a connection that then committed
  fine made `SQLGetDiagRec` blame the wrong connection, defeating the
  per-connection diagnostics the spec tells applications to read.
- A corrupt entry in an environment's connection list no longer makes
  `SQLEndTran(SQL_HANDLE_ENV)` return `SQL_INVALID_HANDLE` for a call whose
  input handle was valid (and with no diagnostic record, since none is pushed
  for that variant). It now records a general error, so a genuinely failing
  connection's own SQLSTATE is no longer suppressed by a corrupt entry
  encountered before it.

[Unreleased]: https://github.com/stackabletech/stackable-odbc-core/commits/HEAD
