# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

This crate has not been published. Everything below is part of its first
release, and the baseline it is measured against is the `stackable-odbc-rs`
monorepo that `stackable-odbc-core` was extracted from — not any released
version of this crate.

Entries marked **Breaking** are therefore not breaking changes in the semver
sense; nothing published is affected. They mark a difference from that monorepo
baseline that a driver built against it has to act on when it moves to this
crate, which is what the two sibling drivers are doing. At the 0.1.0 cut these
markers go away and this section becomes the initial-release notes.

### Migration: the catalog functions

Everything a driver has to change for the catalog rework, in one place.

1. **Six methods return typed rows instead of `Self::Statement`:** `tables` →
   `Vec<TableRow>`, `columns` → `Vec<ColumnRow>`, `primary_keys` →
   `Vec<PrimaryKeyRow>`, `foreign_keys` → `Vec<ForeignKeyRow>`, `statistics` →
   `Vec<StatisticsRow>`, `special_columns` → `Vec<SpecialColumnRow>`. Keep the
   query helpers; return their rows as structs and delete the statement
   construction.
2. **`Backend::table_types` is new and required.** Return the data source's
   table types, e.g. `["TABLE", "VIEW"]`, upper case.
3. **`Backend::catalogs` and `Backend::schemas` are new and defaulted.**
   Implement them if `supports_catalogs`/`supports_schemas` returns `true`; a
   backend that claims either and leaves the method defaulted answers `HYC00`
   for the corresponding enumeration.
4. **Delete any `ORDER BY` added purely for ODBC compliance.** Core sorts every
   catalog result set into its spec order. A backend-side `ORDER BY` is now
   redundant, though harmless.
5. **Delete any `SQL_ATTR_METADATA_ID` handling.** Core normalises identifier
   arguments before calling the backend.
6. **`Backend::tables`' last parameter is now `table_types: &[String]`.** Delete
   any driver-side splitting of the raw `TableType` string; core does it once.
7. **Four more methods are new and defaulted:** `procedures`,
   `procedure_columns`, `column_privileges` and `table_privileges`, returning
   `Vec<ProcedureRow>`, `Vec<ProcedureColumnRow>`, `Vec<ColumnPrivilegeRow>` and
   `Vec<TablePrivilegeRow>`. Nothing to do unless you want `SQLProcedures`,
   `SQLProcedureColumns`, `SQLColumnPrivileges` or `SQLTablePrivileges` to
   return rows: each defaults to an empty result set, which is what those four
   functions returned before. Points 4 and 5 apply to them too.
8. **`SQLColumnPrivileges` now rejects a null `TableName` with `HY009`**, which
   the spec states for it without a `(DM)` marker. This is the one behaviour
   change in point 7's group that an existing test suite can notice — a test
   passing a null there was relying on a spec violation. The other three
   functions must *not* check it, and do not.

### Added

- **A guard test that the set of group-lock acquisition sites is closed**
  (`the_set_of_group_lock_acquisition_sites_is_closed`, `handles/registry.rs`).
  Obtaining a group from the registry is the only way to reach one that is not
  already held, so the production call sites of `group_of` / `group_of_kind`
  are the complete set of places the lock discipline can be broken. The test
  scans the source tree and fails on a site that is not in its documented list.

  This guards the failure mode a loom model structurally cannot catch: a model
  proves things about the code it calls, and says nothing about a *new* nesting
  site added somewhere it does not reach. Not hypothetical —
  `env_before_connection_cannot_deadlock` passed for its whole life while
  proving a property of its own test code.

- **Issue and pull-request templates** (`.github/ISSUE_TEMPLATE/`,
  `.github/pull_request_template.md`). The bug form asks for the ODBC call
  sequence, the SQLSTATE and the Driver Manager, because a defect here is
  almost always a specific function under a specific prior state. The PR
  template asks for the spec basis, including whether a SQLSTATE's row carries
  a `(DM)` marker and what an attribute's stated purpose is — both have caused
  rework.

- **`SqlState::invalid_catalog_name()` and `INVALID_CATALOG_NAME` (`3D000`).**
  `SQLSetConnectAttr`'s `3D000` row carries no `(DM)` marker, so it is the
  driver's to return — but core had no name for it, so a driver told to report
  it had no way to say so. Core still never constructs it: only the data source
  knows which catalogs exist, and the attribute's own description has the driver
  send something to it ("the driver sends a **USE** *database* statement").
  `Backend::set_current_catalog` now documents that "no such catalog" maps to
  this state, and core's propagation of it is pinned by tests on both paths —
  through `SQLSetConnectAttrW`, and through the connect functions for a catalog
  set before connecting.

  The old claim that core "stored the catalog string verbatim without
  validation" was never true of the code: core has always asked the hook and
  stored the value only on success. Two further doc corrections follow from
  that — the connect functions can return `3D000`, which their own diagnostics
  table does not list, and they can return `HYC00` from an unimplemented
  pending-attribute hook, which their doc comments claimed they could not.

- `Backend::set_max_rows` and `Backend::set_max_length` let the data source
  apply `SQL_ATTR_MAX_ROWS` and `SQL_ATTR_MAX_LENGTH`, which were substituted to
  `0` with `01S02` for every driver. Both default to `NotImplemented`, keeping
  that substitution unchanged, so no existing driver's behaviour changes.

  **Core does not emulate either limit, deliberately.** The spec confines both
  to the data source: "a driver should not emulate SQL_ATTR_MAX_ROWS behavior
  for `SQLFetch` or `SQLFetchScroll` ... if it cannot guarantee that
  SQL_ATTR_MAX_ROWS will be implemented properly", and `SQL_ATTR_MAX_LENGTH`
  "should be supported only when the data source (as opposed to the driver) in a
  multiple-tier driver can implement it" — with the further warning that "this
  mechanism should not be used by applications to truncate data". Both rows say
  "this attribute is intended to reduce network traffic", which counting rows or
  bytes in the driver, after they have already crossed the wire, cannot achieve.
  Implement these only where the data source can genuinely cap the result set or
  the column.

  With `set_query_timeout` these three now share one path, `offer_to_data_source`:
  offer to the backend, store on acceptance, substitute with `01S02` on
  `NotImplemented`, and propagate any other error as-is rather than
  misreporting a broken connection as a capped value. The `01S02` list stays
  closed at eight.

- `ConnectParams::login_timeout` and `ConnectParams::connection_timeout` give
  `Backend::connect` the two timeouts it could not previously see.
  `SQL_ATTR_LOGIN_TIMEOUT` and `SQL_ATTR_CONNECTION_TIMEOUT` are set through
  `SQLSetConnectAttr`, not through the connection string, and `connect` receives
  only a `ConnectParams` — so an application setting a 15-second login timeout
  got no timeout, and no driver had any way to offer one. The login timeout is
  settable "Before" only precisely because it bounds that call. Core now copies
  both into the `ConnectParams` it passes, at all three connect entry points.

  They are carried as dedicated fields rather than as synthetic connection-string
  keys, so `ConnectParams::to_connection_string` — which `SQLDriverConnect`
  echoes back to the application in *OutConnectionString* — does not gain keys
  the application never wrote.

  `None` and `Some(0)` are different answers and a backend must not conflate
  them: `None` means unset, so use the driver's own default, while `Some(0)`
  for the login timeout means "the timeout is disabled and a connection attempt
  will wait indefinitely". Core does not enforce either value — `connect` is
  synchronous and no cancel token exists before a connection does — so a backend
  that wants them honoured passes them to its own client library.

- `Backend::set_access_mode` carries `SQL_ATTR_ACCESS_MODE` to the data source.
  The attribute was validated and stored but never applied, so a data source
  with a real read-only session mode never entered it and the optimisation the
  spec offers — "this mode can be used to optimize locking strategies,
  transaction management, or other areas" — was unavailable to every driver. A
  value set before connecting is applied at connect, which the spec's footnote
  [1] calls the interoperable choice.

  **The default is `Ok(())` — accept and ignore — not a refusal**, which is
  where this hook differs from `set_current_catalog` and `set_autocommit`. The
  spec permits it outright: read-only "is used by the driver or data source as
  an indicator that the connection is not required to support SQL statements
  that cause updates to occur ... the driver is not required to prevent such
  statements from being submitted to the data source". So it is a hint, not a
  safety interlock, and ignoring it misleads nobody about correctness. Core
  stores the value only once the hook returns `Ok`, so a backend that does
  refuse cannot have `SQLGetConnectAttr` report a read-only connection that is
  nothing of the kind.

- `Backend::connection_dead`, defaulted to `false`, backs
  `SQLGetConnectAttr(SQL_ATTR_CONNECTION_DEAD)`, which was hardcoded
  `SQL_CD_FALSE`. That is the attribute a connection pool reads before handing a
  connection out, so a pool talking to any driver built on core would cheerfully
  serve a connection whose socket had already closed, and the borrower's first
  query failed for no reason it could see. A backend should answer from liveness
  state it already holds — the spec's own note is that "a driver can improve
  performance by minimizing the number of times that information is sent or
  requested from the server", and a pool may call this on every checkout.

  The default keeps today's answer, so no existing driver changes. Note the
  asymmetry it encodes: `false` means "not known to be dead", not "known to be
  alive", which is the honest reading for a backend with no liveness signal —
  `SQL_CD_TRUE` asserts the connection *has been lost*. A handle with no
  connection at all also reads `SQL_CD_FALSE`, because it never lost one.

- `Backend::set_query_timeout` and the `QueryTimeout` type make
  `SQL_ATTR_QUERY_TIMEOUT` real. The attribute was substituted to `0` with
  `01S02` for every driver, so an application that set a 30-second timeout was
  told the driver had capped it and then waited indefinitely on a runaway query.
  The hook is defaulted, reports `NotImplemented`, and leaves that substitution
  exactly as it was, so **no existing driver's behaviour changes** until it opts
  in. A backend failure that is *not* `NotImplemented` is now reported as-is
  rather than substituted: `01S02` tells an application its timeout was capped,
  which is a different claim from "the connection is broken".

  A backend that opts in says *who enforces the deadline*, because core cannot
  decide that for itself. `QueryTimeout::DataSource` means the data source
  imposes its own deadline and core arms nothing. `QueryTimeout::CoreCancels`
  means the backend cannot set a server-side deadline but can be cancelled, so
  core arms a timer that calls `Backend::cancel` when the deadline passes; an
  execution stopped that way reports `HYT00`, not the `HY008` an explicit
  `SQLCancel` produces. Returning `CoreCancels` asserts that `cancel` really
  cancels — core has no way to check, since `cancel`'s default returns
  `NotImplemented` and `SQLCancel` treats that as "nothing to cancel".

  The reason the enforcer has to be declared rather than inferred is that every
  statement-producing `Backend` method is synchronous and blocks the calling
  thread. Core cannot abandon one; asking the backend to stop the work is the
  only lever it has. A timer is armed at all fourteen statement-producing call
  sites and disarmed when the call returns, so a fast query leaves no thread
  behind and an untimed statement pays only a null check.

  One limit worth knowing: the hook receives the connection, not the statement.
  `SQL_ATTR_QUERY_TIMEOUT` is a statement attribute, so a backend that applies
  it session-wide gives every statement on that connection the most recently set
  value; scoping it per statement is the backend's job. Core's own timer is
  per-statement either way.

