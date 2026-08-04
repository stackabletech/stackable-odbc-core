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

### Migration: numeric parameters are converted to their declared SQL type

Core implements the ODBC spec's [C to SQL: Numeric] table. It previously
implemented two of the three C-to-SQL tables and none of this one, so a numeric
parameter reached the backend as whatever C type it arrived in and
`SQLBindParameter`'s `ParameterType` was discarded. `Backend::execute` receives
only `&[ColumnValue]`, so if core does not honour that argument, nobody does.

**A numeric parameter bound to a character target now arrives as
`ColumnValue::String`.** It previously arrived as `ColumnValue::F64`,
`ColumnValue::I32` and so on. Core renders the number itself, so the length it
range-checks is the value that is sent. A backend matching on the variant must
handle `String` for `SQL_CHAR`, `SQL_VARCHAR`, `SQL_LONGVARCHAR` and their
Unicode counterparts. Likewise an integer bound to `SQL_DECIMAL` now arrives as
`ColumnValue::Decimal`, and one bound to `SQL_SMALLINT` as `ColumnValue::I16`
rather than at the C type's own width.

**A bind or execute that succeeded before can now fail**, with the table's own
SQLSTATEs:

| Target | Test | SQLSTATE |
|---|---|---|
| `SQL_CHAR` / `SQL_VARCHAR` / `SQL_LONGVARCHAR` | rendered digits exceed `ColumnSize` | `22001` |
| the `SQL_W*` character types | the same, in UTF-16 code units | `22001` |
| `SQL_DECIMAL` / `SQL_NUMERIC` / the four integer types | whole digits do not fit | `22003` |
| `SQL_REAL` / `SQL_FLOAT` / `SQL_DOUBLE` | outside the target's range | `22003` |
| `SQL_BIT` | `>0 <2 ≠1` / `<0` or `≥2` | `22001` / `22003` |
| `SQL_INTERVAL_*` | field exceeds the leading precision, or carries a fraction | `22015` |

**Fractional truncation to an exact numeric target returns
`SQL_SUCCESS_WITH_INFO` with `01S07`.** Binding `3.7` to a `SQL_INTEGER`
parameter sends `3` and says so; it previously sent `3` silently. This is the
table's own optional behaviour, which core takes.

**`SQLBindParameter` returns `07006` for pairings the table excludes**:
`SQL_C_FLOAT` or `SQL_C_DOUBLE` to any interval, any numeric C type to a
multi-field interval, and any numeric C type to a target the table does not
list, such as `SQL_GUID`.

Run your driver's test suite against this version before shipping it, as for
the consistency check below.

[C to SQL: Numeric]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-numeric

### Migration: SQLBindCol and SQLBindParameter now run the consistency check

The ODBC spec requires a consistency check whenever `SQL_DESC_DATA_PTR` is set,
and states that it "is always performed when **SQLBindParameter** or
**SQLBindCol** is called". Core did not perform it; it now does, at all four
sites, returning `HY021` (inconsistent descriptor information).

**A bind that succeeded before can now fail.** The checks are the spec's own:
the type must be a valid ODBC C or SQL type or a driver-specific SQL type; a
numeric type's precision and scale must be valid for it; a datetime or interval
type's `SQL_DESC_DATETIME_INTERVAL_CODE` must be one of the valid codes. A
driver whose tests bind, say, `SQL_DECIMAL` with a `DecimalDigits` larger than
its `ColumnSize` will see that call start returning `SQL_ERROR`.

Run your driver's test suite against this version before shipping it. If a bind
that your data source genuinely accepts is now rejected, that is a bug in the
check and not in your driver — report it rather than working around it.

### Migration: the catalog functions

Everything a driver has to change for the catalog rework, in one place.

1. **Six methods return typed rows instead of `Self::Statement`:** `tables` →
   `Vec<TableRow>`, `columns` → `Vec<ColumnRow>`, `primary_keys` →
   `Vec<PrimaryKeyRow>`, `foreign_keys` → `Vec<ForeignKeyRow>`, `statistics` →
   `Vec<StatisticsRow>`, `special_columns` → `Vec<SpecialColumnRow>`. Keep the
   query helpers; return their rows as structs and delete the statement
   construction. Their *arguments* changed shape at the same time — see point 6.
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
6. **Every one of the ten hooks takes a single sealed query object**, not a list of
   positional arguments: `tables(conn, cancel, &TablesQuery<'_>)`,
   `columns(conn, cancel, &ColumnsQuery<'_>)` and so on through
   `TablePrivilegesQuery`. Read the arguments back through the accessors —
   `query.catalog()`, `query.schema()`, `query.table()`. `SQLTables`' table-type list
   is `query.table_types()`, already split, so delete any driver-side splitting of the
   raw `TableType` string. Because the types are sealed, an argument added to a catalog
   hook later is a source-compatible change.
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

### Migration: SQLSetCursorName accepts the prepared state

`SQLSetCursorName` refused any statement holding a backend statement, which
included the prepared-but-unexecuted states S2 and S3. The spec permits those:
its Comments allow a rename "as long as the cursor is in an allocated or
prepared state", and Appendix B's row reads `--` for `S2-S3 Prepared` against
`24000` for `S4 Executed` and `S5-S7 Cursor`.

**`SQLPrepare` -> `SQLSetCursorName` -> `SQLExecute` now works.** It previously
returned `24000` from the middle call, which locked out the standard
positioned-update setup. Nothing that used to succeed now fails: state S4 —
executed with no result set — is still `24000`, and a driver relying on the
earlier over-refusal was relying on a bug.

`StatementHandle` gained a public `executed: bool` and a `note_executed()`
method for the distinction. A driver that constructs or inspects a
`StatementHandle` directly (only the `test-support` paths do) sees the new
field.

### Migration: SQLGetTypeInfo refuses a second call while a cursor is open

`SQLGetTypeInfo` had no open-cursor check, so a second call silently replaced
the result set the first one produced. Its `24000` row gives two of three
clauses to the driver, and Appendix B puts it in one transition table with the
ten catalog functions — whose cursor-states row is `24000` in all three columns
and which core already implements.

**A second `SQLGetTypeInfo` on a statement with an open cursor now returns
`SQL_ERROR` with `24000`.** An application that relied on the overwrite must
call `SQLCloseCursor` or `SQLFreeStmt(SQL_CLOSE)` first, as it already must for
`SQLTables` and its neighbours.

### Added

- **`into_values` on the ten catalog row types**, beside the existing
  `to_values`. It consumes the row, so the strings move instead of being cloned:
  measured at **0.40×** the borrowing form for a 50 000-row result set (3.28 ms →
  1.33 ms). `to_values` remains and now delegates to it, which keeps the spec
  column order defined in one place — two lists in that order is how they come to
  disagree, and an application binds by column number. A driver has nothing to
  change; core's own catalog functions use the consuming form.