- `Backend::current_catalog` and `Backend::set_current_catalog`, both defaulted,
  make `SQL_ATTR_CURRENT_CATALOG` more than a handle-local string.
  `SQLGetConnectAttr` and `SQLGetInfo(SQL_DATABASE_NAME)` — one value under two
  names, per the spec — now read the same two sources in the same order: what
  the application set, else what the session is actually using. Without the
  second source the attribute was write-only, answering `""` while the info type
  answered the real catalog. `SQLSetConnectAttr` asks the backend to switch and
  stores the value only if that succeeds; the default reports `HYC00`, because
  storing a catalog the session never switched to tells an application its
  unqualified names resolve somewhere they do not. A value set before connecting
  is applied at connect, like autocommit and isolation.

- `SQL_PARAM_SUCCESS`, `SQL_PARAM_ERROR`, `SQL_PARAM_SUCCESS_WITH_INFO`,
  `SQL_PARAM_UNUSED` and `SQL_PARAM_DIAG_UNAVAILABLE` are public in
  `types`. `odbc-sys` does not define them, and a driver asserting what its own
  executions wrote into `SQL_ATTR_PARAM_STATUS_PTR` needs to name them rather
  than declare local copies.

- `conformance::info_group_inconsistencies` checks the `SQLGetInfo` groups whose
  members constrain each other, and returns one message per violation. Core
  cannot police a backend's `get_info` at runtime — that method runs first and
  is entitled to answer anything — so the invariants live in the shared harness
  each driver's suite already runs against its real backend: a driver that
  answers `SQL_CATALOG_TERM` but leaves `SQL_CATALOG_NAME` saying `"N"` fails
  its own tests. This is what makes the vendor-terminology group safe without
  `Backend` hooks. Two of the invariants are deliberately one-directional:
  `SQL_PROCEDURES = "Y"` implies a non-empty `SQL_PROCEDURE_TERM` but not the
  converse (the info type is a conjunction that includes driver `{call}`
  support), and an empty `SQL_SCHEMA_TERM` implies `SQL_SCHEMA_USAGE = 0` but
  not the converse (the `SQL_SCHEMA_TERM` page names an `SQL_SCHEMA_NAME` info
  type that does not exist in `sqlext.h`, so the term is its own support
  signal). `conformance::observe_string_value` and
  `conformance::observe_u16_value` join `observe_u32_value` for reading the
  other two value shapes through the real entry point.

- `SQLFetch`, `SQLFetchScroll` and `SQLGetData` now return `HY008` on a
  cross-thread cancel. These consume a cursor an earlier execution opened, so
  they read the token *that* execution minted rather than minting one.
  `StatementBackend` is unchanged and stays independent of
  `Backend::CancelToken`: core resolves the token from the handle registry, the
  same way `SQLCancel` does.

- The ten catalog functions now return `HY008` when a `SQLCancel` from another
  thread interrupted them and the backend implements `Backend::is_cancelled`,
  on the same terms as the execution functions. `SQLStatistics` and
  `SQLSpecialColumns` reclassify only their genuine-error arm: a backend
  answering `NotImplemented` still gets the spec's empty result set, because
  that is an answer rather than a failure.

- `SqlState::operation_canceled()` (`HY008`) and the defaulted
  `Backend::is_cancelled` hook it pairs with, so a driver no longer needs
  `SqlState::new("HY008")`. `cancel` signals and `is_cancelled` observes;
  implement the second whenever you implement the first, or a cancelled call
  still reports whatever SQLSTATE your error mapping produced.

- `SQLExecDirect`, `SQLPrepare`, `SQLExecute` and `SQLParamData` now return
  `HY008` when a `SQLCancel` from another thread interrupted them and the
  backend implements `Backend::is_cancelled`. They previously reported whatever
  the backend's error mapping produced for the resulting failure, usually
  `HY000` — which told the application nothing about why. Only the error path
  is reclassified: the spec allows a cancelled execution to succeed anyway
  ("it is possible for the execution to succeed and return SQL_SUCCESS while
  the cancel is also successful"), so a successful call stays successful.

- `SQLProcedures` now returns real rows when a backend implements the new
  defaulted `Backend::procedures`, instead of always returning an empty result
  set. Core converts the returned `ProcedureRow`s to the spec's 8-column layout
  and sorts them into the spec's order (PROCEDURE_CAT, PROCEDURE_SCHEM,
  PROCEDURE_NAME). Defaulted to `Ok(Vec::new())` rather than `NotImplemented`,
  so a driver that does not override it behaves exactly as before: a data source
  with no stored procedures has none to report, and erroring instead would turn
  a working call into a failure.

- `SQLProcedureColumns`, `SQLColumnPrivileges` and `SQLTablePrivileges` now
  return real rows when a backend implements the new defaulted
  `Backend::procedure_columns`, `Backend::column_privileges` and
  `Backend::table_privileges`. Core owns each column layout and sorts each
  result set into its spec order: PROCEDURE_CAT, PROCEDURE_SCHEM,
  PROCEDURE_NAME, COLUMN_TYPE; TABLE_CAT, TABLE_SCHEM, TABLE_NAME, COLUMN_NAME,
  PRIVILEGE; and TABLE_CAT, TABLE_SCHEM, TABLE_NAME, PRIVILEGE, GRANTEE
  respectively — note that `SQLTablePrivileges` orders by PRIVILEGE *before*
  GRANTEE. All three default to `Ok(Vec::new())`, so a driver that does not
  override them behaves exactly as before.

- `SQL_ATTR_METADATA_ID` support in `SQLProcedures`, `SQLProcedureColumns`,
  `SQLColumnPrivileges` and `SQLTablePrivileges`, completing the catalog family.
  Every string argument of all four is an identifier under `SQL_TRUE` — this
  family has no `SQLTables`-`TableType`-style exemption — so core strips
  delimiters, case-folds per `SQL_IDENTIFIER_CASE` and escapes `%`/`_` per
  `SQL_SEARCH_PATTERN_ESCAPE` before the backend is called. A driver needs no
  code for the feature.

  All four now also return `HY009` when `METADATA_ID` is `SQL_TRUE`,
  `CatalogName` is a null pointer and the data source has catalogs — the one
  clause all four pages state without a `(DM)` marker. `SQLColumnPrivileges`
  additionally rejects a null `TableName` unconditionally, because it is the
  only one of the four whose page states that sentence unmarked; the other
  three deliberately do not, and tests pin the difference in both directions.

- `SQL_PT_UNKNOWN`, `SQL_PT_PROCEDURE` and `SQL_PT_FUNCTION`, the
  `PROCEDURE_TYPE` result-column values `odbc-sys` does not define.
  `SQLProcedureColumns`' `COLUMN_TYPE` needs no counterpart —
  `odbc_sys::ParamType`'s discriminants are exactly that column's value set.

- `Backend::describe_param` and the `ParamDescriptor` it returns, so a backend
  can answer `SQLDescribeParam` for real. Core previously reported a hard-wired
  `VARCHAR(SQL_DEFAULT_PARAM_SIZE)` for every parameter of every statement,
  while `SQLGetInfo(SQL_DESCRIBE_PARAMETER)` advertised `"Y"` and no hook
  existed to override it — a client that sizes its buffers from this sends a
  number as text and gets a type error back from the data source.

  Defaulted to `Ok(None)`, so it is additive: an existing driver keeps
  compiling and keeps the old behaviour. `None` for an individual parameter is
  also the right answer for a backend that can describe some but not all of
  them — core's fallback is a documented, uniform guess, whereas a wrong
  specific type is indistinguishable from a real answer. `SQL_DESCRIBE_PARAMETER`
  stays `"Y"` either way: the spec defines it as whether the driver supports the
  *call*, not how precisely it answers.

- `column_value::write_column_value_at` and the `ChunkWrite` it returns, the
  offset-aware form of `write_column_value` that `SQLGetData`'s chunking loop
  uses. `write_column_value` is unchanged and still the right call for the
  bound-column and `SQLParamData` paths, which deliver a whole value in one go.
  Also `handles::GetDataCursor`, the per-statement read position.

- `bulk_operation_from_raw`, converting `SQLBulkOperations`'s raw `i16` into
  `odbc_sys::BulkOperation` at the FFI boundary like every other ABI value.
  `SQLSetPos`'s `Operation` and `LockType` deliberately have no equivalent:
  `odbc_sys::Operation` and `odbc_sys::Lock` are newtype structs over a private
  `i16` with no accessor, no `From` and no `#[repr]` enum to cast through, so a
  converted value could be compared against their associated constants and used
  for nothing else — neither core nor a driver could recover the raw code to
  forward it. Those two keep validating against `SQL_POSITION` / `SQL_LOCK_*`,
  which is recorded at both sites so it is not "fixed" into a conversion that
  cannot work.

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
- `SQLTables` now serves the `SQL_ALL_CATALOGS`, `SQL_ALL_SCHEMAS` and
  `SQL_ALL_TABLE_TYPES` enumerations, which were previously unimplemented and
  are what a BI tool's navigator uses to browse a data source. Core builds these
  result sets itself, with all columns except the enumerated one set to NULL as
  the spec requires, and `Backend::tables` is not called for them. All three
  sentinels are the string `"%"`, so an enumeration is recognised by which
  argument carries it while the others are empty strings —
  `SQLTables("%", "%", "%")` remains an ordinary match-everything query.
- `Backend::catalogs` and `Backend::schemas` (both defaulted to
  `NotImplemented`), supplying the names for the first two enumerations. Core
  calls them only when `supports_catalogs` / `supports_schemas` already returned
  `true`; when either is `false` it returns an empty result set without asking
  the backend.
- `types::SQL_ALL_CATALOGS`, `types::SQL_ALL_SCHEMAS` and
  `types::SQL_ALL_TABLE_TYPES`.
- `SQL_ATTR_METADATA_ID` is now honoured. It was previously accepted, stored and
  read back by `SQLGetStmtAttr` while changing nothing — so an application that
  set it got confirmation of a behaviour the driver did not have. Core now
  normalises the identifier-valued catalog arguments itself (strip delimiters,
  case-fold per `SQL_IDENTIFIER_CASE`, escape `%`, `_` and the escape character
  per `SQL_SEARCH_PATTERN_ESCAPE`) before the backend sees them, so a backend
  needs no code for the feature at all. Applied to `SQLTables`,
  `SQLColumns`, `SQLPrimaryKeys`, `SQLForeignKeys` (both the PK and the FK
  trio), `SQLStatistics` and `SQLSpecialColumns`. `SQLTables`' `TableType` is
  exempt, as the spec requires: it "is a value list argument, regardless of the
  setting of SQL_ATTR_METADATA_ID". Detection of the three `SQL_ALL_*`
  enumerations still runs on the raw arguments, since a normalised `"%"` would
  be escaped and stop being the sentinel.
- The driver-side `HY009` checks the spec assigns to the driver rather than the
  Driver Manager. All six catalog functions reject a null catalog argument when
  `SQL_ATTR_METADATA_ID` is `SQL_TRUE` and `Backend::supports_catalogs` reports
  catalogs exist (`SQLForeignKeys` checks both of its catalog arguments), and
  `SQLStatistics` and `SQLSpecialColumns` reject a null `TableName`.
  `SQLPrimaryKeys` and `SQLForeignKeys` deliberately do **not** check their
  table-name arguments: those sentences are `(DM)`-marked in their diagnostics
  tables, while the identical sentence in the other two is not.

### Changed

- **The `package` CI job pins its actions and drops its credentials.** It was
  the last job using floating tags (`actions/checkout@v5`,
  `Swatinem/rust-cache@v2`) and the only checkout without
  `persist-credentials: false`. Every action in the workflow is now pinned to a
  commit SHA.

- **The loom models build again, and one of them now proves something.** The
  models had stopped compiling entirely: `query_timer.rs` imported `Condvar`,
  `Mutex` and `Arc` from `crate::sync`, which resolve to loom's under
  `--cfg loom`, and loom's `Arc` has no `CoerceUnsized` impl so it cannot hold
  the type-erased `Arc<dyn Any + Send + Sync>` cancel token at all. 21 errors,
  every loom model down with them. The timer now uses `std::sync` directly, as
  the crate's one documented exception to the single-import rule — loom's
  `Condvar` has no `wait_timeout_while` and its `wait_timeout` ignores the
  duration outright ("TODO: implement timing out"), so an instrumented query
  timer could not model a timeout, which is the only thing about it worth
  modelling. Recorded in `sync.rs`, in `query_timer.rs` and in AGENTS.md, so
  the exception is visible rather than silent.

- **`env_before_connection_cannot_deadlock` now exercises the crate's real
  nested-lock path.** It used to lock two `GroupLock`s of its own in the right
  order, proving the ordering rule is safe to follow and nothing about whether
  the crate follows it — a regression reversing the acquisition order in
  `HandleScope::with_child_group` would not have failed it. It could not do
  better while that function reached the process-wide `registry()`, which
  panics outside an active `loom::model` and cannot be called from inside one
  either. `with_child_group_in` takes a `&Registry`, which is all that stood in
  the way; the model drives it from two threads and loom now reports
  `deadlock; threads = [Blocked, Blocked]` when the order is reversed.

- **`HandleScope::get` does one registry lookup instead of two, and
  `push_diagnostic` one instead of seven.** Every `SQLxxx` entry point in the
  crate reaches its handle through `get`, which called `holds` (a `group_of`
  lookup) and then `resolve` — two acquisitions of the registry lock, two token
  decodes and two bounds checks to answer one question about one slot, plus an
  `Arc` clone `holds` made only to compare and drop, which is an atomic
  increment and decrement on a refcount every other thread on the connection is
  touching. `Registry::resolve_in_group` answers all three questions — live,
  right kind, right group — in a single pass, comparing the group with
  `Arc::ptr_eq` under the read guard and cloning nothing.

  `push_diagnostic`, which runs on **every** error, spelled out a dispatch that
  `diagnostics` already performed: a `holds`, then up to three `get`s each doing
  a `holds` of its own. It now delegates to `diagnostics`, which resolves the
  kind once via the new `resolve_any_in_group`.

  Measured with the new `handle_lookup` benchmark in `bench/`: a bare lookup
  21.16 ns → 15.25 ns (−27.8%), and the error path 76.65 ns → 45.00 ns (−41.3%),
  both p = 0.00. `Registry::resolve` and `Registry::resolve_any` are removed,
  having no callers left.

- **`StatementBackend::close_cursor` is now called by `SQLCloseCursor` and
  `SQLFreeStmt(SQL_CLOSE)`, not only by `SQLEndTran`.** Both previously reached
  only `discard_result_set`, which drops the backend statement — so a backend
  needing to release a server-side cursor, cancel a pending fetch, or return a
  connection to a pool never heard about the most obvious place an application
  closes a cursor, and got a `Drop` in which a failure cannot be reported and
  an async-bridged driver may have no runtime. `close_cursor` is fallible
  precisely because that teardown is a round trip that can fail.

  A failure is now reported with the backend's own SQLSTATE, and the result set
  is discarded **anyway** — otherwise every retry would call the same failing
  backend and the application could never clear the cursor. This mirrors
  `SQLEndTran`'s existing "recorded and carried, not swallowed" handling.

  `SQLFreeStmt(SQL_CLOSE)` calls it only when a cursor is actually open. A
  prepared-but-unexecuted statement (S2/S3) holds a backend statement and no
  cursor, and the spec says the option "has no effect for the application" when
  no cursor is open. `SQLCloseCursor` needs no such check — its `24000` guard
  has already established one is open.

  **Not a compile break.** The trait method is defaulted to `Ok(())`, so a
  driver that never overrode it is unaffected; one that did now sees it called
  in two more places, which is what the trait's own description promises. An
  implementation must be safe to follow with `Drop`, since core still drops the
  statement afterwards.

- **`SQL_DATABASE_NAME` is the current catalog, not the empty string.** The spec
  makes it a second name for one value — "in ODBC 3.x, the value returned for
  this InfoType can also be returned by calling SQLGetConnectAttr with an
  Attribute argument of SQL_ATTR_CURRENT_CATALOG" — so `SQLGetInfo` now reads
  the attribute the connection already stores instead of answering `""` from a
  second place. A backend that knows the real current database still wins:
  `Backend::get_info_raw` is consulted first.

- **`SQL_CURSOR_SENSITIVITY` reports `SQL_UNSPECIFIED` rather than
  `SQL_INSENSITIVE`.** Insensitivity is a promise that no other cursor's changes
  become visible, and core's fetch streams rows from the backend as the
  application asks for them, so it cannot make that promise about rows it has
  not read. `SQL_UNSPECIFIED` — "cursors on the statement handle may make
  visible none, some, or all such changes" — is what core can back.
  `SQLSetStmtAttr(SQL_ATTR_CURSOR_SENSITIVITY)` accepts that value and reports
  `HYC00` for the other two, and `SQLGetStmtAttr` reports the same value the
  info type does.