- `Backend::configure_dsn`, a defaulted hook that supplies a data source's
  keywords to `ConfigDSN`. This is what makes the Windows ODBC Administrator's
  **Add…** and **Configure…** buttons work at all: the Administrator calls
  `ConfigDSNW` with a non-null parent window and an *empty* attribute list, and
  the driver's dialog is what produces the `DSN` keyword. Core previously looked
  for that keyword first and failed with `ODBC_ERROR_INVALID_KEYWORD_VALUE`, so
  **Add…** could never succeed for any driver built on core.

  Core keeps every other part of `ConfigDSN`: request validation, the `DRIVER=`
  reserved-key filter, `SQLValidDSN`, the ADD-overwrites versus
  CONFIG-preserves split, and the registry writes. The default implementation is
  the identity function, so a driver that does not override it keeps core's
  existing headless behaviour exactly. Not breaking.

  For `ODBC_CONFIG_DSN` and `ODBC_REMOVE_DSN`, core reads the data source's
  existing keywords from `ODBC.INI` and merges them under the supplied ones
  before calling the hook, so a driver's dialog never has to touch `odbcinst`.
  The spec requires this rather than merely allowing it: "for information not in
  *lpszAttributes*, it uses information from the system information."
  `ODBC_ADD_DSN` does not prefill, because `SQLWriteDSNToIni` removes the old
  section before creating the new one.

  Core also enforces that the hook does not change a data source name it was
  handed ("**ConfigDSN** displays that name but does not allow the user to
  change it"), because a hook altering `DSN=` on a remove would delete a data
  source the user never named. A cancelled dialog (`Ok(None)`) returns FALSE
  and posts **no** installer error, matching psqlODBC.

  New public module `stackable_odbc_core::setup`, carrying `ConfigRequest`
  (moved from the private `ffi::setup`), `InstallerError` and `SetupError`.
  `InstallerError` has no `DriverSpecific` variant: `ConfigDSN`'s spec table
  names `ODBC_ERROR_DRIVER_SPECIFIC`, but no header defines it, including the
  Windows SDK's own, whose codes end at `ODBC_ERROR_NOTRANINFO` (23). A driver's
  setup failure posts `ODBC_ERROR_REQUEST_FAILED` instead.

- `utf16::utf16_to_string_named`, a public sibling of `utf16::utf16_to_string`
  taking the argument's ODBC name (`"CatalogName"`, `"SQLConnectW's UserName
  argument"`, …). It appears in the `HY090` a `SQL_NTS` scan overrun now
  produces — see the Fixed entry below — and exists because the functions that
  overrun most usefully have several string arguments: `SQLConnectW` takes
  three and `SQLForeignKeys` six, so "a string argument was too long" identifies
  none of them. `utf16_to_string` is unchanged and delegates to it; a driver
  calling the older function needs no edit.

- `SQLGetDiagFieldW` answers `SQL_DIAG_CLASS_ORIGIN` and
  `SQL_DIAG_SUBCLASS_ORIGIN` from the record's SQLSTATE. Both returned the empty
  string, sharing a match arm with `SQL_DIAG_CONNECTION_NAME` and
  `SQL_DIAG_SERVER_NAME`, the only two fields for which the spec sanctions an
  empty value. The spec defines these two exactly: `"ISO 9075"` unless the
  SQLSTATE class is `IM`, and, for the subclass, membership of a closed list of
  forty-two ODBC-specific states. The Windows Driver Manager queries them after
  `SQL_SUCCESS_WITH_INFO`.

- `SQLGetDiagFieldW` answers the four statement-only header fields:
  `SQL_DIAG_ROW_COUNT`, `SQL_DIAG_CURSOR_ROW_COUNT`,
  `SQL_DIAG_DYNAMIC_FUNCTION` and `SQL_DIAG_DYNAMIC_FUNCTION_CODE`. All four
  were unhandled — with the spec-correct `RecNumber` of 0 they returned
  `SQL_ERROR`, and with a positive one they returned `SQL_NO_DATA` — although
  the spec says "*RecNumber* is ignored for header fields".

  `SQL_DIAG_ROW_COUNT` now shares `SQLRowCount`'s computation, which the spec
  requires of it ("The data in this field is also returned in the *RowCountPtr*
  argument of **SQLRowCount**"). `SQL_DIAG_CURSOR_ROW_COUNT` is `0`, derived
  from core setting neither `SQL_CA2_CRC_*` bit in
  `SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2`. The two dynamic-function fields report
  the spec's own "Unknown" row: an empty string and `SQL_DIAG_UNKNOWN_STATEMENT`.
  On a non-statement handle all four return `SQL_ERROR`, as the Header Fields
  table states once per field.

  `types::SQL_CA2_CRC_EXACT` and `types::SQL_CA2_CRC_APPROXIMATE` are new, so a
  backend can state that coupling in its own cursor-attribute answer.

- **`SQL_ASYNC_DBC_NOT_CAPABLE`, `SQL_ASYNC_DBC_CAPABLE` and
  `SQL_CA2_READ_ONLY_CONCURRENCY`** in `types::constants`. The first names the
  value `SQL_ASYNC_DBC_FUNCTIONS` was already answering as a bare `0`; the
  third is the bit `SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2` now sets. None is in
  `odbc-sys`.

- **`SqlState::attempt_to_concatenate_a_null_value`** (`HY020`), and
  `PutDataState`, which records what `SQLPutData` has delivered for the
  parameter currently being filled.

- **`SQL_DEFAULT_PARAM`** (`-5`), which `odbc-sys` does not model.

- **`SQLExtendedFetch` is implemented.** It was a bare `SQL_ERROR` inside the
  `forward_ffi!` macro — no handle validation, no diagnostic, no logging, no
  test — so an ODBC 2.x application, or the Driver Manager mapping a 2.x
  application's `SQLFetchScroll` onto it, saw a failure with an empty diagnostic
  queue and `SQLGetDiagRec` answering `SQL_NO_DATA`. `SQL_FETCH_NEXT` now
  fetches; every other orientation reports `HY106`, and an invalid handle now
  reports `SQL_INVALID_HANDLE` rather than `SQL_ERROR`.

  Per spec the row count and status go to the `RowCountPtr` and `RowStatusArray`
  **arguments**, not to `SQL_ATTR_ROWS_FETCHED_PTR` / `SQL_ATTR_ROW_STATUS_PTR`:
  that buffer "is used only by **SQLExtendedFetch**", and the status array's
  address "is not stored in the `SQL_DESC_STATUS_ARRAY_PTR` field in the IRD".
- **`SQL_FETCH_BOOKMARK`**, which `odbc-sys` lacks — its `FetchOrientation` stops
  at `Relative`. A driver rejecting the orientation needs a name for it.
- **`SQL_ROWSET_SIZE`**, likewise absent from `odbc-sys`, whose
  `StatementAttribute` models only the ODBC 3.x `SQL_ATTR_ROW_ARRAY_SIZE`.
- **The *C to SQL: Numeric* conversion table.** Core now implements all three of
  the spec's C-to-SQL tables. See the migration note above for what changes for
  a driver.
- **`SqlState::interval_field_overflow`** (`22015`), the interval row's outcome.
- **The thirteen concise `SQL_INTERVAL_*` `SqlDataType` constants** and
  **`interval_from_raw`**, neither of which `odbc-sys` provides — it has the
  C-side `CDataType::Interval*` codes and the `SQL_IS_*` subcodes as
  `odbc_sys::Interval`, but no `SqlDataType` constants and no conversion.
- **`is_interval_sql_type`**, for a driver that needs to recognise the family.

- **A driver can reach the user during a connect: `Prompter`.** The new
  `prompt::Prompter` trait has one method, `present_url(&str)`, and the new
  defaulted `Backend::prompter() -> Option<Arc<dyn Prompter>>` is how a driver
  supplies an implementation. A backend reads it back through the new
  `ConnectParams::prompter()` inside its own `connect`. This is what an OAuth
  2.0 external-authentication flow needs: the data source answers the initial
  request with a login URL a human has to visit, and until now there was
  nowhere in the API to put it.

  Core ships no implementation and gains no dependency — no browser opener, no
  dialog, no cargo feature. It carries the trait definition and decides *whether*
  prompting is permitted; the driver supplies *how*.

  `present_url` must return promptly. It presents the URL; it does not wait for
  the user to act on it. A driver polling for the result of an interactive login
  does that in its own `connect`, which is already the call the application is
  blocked in.

  `ConnectParams::prompter()` hands back an owned `Arc`, not a borrow, because
  the prompter routinely outlives the `connect` it arrived on — an interactive
  flow gives it to the client library that presents the URL, and a driver
  caching the resulting credential keeps it for the process.

  Nothing existing changes: `Backend::prompter` is defaulted to `None`, so a
  driver that does not implement it behaves exactly as before, and
  `ConnectParams`' hand-written `Debug` and its `to_connection_string` are both
  blind to the new field — a prompter never appears in a log line or in
  `SQLDriverConnect`'s *OutConnectionString*.

- **`driver_connect_option_from_raw`.** Converts `SQLDriverConnect`'s raw
  *DriverCompletion* `u16` into `odbc_sys::DriverConnectOption`, which is now
  re-exported from `types`. Follows the existing `*_from_raw` family; an
  unrecognised value is `None`, never a transmute.

- **The descriptor fields are reachable.** `SQLGetDescFieldW`,
  `SQLSetDescFieldW`, `SQLGetDescRecW` and `SQLSetDescRec` are implemented over
  the four descriptors a statement owns, and `SQLGetFunctions` reports them
  supported. A binding built entirely through `SQLSetDescField` is a binding:
  `SQLFetch` writes through it, because ODBC makes a bound column *be* an ARD
  record and core now has one storage rather than two. The IRD is answered as a
  computed view over the same `ColumnDescriptor` `SQLColAttributeW` reads, so
  the two cannot disagree about a column; a read before the statement is
  prepared or executed is `HY007`, as the spec requires.

  `SQLGetDescRecW` is newly exported — the `W` suffix because it takes a `Name`
  buffer, and the project exports every string-bearing function in its Wide form
  only.

- **Explicit descriptor handles.** `SQLAllocHandle(SQL_HANDLE_DESC)` and
  `SQLFreeHandle(SQL_HANDLE_DESC)` work; an application descriptor can be swapped
  in through `SQL_ATTR_APP_ROW_DESC` / `SQL_ATTR_APP_PARAM_DESC`, which
  `SQLGetStmtAttr` then reports; and one descriptor may be shared across several
  statements on a connection. Because ODBC makes the descriptor *be* the binding,
  two statements sharing one have one binding set between them — a bind through
  either is visible through both, and `SQLFreeStmt(SQL_UNBIND)` on either clears
  both.

  Freeing an explicit descriptor reverts every statement that used it to its own
  implicit one, as the spec requires, and `SQLDisconnect` frees any still open on
  the connection.

  `SQL_ATTR_APP_ROW_DESC` / `SQL_ATTR_APP_PARAM_DESC` given a descriptor that is
  not on the statement's connection — or a value naming no live descriptor —
  returns `HY024`. That row is not `(DM)`: it states the case verbatim and closes
  with the general rule making it the driver's.

- **`SQLCopyDesc`**, including between descriptors on different connections and
  different environments, which the spec permits. It is exported and reported by
  `SQLGetFunctions`. The copy runs in two phases that never hold two lock groups
  at once, so two copies in opposite directions cannot deadlock; `HY021` is
  checked before anything is written, so a refused copy leaves the target
  untouched where the spec would permit it to be undefined.

  With these, **`SQL_OIC_CORE` is satisfied**: Core-level conformance requires
  allocating and freeing all handle types and manipulating descriptor fields
  through all five descriptor functions.

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

- **Breaking.** `ChunkWrite` has crate-private fields, three accessors and
  `#[non_exhaustive]`. It was the one public struct in the crate with no
  extensibility protection at all, so adding a fourth field was a major break.
  A driver reading `write.delivered` now calls `write.delivered()`; the type is
  produced by core and only read by a driver, so there is no construction path
  to replace. Field doc comments moved to the accessors, which is now the only
  place the API is described, so the two cannot drift.

- **Breaking.** The ten catalog `Backend` hooks take a single sealed query type
  instead of five to eight positional arguments: `tables` takes a
  `&TablesQuery<'_>`, `foreign_keys` a `&ForeignKeysQuery<'_>`, and so on. A
  driver reads the filters through accessors (`query.catalog()`,
  `query.pk_table()`, `query.table_types()`).

  Two defects motivated it. Adding an argument to any of the ten was a major
  break for every driver, and there is no `#[non_exhaustive]` for a function
  signature, so a parameter object is the only mechanism available. And
  `foreign_keys` took six consecutive `Option<&str>`, where transposing a
  primary-key argument with its foreign-key counterpart compiled silently and
  failed at runtime as a wrong or empty result set. This is the argument the
  typed row structs already made for the *return* side, applied to the inputs.

  Eight of the ten build from `Default` and `with_*` setters. `StatisticsQuery`
  and `SpecialColumnsQuery` take their undefaultable arguments through `new`
  instead: `false` for `unique_only` means `SQL_INDEX_ALL`, and no `Scope` or
  `IdentifierType` value is a defensible default, so core does not invent one.

  Two call sites turned out to have no test pinning their argument mapping:
  `SQLTables` and `SQLColumns` both passed an empty catalog and schema in their
  `SQL_ATTR_METADATA_ID` tests and asserted only on the table, so transposing
  those two was invisible to the whole suite. Both are pinned now.

- `ConfigDSNW` logs a `WARN` when `hwndParent` is non-null. The driver ships no
  setup dialog, so the spec's prompt-on-overwrite behaviour ("If it matches an
  existing name and *hwndParent* is not null, **ConfigDSN** prompts the user to
  overwrite the existing name") becomes an unconditional overwrite. A caller
  passing null is unaffected and fully conforming.

- `ConfigDSNW` reports a malformed `lpszAttributes` as
  `ODBC_ERROR_INVALID_KEYWORD_VALUE` instead of proceeding with what it could
  read. A segment with no `=` was skipped with no log at all, and a list missing
  its double-null terminator produced a partial map the caller could not
  distinguish from a complete one, so a data source could be written with
  keywords silently missing. The spec's code says exactly this: "The
  *lpszAttributes* argument contained a syntax error."

  **Migration:** a setup application that passed a stray token in the attribute
  list now gets FALSE with that code, where it previously got TRUE and a
  partially configured data source.

- **Breaking: `SQLSetCursorName` enforces the spec's cursor-name rules.** It
  previously stored any non-empty name and checked nothing else, so three
  unmarked (driver-owed) rows of its diagnostics table went unimplemented. Calls
  that used to succeed can now return:

  - `24000` — the name is set after the statement has executed. The spec allows
    it only while the cursor is "in an allocated or prepared state".
  - `34000` — the name is empty, longer than `SQL_MAX_CURSOR_NAME_LEN`, or starts
    with `SQLCUR` or `SQL_CUR`, which are reserved for driver-generated names.
  - `3C000` — the name is already used by another statement on the same
    connection.

  An empty name moves from `HY090` to `34000`: `HY090` is `(DM)`-marked and means
  "`NameLength` was less than 0 but not equal to `SQL_NTS`", a different
  condition.

- **Breaking: the deprecated ODBC 2.x functions are no longer exported.**
  `SQLAllocConnect`, `SQLAllocEnv`, `SQLAllocStmt`, `SQLFreeConnect`,
  `SQLFreeEnv`, `SQLError`, `SQLTransact`, `SQLGetConnectOption(W)`,
  `SQLSetConnectOption(W)`, `SQLGetStmtOption` and `SQLSetStmtOption` are gone,
  joining `SQLSetScrollOptions` below.

  Appendix G, "Mapping Deprecated Functions", is explicit that a 3.x driver "does
  not have to implement the ODBC 2.x functions", and that the mapping "is
  triggered when the driver is an ODBC 3.x driver and **the driver does not
  support the function that is being mapped**". An export therefore does not add
  a capability — it removes the Driver Manager's, which is the better-informed of
  the two. psqlODBC comments out every one of these in its `.def` for the same
  reason.

  Three of them were actively wrong, and are fixed by the removal rather than by
  repair:

  - `SQLError` returned `SQL_NO_DATA` unconditionally, so an ODBC 2.x application
    saw **no diagnostics at all**. The mapping it suppressed routes to
    `SQLGetDiagRec`, which core implements.
  - `SQLSetConnectOption` passed `StringLength = 0` where the Driver Manager
    passes `SQL_NTS`, so a string-valued attribute was set to the empty string —
    `SQL_ATTR_CURRENT_CATALOG` silently became `""`.
  - `SQLAllocEnv` skipped the Driver Manager's accompanying
    `SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC2)`, leaving a 2.x
    application with 3.x SQLSTATEs and datetime type codes.

  `SQLFreeStmt` is **kept**: it is an ODBC 3.x function, and its `SQL_DROP` option
  is passed through by the Windows Driver Manager rather than mapped.

  `SQLGetFunctions`' ODBC 2.x array still reports all of these as supported, and
  that is correct: it answers "can a 2.x application call this", and it can,
  through the mapping. Only the 3.x bitmap stops claiming them.

  **Migration:** a driver whose `Backend::get_functions` names any of these must
  remove them. A list built from `CORE_EXPORTED_FUNCTIONS`, as the docs
  recommend, needs no change.

- **Breaking: `SQL_MAX_OPTION_STRING_VALUE` is removed.** Its only callers were
  the `SQLGetConnectOption` / `SQLGetStmtOption` shims above. It was also
  misspelled against the header, which calls it `SQL_MAX_OPTION_STRING_LENGTH`
  (`sqlext.h:58`).

- **Breaking: `SQLSetScrollOptions` is no longer exported.** Its spec page
  defines no `Returns` section and no diagnostics table; its one substantive note
  documents what the Driver Manager does "for an application working with an ODBC
  3.x driver that does not support **SQLSetScrollOptions**" — it sets
  `SQL_ROWSET_SIZE` itself. unixODBC's DM implements that mapping in full
  (`SQLGetInfo` to validate the requested concurrency, then `SQLSetStmtAttr` for
  `SQL_ATTR_CONCURRENCY`, `SQL_ATTR_CURSOR_TYPE`, `SQL_ATTR_KEYSET_SIZE` and
  `SQL_ROWSET_SIZE`) and dispatches to the driver's own entry point *only when
  the driver exports one*. Core exported a bare `SQL_ERROR`, which suppressed a
  capability-checked mapping derived from core's own `SQLGetInfo` answers and
  replaced it with a silent failure. psqlODBC ships the same arrangement: it
  never defines the symbol.

  **Migration:** a driver whose `Backend::get_functions` names
  `FunctionId::SetScrollOptions` must remove it. A list built from
  `CORE_EXPORTED_FUNCTIONS`, as the docs recommend, needs no change.

- **`SQL_ROWSET_SIZE` other than 1 now returns `01S02` and substitutes 1**, where
  it was previously accepted silently as an unrecognised attribute. It is the
  rowset `SQLExtendedFetch` reads, so accepting a larger value would return one
  row under `SQL_SUCCESS`. `SQLGetStmtAttr` reports the substituted value, as
  that warning's own text requires. Identical treatment to
  `SQL_ATTR_ROW_ARRAY_SIZE` and `SQL_DESC_ARRAY_SIZE`.

- **Descriptors are separately allocated handles rather than fields of a
  statement.** Each has its own registry slot, parented to the statement for the
  four implicit ones and to the connection for an explicit one, and all join the
  connection's lock group — so no lock and no lock-ordering rule is added. No API
  a driver calls changes. `SQL_ATTR_APP_ROW_DESC` and `SQL_ATTR_APP_PARAM_DESC`
  now report an application-supplied descriptor when one has been set, where
  before they always reported the statement's own.

- **Descriptor header fields are keyed by `SQL_DESC_*` field rather than by the
  statement attribute that names them,** so one field has one value however it is
  reached. The mapping is not one-to-one — `SQL_DESC_ARRAY_SIZE` is
  `SQL_ATTR_ROW_ARRAY_SIZE` on an ARD and `SQL_ATTR_PARAMSET_SIZE` on an APD —
  and one explicit descriptor may be the ARD of one statement and the APD of
  another, which is where two keys for one field would have become two values.

- **`SQL_DESC_ALLOC_TYPE` now reports `SQL_DESC_ALLOC_USER` for an
  application-allocated descriptor.** It was always `SQL_DESC_ALLOC_AUTO`, which
  was correct only while every descriptor was implicit. It remains the one field
  `SQLCopyDesc` never copies.

- **One descriptor record type.** `ColumnBinding`, `ApdRecord` and `IpdRecord`
  become a single `DescriptorRecord` carrying every `SQL_DESC_*` record field,
  which is ODBC's own model — each descriptor role uses a subset, and that is
  why `SQLSetDescField` takes any field identifier against any descriptor.
  `Descriptor` loses its type parameter and gains a `role`. Which descriptor
  holds what is unchanged: a bound parameter is still one record in the APD and
  one in the IPD.

- **A binding is a non-null `SQL_DESC_DATA_PTR`, not a present key.** Records
  now exist as soon as any field is set, so `SQLFetch` and the parameter
  collectors test the data pointer rather than the record's presence. No
  exported behaviour changes for an application that binds through `SQLBindCol`
  and `SQLBindParameter`, which still create and remove records whole.

- **Column and parameter bindings now live in the descriptors that own them.**
  ODBC makes a binding *be* a descriptor record rather than a copy of one, and
  core kept the two side by side: `DescriptorHandle` held nothing but a header,
  while `StatementHandle` carried separate `bindings` and `param_bindings` maps.
  `SQLBindCol` now writes an ARD record; `SQLBindParameter` writes an `ApdRecord`
  for the C-side buffer and an `IpdRecord` for the declared SQL type, since those
  halves belong to different descriptors and one struct spanning both is what
  would make `SQLSetDescField` unimplementable. The eight statement attributes
  that ODBC also defines as descriptor header fields moved onto the ARD and APD
  headers, leaving `stmt.attrs` for everything else.

  This is internal — every exported entry point behaves as before, and the
  existing fetch, bind and parameter suites pass with their assertions unchanged
  — but it removes the second copy of state that would otherwise let a binding
  and its descriptor disagree once `SQLSetDescField` is implemented. `Backend`
  and `StatementBackend` are unaffected; a driver needs no change.

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

- **A bound parameter's undefined negative length indicator is `HY090`, not a
  silent `SQL_NTS`.** `SQLBindParameter`'s *StrLen_or_IndPtr* defines exactly
  five negative values — `SQL_NTS`, `SQL_NULL_DATA`, `SQL_DEFAULT_PARAM`,
  `SQL_DATA_AT_EXEC` and `SQL_LEN_DATA_AT_EXEC(n)`. Both character arms folded
  *every* negative into `SQL_NTS`, so `SQL_NO_TOTAL` (-4), -6 and -42 bound the
  whole null-terminated string and returned `SQL_SUCCESS`: the application asked
  for something undefined and got a value sent to the data source with no
  diagnostic. `SQLExecDirect`'s and `SQLExecute`'s `HY090` rows state this
  condition themselves and carry no Driver-Manager marker for it.

  `SQLPutData` already refused the same class. **`SQL_DEFAULT_PARAM` now
  resolves to NULL on the bound path too**, which is the ruling `SQLPutData`'s
  documentation already recorded — it names a procedure parameter's default, and
  core refuses `{call ...}` with `HYC00`, so no statement core executes has one.
  It previously bound the null-terminated string at the buffer, like the
  undefined values.

- **`SQLGetData` range-checks the column ordinal itself, and answers `07009`.**
  A column number greater than the number of columns in the result set reached
  the backend, which answered with whatever its own error mapping produced —
  usually `HY000`. The clause carries no Driver-Manager marker, so it is the
  driver's to return.

  The reason recorded for delegating it was that "a precise check would require
  an extra round-trip to obtain the column count". That was false:
  `StatementBackend::column_count` is a local accessor with no I/O behind it,
  and core already range-checks `describe_col` against the same call. A driver
  that worked around this in its own `get_data` can drop the workaround.

- **`SQLColAttributeW` answers `HY091` for a field identifier that is not a
  defined value.** It previously answered `HYC00` for every identifier it did
  not recognise. The spec makes these two different claims, and neither row
  carries a Driver-Manager marker, so both are the driver's to return: `HY091`
  is "not one of the defined values and was not an implementation-defined
  value", while `HYC00` is "not supported by the driver". Collapsing them left
  an application unable to tell a garbage identifier from a valid extension the
  driver has not implemented. `SQLGetDescFieldW` and `SQLSetDescFieldW` already
  drew this line; `SQLColAttributeW` was the outlier.

  `SQL_COLUMN_LENGTH` (3), `SQL_COLUMN_PRECISION` (4) and `SQL_COLUMN_SCALE` (5)
  — the ODBC 2.x spellings of three fields — keep `HYC00`, because they *are*
  defined identifiers. Core does not implement them yet; the spec's Backward
  Compatibility section says an ODBC 3.x driver should, and their ODBC 2.x
  semantics differ from their 3.x counterparts, so that is tracked separately.

- **A chunked `SQLGetData` no longer re-reads and re-converts the whole column
  for every part.** Each call asked the backend for the value again and converted
  it again, so draining an N-byte column through a K-byte buffer cost O(N²/K).
  With the 64 KiB column and 512-byte buffer the benchmark uses, that was 128
  materialisations of 64 KiB to deliver 64 KiB — and the chunk size is the
  *application's* own buffer, so nothing it could do avoided the amplification.
  A hostile or merely large value therefore multiplied both CPU and allocator
  traffic by the number of parts, which is the denial-of-service class the
  security audit raised as S1.

  `GetDataCursor` now carries the converted value — `CachedChunkSource`, either
  UTF-16 code units for `SQL_C_WCHAR` or bytes for `SQL_C_CHAR` and
  `SQL_C_BINARY` — materialised on the first call for a column and reused by
  every later part. Measured on `ffi_get_data_chunked/64KiB_over_512B_chunks`:
  **219.03 µs → 23.47 µs**, −89.9% (p = 0.00), 285 MiB/s → 2.60 GiB/s.
  `ffi_fetch_bound` is unchanged (p = 0.94), which is the check that the shared
  character writers were split without cost to the bound-column path.

  Three properties are deliberately preserved. The C type is part of the cache
  key, because an application may legally change target type between parts and
  that invalidates the conversion rather than only the offset. The cache is
  cleared before every `SQLFetch`, as the cursor position already was, so it
  cannot outlive its row. And it is built only where the string or byte form
  *is* the value — `ColumnValue::String` and `ColumnValue::Bytes` — so a value
  that must be *rendered* to text keeps the previous path, which is what leaves
  the per-call `22003` whole-digits check (which reads `BufferLength`, and so
  must be re-evaluated) exactly where it was.

- **`SQLBindParameter` reports `HY105` for an unrecognised `InputOutputType`,
  not `HY024`.** `HY105` ("Invalid parameter type") is the row that function's
  page gives this exact condition — "(DM) The value specified for the argument
  *InputOutputType* was invalid" — while `HY024` appears nowhere on the page, so
  an application matching on the states the spec lists never saw it coming. The
  row carries `(DM)` and is guarded anyway, on the same footing as the `07009`
  beside it: core is linked directly by its own tests and by embedders with no
  Driver Manager in front of it, and core cannot proceed without knowing whether
  a parameter is an input or an output, so there is no most-permissive fallback
  to take instead.

  Found by a new guard rather than by review, and the guard is the more durable
  half of this entry:
  `every_sqlstate_a_function_body_returns_is_in_its_table_or_declared_off_table`
  scans each FFI function's body for `SqlState::` factory calls and fails the
  build unless each state is in that function's transcribed diagnostics table or
  declared with the house off-table phrase. The existing guard reads doc comments
  and asks whether the spec agrees, which cannot see a state the code returns and
  the doc comment never mentions — the gap this site sat in. It found exactly one
  violation across all sixty exported functions.

- **`SQLGetDescRecW` counts its `Name` buffer and length in characters, not
  bytes.** `BufferLength` was halved and `*StringLengthPtr` doubled, so an
  application that passed a buffer of *n* `SQLWCHAR`s got half of it used, and a
  six-character parameter name was reported as 12. Both are now taken and
  reported as the spec's own wording has them: "Length of the `*Name` buffer, in
  characters" and "the number of characters of data available to return".

  **This is an ABI-visible change for any application that called this
  function.** One that passed `sizeof(buf)` (bytes) now declares twice the
  capacity it has, so a long enough name overruns it; one that passed a
  character count now gets the whole name where it previously got half. There is
  no compatibility shim, because the two readings are indistinguishable at the
  call.

  `SQLGetDescFieldW` is **not** affected and keeps its byte counts, which is the
  distinction that settled this: its page says "total number of bytes" and
  carries the clause `SQLGetDescRec`'s page does not — "if the value in
  `*ValuePtr` is of a Unicode data type (when calling `SQLGetDescFieldW`), the
  `BufferLength` argument must be an even number". `SQLGetCursorNameW` ("in
  characters", no even-number clause) was already on the character side.

  The spec page describes the ANSI signature, where characters and bytes
  coincide, so the drivers were checked rather than inferred from. FreeTDS
  reaches this name through `odbc_set_dstr`, which routes to the
  character-counted `odbc_set_string` and not the byte-counted
  `odbc_set_string_oct` it uses elsewhere (`include/freetds/odbc.h`); psqlODBC's
  `SQLGetDescRecW` assigns `*StringLength` the UTF-16 unit count returned by
  `utf8_to_ucs2_lf` (`odbcapi30w.c`); MySQL Connector/ODBC abstains, its
  `SQLGetDescRecW` being `NOT_IMPLEMENTED` (`driver/unicode.cc`). unixODBC's
  Driver Manager is the one dissenting voice and dissents from itself — it
  passes both values to a Unicode driver untouched, constraining nothing, while
  its ANSI path does `*string_length *= sizeof(SQLWCHAR)` and hands the
  application a byte count for the same call (`DriverManager/SQLGetDescRecW.c`).

- **A float C target no longer reports `01S07` for an inexact narrowing.**
  Fetching an `f64` or an `i64` into `SQL_C_FLOAT` returned
  `SQL_SUCCESS_WITH_INFO` with `01S07` ("fractional truncation") whenever the
  `f32` written back did not compare equal to the source: `0.1`, `16_777_217`,
  and an underflow to `0.0` all warned. They now return `SQL_SUCCESS`, with the
  same value written as before.

  The *SQL to C: Numeric* row for `SQL_C_FLOAT`/`SQL_C_DOUBLE` has exactly two
  cells — in range → *Data* / n/a, out of range → *Undefined* / `22003` — and no
  `01S07`. The rows either side of it do have one, the integer row for
  "truncation of fractional digits" and the `SQL_C_BIT` row for "greater than 0,
  less than 2, and not equal to 1", so the float row's omission is a distinction
  the table draws rather than a gap. Neither psqlODBC (`convert.c`, `case
  SQL_C_FLOAT`) nor MySQL Connector/ODBC (`driver/results.cc`, `sql_get_data`)
  reports anything on this path.

  **This is a severity change from warning to success, at all five entry points
  that reach `write_column_value`** — `SQLFetch`, `SQLFetchScroll` and
  `SQLGetData`, plus `SQLExecDirect` and `SQLExecute` through their bound output
  parameters. An application that watched for `01S07` to detect precision loss
  in a `SQL_DOUBLE`-to-`SQLREAL` fetch stops seeing it; there is no diagnostic
  that reports it, because the table defines none.

  A NaN is the case that shows the warning was wrong and not merely
  unauthorised: no comparison calls a NaN equal to its source, so a faithfully
  delivered NaN reported a truncation that never happened. It is now delivered
  with `SQL_SUCCESS`. Unchanged either side of this: an out-of-range narrowing
  is still `22003` with nothing written, and a dropped *fraction* still reports
  `01S07` on the paths whose own rows define it — an integer target and
  `SQL_C_BIT`.

- **A character or decimal literal too large for `f64` is now `22003` with
  nothing written, where it used to deliver an infinity and call it success.** A
  `SQL_VARCHAR` column holding `1e400`, fetched as `SQL_C_FLOAT` or
  `SQL_C_DOUBLE`, parsed to `f64::INFINITY` and was written as one. *SQL to C:
  Character*'s row for those two targets gives it the second of its three cells,
  "outside the range of the data type to which the number is being converted" →
  *Undefined* / `22003`, so neither the buffer nor the length indicator is
  touched now. This is the same reading the bind direction already took
  (`param_convert`'s `to_double` rejects a non-finite parse as out of range), so
  the two directions now agree on the same literal.

  The check is at the parse rather than in the conversion arm, because by the
  time an overflowed literal reaches the arm it is the same `f64` as a column
  that genuinely holds an infinity — and a genuine `'Infinity'::float8` must
  stay readable. Text *spelled* `Infinity`, `inf` or `NaN` is therefore
  unaffected, and so are the neighbouring cells: text that is not a
  *numeric-literal* stays `22018`, and an underflow (`1e-400`) stays
  `SQL_SUCCESS` with `0.0`, because zero is a value the target holds. An
  integer C target is unaffected in SQLSTATE — an over-range literal was
  already `22003` there, by the exact-numeric row — and only its diagnostic
  message changes.

- **A fetch that narrows a number too large for `SQL_C_FLOAT` now returns
  `SQL_ERROR` with `22003` and writes nothing, where it used to write an
  infinity and call it a warning.** A finite `f64` beyond `±f32::MAX` — a
  `SQL_DOUBLE` column holding `1e300`, say — saturated to `±inf` on the way
  into the application's `SQLREAL`, and the arm reported `SQL_SUCCESS_WITH_INFO`
  with `01S07` ("fractional truncation") because the value had changed. The
  *SQL to C: Numeric* row for `SQL_C_FLOAT`/`SQL_C_DOUBLE` has only two cells,
  in range → *Data* / n/a and out of range → *Undefined* / `22003`, and no
  `01S07` at all: an application that treated the warning as a warning kept an
  infinity its data source never held.

  **This is a severity change from warning to error, visible at all five entry
  points that reach `write_column_value`** — `SQLFetch`, `SQLFetchScroll` and
  `SQLGetData`, plus `SQLExecDirect` and `SQLExecute` through their bound
  output parameters. Both output columns are now left alone, so a buffer and a
  length indicator that previously came back written come back untouched, which
  is what the row's two "Undefined" cells require.

  Three neighbouring cases are unchanged, and deliberately:

  - **A source value that really is `±infinity`** narrows exactly and is
    delivered as before. The finiteness half of the new test is what keeps a
    PostgreSQL `'Infinity'::float8` readable through `SQL_C_FLOAT`.
  - **Underflow is not overflow.** A subnormal `f32` and zero are values `f32`
    can hold, so they stay inside the row's in-range cell: `1e-300` still
    writes `0.0`, and the smallest positive `f32` subnormal still returns
    `SQL_SUCCESS`. psqlODBC (`convert.c`, `case SQL_C_FLOAT`) and MySQL
    Connector/ODBC (`driver/results.cc`, `sql_get_data`) both narrow with a
    plain C cast and range-check neither end, so neither treats an underflow as
    an error.
  - **An in-range narrowing that loses precision reported `01S07`** with the
    value written when this entry was written — `0.1` as an `f64` warned. That
    warning was core's own rather than the row's, and the `i64` → `SQL_C_FLOAT`
    arm made the same claim; both were removed together by the entry above,
    which was the separate question this one deferred.

- **A cancellation delivered by core's own query timer now reports `HYT00` on
  every later call in that cursor's life, not `HY008` on all but the first.**
  A `SQL_ATTR_QUERY_TIMEOUT` that core enforces (a backend answering
  `QueryTimeout::CoreCancels`) is delivered through `Backend::cancel`, the same
  mechanism `SQLCancel` uses, so core has to record which of the two happened.
  It recorded it on the `QueryTimer` guard, which lives for exactly one backend
  call. A deadline expiring in the window between the backend call returning
  and the guard being dropped therefore signalled the token and left no trace
  the *next* call could read: the execution itself stayed successful — which the
  spec permits, "it is possible for the execution to succeed and return
  SQL_SUCCESS while the cancel is also successful" — and the following
  `SQLFetch`, quite likely failing because the delivered cancel had killed the
  server-side cursor, saw a signalled token, a timer of its own that had not
  fired, and reported "operation canceled" to an application that had set a
  deadline and never called `SQLCancel`.

  The record now lives beside the backend's token, in the allocation
  `mint_cancel_token` stores and `Registry::cancel_of` clones out, so it is
  minted and discarded per execution exactly as the token is. `SQLGetData` is
  unaffected on purpose: its diagnostics table carries no `HYT00` row at all,
  so a `SQLGetData` failing on a timed-out cursor keeps `HY008`.

  No driver-facing API changed; `Backend::CancelToken` is untouched.

- **An `SQL_NTS` argument longer than the `MAX_NTS_SCAN` scan limit is now
  `HY090`, where it used to be silently truncated to that many units.**
  Most seriously, **a statement passed to `SQLExecDirectW` as `SQL_NTS` was
  executed truncated** — usually a syntax error with a baffling message, and
  where the cut landed after a syntactically complete prefix, *a different
  statement than the application wrote*. A multi-row
  `INSERT ... VALUES (...),(...)` past the limit is the ordinary way to hit it.

  Core bounds every `SQL_NTS` scan at `MAX_NTS_SCAN` (1 048 576 units — see the
  next entry for how that value is arrived at) so that a buffer whose terminator
  the application forgot is not read past its own allocation. That bound stays.
  What changed is that reaching it is now reported instead of being
  indistinguishable from success: no scan can tell "there is no terminator" from
  "the terminator is past the limit", so the rule is stated as the one that
  needs no such distinction — **an `SQL_NTS` argument is limited to
  `MAX_NTS_SCAN` units, whatever is in it.**

  An **explicitly declared** length is not limited, at any size: there is
  nothing to scan for. An application with a statement, connection string or
  filter longer than the limit passes its real length.

  Every entry point that resolves `SQL_NTS` is affected, and this is the
  complete list:

  | Entry point | Argument(s) now limited |
  |---|---|
  | `SQLExecDirectW` | `StatementText`; a bound `SQL_C_CHAR`/`SQL_C_WCHAR` parameter |
  | `SQLPrepareW` | `StatementText` |
  | `SQLExecute` | a bound `SQL_C_CHAR`/`SQL_C_WCHAR` parameter |
  | `SQLPutData` | the data-at-execution chunk at `DataPtr` |
  | `SQLDriverConnectW` | `InConnectionString` |
  | `SQLBrowseConnectW` | `InConnectionString` |
  | `SQLConnectW` | `ServerName`, `UserName`, `Authentication` |
  | `SQLNativeSqlW` | `InStatementText` |
  | `SQLSetConnectAttrW` | `SQL_ATTR_CURRENT_CATALOG`'s value |
  | `SQLSetCursorNameW` | `CursorName` |
  | `SQLSetDescFieldW` | `SQL_DESC_NAME`'s value |
  | `SQLTables`, `SQLColumns`, `SQLPrimaryKeys`, `SQLForeignKeys`, `SQLStatistics`, `SQLSpecialColumns`, `SQLProcedures`, `SQLProcedureColumns`, `SQLColumnPrivileges`, `SQLTablePrivileges` | every name argument |

  `HY090` ("invalid string or buffer length") is in the diagnostics table of
  every one of them, so no function reports a state its own table omits. The
  condition is not a clause of any of those rows — the spec's `HY090` clauses
  are about a negative length that is not `SQL_NTS` — and each doc comment says
  so.

  Four of these call sites did worse than truncate, and are fixed with it:

  - A bound **`SQL_C_CHAR`** parameter resolved its terminator with
    `CStr::from_ptr`, which is **unbounded** — the one `SQL_NTS` scan in the
    crate with no limit at all, so a buffer missing its terminator was read past
    its own allocation.
  - A bound **`SQL_C_WCHAR`** parameter read the scan through
    `unwrap_or_default`, sending the **empty string** to the data source with no
    diagnostic — worse than the truncation, because `''` is a legal value the
    backend cannot question.
  - `SQLConnectW` read `UserName` and `Authentication` inside `if let Ok(..)`,
    discarding the error: an overrun connected with whatever credentials the DSN
    supplied, under a *UserName* the application believed it had passed.
  - `SQLSetCursorNameW` rewrote *every* failure as `HY009` "Cursor name pointer
    is null". A null `CursorName` is still `HY009`; anything else now reports
    its own state.

  `ConfigDSNW`'s attribute-list parser already reported its own overrun
  (`AttributeSyntaxError::Unterminated`) and is unchanged.

- **`MAX_NTS_SCAN` is 1 048 576 units, not 32 767, so an `SQL_NTS` statement of
  ordinary generated length is no longer refused.** The entry above turned a
  silent truncation into a clean `HY090`, which is strictly better for the same
  input but did not make the *value* right. 32 767 is `i16::MAX`, the width of
  ODBC's *name-length* arguments — `SQLDriverConnect`'s `StringLength1`, the
  catalog functions' `NameLength` group — and not the width of the arguments the
  bound actually governs: `SQLExecDirect`'s and `SQLPrepare`'s `TextLength` and
  `SQLNativeSql`'s `TextLength1` are `SQLINTEGER`. A batched
  `INSERT ... VALUES`, or an `IN` list built from a key set, passes 32 767
  characters routinely, so core was refusing **for length** input that no other
  driver refuses for length. That is narrower than "the other drivers execute
  these statements", which is the data source's answer rather than the driver's:
  what the surveyed source shows is that no length threshold exists in them at
  all — psqlODBC's `ucs2strlen` and `make_string`, MySQL Connector/ODBC's
  `sqlwcharlen` and FreeTDS's `strlen`/`wcslen` are unbounded, and
  `strnlen`/`wcsnlen` appear in none of them. The survey's one counter-example,
  recorded rather than dropped: MySQL Connector/ODBC does have an `HY090` length
  limit, `GET_NAME_LEN` at 192 bytes, but it is a post-hoc MySQL identifier check
  on the catalog functions' name arguments and is never applied to SQL text.

  It was not hidden behind a Driver Manager either: unixODBC forwards `SQL_NTS`
  unchanged for a Unicode application talking to a Unicode driver, resolving the
  length itself only on the ANSI path, so a W-only driver sees raw `SQL_NTS` from
  every Unicode application.

  It also contradicted core's own answer to `SQL_MAX_STATEMENT_LEN`, which
  `default_get_info` reports as `0` — the spec's "no maximum length or the
  length is unknown".

  **Nothing else about the rule changed.** It is still a length limit on
  `SQL_NTS` and not a malformed-input check, still `HY090` at exactly the same
  entry points and arguments listed in the table above, and an explicitly
  declared length is still unlimited at any size.

  The spec fixes no maximum, so the value is core's judgement and the reasoning
  is recorded on the constant. In short: for a correct application the cap costs
  nothing at any value, because the scan stops at the terminator; it is paid
  only by a buffer that reaches it, where the read is already past the
  allocation and already undefined, and a smaller cap makes that read shorter
  rather than sound. The size is then set against a statement anyone can
  construct and count rather than a remembered field figure: a key-set `IN` list
  of UUIDs in canonical text form costs 39 **code units** per key (36 plus two
  quotes and a comma), so ten thousand keys is 390 000 code units. 1 048 576 is
  the first power of two above 10^6 and 2.7× that; `1 << 19` is only 1.34× it,
  inside the band the construction generates rather than above it; `1 << 21` and
  beyond buy headroom no construction here reaches. Code units, not bytes — the
  scanned buffer is `u16`, so a byte figure would be twice the number compared
  against the cap.

  Worst case, reached only by a buffer whose terminator is not inside the cap:
  `utf16_to_string` reads 2 MiB and holds a 2 MiB `Vec<u16>` (plus, only on the
  path that finds a terminator and decodes, a `String` of up to 3 bytes per code
  unit); `nts_utf16_len` reads 2 MiB and allocates nothing; `nts_byte_len` reads
  1 MiB and allocates nothing.

  `ConfigDSNW`'s `MAX_ATTRIBUTE_SCAN` stays at `i16::MAX` and is now documented
  as a sibling bound rather than a mirror of this one. It governs a single
  `Keyword=Value` segment of a DSN attribute list, whose parts the spec sizes at
  `SQL_MAX_DSN_LENGTH` (32) and `SQL_MAX_OPTION_STRING_LENGTH` (256); a shared
  constant would tie a DSN keyword's length to a statement's.

- **`SQLAllocHandle` now answers `HY014` when the handle registry is
  exhausted.** It previously returned `SQL_ERROR` with *no diagnostic record at
  all* for `SQL_HANDLE_ENV`, `SQL_HANDLE_DBC` and `SQL_HANDLE_STMT`, and with
  `HY000` for `SQL_HANDLE_DESC`. `HY014` ("limit on the number of handles
  exceeded") is the code the function's own diagnostics table lists for exactly
  this condition, and the doc comment claimed no limit was imposed.

  A limit does exist. A token packs a slot index into half a `usize`, so the
  ceiling is `2^32 - 1` live handles on a 64-bit target — but **65 535 on a
  32-bit one**, which is not a hypothetical: Excel and Access are 32-bit on
  Windows, and a handle-leaking application reaches 65 535.

  The diagnostic goes to `InputHandle`, which the spec names as this call's
  output channel — the environment for a connection, the connection for a
  statement or an explicit descriptor. **`SQL_HANDLE_ENV` is the one arm that
  cannot carry it**, because its `InputHandle` is `SQL_NULL_HANDLE` and the
  handle an application would read the diagnostic from does not exist yet; it
  still fails with `SQL_ERROR`, and a test pins that rather than leaving it to
  a comment.

  **For driver authors:** `alloc_environment`, `alloc_connection` and
  `alloc_statement` are `pub(crate)`, so nothing outside core calls them, but
  they now return `Result<(), AllocFailure>` instead of `SqlReturn`. The new
  type exists so registry exhaustion cannot be confused with a bad parent
  handle — a future error path has to say which it is rather than inheriting a
  SQLSTATE by accident.

- **The `# Spec compliance` SQLSTATE list on every FFI function now matches the
  spec's own Diagnostics table**, and a test keeps it that way. An audit of all
  sixty exported functions found roughly forty defects with one root cause: the
  `(DM)` annotations had never been checked against the tables they claimed to
  transcribe. They ran in both directions — unmarked rows written off as the
  Driver Manager's (`01000`, `HY001`, `HY013`, `HYT01` across the binding and
  parameter functions; `IM017`/`IM018` across nineteen functions; `01S08` and
  `IM009` in `SQLDriverConnect`), and `(DM)`-marked rows presented as ordinary
  driver checks (`SQLFreeHandle`'s and `SQLNumResultCols`' `HY010`,
  `SQLEndTran`'s `08003`, and a dozen more).

  Twenty rows are marked on only *part* of themselves, and every one of those
  doc comments generalised the whole row away. `HY090` in the catalog functions
  is the widest: seven of the twelve pages carry a second, unmarked sentence
  about a name length exceeding the maximum for that name, and five do not —
  so the row a single wording covered is really two. `SQLBulkOperations`' and
  `SQLSetPos`' `HY092`, `SQLSetPos`' `HY109` and `24000`, `SQLGetData`'s
  `07009`, `24000` and `HY090`, `SQLFetchScroll`'s `HY106`, `SQLPrepare`'s
  `24000`, and `SQLAllocHandle`'s `HY001` are the rest.

  Several claims the code contradicted are corrected too: `HYT00` is
  *originated* by core's query timer rather than propagated as `HY000`;
  `40001`, `40003` and `HYT01` are propagated unchanged rather than degraded;
  `SQLGetTypeInfo`'s `HY010` and `SQLBindParameter`'s `HY021` are returned by
  the driver despite being documented otherwise; `SQLDescribeParam`'s `08S01`
  and `HYT01` said no backend query happens beside a real
  `Backend::describe_param` call; and `SQLExtendedFetch`'s note had the Driver
  Manager's mapping direction backwards. Around thirty rows the spec lists were
  missing from their function's list entirely, and two blanket range bullets —
  `SQLFetchScroll`'s `22001–22018` and the connect functions' `IM001–IM018` —
  claimed states their tables do not have.

  **No behaviour changed.** Every driver-side check named above stays exactly as
  it was — several are load-bearing for memory safety rather than for the spec,
  which is now what they say. The one substantive addition is a `TODO(spec)` at
  the handle-registry exhaustion paths, where `HY014` is the listed code and
  three of the four arms answer `SQL_ERROR` with no diagnostic at all.

  **For driver authors:** the transcription lives in
  `src/types/diagnostics_table.rs` and is core-internal, so nothing to adopt.
  But the four verdict phrasings it recognises are worth copying into a driver's
  own FFI docs, and the module's docs say plainly what the guard does not check —
  it proves the row set and the `(DM)` attribution, never whether the *reason* a
  row is not returned is true.

- `SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC2)` is accepted rather than
  rejected with `HY024`. It is one of the three values the attribute's table
  defines, and unixODBC's Driver Manager forwards it to the driver verbatim at
  connect time — a driver that refuses it is recorded by that Driver Manager as
  an ODBC 2.x driver, which is the opposite of the intent. The version is stored
  and reported back through `SQLGetEnvAttr`; core answers nothing else
  differently, because the spec's 2.x SQLSTATE and datetime-type mapping is the
  Driver Manager's and is driven by what the *application* requested.

  `EnvironmentHandle::odbc_version` is now a `DeclaredOdbcVersion` rather than an
  `odbc_sys::AttrOdbcVersion`, because `odbc-sys` deliberately has no
  `SQL_OV_ODBC2` variant. `types::declared_odbc_version_from_raw` is the
  conversion the FFI boundary uses; `attr_odbc_version_from_raw` is unchanged and
  still cannot name `SQL_OV_ODBC2`.

- `ConfigDSNW` validates `fRequest` before anything else. It was matched last,
  so an out-of-range request carrying a malformed attribute list posted
  `ODBC_ERROR_INVALID_KEYWORD_VALUE` and never reached the arm that posts
  `ODBC_ERROR_INVALID_REQUEST_TYPE` — which the spec ties to that condition
  alone. `odbcinst.h`'s `ODBC_ADD_SYS_DSN` (4) and its neighbours are real
  `SQLConfigDataSource` flags that reach `ConfigDSN` and must be rejected as
  request types.

- A string-returning function given a **non-null** buffer of length zero now
  returns `SQL_SUCCESS_WITH_INFO` and posts `01004`, instead of `SQL_SUCCESS`.
  Nothing is written in that case, not even the null terminator, so reporting
  success made total truncation indistinguishable from a complete write, and the
  length reported back is the length *needed*, which is the same number either
  way. A **null** buffer is unchanged and still `SQL_SUCCESS`: that is the
  length-query form the spec sanctions.

  This is in the shared `write_utf16` helper, so it applies to
  `SQLGetConnectAttrW`, `SQLGetCursorNameW`, `SQLGetDescFieldW`,
  `SQLGetDescRecW`, `SQLGetInfoW`, `SQLDescribeColW`, `SQLColAttributeW`,
  `SQLGetDiagRecW` and `SQLGetDiagFieldW`. Every one of those lists `01004` (or
  the equivalent `SQL_SUCCESS_WITH_INFO` row) in its own diagnostics table.
  `SQLDriverConnectW` and `SQLBrowseConnectW` discard the value and are
  unaffected, deliberately.

  `conformance::observe_info_value_kind` probes the write shape with a non-null
  zero-length buffer, so it now reports `SQL_SUCCESS_WITH_INFO` for a
  `String`-shaped info type. A driver test suite asserting on its return value
  wants "not `SQL_ERROR`" rather than "is `SQL_SUCCESS`".

  **Migration:** an application that passed a zero-length non-null buffer as a
  length probe now sees `SQL_SUCCESS_WITH_INFO`. Passing a null pointer is the
  spec's length-probe form and keeps returning `SQL_SUCCESS`.

- `SQLGetDiagFieldW` no longer rejects a negative `BufferLength` for an
  integer-valued field. The check ran ahead of the field match, so
  `SQL_DIAG_NATIVE`, `SQL_DIAG_COLUMN_NUMBER` and `SQL_DIAG_ROW_NUMBER` failed
  on the sentinels the spec tells applications to pass — "If *\*DiagInfoPtr*
  contains a fixed-length data type, *BufferLength* is SQL_IS_INTEGER,
  SQL_IS_UINTEGER, SQL_IS_SMALLINT, or SQL_IS_USMALLINT, as appropriate",
  all of which are negative. The spec's `SQL_ERROR` condition names character
  strings only, and that is now what is checked.

- `ConfigDSNW(ODBC_CONFIG_DSN)` modifies a data source instead of re-creating
  it. It shared `ODBC_ADD_DSN`'s body, so it called `SQLWriteDSNToIni` — which
  "removes the old section before creating the new one" — and a modify carrying
  three keywords deleted every other keyword the data source had. It also never
  checked that the data source existed, so a CONFIG of an unknown name silently
  created one. Both are now per spec: existence is checked, and changes go
  through `SQLWritePrivateProfileString` only.

  A source comment claiming that keywords absent from a call are not removed has
  been deleted; `SQLWriteDSNToIni`'s own page says the opposite, and it is
  `ODBC_ADD_DSN` that this affects.

- `ConfigDSNW` returns FALSE when a registry write fails. It discarded
  `SQLWritePrivateProfileString`'s result, so a data source whose name
  registered but whose attributes did not was reported as configured. The spec
  pairs the installer error buffer with the other answer — "When **ConfigDSN**
  returns FALSE, an associated *\*pfErrorCode* value is posted" — so the posted
  cause was returned alongside TRUE and no caller had reason to read it.

- `ConfigDSNW` no longer writes a `DRIVER=` attribute into the data source's
  own section. The attribute-write loop skipped only the `DSN` keyword, so a
  `DRIVER=` pair in `lpszAttributes` was written over the value
  `SQLWriteDSNToIni` had just taken from the `lpszDriver` argument — repointing
  the DSN at whatever DLL the attribute list named. The spec forbids it twice:
  "(**ConfigDSN** does not accept the **DRIVER** keyword.)" and "**ConfigDSN**
  may not delete or change the value of the **Driver** keyword." The pair is now
  dropped with a `warn!`, matching the spec's "does not accept" wording, and the
  driver the caller asked for still reaches the registry through `lpszDriver`.

- `ConnectParams::to_connection_string` renders a value containing `}` so that
  it survives being parsed again: the value is brace-quoted and every `}` inside
  is doubled, and `ConnectParams::parse` un-doubles it. A single `}` used to end
  its own quoted run, so everything after it parsed as further keywords — and
  since `SQLBrowseConnectW` returns this string to the application as one the
  spec calls "suitable to use, in conjunction with `SQLDriverConnect`", a
  hostile `odbc.ini` value could inject keywords into the application's *next*
  connect. Values whose edges are whitespace are now braced for the same reason.
  The convention is unixODBC's Driver Manager's, in both directions.

- `SQLNativeSqlW` reports `01004` and `SQL_SUCCESS_WITH_INFO` when
  *OutStatementText* is non-null and *BufferLength* is zero. Total truncation
  returned plain `SQL_SUCCESS`, so an application sizing its buffer from the
  first call saw success and an empty output. A null *OutStatementText* is
  unchanged: that is a length query, not a truncation.

- `SQLDriverConnectW` and `SQLConnectW` clear any abandoned `SQLBrowseConnect`
  state on success. `SQLBrowseConnectW` already did so on its own; without the
  other two, an abandoned browse's accumulated attributes merged into the next
  browse on the same handle.

- `SQLSetConnectAttrW(SQL_ATTR_CURRENT_CATALOG, NULL)` returns `HY009` rather
  than quietly forgetting the stored catalog and reporting success. The spec's
  row — "the *Attribute* argument identified a connection attribute that
  required a string value, and the *ValuePtr* argument was a null pointer" —
  carries no `(DM)` marker, and the spec defines no operation that unsets a
  catalog. Checked against psqlODBC and MySQL Connector/ODBC: neither implements
  null-as-clear.

  **Migration:** an application that used a null pointer to drop core's stored
  override now gets `SQL_ERROR`. There was never a corresponding change at the
  data source, so nothing it relied on was real.

- `SQLGetConnectAttrW` no longer fails with `HY000` when the application offers
  a buffer of 64 KB or more. Its *BufferLength* is `SQLINTEGER`, the spec
  defines no error for a large one, and a value that genuinely does not fit is
  still reported as `01004`.

- `SQLGetConnectAttrW` reports `HYC00` for a connection attribute the spec
  defines but this driver does not answer — `SQL_ATTR_QUIET_MODE`,
  `SQL_ATTR_TRACEFILE`, `SQL_ATTR_TRANSLATE_LIB`, `SQL_ATTR_ENLIST_IN_DTC`,
  `SQL_ATTR_ASYNC_DBC_FUNCTIONS_ENABLE` and the rest. It answered `HY092` for
  all of them, which the spec reserves for an identifier that is not an ODBC
  connection attribute at all; that case is unchanged.

- `SQLDisconnect` cancels an in-progress `SQLBrowseConnect` sequence rather than
  reporting `08003`. `handle.connection` is `None` for the whole of a browse, so
  the `08003` guard answered the one call the spec names as the way out of one.
  The accumulated browse attributes are taken rather than read, so an abandoned
  browse cannot contaminate the next one on that handle.

- **Withdrawing `SQL_ATTR_QUERY_TIMEOUT` disarms core's timer.** Setting the
  attribute back to `0` — the spec's "there is no timeout" — took the
  store-only path, so `SQLGetStmtAttr` reported `0` while the deadline
  recorded by the earlier set stayed on the statement. Every later
  `SQLExecute`, `SQLExecDirect`, `SQLFetch`, `SQLParamData` and catalog call
  armed a timer from that field, so a backend answering
  `QueryTimeout::CoreCancels` had its query cancelled and reported `HYT00`
  against a deadline the application had already removed. Only a backend that
  delegates enforcement to core was affected; one that answers
  `QueryTimeout::DataSource` never had the field set.

- **Resetting `SQL_ATTR_MAX_ROWS`, `SQL_ATTR_MAX_LENGTH` or
  `SQL_ATTR_QUERY_TIMEOUT` to its default now reaches the backend.** The three
  arms that offer a value to the data source were guarded on the value not
  being the default, so the reset fell through to the store-only catch-all and
  `Backend::set_max_rows`, `Backend::set_max_length` and
  `Backend::set_query_timeout` were never told. A data source told to cap a
  result set at ten rows kept capping it while `SQLGetStmtAttr` reported no
  limit — the spec's read-back contract, "`SQLGetStmtAttr` can be called to
  determine the temporarily substituted value", describing a state that
  existed only in core. The default is offered but never *substituted*: the
  value core would substitute is the one the application asked for, so setting
  it returns `SQL_SUCCESS` with no diagnostic, as it did before. A driver that
  implements any of the three hooks should expect a call with `0` where it
  previously got none.

- **`SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2` now reports
  `SQL_CA2_READ_ONLY_CONCURRENCY`.** It answered `0`, which claims no
  concurrency is supported for the only cursor core has — while
  `SQLSetStmtAttr(SQL_ATTR_CONCURRENCY)` accepts `SQL_CONCUR_READ_ONLY`
  unchanged and substitutes every other value back to it with `01S02`. That
  bit asserts exactly what the attribute does, so the two contradicted each
  other. The rest of the bitmask stays clear: it describes updatable cursors,
  row-count exactness and positioned-statement simulation, none of which core
  does.

- **`SQLFetchScroll` logs its own return value.** The `SQL_FETCH_NEXT` branch —
  the only one that fetches — returned through `sql_fetch`, so the exit log read
  `SQLFetch -> ...` and `SQLFetchScroll`'s own `debug!` never ran. It now calls
  the shared fetch body directly, as `SQLExtendedFetch` already did. No
  behaviour change beyond the log.

- **`SQLExecute` refuses to re-execute over an open cursor.** `SQLExecDirect`
  already returned `24000` for it; `SQLExecute` executed anyway and recomputed
  the cursor state afterwards. The spec's Comments are direct — "to execute a
  SELECT statement more than once, the application must call SQLCloseCursor
  before reexecuting" — and Appendix B's cursor-states table for this function
  reads `24000 [p]` in every column. An unprepared statement is still `HY010`,
  which is the `[np]` half of the same row. The doc comment no longer describes
  this state as propagated from the backend: `cursor_open` is core-owned and
  the backend never sees it.

- **`SQLExecDirect` and `SQLExecute` return `SQL_NO_DATA` for a searched DML
  that affected no rows.** Both success paths could only answer `SQL_SUCCESS`
  or `SQL_SUCCESS_WITH_INFO`, so the outcome both spec pages describe in the
  same sentence was unreachable. Core decides it from
  `StatementBackend::column_count` and `StatementBackend::row_count`: no
  columns plus a counted zero rows. A `SELECT` with an empty result set is
  unaffected, and a backend whose `row_count` returns `None` or `SQL_NO_TOTAL`
  keeps `SQL_SUCCESS` — an absent count is not a count of zero.

- **`SQLMoreResults` discards the result set it reports away.** It returned
  `SQL_NO_DATA` and left the cursor open, so a following `SQLFetch` re-read the
  same rows and a following `SQLExecDirect` was refused with `24000`. Appendix
  B's `SQL_NO_DATA` entries for this function are `S1` when the statement was
  not prepared and `S2`/`S3` when it was — `SQLFreeStmt(SQL_CLOSE)`'s row
  exactly — so it now tells the backend and discards, and reports an `08S01`
  from a failing `close_cursor` rather than swallowing it. A statement in `S1`
  or `S2`/`S3` is still left untouched.

- **`SQLConnectW` now hands the backend the DSN name it connected with.**
  `ConnectParams` was built only from the keys `SQLGetPrivateProfileStringW`
  enumerates out of the DSN's `odbc.ini` section, and a section lists the
  keywords inside it rather than its own heading — so the DSN name, the one
  parameter this entry point is named for, was the one a backend could not
  see. `ConnectParams::dsn()` answered `None` here while answering `Some` for
  the same DSN through `SQLDriverConnectW`, where `merge_dsn_params` merges
  the file's keys underneath a connection string that already carried `DSN=`.
  That asymmetry reaches applications through `SQLGetInfo`: the spec makes
  `SQL_DATA_SOURCE_NAME` "the value of the *ServerName* argument in
  SQLConnect", and a driver had nothing to answer it with. The name is
  inserted after the section's keys, so a stray `DSN` keyword inside the
  section cannot displace the name the application actually connected with.

- **`SQL_ATTR_ROW_NUMBER` is no longer echoed back from the application.** The
  read-only attribute was stored verbatim by `SQLSetStmtAttr`'s catch-all and
  returned by `SQLGetStmtAttr`, so an application that wrote `42` read `42`
  back as the number of the current row. Nothing in core's fetch or cursor
  code writes it. Refusing the write stays the Driver Manager's job —
  `SQLSetStmtAttr`'s `HY092` row is `(DM)` for "the value specified for the
  argument *Attribute* was a read-only attribute" — so core accepts the call
  and discards the value, exactly as it already does for
  `SQL_ATTR_IMP_ROW_DESC` and `SQL_ATTR_IMP_PARAM_DESC`. Reading the attribute
  with a cursor open now always reports `0`.

- **`ConfigDSNW` returned FALSE without saying why.** The spec makes the
  installer error buffer the function's only channel — "When **ConfigDSN**
  returns FALSE, an associated *\*pfErrorCode* value is posted to the installer
  error buffer by a call to **SQLPostInstallerError**" — and core never called
  `SQLPostInstallerError` at all, so the Windows ODBC Administrator showed a
  failed DSN creation with no cause. Each of its own failure paths now posts:
  `ODBC_ERROR_INVALID_NAME` for a null driver, `ODBC_ERROR_INVALID_KEYWORD_VALUE`
  for a missing `DSN` keyword or a name `SQLValidDSN` rejects, and
  `ODBC_ERROR_INVALID_REQUEST_TYPE` for an unknown request. A failure *inside*
  `odbccp32` is left to it, which has already posted a more specific cause.

  It also now calls `SQLValidDSN`, which the spec says `ConfigDSN` "should" call
  to check the name's length and characters.

- **`ConfigDSNW` had no panic guard.** It is an `extern "system"` boundary, so an
  unwind across it reached the ODBC Administrator — a C++ process that cannot
  receive a Rust panic. It now runs inside `panic_safe_unlocked`, which is also
  what `SQLCancel` uses. `panic_safe`, which every `SQL*` entry point uses, is
  inapplicable here rather than merely unnecessary: it needs a handle token to
  lock a group by and a diagnostic queue to push through, and `ConfigDSN` is
  handed no ODBC handle at all. A caught panic posts `ODBC_ERROR_REQUEST_FAILED`.

- **Unsigned numeric parameters no longer wrap negative.** `SQL_C_UBIGINT` was
  read as a `u64` and cast to `i64`, so every value above `i64::MAX` reached the
  data source as a negative number; `SQL_C_USHORT` and `SQL_C_UTINYINT` had the
  same shape. The reads now go through `i128`, where no cast can wrap, and the
  declared target's own range check decides what fits.
- **`SQL_C_TINYINT` is accepted.** `c_data_type_from_raw` normalised the
  deprecated ODBC 2.x spellings `SQL_C_LONG` (4) and `SQL_C_SHORT` (5) to their
  signed 3.x equivalents but had no arm for `SQL_C_TINYINT` (-6), so binding it
  was refused. `odbc-sys` models none of the three; that is a gap in the
  binding, not in the ABI core accepts.
- **The descriptor consistency check validates an interval's
  `SQL_DESC_DATETIME_INTERVAL_PRECISION`.** Its fifth clause was documented as
  unenforceable — "core supports no interval types" — which the numeric
  conversion's interval row falsified.

- **A parameter bound to NULL is no longer reported as unbound.** Binding a
  NULL with a null `ParameterValuePtr` and an indicator of `SQL_NULL_DATA` —
  which is how every client sends one, and what pyodbc sends for `None` —
  reported `07002` ("the number of parameters specified in `SQLBindParameter`
  was less than the number of parameters in the SQL statement") at execute time.
  Every NULL parameter failed, at any position, for any type, so
  `WHERE col = ?` with a NULL, and any BI tool's optional filter, could not be
  expressed at all. The diagnostic blamed the application for failing to bind a
  parameter it had bound.

  `ParamRecords::get` tested the APD's `SQL_DESC_DATA_PTR` alone to decide
  whether a record was a binding. `SQLBindParameter`'s *ParameterValuePtr*
  section allows exactly this shape: "An application can set the
  *ParameterValuePtr* argument to a null pointer, as long as
  `*StrLen_or_IndPtr` is `SQL_NULL_DATA` or `SQL_DATA_AT_EXEC`." The Driver
  Manager's own `HY009` agrees, firing only when *both* pointers are null — and
  so did `sql_bind_parameter`, which removes a binding on that same pair. The
  read path was the one place that disagreed. A record now counts as a binding
  when either pointer is non-null.

  The spec scopes that allowance — "(This applies only to input or
  input/output parameters.)" — so `write_output_params` keeps the stricter
  test: writing an output value needs a real buffer, and `write_column_value`
  declines a null target while still writing the length indicator, which would
  otherwise report a length for a value it never stored.

  `DescriptorRecord::is_bound` is deliberately unchanged. `SQLBindCol`'s column
  path uses it, and there a null `TargetValuePtr` really does unbind.

- **`SQLDriverConnect`'s *DriverCompletion* is no longer discarded.** The
  argument was accepted and ignored, and the doc comment said so outright. An
  application passing `SQL_DRIVER_NOPROMPT` — the spec's instruction that the
  driver must not prompt the user — was getting a driver with no way to honour
  it, because the value never reached `Backend::connect`.

  It now decides exactly one thing, which is the only thing core does that a
  prompt could affect: whether the backend is handed `Backend::prompter`.
  `SQL_DRIVER_NOPROMPT` withholds it, so a backend needing interactive
  authentication has nothing to call and the rule cannot be forgotten at a call
  site. `SQL_DRIVER_COMPLETE`, `SQL_DRIVER_PROMPT` and
  `SQL_DRIVER_COMPLETE_REQUIRED` all permit it.

  `SQLConnect` and `SQLBrowseConnect` have no such argument, and their absence
  is read as permitting a prompt rather than forbidding one. `SQLConnect` is the
  DSN path, which is how `isql` and Excel connect, so those are the likeliest
  interactive callers of the whole driver; treating the missing argument as
  `SQL_DRIVER_NOPROMPT` would lock DSN connections out of interactive
  authentication entirely, and no spec text asks for that.

  An unrecognised *DriverCompletion* is accepted and permits a prompt. The
  spec's state for it is `HY110`, but **both** clauses of that row carry `(DM)`,
  so the check belongs to the Driver Manager and core adds none; a value
  reaching the driver at all means no Driver Manager validated it, and the
  permissive reading is the one that does not silently disable a feature the
  application may be relying on. Core logs a `warn!` for it.

- **`SQL_DESC_DATETIME_INTERVAL_CODE` is the subcode, not the concise type.**
  `SQLColAttributeW` reported `SQL_TYPE_TIMESTAMP` (93) for a timestamp column
  where the spec defines `SQL_CODE_TIMESTAMP` (3); likewise 91 for
  `SQL_CODE_DATE` and 92 for `SQL_CODE_TIME`. `sqlext.h` builds one value from
  the other, which is what made them easy to conflate. An application reading
  the field to distinguish a date from a timestamp got a value outside the range
  the field is defined over. The mapping now lives beside `verbose_type` in
  `types/col_attr.rs`, because a concise datetime type determines both answers
  and a second mapping is a second thing to be wrong; every writer of
  `SQL_DESC_CONCISE_TYPE` and both readers share it.

- **The descriptor functions no longer fail silently.** `SQLGetDescFieldW`,
  `SQLSetDescFieldW` and `SQLSetDescRec` returned a bare `SQL_ERROR` with no
  diagnostic record, so an application knew something had failed and could not
  learn what. Fixing that gave each descriptor a diagnostic queue of its own and
  a way to reach it — through the owning statement, since `HandleKind::Desc` is
  one kind covering four roles — which is what the implementations above are
  built on.

  The intermediate step, where the three answered `HYC00` and `SQLGetFunctions`
  reported all five descriptor functions unsupported, is superseded within this
  same unreleased cycle: four of the five are now implemented and advertised.
  What survives it is the rule that produced it — `SQLGetFunctions` must not
  claim a function before it works, and of the two available lies, reporting a
  function supported when it is not is the more damaging one. `SQLCopyDesc` is
  still held to it.

- **`SQLSetStmtAttrW` no longer accepts a descriptor swap it cannot honour.**
  Setting `SQL_ATTR_APP_ROW_DESC` or `SQL_ATTR_APP_PARAM_DESC` to an explicitly
  allocated descriptor returned `SQL_SUCCESS` and was ignored, so an application
  could swap in its own ARD, be told it worked, and have the statement's own
  used instead. It now returns `HYC00`: core cannot allocate an explicit
  descriptor at all, so it cannot honour one being swapped in. `SQL_NULL_DESC`
  still succeeds, because reverting to the implicit descriptor is the state core
  is already in. The two implementation descriptors are untouched — `HY017` is
  **(DM)** on both of its clauses, so core adds neither check, and
  `SQLSetStmtAttrW`'s doc comment now records that so nobody adds it later.

- **`SQL_DATE` (9) is a date again, not a timestamp.** `odbc-sys` names the
  value 9 `DATETIME`, after the ODBC 3.x *verbose* `SQL_DATETIME`, and core had
  grouped it with `SQL_TYPE_TIMESTAMP` on the strength of that name.
  `SQLBindParameter`'s `ParameterType` and a column's reported type are both
  **concise** types, where 9 is the ODBC 2.0 `SQL_DATE` — so an ODBC 2.x
  application binding a date parameter was getting it converted as a timestamp,
  and a column of that type was named `TIMESTAMP`. Fixed in `param_convert`,
  `binary_convert` and `types::col_attr`, each with a test. AWS's Redshift ODBC
  driver reads it the same way, opening that branch of
  `convertCParamDataToSQLData` with `case SQL_TYPE_DATE: case SQL_DATE:`.
  `col_attr::verbose_type` is unaffected: it deliberately maps only the 3.x
  concise codes 91-95, because there 9 genuinely is the verbose value.

- **A `SQL_C_BINARY` parameter is now converted to the SQL type the application
  declared.** Both delivery paths previously returned the raw bytes whatever
  `ParameterType` said, and `Backend::execute` receives only `&[ColumnValue]`,
  so the declared type was lost entirely — a parameter bound `SQL_C_BINARY` +
  `SQL_INTEGER` reached the data source as four uninterpreted bytes. Core now
  implements the "C to SQL: Binary" table: the fixed-width numeric targets, the
  three datetime structs, and `SQL_BIT`, each requiring exactly the type's width
  and reporting `22003` otherwise. Byte order is native, which the spec never
  states — the ruling and its evidence are recorded on `crate::binary_convert`.
  `SQL_DECIMAL`/`SQL_NUMERIC` and every character target are refused with
  `07006`, because ODBC specifies neither a decimal width nor an encoding for
  these bytes; that refusal is raised by `SQLBindParameter` rather than at
  execute time, so an application fails before running its query.

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

- **`SQLGetDescRecW` failed on every ARD and APD.** It read `SQL_DESC_NULLABLE`
  and `SQL_DESC_NAME` through the same path `SQLGetDescField` uses, which
  answers `HY091` for a field the descriptor's role leaves undefined — and both
  are undefined on an application descriptor, so the call could not succeed at
  all. `SQLGetDescRec`'s diagnostics table lists no `HY091` row, and its
  Comments section names this exact case: "calling SQLGetDescRec for the
  SQL_DESC_NAME or SQL_DESC_NULLABLE field of an APD or ARD will return
  SQL_SUCCESS but an undefined value for the field." Both now come back as
  `SQL_NULLABLE_UNKNOWN` and an empty name with `SQL_SUCCESS`.
  `SQLGetDescFieldW` is unchanged and still answers `HY091`, which is its own
  table's row.

- **`SQLCopyDesc` dropped two header fields whenever an IRD or IPD was
  involved.** `SQL_DESC_ARRAY_STATUS_PTR` and `SQL_DESC_ROWS_PROCESSED_PTR` are
  stored on the owning statement for those two roles — they are
  `SQL_ATTR_ROW_STATUS_PTR`, `SQL_ATTR_ROWS_FETCHED_PTR`,
  `SQL_ATTR_PARAM_STATUS_PTR` and `SQL_ATTR_PARAMS_PROCESSED_PTR` — and the copy
  read and wrote only the descriptor's own header map. The spec is unqualified:
  "All fields of the descriptor, except SQL_DESC_ALLOC_TYPE ..., are copied,
  whether or not the field is defined for the destination descriptor." Both are
  now snapshotted from the source's statement and routed to the target's, so a
  copy onto an IPD is readable through `SQLGetDescField` and through
  `SQLGetStmtAttr` alike. The two-phase locking is unchanged: phase one still
  holds only the source's group.

- **`07009` was missing for an IPD record 0 and for a negative record number on
  an IPD or IRD.** `SQLSetDescRec`'s row has no `(DM)` marker on either clause —
  "The RecNumber argument was set to 0, and the DescriptorHandle referred to an
  IPD handle. The RecNumber argument was less than 0" — and its negative clause
  names no descriptor role, so it now holds for all four and is reported ahead
  of an IRD's `HY016`. A negative record number on an IPD previously wrapped to
  record 0 and wrote a record the caller never asked for.
  `SQLGetDescFieldW` and `SQLSetDescFieldW` gained the corresponding clause,
  "The FieldIdentifier argument was a record field, the RecNumber argument was
  0, and the DescriptorHandle argument was an IPD handle" — a *record* field
  only, so `SQL_DESC_COUNT` at record 0 still answers. This is unrelated to
  bookmark records, which remain out of scope: bookmarks are an ARD and IRD
  concept and an IPD has none.

- **`SQLSetDescFieldW` accepted a `SQL_DESC_NAME` longer than
  `SQL_MAX_IDENTIFIER_LEN`.** The spec's `22001` row is unmarked — "The
  FieldIdentifier argument was SQL_DESC_NAME, and the BufferLength argument was
  a value larger than SQL_MAX_IDENTIFIER_LEN" — and the doc comment's stated
  reason for skipping it ("core imposes no length limit of its own") was beside
  the point: the limit is the *backend's*, and core already answers it from
  `Backend::catalog_result_column_widths().identifier_len`. A name longer than
  that is now `22001`. **A driver declaring a small `identifier_len` will see
  calls fail that previously succeeded**, which is the report the application
  was entitled to.

- **`SQLSetDescFieldW`'s documented diagnostics understated one check.** Its
  `HY105` line read "**(DM)**; not returned here", which is true of `HY105` but
  reads as though core validates nothing: an unrecognised
  `SQL_DESC_PARAMETER_TYPE` has always been rejected, as `HY092`. The line now
  says so, and a test pins it. No behaviour change.

- **`SQLPutData` corrupted every wide data-at-execution value.** It resolved
  `SQL_NTS` with a byte-wise scan whatever the parameter was bound as, so a
  `SQL_C_WCHAR` value stopped at the first zero byte — index 1 of any ASCII
  text in UTF-16LE. The one surviving byte then failed to pair under
  `chunks_exact(2)` and the parameter reached the backend as an empty string,
  with no diagnostic. `SQL_NTS` is now resolved in the C type the APD records,
  which is what the spec says it is: "The data must be in the C data type
  specified in the *ValueType* argument of **SQLBindParameter**."

- **`SQLPutData`'s `SQL_NTS` scan is bounded.** It used `CStr::from_ptr`, which
  scans until it finds a terminator; a buffer without one was read past its own
  allocation. It now shares `utf16_to_string`'s `MAX_NTS_SCAN` bound, which was
  already applied everywhere else in the crate.

- **`SQLBindCol` no longer destroys an indicator-only binding.** Passing a null
  `TargetValuePtr` removed the column's ARD record outright, whatever
  `StrLen_or_IndPtr` was, so the state the spec describes in as many words —
  "An application can unbind the data buffer for a column but still have a
  length/indicator buffer bound for the column, if the `TargetValuePtr`
  argument in the call to `SQLBindCol` is a null pointer but the
  `StrLen_or_IndPtr` argument is a valid value" — could not be reached. An
  application asking only for a column's length got nothing and no diagnostic.
  The record is now removed only when *both* pointers are null, which is the
  pair `SQLBindParameter` already treated as an unbind, and `SQLFetch` writes
  the length for such a column without touching the absent data buffer.

  The mature drivers split on this and core follows the spec: MySQL
  Connector/ODBC unbinds only when both pointers are null, while psqlODBC clears
  the whole binding on a null `TargetValuePtr` regardless. The spec sentence is
  unconditional, and an application that asked only for a length has no other
  way to obtain one.

- **`SQLPutData` reports `HY020`.** Sending `SQL_NULL_DATA` after data had
  already been put cleared the accumulated buffer and answered `SQL_SUCCESS`,
  so the application's data disappeared with no diagnostic; the reverse order
  concatenated onto a parameter the application had declared NULL. The spec's
  row carries no `(DM)` marker — "SQLPutData was called more than once since
  the call that returned SQL_NEED_DATA, and in one of those calls, the
  `StrLen_or_Ind` argument contained SQL_NULL_DATA or SQL_DEFAULT_PARAM" — so
  it is the driver's to return, and both orderings now do.

- **`SQLParamData` called twice in a row reports `HY010`.** It finalised the
  requested parameter from an empty accumulated buffer, which it read as NULL,
  so an application that lost track of its own data-at-execution loop inserted
  a silently wrong row. The spec's `HY010` row states it directly, after the
  `(DM)` clause and unmarked: "The previous function call was a call to
  **SQLParamData**." The state survives the error, so the application recovers
  by calling `SQLPutData` for the parameter it was already asked for.

- **A zero-length data-at-execution value is no longer NULL.**
  `SQLPutData(ptr, 0)` sends a zero-length value and
  `SQLPutData(_, SQL_NULL_DATA)` sends NULL; inferring NULL from an empty
  buffer collapsed the two, so an empty string could not be sent this way at
  all.

- **`SQLPutData` recognises `SQL_DEFAULT_PARAM`.** The constant appeared nowhere
  in the crate, so an indicator of `-5` fell into the generic negative branch
  and reported `HY090` — telling the application its length was malformed when
  the spec's own *StrLen_or_Ind* description lists the value: "is SQL_NTS,
  SQL_NULL_DATA, or SQL_DEFAULT_PARAM". It is now accepted and resolves to
  NULL, and it participates in the `HY020` concatenation rule, whose row names
  it beside `SQL_NULL_DATA`.

  Accepting rather than refusing follows psqlODBC, which pairs the two
  constants in `PGAPI_PutData` and answers `SQL_SUCCESS` for both while raising
  "Invalid string or buffer length" for every other negative value; MySQL
  Connector/ODBC does not recognise the constant at all. The `07S01` row was
  considered and not taken — no mature driver returns it here. NULL is the only
  value the request can resolve to in core, which is a fact about core rather
  than a guess about a data source: `SQL_DEFAULT_PARAM` names a *procedure*
  parameter's default, and core refuses `{call ...}` and `{?= call ...}` with
  `HYC00`, so no statement it executes has a parameter carrying one.

- **`SQLPutData` accepts a null `DataPtr` with a length of zero.** Its `HY009`
  guard refused every null pointer, which is stricter than the clause it stands
  in for: "(DM) The argument *DataPtr* was a null pointer, and the argument
  *StrLen_or_Ind* was **not** 0, SQL_DEFAULT_PARAM, or SQL_NULL_DATA." A
  zero-length put is how an application sends an empty value, and it now works.

- **Five doc comments corrected against the spec's diagnostics tables.**
  `SQLBindParameter` claimed `HY021` was driver-manager-handled, though its row
  carries no `(DM)` marker and the function returns it; `SQLBindCol` did not
  list `HY021` at all, though it returns it under `SQLSetDescRec`'s
  consistency-check mandate, and justified deferring `07009` by asserting a
  Driver Manager check that does not exist — binding before a result set exists
  is the real reason. `SQLPutData` was missing `07006`, `08S01` and `HY008`,
  `SQLParamData` was missing `22026`, and `SQLDescribeParam` was missing
  `21S01`.

- **`SQLFetch` no longer offsets a null `SQL_DESC_DATA_PTR` or
  `SQL_DESC_INDICATOR_PTR` by `SQL_ATTR_ROW_BIND_OFFSET_PTR`.** The offset was
  applied to every bound pointer unconditionally, including the ones
  `collect_bindings` deliberately admits with one pointer null — an
  indicator-only binding, or a data buffer with no indicator supplied. Adding a
  non-zero offset to a null pointer produces a non-null address built from the
  offset alone, so a live `SQL_ATTR_ROW_BIND_OFFSET_PTR` turned an absent
  pointer into a wild one: the `22002` check saw a "supplied" indicator that was
  never there, and `write_column_value` wrote through whatever address the
  offset happened to name. Both are now left null, matching the spec's own
  framing of the attribute as shifting a *buffer* — a pointer with no buffer
  behind it has nothing to shift.

- **`SQLCopyDesc`'s phase one now runs under a panic guard.** It copies a
  descriptor in two lock phases — the source's group alone, then the target's
  — and only phase two was wrapped by `panic_safe`. Phase one calls
  `describe_col` (via `snapshot_ird`) whenever the source is an IRD, which is
  driver-author code and the exact panic surface every other `Backend` call
  runs under a guard for; a panic there had no `catch_unwind` above it at all,
  so it unwound straight through `HandleScope::with_group` and across the
  `extern "system"` boundary `forward_ffi!` generates for
  `SQLCopyDesc` — aborting the process rather than returning `SQL_ERROR`.

  The fix is a new `panic::catch_panic_as_error`, narrower than
  `panic_safe_unlocked`: phase one has no target handle to post a diagnostic
  through yet, so it folds a caught panic into the same `OdbcError::Panic` a
  non-panicking phase-one failure (`HY007`) already produces, and phase two's
  ordinary `panic_safe` posts it to the target's queue as `HY000` — exactly
  where the spec says this call's diagnostics belong. No SQLSTATE this
  function returns changed; the previously-unguarded path now returns one
  (`HY000`) instead of aborting.

- **`SQLGetData` with a zero-length buffer no longer consumes the column.**
  `write_wchar`, `write_char` and `write_binary` shared one branch for a null
  `target_value_ptr` (a pure length query) and a non-null one with
  `buffer_length` 0 (the standard "how large a buffer do I need" probe): both
  returned plain `SQL_SUCCESS`. `SQLGetData`'s own `cursor.done` is derived
  from that return value — anything other than `SQL_SUCCESS_WITH_INFO` marks a
  chunkable column exhausted — so the probe's `SQL_SUCCESS` silently closed the
  column, and the documented follow-up call with a buffer sized from the
  reported length got `SQL_NO_DATA` instead of the value. The spec's own step
  5 already draws this line: "If the data buffer supplied is too small to hold
  the null-termination character, SQLGetData returns SQL_SUCCESS_WITH_INFO and
  SQLSTATE 01004" — a zero-length buffer is always too small to hold it, so
  it is the same case as any other partial write, not a length query. The
  three writers now split the same way `write_utf16` already does elsewhere in
  this crate: null target stays `SQL_SUCCESS`, non-null target with no room
  (`buffer_length <= 0`, or `< 2` for `SQL_C_WCHAR`'s two-byte terminator)
  becomes `SQL_SUCCESS_WITH_INFO` with `01004` and the cursor left resumable.

  **For driver authors:** the same three writers also serve `SQLFetch`'s
  bound-column path (`SQLBindCol` with `BufferLength` 0), which now reports
  `SQL_SUCCESS_WITH_INFO`/`01004` for that row instead of silently discarding
  the value — the same shared-branch bug, on a call shape no test in this
  crate previously exercised. A `SQLGetData` call with a non-zero
  `BufferLength`, or any bound column with a real `BufferLength`, is
  unaffected.

- **`SQL_ATTR_PARAM_BIND_OFFSET_PTR` is now applied when reading bound
  parameters.** `SQLSetStmtAttrW` accepted the attribute, stored it on the APD
  header as `SQL_DESC_BIND_OFFSET_PTR` and handed it back from
  `SQLGetStmtAttrW` — and nothing ever read it. Every execution therefore sent
  the value at the *bound* address, so an application that binds `&row.field`
  once and moves between parameter rows by writing a new offset (which is the
  entire purpose of the attribute, per `SQLBindParameter`'s "Rebinding with
  Offsets") silently sent its first row's values over and over, with no
  diagnostic. The offset is now dereferenced once per execution and added to
  both `SQL_DESC_DATA_PTR` and `SQL_DESC_INDICATOR_PTR` at every reader: the
  bound-parameter read, the data-at-execution scan that decides whether a
  parameter is streamed, and the write-back of output parameters.

  A null pointer is still never offset, matching the rule `SQLFetch` follows on
  the row side: the attribute shifts a *buffer*, and `null + offset` would turn
  an absent buffer into a wild address. The spec states this for the parameter
  side outright — the offset is added "if none of the values in the
  SQL_DESC_DATA_PTR, SQL_DESC_INDICATOR_PTR, and SQL_DESC_OCTET_LENGTH_PTR
  fields is a null pointer". Core reads that per pointer rather than
  all-or-nothing, as MySQL Connector/ODBC's `ptr_offset_adjust` does, because
  the literal reading would withhold the offset from a data buffer merely
  because the parameter was bound with no indicator — the commonest binding
  there is for a fixed-width C type.

  `SQLParamData`'s echoed data-at-execution pointer is the one deliberate
  exception: it stays the **unoffset** `SQL_DESC_DATA_PTR`. The spec's
  *ValuePtrPtr* description returns the address "as contained in the
  SQL_DESC_DATA_PTR descriptor record field", and the offset arithmetic in its
  Comments section is given only for the *column* case and defined there in terms
  of `SQL_ATTR_ROW_BIND_OFFSET_PTR`. psqlODBC agrees for the single-parameter-set
  configuration core supports; MySQL Connector/ODBC does not, and the reasoning
  for preferring the spec's wording is recorded at the write site in
  `ffi/params.rs`.

  **For driver authors:** a backend that previously received the base row's
  values from an application using this attribute now receives the row the
  application actually selected. No `Backend` method changed, and a driver whose
  applications never set the attribute is unaffected.

- **`SQL_C_BIT` now follows the spec's three-way range rule when reading a
  value.** Both numeric arms of the fetch-side conversion reduced to "non-zero
  becomes 1", so every value outside the bit range was silently accepted:
  `SQLGetData`/`SQLFetch` answered `SQL_SUCCESS` and wrote 1 for `5`, for `-1`
  and for `2.0`. The [SQL to C: Numeric] table gives three outcomes instead —
  "Data is 0 or 1" converts with no diagnostic, "greater than 0, less than 2,
  and not equal to 1" writes the truncated data with `01S07`, and "less than 0
  or greater than or equal to 2" is `22003` with nothing written — and the
  identical row in [SQL to C: Character] governs a numeric string reaching the
  same target. The fractional case was also delivering the wrong *value*: `0.5`
  wrote 1, where truncating the fractional part gives 0.

  The infinities answer the third test — `+inf` is greater than 2 and `-inf` is
  less than 0 — and NaN answers none of the three, so all three are `22003`;
  `-0.0` equals `0.0` and is the "0 or 1" case. An integer
  source cannot carry a fraction, so the `01S07` outcome is reachable only from
  a float or a numeric string.

  **For driver authors:** an application binding `SQL_C_BIT` against a column
  whose values are not 0 or 1 now sees `SQL_ERROR`/`22003` where it previously
  got `SQL_SUCCESS` and a 1, and `SQL_SUCCESS_WITH_INFO`/`01S07` for a fraction
  in that range. No `Backend` method changed. A backend that means "true" should
  deliver `ColumnValue::Bool` or a 0/1 numeric rather than relying on the old
  coercion.

- **Character and `DECIMAL` text reaching an integer C type is now converted
  exactly, instead of through `f64`.** `parse_numeric_text` tried `i64` first
  and fell back to `f64`, so any text carrying a fractional part went through a
  53-bit mantissa on its way to `SQL_C_SBIGINT` and friends. Two things were
  wrong with that. The value could be silently altered above 2^53 —
  `"9007199254740993.5"` delivered 9007199254740994 with a clean `SQL_SUCCESS` —
  and the fraction was *rounded* where the exact-numeric row says "Data
  converted with truncation of fractional digits", with `01S07`. A value that
  fits the target only after truncation was rejected outright:
  `"18446744073709551615.9"` to `SQL_C_UBIGINT` is `u64::MAX`, but the nearest
  `f64` is 2^64 and the range test answered `22003`.

  Two tables govern this and they agree: [SQL to C: Character] for a
  `ColumnValue::String` (an `SQL_CHAR`/`SQL_VARCHAR` column) and [SQL to C:
  Numeric] for a `ColumnValue::Decimal` (`SQL_DECIMAL`/`SQL_NUMERIC`). Their
  exact-numeric rows list the same twelve C types with the same three outcomes;
  only the character table adds a fourth, "Data is not a *numeric-literal*" →
  `22018`, which a numeric SQL source cannot reach.

  Text bound for one of the eight integer C types now goes through
  `param_convert`'s `DecimalLiteral`, the same exact-digit machinery the
  parameter side has used since the C-to-SQL tables were transcribed: truncated
  toward zero, so `"-3.9"` delivers `-3` and not `-4`, with `01S07` when
  anything was dropped and no diagnostic when the fraction was all zeros.
  Losing whole digits stays `22003` with nothing written, and text that is not a
  *numeric-literal* stays `22018`. `SQL_C_FLOAT`, `SQL_C_DOUBLE` and
  `SQL_C_BIT` keep the `f64` path — the first two because `f64` is where the
  value is going, the third because its own row of the tables turns on a
  fraction rather than discarding one.

  **For driver authors:** three observable changes, no `Backend` method touched,
  and nothing to adjust in a backend returning `ColumnValue::Decimal`.

  - An application fetching a fractional value into an integer C type now gets
    `SQL_SUCCESS_WITH_INFO`/`01S07` where it previously got `SQL_SUCCESS`.
  - The delivered integer may differ by one from the old `f64`-rounded answer.
  - A *negative* fraction above `-1` into an **unsigned** target — `"-0.5"` into
    `SQL_C_UTINYINT` — now writes `0` with `01S07` where it previously answered
    `SQL_ERROR`/`22003`. That is an error becoming a success, the most
    surprising direction, and it is what the row says: only *whole* digits must
    survive, and `-0.5` has none to lose. The old behaviour came from testing
    the `f64`'s sign before truncating it.

- **A numeric literal with a pathological exponent is now refused before it is
  expanded, instead of asking for a multi-gigabyte allocation.**
  `DecimalLiteral::to_decimal_string` and `DecimalLiteral::to_integer` expand an
  exponent into plain decimal notation with `"0".repeat(scale)`, and `scale`
  came straight from the literal's own `i32` exponent. `"1e2147483646"`
  therefore asked for a 2 GB `String`, with a second copy in the `format!` on
  the next line; `"1e-2147483647"` took the other branch and panicked inside
  `format!` instead.

  **A second, narrower panic on the same branch is fixed with it.** That branch
  padded through `format!`'s `{digits:0>scale$}` width, which Rust caps at
  `u16::MAX`, so *any* scale from 65 536 up panicked with "Formatting argument
  out of range" rather than rendering — including scales well inside the new
  bound, and inside what PostgreSQL `numeric` can hold. It builds its padding
  with `"0".repeat` now, so every accepted value renders: `"1e-65536"` and
  `"15e-200000"` return the correct decimal string where they previously
  brought down the conversion. Lowering the bound to `u16::MAX` instead was
  rejected — it would have traded a contained panic for wrong answers on
  legitimate data.

  **This is a security fix, not hardening.** A failed allocation *aborts* the
  process — it does not unwind — so `panic_safe` cannot turn it into a
  `SQL_ERROR`, and the host application goes down with the driver. The exponent
  is not only the application's to choose: since character and `DECIMAL` column
  text began reaching `DecimalLiteral` (the entry above), a **data source** can
  reach this by returning `ColumnValue::Decimal("1e2147483646")`, which puts the
  input outside the driver's trust boundary. Verified: under `ulimit -v
  1048576`, the test process previously died with `memory allocation of
  2147483646 bytes failed`.

  A new `MAX_DECIMAL_EXPANSION_DIGITS` (2^20) bounds the digits an expansion may
  *synthesise* — the zeros the exponent asks for, and not the significant digits
  the source text already contained. The one exception is leading fractional
  zeros, which are counted because the renderer really does re-synthesise them
  (see below). The bound clears PostgreSQL `numeric`'s documented
  131 072 + 16 383 digits, the widest exact type any mainstream data source
  offers, by more than seven times, and three `const` assertions hold the
  derivation from both sides — the bound cannot be tightened onto the ODBC
  precision alone, nor loosened back into a hazard, without failing to compile.
  The transient peak is capped at about 2 MiB: each `"0".repeat` is consumed by
  a `format!` that copies it, so both are briefly live. That is roughly three
  orders of magnitude below the ~2 GiB one unbounded exponent asks for.

  The bound measures each rendering branch separately, so it refuses only a
  value that would actually allocate: it costs nothing to render `"0e2147483646"`
  as `"0"` or to truncate `"1e-2000000"` toward zero, and both still do exactly
  that. Refused are a non-zero mantissa at a large *non-negative* exponent
  (`"1e2147483646"`, which expands to 2 GB of trailing zeros) **and** any
  mantissa, zero included, at a large *negative* one (`"1e-2147483647"`,
  `"0e-2147483647"` — that branch pads out to the full scale whether the
  mantissa is zero or not). Leading fractional zeros count toward the bound
  even when the caller spelled them out, because the renderer re-synthesises
  them. Every case named here is a magnitude no source in the table above can
  hold.

  The two conversion directions name different SQLSTATEs for it, and both are
  their own table's:

  - **Binding** an over-range literal is `22001`. The [C to SQL: Character]
    table's `SQL_DECIMAL`/`SQL_NUMERIC`/integer row gives `22001` for both of
    its lossy outcomes and lists no `22003` at all.
  - **Fetching** one into an integer C type is `22003`, per the exact-numeric
    row of [SQL to C: Character] and [SQL to C: Numeric], with
    `*TargetValuePtr` left untouched.

  A literal that is merely *long* is unaffected, as are `SQL_REAL`/`SQL_FLOAT`/
  `SQL_DOUBLE` and `SQL_BIT`, which never expanded — their rows go through
  `f64`, where an over-range exponent is already `22003`.

- **Fetching a float into an integer C type now truncates before it
  range-checks, and reports `01S07` when it drops a fraction.** Two things
  change for an application, in opposite directions.

  It sees `SQL_SUCCESS_WITH_INFO` and `01S07` ("fractional truncation") where it
  previously saw a clean `SQL_SUCCESS`: `ColumnValue::F64(3.9)` fetched as
  `SQL_C_SLONG` still delivers `3`, but the dropped `.9` is now reported. The
  exact-numeric row of [SQL to C: Numeric] calls this outcome "Data converted
  with truncation of fractional digits" and gives it `01S07`, and that table
  covers `SQL_REAL`, `SQL_FLOAT` and `SQL_DOUBLE` as well as the exact types.
  Code that treats any non-`SQL_SUCCESS` return as a failure will see these
  fetches as failing.

  And it sees conversions succeed that previously failed with `22003`. The range
  test ran against the *untruncated* value, so `127.5` into `SQL_C_STINYINT` was
  rejected even though its truncation, `127`, fits. What the table's third
  outcome protects is whole digits, so that value now writes `127` with `01S07`.
  The same off-by-a-fraction rejection existed at every boundary of all eight
  integer targets — `SQL_C_STINYINT`, `SQL_C_UTINYINT`, `SQL_C_SSHORT`,
  `SQL_C_USHORT`, `SQL_C_SLONG`, `SQL_C_ULONG`, `SQL_C_SBIGINT` and
  `SQL_C_UBIGINT` — and all eight now share one implementation. `-0.5` into an
  unsigned target is part of this: it has no whole digits to lose, so it writes
  `0` with `01S07` instead of `22003`, which is the reading the text path
  already took.

  Unchanged: a value whose *truncation* still does not fit is `22003` with
  nothing written, and `NaN` and `±infinity` stay `22003` — they have no
  truncation. `SQL_C_BIT`, `SQL_C_FLOAT` and `SQL_C_DOUBLE` have their own rows
  and are untouched.

- **Fetching character data into a datetime C type now converts across the
  three literal forms instead of refusing with `22018`.** [SQL to C: Character]
  lets each datetime C struct accept more than its own form, and core
  implemented only the matching pairs. Three outcomes changed, all from
  `SQL_ERROR` with `22018` ("invalid character value for cast") to a
  conversion:

  | Column text | `TargetType` | Now | SQLSTATE |
  |---|---|---|---|
  | `2026-07-21 00:00:00` | `SQL_C_TYPE_DATE` | the date | none |
  | `2026-07-21 10:30:15` | `SQL_C_TYPE_DATE` | the date, time dropped | `01S07` |
  | `2026-07-21 10:30:15` | `SQL_C_TYPE_TIME` | the time, date ignored | none |
  | `2026-07-21 10:30:15.5` | `SQL_C_TYPE_TIME` | the time, fraction dropped | `01S07` |
  | `10:30:15` | `SQL_C_TYPE_TIMESTAMP` | the time on today's UTC date | none |

  That is the full set: the three rows are `SQL_C_TYPE_DATE` from a
  timestamp-value, `SQL_C_TYPE_TIME` from a timestamp-value, and
  `SQL_C_TYPE_TIMESTAMP` from a time-value. The pairs that already worked —
  date text to `SQL_C_TYPE_DATE`, time text to `SQL_C_TYPE_TIME`, timestamp
  text to `SQL_C_TYPE_TIMESTAMP`, and date text to `SQL_C_TYPE_TIMESTAMP` at
  midnight — are unchanged, as is `22018` for text that is no datetime literal
  at all.

  **Applications will see `SQL_SUCCESS_WITH_INFO` where they saw
  `SQL_ERROR`.** The two `01S07` rows write the truncated value, per the
  table's own *TargetValuePtr* column; code treating any non-`SQL_SUCCESS`
  return as a failure will now discard data it was given. Which part provokes
  the warning differs per row and follows the footnotes: for
  `SQL_C_TYPE_DATE` any non-zero time field does ([c], "the time portion ... is
  truncated"), while for `SQL_C_TYPE_TIME` only a non-zero fraction does, since
  [d] makes the discarded date part of the conversion rather than a loss.

  A time-only literal read as `SQL_C_TYPE_TIMESTAMP` takes its date fields from
  the current UTC date, per footnote [g], and keeps its own fractional seconds
  in nanoseconds. This differs from a `SQL_TYPE_TIME` *column* read into the
  same struct, which zeroes the fraction — that is the [SQL to C: Time] table's
  rule for a different source type, and both are now implemented as written.

  Text that parses as a datetime but carries an out-of-range field still
  answers `22007`, not `22018`, on all three paths.

- **Impossible calendar dates are now refused instead of being converted.**
  The shared literal parser validated the day only as `1`–`31`, so
  `2024-02-30`, `2023-02-29` and `2024-04-31` were accepted. It now checks the
  day against the length of the month, with the proleptic Gregorian leap rule:
  divisible by 4, except centuries, except every fourth century — so
  `2000-02-29` is valid and `1900-02-29` is not. February has 29 days in a leap
  year and 28 otherwise; April, June, September and November have 30; January,
  March, May, July, August, October and December have 31. That is the whole
  change: a day of `0`, a month outside `1`–`12`, every syntactically malformed
  form and the year are all unaffected.

  **Both directions change, because both use the parser.** Fetching such a
  value into any of `SQL_C_TYPE_DATE`, `SQL_C_TYPE_TIME` (from the timestamp
  form, whose date portion the conversion would otherwise ignore) and
  `SQL_C_TYPE_TIMESTAMP` was `SQL_SUCCESS` with a nonsense or silently
  date-stripped struct, and is now `SQL_ERROR` with `22007` and nothing written;
  binding a character parameter carrying one to `SQL_TYPE_DATE`,
  `SQL_TYPE_TIME` or `SQL_TYPE_TIMESTAMP` was forwarded to the backend as a
  valid `ColumnValue` and is now refused with `22007` before the statement
  executes. The timestamp form (`2024-02-30 10:00:00`) is refused on both paths
  too.

  `22007` ("invalid datetime format"), not `22018`, on the module's existing
  rule: the text is a recognised literal whose field is out of range, which is
  what separates it from text that is no datetime at all.

- **Fetching a numeric column into a character buffer too small for its whole
  digits is now an error with no data, where it was a warning with truncated
  data.** Applications will notice this sharply: a call that returned
  `SQL_SUCCESS_WITH_INFO` with `01004` and a value in the buffer now returns
  `SQL_ERROR` with `22003` and **nothing written — neither the data nor the
  length indicator**.

  **Five entry points reach it**, which is every caller of the shared
  marshalling routine: `SQLFetch` and `SQLFetchScroll` (bound columns),
  `SQLGetData`, and — through output parameters — `SQLExecDirect` and
  `SQLExecute`.

  `ColumnValue::I64(123456)` read into a four-byte `SQL_C_CHAR` buffer
  delivered `"123"`. That is not a truncated string, it is a different number,
  and [SQL to C: Numeric] separates the two cases in as many words. Its
  `SQL_C_CHAR` and `SQL_C_WCHAR` rows read:

  | Test | \**TargetValuePtr* | \**StrLen_or_IndPtr* | SQLSTATE |
  |---|---|---|---|
  | Character byte length < *BufferLength* | Data | Length of data in bytes | n/a |
  | Number of whole (as opposed to fractional) digits < *BufferLength* | Truncated data | Length of data in bytes | `01004` |
  | Number of whole (as opposed to fractional) digits >= *BufferLength* | Undefined | Undefined | `22003` |

  Core implemented the middle row for every case and never the last one.
  Losing only *fractional* digits is unchanged: `1.25` into four bytes still
  delivers `"1.2"` with `01004`.

  **Affected sources** are the nine numeric SQL types the table names, which
  reach core as seven `ColumnValue` variants: `I8` (`SQL_TINYINT`), `I16`
  (`SQL_SMALLINT`), `I32` (`SQL_INTEGER`), `I64` (`SQL_BIGINT`), `F32`
  (`SQL_REAL`), `F64` (`SQL_FLOAT`/`SQL_DOUBLE`) and `Decimal`
  (`SQL_DECIMAL`/`SQL_NUMERIC`). **Affected targets** are `SQL_C_CHAR` and
  `SQL_C_WCHAR`, and those only.

  **Core is now stricter here than the two most widely deployed open-source
  drivers.** Neither psqlODBC nor MySQL Connector/ODBC implements this row:
  both return `01004` with truncated data, write the full length, and let the
  application keep reading in chunks. This entry is not "core caught up" — it is
  core diverging, deliberately, because the table is unambiguous and a truncated
  number is a wrong number. Applications validated only against those two
  drivers are the ones most likely to notice.

  **A numeric output parameter fails the whole execution rather than
  truncating.** `write_output_params` shares the same marshalling routine and
  propagates its error with `?`, where the old `SQL_SUCCESS_WITH_INFO` was
  deliberately discarded — that path has no diagnostic queue to raise `01004`
  on. So a numeric output parameter bound to `SQL_C_CHAR` with a buffer too
  small for its whole digits now fails `SQLExecDirect` or `SQLExecute` outright,
  where it previously truncated silently. No legitimate no-buffer call is
  affected: the loop already skips unbound records, so the data pointer is
  non-null, and a zero `SQL_DESC_OCTET_LENGTH` takes the no-buffer exemption
  below. `SQLParamData` is **not** affected — it completes a data-at-execution
  sequence and executes, but does not write output parameters back at all. In
  practice no in-tree backend produces output parameters yet, so this clause is
  forward-looking.

  **A long numeric rendering can no longer be retrieved in parts.** A
  `DECIMAL(38,0)` renders to 39 characters; reading it through a 32-byte
  `SQL_C_CHAR` buffer is now `22003` on the first call, where both reference
  drivers deliver it in chunks. That follows from the row — the check is a
  property of the value, not of the not-yet-delivered remainder — and is
  spec-defensible, since the numeric types are absent from `SQLGetData`'s
  "Retrieving Variable-Length Data in Parts" list. The column is **not**
  consumed: `SQLGetData` does not advance its cursor on the error path, so the
  same column reads normally once the application supplies a buffer that fits.

  **Nothing else moves.** A genuine character column that does not fit is still
  `01004` with truncated data, because [SQL to C: Character] has no `22003` row
  at all. The other fifteen `ColumnValue` variants are unchanged: `Bool`
  (`SQL_BIT`), the four datetime variants (`Date`, `Time`, `Timestamp`,
  `TimestampTz`), `String`, `Json`, `Bytes`, `Guid`, `Array`, `Map`, `Row`, the
  two interval variants (`IntervalYearMonth`, `IntervalDayTime`) and `Null`,
  which is answered with `SQL_NULL_DATA` before any of this runs.

  Two rulings worth knowing, both pinned by tests:

  - **A minus sign counts as a whole-digit position.** The table says "digits",
    and a sign is not one, but the boundary it draws is exactly "the whole part
    plus the null terminator must fit" and a sign occupies a byte just as a
    digit does. Reading it out would deliver `"-12"` for `-123.45` in a
    four-byte buffer — the wrong number, which is what the row exists to
    prevent.
  - **A call that supplies no buffer is exempt**, and keeps its previous
    behaviour exactly. That covers the zero-length length probe
    (`BufferLength` 0 with a real pointer, the documented "how much room do I
    need" call) and the indicator-only binding `SQLBindCol` permits ("An
    application can unbind the data buffer for a column but still have a
    length/indicator buffer bound for the column"). The row exists to stop a
    wrong number reaching the application's buffer; where there is no buffer,
    that cannot happen. Both reference drivers special-case the probe the same
    way, and `SQLGetData`'s own prose protects it by returning `HY090` when
    `BufferLength` is less than 0 *but not* when it is 0. A `BufferLength` of 1
    on `SQL_C_CHAR` is **not** exempt — there is a buffer there, and delivering
    `""` for a number is the wrong number.

  **Still not implemented, and unchanged by this entry:** the `22003` rows on
  the sibling conversion tables, which are keyed to a fixed minimum buffer width
  rather than to a digit count — `SQL to C: Bit`'s "*BufferLength* <= 1" and the
  date, time and timestamp pages' minimum widths ("*BufferLength* < 20" for a
  timestamp). Core returns `01004` for all of those. Two further numeric gaps
  were also open when this entry was written and are **not** addressed here: an
  `f64` overflowing an `SQL_C_FLOAT` target reported `01S07` where the same table
  says `22003` — closed since, by the two float entries above — and
  `SQL_C_NUMERIC` has no conversion arm at all, which is still open.

- **A backend reporting a column count that does not fit `u16` no longer makes
  `SQLDescribeColW` and `SQLColAttributeW` reject every column, including
  valid ones.** Both range-check `column_number` against
  `StatementBackend::column_count` before calling the backend, and narrowed
  that count with `u16::try_from(column_count).unwrap_or(0)`. Because
  `column_count` returns `i16` — the `SQLNumResultCols` ABI type, whose max
  (32 767) already fits `u16::MAX` (65 535) — the only value that can fail
  that conversion is a **negative** count from a misbehaving backend, not an
  oversized one. Collapsing a failed conversion to 0 turned "this count can't
  be trusted" into "column 1 is out of range", which is the wrong direction:
  every column number then failed a check meant to catch only a column past
  the end. The narrowing now saturates up to `u16::MAX` instead (with a
  `tracing::warn!`), so an unrepresentable count makes the range check
  permissive rather than universally hostile, and `describe_col`'s own answer
  — not a manufactured `07009` — is what the application sees.

[C to SQL: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/c-to-sql-character
[SQL to C: Time]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-time
[SQL to C: Numeric]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-numeric
[SQL to C: Character]: https://learn.microsoft.com/en-us/sql/odbc/reference/appendixes/sql-to-c-character
[Unreleased]: https://github.com/stackabletech/stackable-odbc-core/commits/HEAD