- **Breaking:** ten new **required** `Backend` methods, each stating something
  about the data source that core was previously deciding on its behalf.
  `quoted_identifier_case` (`SQL_QUOTED_IDENTIFIER_CASE`, the counterpart of the
  already-required `identifier_case`, and independent of it); `txn_capable`
  (`SQL_TXN_CAPABLE`, which had no arm anywhere and so reported `SQL_TC_NONE` —
  "transactions not supported" — even for a backend declaring an isolation
  level and implementing `end_tran`); `integrity` (`SQL_INTEGRITY`, a property
  of the data source's DDL, not of the driver); `multiple_active_txn`
  (`SQL_MULTIPLE_ACTIVE_TXN`, where the old `"N"` understated any driver with
  independent connections); `special_characters` (`SQL_SPECIAL_CHARACTERS`,
  where an empty list is an answer applications act on when deciding what to
  quote, as with `keywords`); `accessible_procedures`
  (`SQL_ACCESSIBLE_PROCEDURES`, the counterpart of `accessible_tables`); and the
  identity four, `driver_name`, `driver_version`, `dbms_name` and
  `dbms_version`, which previously answered with the empty string.

  A test pins `txn_capable` against `txn_isolation_options`: `SQL_TC_NONE` if
  and only if the isolation bitmask is `0`.

  `driver_name` and `driver_version` take no connection, so core now answers the
  whole group the Windows Driver Manager asks for before `SQLDriverConnectW` —
  `SQL_DRIVER_NAME`, `SQL_DRIVER_VER`, `SQL_DRIVER_ODBC_VER`,
  `SQL_ASYNC_DBC_FUNCTIONS` and `SQL_MAX_CONCURRENT_ACTIVITIES`. Overriding
  `Backend::get_info_pre_connect` for those is no longer necessary; it stops
  being a checklist item a driver can forget and becomes two declarations the
  compiler asks for.

- **Breaking:** the ten catalog row types — `TableRow`, `ColumnRow`,
  `PrimaryKeyRow`, `ForeignKeyRow`, `StatisticsRow`, `SpecialColumnRow`,
  `ProcedureRow`, `ProcedureColumnRow`, `ColumnPrivilegeRow` and
  `TablePrivilegeRow` — are `#[non_exhaustive]`, so core can add a column to a
  spec result set without breaking every driver that constructs one. Rust
  rejects a struct expression for such a type outside its own crate, including
  `..Default::default()` (`E0639`), so each type gained one consuming setter per
  column, generated from its field list by the `catalog_rows!` macro. Each takes
  `impl Into<T>`: an `Option<String>` column accepts a bare `String`, a `String`
  column accepts a `&str`, and a nullable numeric column accepts the bare
  number. Migration: replace a struct expression with `Default` plus setters,

  ```rust
  let row = TableRow::default().catalog(catalog).name(name).table_type("TABLE");
  ```

  Adding a column after this is additive — one more setter, which no driver has
  to react to.

- **Breaking:** `Backend::table_types` is a new **required** method, returning
  the table types the data source has (e.g. `["TABLE", "VIEW"]`) for
  `SQLTables`' `SQL_ALL_TABLE_TYPES` enumeration. It has no safe default: an
  empty list is a claim that the data source has no table types, and unlike
  catalogs and schemas there is no capability method to derive it from.
  Migration: add it to your `impl Backend`, in upper case.

- **Breaking:** `Backend::tables` now returns `Vec<TableRow>` instead of
  `Self::Statement`. Core converts the rows to the spec's column layout, sorts
  them into the spec's order (TABLE_TYPE, TABLE_CAT, TABLE_SCHEM, TABLE_NAME)
  and builds the result set itself, so a driver no longer needs an ORDER BY for
  correctness and can no longer get the column order wrong. Migration: return
  the rows your query produced as `TableRow`s and delete the statement
  construction.

- **Breaking (behavioural, not API):** a statement's cancel token is now minted
  per execution rather than once per statement. A backend that stored
  per-statement state on its `CancelToken` must move it onto its `Statement`.

  This fixes a statement being permanently unusable after `SQLCancel`:
  `Backend::cancel` marks the token, and a token reused by the next execution
  stays marked, so every later call on that statement observed a cancellation
  that was not its own. The spec requires the opposite — "After the statement
  has been canceled, the application can call SQLExecute or SQLExecDirect
  again." The previous create-once rule was defending against a `SQLCancel`
  that reaches a finished execution and does nothing, which the spec states is
  correct behaviour: "a call to SQLCancel when no processing is being done on
  the statement ... has is [sic] no effect at all."

- **Breaking:** `Backend::tables`' `table_type` parameter is now
  `table_types: &[String]` — the parsed value list rather than the raw string.
  The spec defines `TableType` as comma-separated values, optionally
  single-quoted (`'TABLE','VIEW'` or `TABLE, VIEW`); core now splits and trims
  it once instead of every driver doing so. An empty slice means no table-type
  filter, which is what `None` meant before. Migration: delete your own
  splitting and match against the slice.

- **Breaking:** `Backend::columns`, `primary_keys`, `foreign_keys`,
  `statistics` and `special_columns` now return `Vec<ColumnRow>`,
  `Vec<PrimaryKeyRow>`, `Vec<ForeignKeyRow>`, `Vec<StatisticsRow>` and
  `Vec<SpecialColumnRow>` respectively, instead of `Self::Statement`. As with
  `tables`, core sorts each result set into its spec order and owns the column
  layout. `SQLForeignKeys` uses the FK ordering (FKTABLE_CAT, FKTABLE_SCHEM,
  FKTABLE_NAME, KEY_SEQ) when `PKTableName` was supplied and the PK ordering
  otherwise. Migration: return the rows your query produced as the matching row
  struct and delete the statement construction; any ORDER BY added purely for
  ODBC compliance can go.

- An infinite `f32`/`f64` read as `SQL_C_CHAR` or `SQL_C_WCHAR` now renders as
  `Infinity`/`-Infinity` rather than Rust's `inf`/`-inf`. The ODBC spec defines
  no textual form for a non-finite float, so this is decided by ecosystem fit:
  Trino, its JDBC driver and PostgreSQL all use `Infinity`. Because this is
  core's shared coercion path, which a driver cannot override, core should not
  impose a Rust-ism on every backend. `NaN` is unchanged — Rust and Java already
  agree on it. Both spellings still parse back into a float, so a backend
  returning either as text is unaffected.

- `HandleScope` is now `!Send`. It is only valid while the group's `MutexGuard`
  is held, and a guard is itself `!Send` because releasing a lock from a thread
  other than the one that took it is undefined for the underlying primitive; a
  `Send` scope could be handed to a scoped thread that then reached handle
  contents while claiming a lock held elsewhere. Unreachable in practice — every
  closure receiving a scope is in-crate, `HandleScope::new` is `pub(crate)`, and
  no driver is ever handed one — so this closes a hole rather than fixing a live
  bug, and cannot break any downstream code, which has no way to hold a scope in
  the first place. A compile-time assertion pins it.

- **Breaking.** `EscapeDialect`, `ColumnDescriptor` and `TypeInfoRow` have
  crate-private fields and a full set of accessors. All three were
  `#[non_exhaustive]` with every field `pub` and a doc comment telling you to
  use the builders — a combination that guaranteed neither: `#[non_exhaustive]`
  stopped struct-literal construction, but nothing stopped a driver reading or
  *assigning* a field directly, so the builders were advisory and core could
  never add an invariant to one.

  A driver that reads `desc.name` now calls `desc.name()`; the 37 accessors
  cover every field, so no read has to be given up. Assignment outside the
  builders is gone by design — that is what the seal is for. Field doc comments
  moved to the accessors, which is now the only place the API is described, so
  the two cannot drift.

  Deliberately complete rather than minimal: adding an accessor later is
  source-compatible, so the cost of covering a field nobody reads yet is nil
  while the cost of omitting one is a break.

- **Breaking.** `types` re-exports its constants by name instead of through a
  `pub use constants::*` glob. Under the glob every `pub const` added to
  `types/constants.rs` joined the public API silently; the surface is now a
  decision rather than a by-product. Because `constants` is a private module, a
  constant left out of the list and not used inside core is `dead_code`, which
  the clippy hook already fails on — so an omission is caught at the point it is
  made rather than discovered by a driver.

  No constant that a driver can reach today was removed by this change alone
  (see **Removed** for the five that went, each for its own reason). In
  particular the `SQL_AT_*`, `SQL_OJ_*`, `SQL_FN_*` and `SQL_SQ_*` bitmask
  families are all still exported despite core never referencing them: they are
  the vocabulary a driver needs to build the values `Backend`'s required
  capability methods return as bare integers. Reference count is not a signal of
  whether a constant is needed here.

- **Breaking.** `SQL_DATETIME` is derived from `odbc_sys::SqlDataType::DATETIME`
  rather than restated as `9`. Its counterpart `SQL_INTERVAL` stays a literal
  because `odbc-sys` has no `SqlDataType::INTERVAL`, only the concise
  `EXT_INTERVAL_*` codes. The type is unchanged (`i16`), so no caller is
  affected; the point is that the value now has one definition instead of two.

- **Breaking.** The `Backend` capability declarations take
  `&Self::Connection`. `SQLGetInfo` is a per-connection call, so what a data
  source can do is a property of the connection rather than of the driver
  binary; as associated functions these forced a driver to pick one answer for
  every server it would ever talk to. A backend gating a capability on server
  version can now read it from the connection, which is what `types::version`
  was added for and had no way to be used for.

  Affected: the 25 required capability methods (`supports_catalogs`,
  `supports_schemas`, `alter_table_support`, `outer_join_capabilities`,
  `group_by`, `null_collation`, `identifier_case`, `correlation_name`,
  `non_nullable_columns`, `expressions_in_order_by`, `sql_conformance`,
  `subqueries`, `column_alias`, `concat_null_behavior`, `union_support`,
  `convert_functions`, `order_by_columns_in_select`, `accessible_tables`,
  `data_source_read_only`, `search_pattern_escape`, `keywords`,
  `timedate_add_intervals`, `timedate_diff_intervals`, `default_txn_isolation`,
  `txn_isolation_options`), plus `get_type_info` and `escape_dialect`.

  Three declarations deliberately keep no connection: `cursor_commit_behavior`,
  `cursor_rollback_behavior` and `catalog_result_column_widths`. The first two
  because `SQLGetInfo` must answer `SQL_CURSOR_COMMIT_BEHAVIOR` and
  `SQL_CURSOR_ROLLBACK_BEHAVIOR` before a connection exists — the Windows Driver
  Manager queries info types ahead of `SQLDriverConnectW`, and the fallback
  there is `SQL_CB_DELETE`, the claim those hooks exist to stop core inventing.
  The third because four catalog functions return an empty result set without
  resolving a connection, and requiring one would add a lookup and an error path
  to paths that have neither. The general rule behind both: a declaration
  consumed on a path that has no connection cannot require one.

  `default_get_info` and `common_get_info_raw` take `Option<&B::Connection>`
  accordingly. Pre-connect they answer only what is knowable without a data
  source — driver identity, core's own implementation facts, and the limits the
  spec defines `0` for — and return `None` for the rest, where they previously
  answered from an invented value.

  Two consequences worth noting for drivers. `SQLGetTypeInfo` now requires an
  open connection, since the type list is the data source's. And
  `SQL_ATTR_TXN_ISOLATION` set before connecting is checked only for naming
  exactly one level; the check against `txn_isolation_options` moves to connect
  time, in `apply_pending_txn_isolation`, so an unsupported level fails the
  connect rather than being applied silently.

- `#![deny(missing_docs)]`, with the 183 doc comments it required. Every public
  item now documents itself, including all 78 `FunctionId` variants, the 43
  catalog result-set column variants, the 19 `SQLGetTypeInfo` row fields, every
  `OdbcError` variant and both `Backend` associated types. `common_get_info_raw`
  — the one function AGENTS.md tells every driver to call — had none: its doc
  block sat directly above a private function with no blank line between them,
  so rustdoc merged the two and attached both to the private one.
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
- `test_support::attach_connection` and `test_support::detach_connection`,
  behind the same default-off `test-support` feature as `conformance`. They put
  a connection object into an allocated connection handle without calling
  `Backend::connect`, and take it back out without calling
  `Backend::disconnect`, so a driver can exercise core's *connected* paths —
  the `SQLGetInfoW` fallback chain, the catalog functions, the conformance suite
  — with no data source running. `handles` is `pub(crate)`, so a driver
  previously reached into `ConnectionHandle` directly; these are the supported
  replacement, taking the same opaque token an application holds and validating
  it the way every FFI entry point does. `detach_connection` exists because
  `SQLFreeHandle` refuses a still-connected handle (`HY010`), and calling
  `SQLDisconnect` on a connection that never opened is not always meaningful.
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
- **Breaking:** `TypeInfoRow::nullable` is a `Nullable`, not an `i16`, matching
  `ColumnDescriptor::nullable`: the two describe the same ODBC concept, and
  leaving one a raw `SQL_NULLABLE_*` literal was the exact kind of untyped
  integer the project's own conventions rule out elsewhere. `TypeInfoRow::new`'s
  default and `TypeInfoRow::with_nullable` change accordingly. A driver's
  `get_type_info` rows built with `.with_nullable(1)` need
  `.with_nullable(Nullable::SqlNullable)` (or `SqlNoNulls` /
  `SqlNullableUnknown`) instead; a row still assembled as a struct literal
  needs the same field-value change.
- **Breaking:** `ColumnDescriptor` is `#[non_exhaustive]` and gains `searchable`,
  `literal_prefix`, `literal_suffix`, `table_name`, `schema_name` and
  `catalog_name`. `SQLColAttribute` hard-coded all six; a backend that tracks a
  column's origin or its type's literal form can now report it, and the previous
  values remain the defaults. Build descriptors with `ColumnDescriptor::new` and
  the `with_*` builders, which stay source-compatible as fields are added.
- **Breaking:** `EscapeDialect`, `TypeInfoRow`, `CatalogResultColumnWidths` and
  `FunctionId` are `#[non_exhaustive]`, and the first three have constructors
  and `with_*` builders — without which a `#[non_exhaustive]` struct simply
  cannot be built from another crate. `EscapeDialect` is the cautionary case:
  adding `rewrite_scalar_fn` to it was already a silent breaking change (commit
  `886007b`, labelled `feat:`), which cost nothing only because nothing was
  released.

  Most of `TypeInfoRow`'s builders are `const fn`; the constructor and the
  three builders that set a string field are not — see the entry below on
  `TypeInfoRow`'s `Cow` fields for why.
- **Breaking:** `FetchResult` and `OutputParam` are `#[non_exhaustive]` too.
  `FetchResult` gaining a variant is not a hypothetical: block cursors will
  need one, and a driver that matches on it exhaustively today should get a
  compiler nudge then rather than force every other driver through a version
  bump. `OutputParam` already had `OutputParam::new`, so it stays constructible
  from a driver crate without any further change.
- **Breaking:** `Backend::sensitive_connect_keywords`, `Backend::get_functions`,
  `Backend::get_type_info`, `Backend::browse_connect_attrs`,
  `Backend::search_pattern_escape` and `Backend::keywords` return
  `Cow<'static, ...>` instead of `&'static ...`. `get_type_info`,
  `search_pattern_escape` and `keywords` take a connection because their
  answer can genuinely differ by data source, so it is computed rather than
  a `'static` borrow — a backend deriving its answer from a server-version
  probe, say, had no way to express that under the old signature.
  `sensitive_connect_keywords` and `browse_connect_attrs` take no connection
  and their answer never varies; they change to the same shape only for
  uniformity of the trait's vocabulary. The three list-returning methods use
  `Cow<'static, [Cow<'static, str>]>`, not `Cow<'static, [&'static str]>`, so
  each element can be owned too — an outer `Cow` around borrowed `&'static
  str` elements would still block a backend that computes its keyword list at
  runtime.

  `TypeInfoRow`'s own `type_name` field and its four `Option<&'static str>`
  fields are `Cow<'static, str>` / `Option<Cow<'static, str>>` for the same
  reason: a `Cow`-returning `get_type_info` is cosmetic if the rows it builds
  still cannot hold an owned string. `TypeInfoRow::new`,
  `with_literal_affixes`, `with_create_params` and `with_local_type_name` take
  `impl Into<Cow<'static, str>>`, so a `&'static str` literal still works
  unchanged. Converting a value through `Into` cannot run in a const context,
  so those four are no longer `const fn`; a driver that built its type list in
  a `static` now assembles it from a function instead (behind an `OnceLock` if
  that is expensive to repeat). The remaining builders touch no string field
  and stay `const`.
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
  generation rather than a tag. `ConnectionHandle::env` and
  `StatementHandle::conn` hold tokens rather than addresses.
  No application or Driver Manager sees any difference — `SQLHANDLE` is opaque
  to both — and `forward_ffi!`'s exported signatures are unchanged, so a driver
  crate needs no edit unless it reached into these internals, which nothing in
  the FFI surface requires.
- **Breaking:** `EnvironmentHandle::connections` and
  `ConnectionHandle::statements` are removed. Parentage is derived from the
  handle registry (`Registry::children_of`) rather than stored as a list on the
  parent, which is what lets a walk over a connection's statements — or an
  environment's connections — take an owned snapshot instead of a borrow of a
  field another thread could be freeing an entry out from under. `handles` is
  now `pub(crate)`, so nothing outside this crate reached these fields anyway.
- **Breaking:** `as_handle_ref` and `try_get_diagnostic_queue`, the two
  functions that reached a handle's contents by resolving its token directly,
  are removed. A `HandleScope` is now the only way to reach handle contents,
  and `panic_safe` — which locks the target's lock group before constructing
  one — is the only way most code gets one; see the per-connection lock
  entries below.
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
- `test_support::attach_connection` and `test_support::detach_connection` now
  hold the connection's lock group for the duration, exactly as an FFI entry
  point would, and catch a panic instead of letting it unwind out of this
  crate into the driver's test binary. Signatures are unchanged; a caller
  only sees this if it calls one of these while another thread is inside a
  call on the same connection, in which case it now blocks rather than racing
  an unguarded read or write of the connection handle, or if the closure
  panics, in which case the caught panic is reported as `Err(OdbcError::Panic)`
  rather than silently discarded — previously the panic never reached this
  code (it always ran to completion), so there was nothing to report.
- `SQLGetDiagRecW` and `SQLGetDiagFieldW` now hold the queried handle's lock
  group while reading its diagnostics, so a concurrent call on the same
  handle can no longer read the diagnostic queue mid-mutation — previously
  the only two FFI entry points reaching handle state without the group lock.
  They also gain panic protection: a panic inside either previously unwound
  across the C ABI (undefined behaviour); it is now caught and reported as
  `SQL_ERROR`, consistent with every other entry point. Neither change alters
  their spec-observable behaviour: both still read without clearing the queue
  or posting a diagnostic for themselves, per the spec's own exception to the
  clear-at-entry rule for these two functions.
- **Breaking.** `Backend::cancel` no longer takes `&mut Self::Statement`.
  `SQLCancel` must be able to signal a statement from a thread holding no lock
  on its connection, concurrently with another thread executing on it — a
  `&mut Self::Statement` cannot be produced under that constraint at all.
  `Backend` gains an associated `CancelToken: Send + Sync + 'static` and
  `cancel_token(conn: &Self::Connection) -> Self::CancelToken`; `cancel` now
  takes `&Self::CancelToken`. Core builds the token once, the first time a
  statement makes a backend call, and never replaces it — see
  `Backend::CancelToken`'s doc comment for the two legitimate token shapes
  (standalone vs. aliasing the connection) and the failure modes each guards
  against.

  `cancel_token` has no default, so every existing driver needs one to keep
  compiling; a backend that cannot cancel anything can add the minimal stub
  `type CancelToken = ();` and `fn cancel_token(_: &Self::Connection) {}` and
  move on (`cancel` itself already defaults to `NotImplemented`).

  All nine statement-producing methods (`exec_direct`, `prepare`, `execute`,
  `tables`, `columns`, `primary_keys`, `foreign_keys`, `statistics`,
  `special_columns`) now take `cancel: &Self::CancelToken` immediately after
  `conn`. `get_type_info` does not, since it returns a `&'static` slice with no
  I/O to cancel. This is what makes `cancel_token`'s doc comment followable for
  a backend whose cancellation needs a value only known at execution time (a
  query id, say): `cancel_token` returns an empty shared slot, and the
  statement-producing call that actually runs the query fills it, because it
  now receives that exact token.

### Removed

- **Breaking.** `SQL_ADD`, `SQL_UPDATE_BY_BOOKMARK`, `SQL_DELETE_BY_BOOKMARK`
  and `SQL_FETCH_BY_BOOKMARK`. `odbc_sys::BulkOperation` is a `#[repr(u16)]`
  enum carrying all four, so these were the redefinition the crate's odbc-sys
  rule forbids. Replace a use with `BulkOperation::Add as i16` (the form the
  spec-value table in AGENTS.md already prescribes), or convert an incoming raw
  value with `bulk_operation_from_raw`.

- **Breaking.** `SQL_DIAG_MESSAGE_TEXT`, which restated
  `odbc_sys::HeaderDiagnosticIdentifier::MessageText` as the literal `6`. It was
  already dead inside core: `ffi/diag.rs` defines its own private constant
  derived from the odbc-sys variant and used that instead, so the exported one
  had no reader and no protection against drifting from the value core actually
  compared against. Use `HeaderDiagnosticIdentifier::MessageText as i16`.

### Fixed

- **`SQLBindParameter`'s `ColumnSize` is now enforced for character and binary
  parameters.** A value longer than the declared size is rejected with `22001`
  ("string data, right truncation") instead of reaching the backend whole, per
  the `SQL_CHAR`, `SQL_WCHAR` and `SQL_BINARY` rows of the "C to SQL:
  Character" conversion table and the binary row of "C to SQL: Binary". A
  `ColumnSize` of `0` still means "no size declared" and disables the check, as
  it already does for `SQL_DECIMAL` and `SQL_NUMERIC`. The narrow character row
  is measured in characters rather than the spec's literal bytes, because
  `ColumnSize` is declared in characters and a literal reading rejects valid
  multi-byte values; the reason is recorded on `text_to_sql_type`. A
  `SQL_C_BINARY` parameter bound to a *non*-binary SQL type is still unchecked
  and still reaches the backend as raw bytes — that is a missing conversion
  rather than a missing check, and is tracked on the same doc comment.

- **`SQLDescribeColW` and `SQLColAttributeW` no longer report every describe
  failure as `07009` "column number out of range".** Both wrapped
  `StatementBackend::describe_col` in a `map_err(|_| ...)` that discarded the
  backend's error outright and substituted `07009` with a "column number N out
  of range" message — so a communication failure, a cancellation, or any other
  genuine error reached the application as a bad column number. Core now does
  the range check itself against `StatementBackend::column_count` before
  calling, which the spec permits: the `(DM)` marker on the `07009` row covers
  only its bookmark clause, leaving "greater than the number of columns in the
  result set" to the driver. With the range case handled first, the backend's
  own SQLSTATE propagates unchanged — `08S01` for a link failure, `HY000`
  otherwise — and `07009` is returned only for the case its message describes.
  `SQLColAttribute`'s table lists no `08S01` row, but its page states it "can
  return any SQLSTATE that can be returned by SQLPrepare or SQLExecute" when
  called between the two, so one passing through is legal and is not filtered.

  This also unblocks `HY008` on both functions, which the `HY008` work could
  not reach: reclassifying a cancelled call was a no-op while the SQLSTATE was
  being overwritten unconditionally. A `describe_col` failure whose cancel
  token reports signalled is now reported as `HY008`.

- **`SQL_ATTR_QUERY_TIMEOUT` now bounds `SQLFetch`, not only the
  statement-producing calls.** Core armed its timer at `SQLExecDirect`,
  `SQLPrepare`, `SQLExecute`, `SQLParamData` and the ten catalog functions, and
  nowhere on the cursor-consuming path — so against a data source that returns
  column metadata before it computes anything, the deadline bounded the one call
  that was already fast and left the slow one unbounded. Measured against a live
  Trino coordinator with no Driver Manager involved, under a two-second
  deadline: `SQLExecDirect` on `SELECT count(*) FROM tpcds.sf10.store_sales`
  returned `SQL_SUCCESS` in 0.1 s, and the following `SQLFetch` returned
  `SQL_SUCCESS` after 24.6 s. `SQLFetch`'s diagnostics table carries `HYT00`
  with no `(DM)` marker and names this attribute directly, and its "Errors and
  Warnings on the Entire Function" section gives `HYT00` as its example, so the
  site was the driver's to arm. A backend answering `QueryTimeout::CoreCancels`
  now gets a deadline over the fetch and its bound-column reads, relabelled
  `HYT00` on expiry. `SQLFetchScroll` is covered by the same change, since
  `SQL_FETCH_NEXT` delegates to `SQLFetch` and every other orientation is
  rejected with `HY106` before reaching the backend. `SQLGetData` is deliberately
  **not** armed: its diagnostics table carries `HYT01` and no `HYT00` row.

- **`SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` now returns `HY011` when a
  transaction is open.** The spec states this three times — the `HY011` row
  ("the *Attribute* argument was SQL_ATTR_TXN_ISOLATION, and a transaction was
  open"), the attribute's own description ("an application must call
  `SQLEndTran` to commit or roll back all open transactions on a connection,
  before calling `SQLSetConnectAttr` with this option") and footnote [3] — and
  core checked none of them, so an application could change isolation level
  mid-transaction and get `SQL_SUCCESS` for a change the data source could not
  honour retroactively.

  Core now tracks it as `ConnectionHandle::txn_dirty`: set when a
  statement-producing call runs while `SQL_ATTR_AUTOCOMMIT` is
  `SQL_AUTOCOMMIT_OFF`, cleared by `SQLEndTran` and by switching autocommit back
  on (which the spec says commits any open transaction). It is set *before* the
  backend call rather than after it succeeds, because a call that fails partway
  may still have opened a transaction and the spec's requirement is to refuse
  the change while one might be open. No backend hook and no driver change.

- **`SQLSetConnectAttr(SQL_ATTR_CURRENT_CATALOG)` now returns `24000` when a
  result set is pending.** The spec's row — "the *Attribute* argument was
  SQL_ATTR_CURRENT_CATALOG, and a result set was pending" — carries no `(DM)`
  marker, so it is the driver's to return, and it was not returned at all.
  Switching the catalog out from under an open cursor leaves the application
  fetching rows from one catalog while its unqualified names resolve in another.

  A pending result set is an open cursor on one of the connection's statements,
  which core already tracked per statement, so this needs no new state and no
  backend hook. Note that this is deliberately *not* the same condition as the
  neighbouring `HY011` row ("the *Attribute* argument was
  SQL_ATTR_TXN_ISOLATION, and a transaction was open"), which remains
  unimplemented: a `SELECT` under autocommit leaves a cursor open with no
  transaction, so answering either condition with the other's state would make
  both wrong.

- **`Backend::cancel_token`'s doc comment described the opposite lifetime to the
  one core implements.** It stated that a token, once built, "is never replaced
  for the life of the statement, including across a later `SQLExecute` on the
  same handle". `mint_cancel_token` in fact mints a new token at every
  statement-producing call and replaces the statement's stored one, which is the
  deliberate behaviour — a single token per statement left a cancelled statement
  permanently unusable, contradicting the spec's "After the statement has been
  canceled, the application can call SQLExecute or SQLExecDirect again." No
  behaviour change; the documentation a driver author designs their token
  against now matches the code.

- **`SQLGetStmtAttr` wrote four bytes where the spec declares `SQLULEN`.** Every
  non-pointer attribute on the `SQLSetStmtAttr` page is declared "An SQLULEN
  value" — not one is `SQLUINTEGER` — and `BufferLength` is ignored for them, so
  the application's buffer is `SQLULEN`-wide and the driver must fill it.
  `SQLGetStmtAttr`'s own Comments describe the alternative as a defect to work
  around: "if the value is a SQLULEN value, some drivers may only write the
  lower 32-bit or 16-bit of a buffer and leave the higher-order bit unchanged.
  Therefore, applications should use a buffer of SQLULEN and initialize the
  value to 0 before calling this function." An application that did not zero its
  buffer read `SQL_ATTR_MAX_ROWS` as `0xFFFFFFFF00000000` — an enormous row
  limit — where the driver meant "no limit". All twenty integer-valued reads now
  write a full `SQLULEN`; the pointer-valued ones already did. On the connection
  side only `SQL_ATTR_ASYNC_ENABLE` and `SQL_ATTR_ODBC_CURSORS` are `SQLULEN`
  and were widened; every other connection attribute really is `SQLUINTEGER` and
  is unchanged.

- **`SQLSetStmtAttr` now answers for every value it cannot honour.** Core drives
  one forward-only, read-only cursor over one parameter set, and the spec gives
  two ways to say so. The `01S02` row names the eight attributes a driver may
  substitute for — `SQL_ATTR_CONCURRENCY`, `SQL_ATTR_CURSOR_TYPE`,
  `SQL_ATTR_KEYSET_SIZE`, `SQL_ATTR_MAX_LENGTH`, `SQL_ATTR_MAX_ROWS`,
  `SQL_ATTR_QUERY_TIMEOUT`, `SQL_ATTR_ROW_ARRAY_SIZE` and
  `SQL_ATTR_SIMULATE_CURSOR` — and each is now stored at the value core
  actually uses, with `01S02` posted so `SQLGetStmtAttr` reports it back.
  Attributes off that closed list have no substitution to offer and report
  `HYC00`: `SQL_ATTR_USE_BOOKMARKS` other than `SQL_UB_OFF` (bookmarks are not
  implemented and `SQL_ATTR_FETCH_BOOKMARK_PTR` is not read),
  `SQL_ATTR_RETRIEVE_DATA` = `SQL_RD_OFF` (`SQLFetch` writes bound buffers
  unconditionally), `SQL_ATTR_CURSOR_SENSITIVITY` = `SQL_SENSITIVE`,
  `SQL_ATTR_ENABLE_AUTO_IPD` = `SQL_TRUE` (a case the spec's `HYC00` row names,
  given `SQL_ATTR_AUTO_IPD` is `SQL_FALSE`), and `SQL_ATTR_ASYNC_ENABLE` =
  `SQL_ASYNC_ENABLE_ON` (`SQL_ASYNC_MODE` is `SQL_AM_NONE`). An application
  that asks for any of these no longer reads its own request back as though the
  driver had agreed to it. `SQL_ATTR_CURSOR_SCROLLABLE` and
  `SQL_ATTR_PARAMSET_SIZE` are substituted although the `01S02` list does not
  name them; both deviations are documented at their arms.

- **Every statement attribute the driver recognises is now readable.**
  `SQL_ATTR_KEYSET_SIZE`, `SQL_ATTR_PARAM_BIND_TYPE`,
  `SQL_ATTR_PARAMS_PROCESSED_PTR`, `SQL_ATTR_PARAM_STATUS_PTR`,
  `SQL_ATTR_PARAM_BIND_OFFSET_PTR`, `SQL_ATTR_PARAM_OPERATION_PTR`,
  `SQL_ATTR_ROW_OPERATION_PTR`, `SQL_ATTR_FETCH_BOOKMARK_PTR` and
  `SQL_ATTR_ASYNC_STMT_EVENT` have `SQLGetStmtAttr` arms, so a value
  `SQLSetStmtAttr` accepts can be read back rather than answering `HYC00`. A
  test drives this off `statement_attribute_from_raw`, so a recognised
  attribute that is not readable fails rather than being noticed later.

- **An execution reports its parameter set through
  `SQL_ATTR_PARAMS_PROCESSED_PTR` and `SQL_ATTR_PARAM_STATUS_PTR`.**
  `SQLSetStmtAttr` stores both pointers, and `SQLExecDirect`, `SQLExecute` and
  the `SQLParamData` data-at-execution completion now write through them: the
  processed count is `1`, since `SQL_ATTR_PARAMSET_SIZE` is pinned at 1, and the
  first status-array element is `SQL_PARAM_SUCCESS` or, when the execution
  failed, `SQL_PARAM_ERROR`. This is the parameter-side counterpart of what
  `SQLFetch` already wrote through `SQL_ATTR_ROWS_FETCHED_PTR` and
  `SQL_ATTR_ROW_STATUS_PTR`, and an application that binds a status array to
  detect per-set errors now reads a value rather than its own initial buffer
  contents.

- **`SQLSetConnectAttr` enforces the state and support rules its spec page
  assigns to the driver.** `SQL_ATTR_PACKET_SIZE` reports `HY011` once the
  connection is open, which the spec states directly — "if the application sets
  packet size after a connection has already been made, the driver will return
  SQLSTATE HY011 (Attribute cannot be set now)". `SQL_ATTR_ASYNC_ENABLE` =
  `SQL_ASYNC_ENABLE_ON` and `SQL_ATTR_ENLIST_IN_DTC` report `HYC00`: core is
  synchronous and `SQL_ASYNC_MODE` is `SQL_AM_NONE`, and core enlists in no
  distributed transaction, so accepting either would leave an application
  believing in behaviour it does not get. Unrecognized attributes are still
  accepted silently for Driver Manager and tool compatibility.
  `SQL_ATTR_ASYNC_ENABLE` and `SQL_ATTR_TRANSLATE_OPTION` also gained
  `SQLGetConnectAttr` arms, so every attribute the setter stores can be read
  back.

- **`SQL_ATTR_METADATA_ID` set on a connection never reached its statements.**
  `SQLSetStmtAttr`'s Comments make it one of exactly two attributes an
  application may set at the connection level — "ODBC 3.x statement attributes
  cannot be set at the connection level, with the exception of the
  SQL_ATTR_METADATA_ID and SQL_ATTR_ASYNC_ENABLE attributes, which are both
  connection attributes and statement attributes, and can be set at either the
  connection level or the statement level". `SQLSetConnectAttr` stored the
  value and `SQLGetConnectAttr` read it back correctly, but nothing else ever
  looked: `metadata_id_enabled` consults the statement's own attribute map, and
  a statement was allocated with an empty one. An application taking the
  connection-level route got `SQL_SUCCESS`, saw its value echoed back, and then
  had every catalog call treat its arguments as search patterns rather than
  identifiers — no case folding, no `%`/`_` escaping, and wrong result sets
  with no diagnostic to explain them. A statement now starts from its
  connection's value. Per the ODBC 2.x rule the connection-level route
  inherits, this is the default for statements allocated *afterwards*;
  statements that already exist are untouched, and a later `SQLSetStmtAttr`
  overrides the inherited value as before. `SQL_ATTR_ASYNC_ENABLE`, the other
  of the two, is deliberately not inherited: core reports `SQL_AM_NONE` for
  `SQL_ASYNC_MODE`, so the only value a connection can hold is the statement
  default already.

- **Character parameter data ignored the SQL type it was bound as.**
  `SQLBindParameter` takes two types — `ValueType`, the C type the value
  arrives in, and `ParameterType`, the SQL type the data source is to receive —
  and ODBC makes the driver convert between them. `read_param_value` matched on
  the C type alone, so `ParameterBinding::sql_type` was recorded and never
  read. For every C type but the two character ones that lost nothing, because
  the C type already fixes the value's shape; for `SQL_C_CHAR` and
  `SQL_C_WCHAR` it discarded the only statement of what the text *was*.
  `SQL_C_CHAR` + `SQL_NUMERIC` — what pyodbc emits for a `Decimal`, and what
  any client emits for a value it delivers as text — reached the backend as
  `ColumnValue::String`, so a driver that renders its parameters emitted
  `WHERE amount = '12.34'` against a decimal column and the data source
  rejected the comparison. The new `param_convert` module is the spec's
  "C to SQL: Character" table transcribed: decimal, exact-integer,
  approximate-numeric, `SQL_BIT`, binary (hexadecimal pairs) and the three
  datetime targets, each with the SQLSTATE that table's third column gives —
  `22018` for text that is not a literal of the declared type, `22001` for a
  conversion that would truncate, `22003` for out of range, `22008` for a
  datetime component the target cannot hold. **Driver-visible:** a backend now
  receives `ColumnValue::Decimal`, `I32`, `Timestamp` and so on where it
  previously received `String` for these bindings; one that parsed the string
  itself can drop that code, and one that matched only `String` must handle the
  typed variants. Character SQL types, the interval types, `SQL_GUID` and
  driver-specific type identifiers are unchanged and still arrive as `String`.
  `SQLPutData` data-at-execution text goes through the same conversion, so the
  two routes to a parameter agree.

- **A `DECIMAL` parameter's declared precision and scale were not enforced.**
  `text_to_sql_type` checked that character parameter data denoted the declared
  type but never measured it against `SQLBindParameter`'s `ColumnSize` and
  `DecimalDigits`, so `12.345` bound as `DECIMAL(10,2)` reached the backend
  whole and a thirty-digit value bound the same way did too. Both are `22001`
  on the spec's "C to SQL: Character" table — "data converted with truncation
  of fractional digits" and "conversion of data would result in loss of whole
  (as opposed to fractional) digits" — and both are now returned. A
  `ColumnSize` of `0` reads as "no size declared" rather than a zero-digit
  column, since no decimal has zero digits of precision and the spec defines no
  sentinel; a negative `DecimalDigits` disables the check, because it asks for a
  rounding core has none to apply. Trailing zeros beyond the declared scale are
  not truncation, and the value is passed on as the application wrote it — this
  check validates, it does not reshape digits. Only `SQL_DECIMAL` and
  `SQL_NUMERIC` are checked: `ColumnSize` is mantissa bits for the approximate
  numerics, whose test is range and already applied, and the spec states that
  "for other data types, the *ColumnSize* argument is ignored". The declared
  size for character and binary targets remains unenforced; `text_to_sql_type`'s
  "Declared size" note records what is missing and why the character case needs
  a `Backend` hook to answer at all.

- **`?` was counted as a parameter marker inside quoted identifiers and
  comments.** `count_params` tracked single-quoted string literals and nothing
  else, so `SELECT "a?b" FROM t` reported one parameter and `SELECT 1 -- huh?`
  reported one too. `SQLNumParams` over-reported, `collect_params` padded the
  phantom marker with a value, and a driver whose own substitution scan
  mirrored core's rewrote the identifier along with it. The scan now skips
  string literals, delimited identifiers, `--` line comments and `/* … */`
  block comments, taking the identifier delimiters from the backend's
  `EscapeDialect` rather than assuming `"`. The region helpers are `escape`'s
  own, shared with `translate_escapes` so the two scans cannot drift apart
  again.

- **A parameter marker with no bound value was padded with NULL.**
  `collect_params` emitted `ColumnValue::Null` for a marker the application
  never called `SQLBindParameter` for, so `WHERE x = ?` with nothing bound ran
  as `WHERE x = NULL`, matched no row and reported success — the application
  saw an empty result set rather than its own mistake. Both `SQLExecute` and
  `SQLExecDirectW` now report `07002` (COUNT field incorrect), which is the
  first clause of that row on both diagnostics tables and carries no `(DM)`
  marker. The data-at-execution scan rejects the same gap, so it is not a
  second route to the old behaviour. A `SQL_PARAM_OUTPUT` binding still yields
  `Null` and is unaffected: it has no input value by definition, and reading
  its uninitialised buffer would be unsound.

- The 32 `HY008` doc comments across `src/ffi/` claimed the state could not
  arise, on one of two false grounds: that "the `Backend` trait is synchronous"
  — which says nothing about another thread cancelling — or that it was
  `(DM)`-handled, which the spec contradicts, since its `HY008` row carries no
  `(DM)` marker on any of these pages. Each now states which of the row's two
  clauses applies and why, in one of three shapes: the call reclassifies, it is
  connection-level and has no token to observe, or it makes no fallible backend
  call for a cancellation to be reported through.

- `SQLDescribeParam` now returns `HY008` on a cross-thread cancel.
  `Backend::describe_param` is a fallible backend call, so a backend answering
  it over the wire could be cancelled mid-lookup and reported `HY000`.

- `SQLAllocHandle` returned a bare `SQL_ERROR` with no diagnostic for
  `SQL_HANDLE_DESC` and `SQL_HANDLE_DBC_INFO_TOKEN`. It now posts `HYC00`, which
  this function's diagnostics table lists un-annotated for exactly that case.

- `SQLFreeStmt` returned `SQL_ERROR` for an unrecognised `Option` *before*
  entering `panic_safe`, so there was no handle to post onto and `SQLGetDiagRec`
  answered `SQL_NO_DATA`. The parse now happens inside `panic_safe` and posts
  `HY092`, which the function's own documentation already promised.

- `SQLFreeHandle` returned a bare `SQL_ERROR` with no diagnostic for
  `SQL_HANDLE_DESC` and `SQL_HANDLE_DBC_INFO_TOKEN`, so `SQLGetDiagRec` answered
  `SQL_NO_DATA` and the application had a failure it could neither report nor
  branch on. It now posts `HY000` — the code this function's diagnostics table
  actually offers, which lists no `HYC00`.

- `SQLFreeHandle` and `SQLAllocHandle` did not clear the relevant handle's
  diagnostic queue at entry, so a failed call served the *previous* call's
  SQLSTATE as record 1 and an application reading diagnostics saw an error
  describing something else. This was reachable exactly when it mattered: a
  `SQLFreeHandle` that fails leaves the handle valid, which is when an
  application reads diagnostics at all. With these two, every FFI entry point
  that should clear its queue now does.

- `SQLDescribeCol` reported a column size of `18446744073709551612`
  (`SQL_NO_TOTAL` widened into the `SQLULEN` the parameter actually is) for any
  column whose length the backend could not determine, such as an unbounded
  `VARCHAR` — which is every column a `DESCRIBE`, `SHOW` or `EXPLAIN` returns.
  An application sizing a buffer from it asks for 18 exabytes. The spec's
  `ColumnSizePtr` text requires `0` for that case; `SQL_NO_TOTAL` belongs to
  `SQL_DESC_LENGTH` and `SQL_DESC_DISPLAY_SIZE` via `SQLColAttributeW`, which
  are unchanged.

- Five statement attributes were accepted, stored and then never acted on, so
  `SQLGetStmtAttr` read them back and confirmed a behaviour the driver did not
  have. They now split two ways, along the line the spec draws.

  `SQL_ATTR_MAX_ROWS` and `SQL_ATTR_QUERY_TIMEOUT` are substituted with `0` (no
  limit, no timeout) and reported as `01S02`. Both are named on the spec's own
  01S02 substitution list, which is closed — nothing counts rows and `Backend`
  is synchronous with no deadline, so an application that set a 30-second
  timeout and got `SQL_SUCCESS` would wait forever on a runaway query.

  `SQL_ATTR_ROWS_FETCHED_PTR`, `SQL_ATTR_ROW_STATUS_PTR` and
  `SQL_ATTR_ROW_BIND_OFFSET_PTR` are now **honoured** rather than substituted.
  The 01S02 list names none of them, and there is no "similar value" to
  substitute for a pointer; with `SQL_ATTR_ROW_ARRAY_SIZE` pinned at 1 the
  rowset has exactly one row, so `SQLFetch` writes `1` (and `0` at
  `SQL_NO_DATA`) through the rows-fetched pointer, `SQL_ROW_SUCCESS` /
  `SQL_ROW_SUCCESS_WITH_INFO` into the first row-status element, and adds the
  bind offset to every bound column and indicator address. Ignoring the last of
  those put bound data at the base address instead of the offset one.

  The existing `set_and_get_query_timeout` test asserted the old behaviour —
  that setting 30 succeeds and reads back 30 — so it pinned the defect and has
  been rewritten.

- `SQL_PARAM_OUTPUT` parameters are no longer read as input values on execute.
  An output-only parameter has no input to send: the application binds that
  buffer for the *driver* to fill and never has to initialise it. Core read it
  anyway and passed whatever it found to `Backend::execute`.

  For `SQL_C_CHAR` with an absent or `SQL_NTS` indicator that was undefined
  behaviour, not just a wrong value — the read falls back to `CStr::from_ptr`,
  which scans for a terminator the application had no reason to write, so the
  scan runs past the end of the buffer. Miri reports the out-of-bounds access on
  the regression test when the fix is removed.

  `collect_params` now emits `ColumnValue::Null` for an `SQL_PARAM_OUTPUT`
  binding, the mirror image of `write_output_params` refusing to write back
  through an input-only one. `SQL_PARAM_INPUT_OUTPUT` is still read, since it
  does carry an input value.

- `SQLGetInfoW` no longer writes four bytes into a two-byte buffer for
  `SQL_ODBC_API_CONFORMANCE` (9), `SQL_ODBC_SAG_CLI_CONFORMANCE` (12),
  `SQL_ODBC_SQL_CONFORMANCE` (15) and `SQL_MAX_PROCEDURE_NAME_LEN` (33). The
  spec declares all four `SQLUSMALLINT`, and `SQLGetInfo` *ignores*
  `BufferLength` for a non-string value — the driver is required to assume the
  buffer matches the type the spec declares — so an application that correctly
  allocated two bytes had two more overwritten past the end.

  These four are the ones `odbc_sys::InfoType` does not model, so
  `info_type_from_raw` returned `None` and the existing shape-aware fallback had
  no shape to honour, dropping them on the generic `U32(0)`. They are now listed
  explicitly, which is the only option available: being absent from odbc-sys is
  precisely what leaves nothing to derive the shape from. `SQL_CURSOR_ROLLBACK_BEHAVIOR`
  (24) is in the same position but was already answered by `common_get_info_raw`.

- `SQLGetData` can retrieve variable-length data in parts, which is what the
  spec's whole "Retrieving Variable-Length Data in Parts" section describes and
  what the documented application pattern
  `while ((rc = SQLGetData(...)) == SQL_SUCCESS_WITH_INFO)` depends on. Every
  call previously restarted at the beginning of the value, so that loop never
  terminated: an application reading a column larger than its buffer hung, and
  one that ignored the truncation warning silently kept only the first chunk.

  A statement now tracks how far `SQLGetData` has read. Each call delivers the
  next part and returns `SQL_SUCCESS_WITH_INFO` with `01004`, the last part
  returns `SQL_SUCCESS`, and a further call returns `SQL_NO_DATA`.
  `*StrLen_or_Ind` reports the length still to come at the start of that call
  rather than the whole value's length, per the spec's step 7 — it decreases as
  the loop proceeds, which is what lets an application size its final read.

  Three behaviours follow the spec rather than convenience. Fixed-width targets
  are not chunkable: "SQLGetData cannot be used to return fixed-length data in
  parts", so the second call for one returns `SQL_NO_DATA`. The position is per
  statement rather than per column, because "successive calls to `SQLGetData`
  will retrieve data from the last column requested; prior offsets become
  invalid" — a per-column cache would preserve an offset the spec has already
  invalidated. And it is discarded whenever the cursor moves or the result set
  goes away.

  No `Backend` change: chunking operates on the *converted* form, which only
  core can measure, so a backend keeps returning whole values from `get_data`
  and needs no offset. Streaming a value the backend never materialises would
  need one, and can be added later as a defaulted method without a break.

- `SQLParamData`, `SQLFetchScroll`, `SQLSetEnvAttr` and `SQLGetEnvAttr` now
  clear the handle's diagnostic records at the start of the call, as the spec
  requires of every function except `SQLGetDiagRec` and `SQLGetDiagField`.
  Previously a record from a failed call could still be on the queue during a
  later successful one, so `SQLGetDiagRec` reported an error belonging to a
  call that had already completed. `SQLParamData` was the worst affected: it is
  the data-at-execution loop, so one stale record was re-reported on every
  iteration.

- `SQLGetData` and `SQLFetch` perform the temporal struct conversions the
  SQL-to-C table requires. `write_column_value` had an arm for each type to its
  own C struct and nothing else, so four legal conversions fell through to the
  catch-all and were refused with `07006`:

  - `SQL_TYPE_DATE` → `SQL_C_TYPE_TIMESTAMP` (time fields zeroed)
  - `SQL_TYPE_TIME` → `SQL_C_TYPE_TIMESTAMP` (date fields set to the current
    date, fractional seconds zeroed)
  - `SQL_TYPE_TIMESTAMP` → `SQL_C_TYPE_DATE` (`01S07` if a time was dropped)
  - `SQL_TYPE_TIMESTAMP` → `SQL_C_TYPE_TIME` (`01S07` if a fraction was
    dropped; a discarded date is not a truncation)

  pyodbc requests `SQL_C_TYPE_TIMESTAMP` for temporal columns, so every query
  selecting a bare `DATE` or `TIME` failed, as did the `{fn CURDATE()}`,
  `{fn CURTIME()}`, `{d '...'}` and `{t '...'}` escapes that resolve to one.

  The cross pairs stay `07006`: the spec's table has no `SQL_C_TYPE_TIME` row
  for a date, and no `SQL_C_TYPE_DATE` row for a time.

  `ColumnValue::TimestampTz` gains the same three targets. It has no row in the
  spec's table, but supporting only `SQL_C_TYPE_TIMESTAMP` for it would leave
  the identical hole, with a zoned column refusing a plain date where an
  unzoned one succeeds.

  The existing temporal tests all read their values as `SQL_C_CHAR` or
  `SQL_C_WCHAR`, which take the string-coercion catch-all, so the entire
  struct-target half of the table was untested. The new test walks the spec's
  table instead, asserting the illegal pairs are refused as well as that the
  legal ones succeed.

  Note that `SQL_TYPE_TIME` → `SQL_C_TYPE_TIMESTAMP` makes `write_column_value`
  read the wall clock, for that one pair only. The spec requires "the current
  date" without saying in which zone; core uses UTC, because the standard
  library has no timezone database and a driver-specific answer would differ
  between backends.

- `SQLGetDiagFieldW` uses the real values for `SQL_DIAG_COLUMN_NUMBER` and
  `SQL_DIAG_ROW_NUMBER`. They were hand-written as `12` and `13`; `sqlext.h`
  defines them as `-1247` and `-1248`. Three things went wrong at once: the real
  identifiers fell through to the unknown-field arm and returned `SQL_NO_DATA`;
  `12` is `SQL_DIAG_DYNAMIC_FUNCTION_CODE`, so an application asking which
  statement had executed was answered `-1`, which is `SQL_DIAG_CREATE_INDEX`,
  for every statement; and `13` answered a field that does not exist. The
  identifiers now derive from `odbc_sys::HeaderDiagnosticIdentifier` rather than
  being restated, which is what the named-constant rule exists to prevent.

  The two fields are also typed differently by the spec — `SQL_DIAG_COLUMN_NUMBER`
  is `SQLINTEGER`, `SQL_DIAG_ROW_NUMBER` is `SQLLEN` — and shared one arm writing
  four bytes, so a caller's `SQLLEN` kept its high half on a 64-bit platform.

- `SQLGetStmtAttrW(SQL_ATTR_CURSOR_SENSITIVITY)` no longer contradicts
  `SQLGetInfoW(SQL_CURSOR_SENSITIVITY)`. A second, private `SQL_INSENSITIVE`
  held the value `2`, which `sql.h` assigns to `SQL_SENSITIVE`, while
  `default_get_info` reported the shared constant's `1`. The same statement
  described its cursor two ways depending on which function was asked.

- `SQLFetch` returns `24000` when the statement was executed but produced no
  result set. It guarded on `statement.is_some()`, but a statement that
  produced no result set — an `UPDATE`, ODBC state S4 — keeps its statement and
  only closes the cursor, as does one that is prepared but not yet executed
  (S2/S3). Fetching in either state drove the backend instead of refusing. The
  guard now reads `cursor_open`, which is the state the spec's wording names.
  `24000` carries no `(DM)` marker in the `SQLFetch` diagnostics table, so it is
  the driver's to return.

- `SQLPrepareW` no longer discards parameter bindings. `SQLBindParameter`'s
  spec names the only three things that unbind a parameter — another
  `SQLBindParameter`, `SQLFreeStmt(SQL_RESET_PARAMS)`, and `SQLSetDescField`
  setting the APD's `SQL_DESC_COUNT` to 0 — and `SQLPrepare` is not among them.
  `SQLPrepare`'s own Comments confirm it from the other side: an application
  "should unbind all parameters that applied to an old SQL statement before
  preparing a new SQL statement", which is advice only a driver that keeps them
  could need.

  The ordinary `SQLBindParameter` → `SQLPrepare` → `SQLExecute` order lost every
  binding, and `collect_params` substitutes a NULL for each unbound slot, so the
  statement ran with all-NULL parameters and returned the wrong rows with
  `SQL_SUCCESS` — no diagnostic, nothing for the application to detect. Power BI
  binds folded predicates this way.

  Bindings above the new statement's parameter count are simply not read;
  `collect_params` walks `1..=param_count`. That is the stale-parameter hazard
  the spec warns about, and it is the application's to avoid.

- Escape translation is linear in nesting depth again. `{fn NAME(args)}`
  translated its argument list, discarded the result when the dialect declined
  to rewrite the call, and left the caller to translate the same span a second
  time, so the work doubled at every nesting level. `MAX_ESCAPE_DEPTH` bounded
  the recursion but not the work — it set the exponent. Since
  `EscapeDialect::ansi_default` declines every call and `Backend::escape_dialect`
  returns it by default, this was the path every driver took unless it
  implemented `rewrite_scalar_fn`, and a driver that did was still exposed for
  every function name it declined.

  A 631-byte `SELECT`-able string nested to the depth limit needed 2^63
  translations and never returned; measured, 26 levels took 9.5 seconds and 40
  levels did not finish in 25. `SQLExecDirectW` reaches this whenever
  `SQL_ATTR_NOSCAN` is off, which is the default, as do `SQLPrepareW` and
  `SQLNativeSqlW`. `SQLCancel` cannot interrupt it and the output stays a few
  hundred bytes, so nothing signals the hang. The same input now translates in
  tens of microseconds.

  The existing depth tests missed it because they nest through `{oj}`, which
  does not double, and the one `{fn}` test uses `MAX_ESCAPE_DEPTH + 1`, where
  the depth error fires on the first descent and short-circuits.

- `init_logging` no longer aborts the host process when a global `tracing`
  subscriber already exists. It installed its subscriber with
  `SubscriberInitExt::init`, which is `try_init().expect(...)`, and
  `SQLAllocHandle(SQL_HANDLE_ENV, ...)` runs it *before* entering `panic_safe` —
  so the panic unwound across the `extern "system"` boundary, which is undefined
  behaviour. It is the first call every ODBC application makes, and losing the
  race is ordinary: any Rust host that uses `tracing`, or a second driver built
  on this crate loaded into the same Driver Manager, gets there first. The panic
  also poisoned the `Once`, so every later call panicked with "Once instance has
  previously been poisoned", turning one lost race into a permanently dead
  driver. Installing the subscriber is now best-effort: whichever subscriber got
  there first is kept and the driver logs nowhere rather than failing.

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
  thread while another is mid-call on it; ODBC forbids that outright, and no
  amount of internal synchronisation changes it. Ordinary concurrent calls on
  the same handle are a different matter: this crate no longer depends on the
  Driver Manager to serialise those either, since it does not on every
  platform — the Windows Driver Manager does not serialise calls to a handle
  at all, and `SQLAllocHandle`'s own Comments section says drivers "must
  therefore support safe, multithread access to this information." See the
  per-connection lock entries below.
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
- Handle contents are now synchronised per connection, through a `GroupLock`
  a connection shares with all of its statements and a `HandleScope` that is
  the only way to reach a handle's fields. Previously nothing in this crate
  serialised concurrent calls on the same handle at all; a `SQLCancel` running
  on a second thread could race the executing thread's own mutation of the
  same diagnostic queue and binding maps, which could double-free.
- `SQLEndTran` walks an owned snapshot of a connection's statements (an
  environment's connections, for `SQL_HANDLE_ENV`), taken from the registry via
  `Registry::children_of` rather than borrowed from a field on the parent
  handle. Freeing a statement mid-walk on another thread can therefore no
  longer shift the sequence the walk is iterating, which is what made
  `SQLEndTran(SQL_HANDLE_ENV)` racing a concurrent `SQLFreeHandle` on one of its
  connections unsound when the list was a field of the environment.
- **Breaking.** `SQLCancel` no longer takes its statement's connection lock
  unconditionally. It clones the statement's cancel token out of the registry,
  then attempts the connection group with `try_lock` instead of a blocking
  `lock`: when another thread holds it, `SQLCancel` signals `Backend::cancel`
  and returns immediately, clearing no diagnostics and posting none, matching
  the spec's own carve-out for "a function running on the statement on another
  thread" ("only SQL_SUCCESS or SQL_ERROR can be returned; no diagnostic
  information is returned"). Previously it blocked on the same lock the query
  it was asked to cancel was holding, so a cross-thread `SQLCancel` could never
  run concurrently with the call it targeted — the one scenario this crate's
  whole locking design exists to make sound. When the connection is free,
  `SQLCancel` takes the uncontended lock and runs its full path: clearing
  diagnostics, discarding any pending data-at-execution state, and posting its
  own diagnostic on a failing `Backend::cancel`.

  Two consequences of the lock-free path are worth knowing about rather than
  discovering:
  - `try_lock` cannot tell "a sibling statement on this connection is busy"
    apart from "my own operation is busy": either makes `SQLCancel` take the
    cross-thread branch, so a merely-idle statement's data-at-execution state
    is occasionally left uncleared where it strictly could have been. Harmless,
    and explicitly spec-legal ("How the function is canceled depends on the
    driver and the operating system").
  - A `SQLGetDiagRecW`/`SQLGetDiagFieldW` call immediately following a
    cross-thread `SQLCancel` now blocks until the cancelled call has unwound
    through the backend, because both of those take the connection's lock and
    reading the diagnostic queue while another thread pushes to it is
    undefined behaviour. `SQLCancel` itself still returns promptly; the wait
    moves to whichever call reads diagnostics next, bounded by the backend's
    own cancel latency.

[Unreleased]: https://github.com/stackabletech/stackable-odbc-core/commits/HEAD
