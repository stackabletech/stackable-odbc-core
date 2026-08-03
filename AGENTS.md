# Agent Guide

Implementation details for AI agents working on `stackable-odbc-core`.

`stackable-odbc-core` is the database-independent framework that concrete ODBC
driver crates build on. It contains **zero** database-specific code: a driver
implements the `Backend` and `StatementBackend` traits and calls the
`forward_ffi!` macro to export the C ABI. This guide therefore describes the
framework itself and, where relevant, how a downstream driver crate consumes it.

## Quick Reference

| Topic | When to Read |
|-------|-------------|
| [Adding a new ODBC function](#adding-a-new-odbc-function) | Implementing or moving a function from stubs |
| [odbc-sys usage](#odbc-sys-usage) | Using ODBC types, enums, or constants |
| [Converting raw values](#converting-raw-values-to-strongly-typed-enums) | Handling raw integers from the C ABI |
| [Adding a new driver](#adding-a-new-driver) | Creating a new backend crate on top of core |
| [Testing](#testing) | Writing tests, or running Miri / fuzz |
| [Descriptors](#descriptors) | Touching a binding, a descriptor field, the `HY021` consistency check, or a statement attribute that is a header field |
| [Concurrency: the lock discipline](#concurrency-the-lock-discipline) | Understanding the per-connection lock, `HandleScope`, `SQLCancel`'s exemption, or loom |
| [Architecture](#architecture) | Understanding call flow or crate layout |

## Conventions

- Edition 2024, resolver 3, Rust 1.95.0
- `snafu` for errors (the `unwrap_used`, `unwrap_in_result` and `panic` clippy
  lints are denied outside tests)
- `tracing` for logging (not `println!` or `log`)
- `#[repr(C)]` on all handle structs, for a defined, non-reordered layout on a
  type that is heap-allocated via `Box::into_raw` and later reclaimed via
  `Box::from_raw` at that same raw address. Handle validation never
  dereferences these structs at all — it is a slot index and generation
  compare against the registry — so no field's offset, including
  `HandleHeader`'s, is load-bearing for that.
- `extern "system"` on all FFI exports (resolves to correct ABI on both Windows and Linux)
- `odbc-sys` links against `libodbc`/`libodbcinst`, so building or testing needs
  the unixODBC dev libraries installed (`unixodbc-dev` on Debian/Ubuntu). No DSN
  or running Driver Manager is required. Miri is the exception — it interprets
  rather than links, so it needs no system libraries.
- **`#[cfg(windows)]` code is compilable from Linux, and should be compiled
  before it is pushed.** A plain `cargo check` does not look at it at all, so
  `ffi/setup.rs` and `ConfigDSNW` can be edited into a state that builds and
  tests clean locally and fails on the Windows runner:

  ```bash
  rustup target add x86_64-pc-windows-msvc          # once
  cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings
  ```

  This links nothing and needs no Windows host — `raw-dylib` resolves
  `odbccp32` at link time, which a `check`/`clippy` run never reaches. It is not
  a substitute for *running* the code, which only a Windows host with a Driver
  Manager can do; it closes the compile-and-lint half, which is where the
  regressions actually are.

- **`bench/` and `fuzz/` are separate Cargo workspaces, so nothing at the repo
  root compiles them.** Not `cargo test`, not `cargo clippy --all-targets`, and
  not a single `pre-commit` hook. `bench/benches/handle_lookup.rs` contains a
  full `impl Backend`, so **any change to the `Backend` or `StatementBackend`
  trait breaks it silently** — every local check passes and CI's "Compile
  benchmarks" step fails. That has already happened once, when the catalog hooks
  moved to query types. After touching either trait:

  ```bash
  (cd bench && cargo build --benches)
  (cd fuzz && cargo +nightly build --target x86_64-unknown-linux-gnu)
  ```

  The generalisable form: `pre-commit run --all-files` is the source of truth
  for everything *in the root workspace*, and these two directories are outside
  it by design (see the Benchmarks and Fuzzing sections for why). A detached
  workspace is invisible to exactly the checks you would expect to catch it.

### Changelog

This project keeps a [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
`CHANGELOG.md` and follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Every user-facing change (public API, behaviour, spec-compliance fixes) gets an
entry under the `## [Unreleased]` heading in the appropriate
`Added` / `Changed` / `Fixed` / `Removed` group. Because core is a published
library consumed by driver crates, treat any change to a public type, trait
method, or exported FFI contract as user-facing.

### Logging in FFI functions

Every `pub unsafe fn` in `ffi/` must follow this structure:

```rust
// 1. If no parsing: single debug! at entry
tracing::debug!("SQLFunctionW(handle={:?}, param={})", handle, param);

// OR if parsing anything — raw integers to enums, or UTF-16 pointers to strings:
// 1a. TRACE: raw inputs before parse
tracing::trace!("SQLFunctionW(handle={:?}, raw={})", handle, raw_int);
// 1b. DEBUG: parsed/typed values after
tracing::debug!("SQLFunctionW: attr={:?}", parsed_attr);
// 1b. ...and for a function taking string arguments, name every one of them:
tracing::debug!(
    "SQLFunctionW(handle={:?}, catalog={:?}, schema={:?}, table={:?})",
    handle, catalog, schema, table,
);

// 2. WARN: intentional spec deviations (silent accepts, ignored features)
tracing::warn!("SQLFunctionW: accepting unrecognized X (DM compatibility)");

// 3. DEBUG: return value — always; requires let ret = unsafe { panic_safe(...) }; pattern
tracing::debug!("SQLFunctionW -> {:?}", ret);
```

Rules: no passwords or connection string content; `error!` only for validation failures expressed via `OdbcError` (avoid double-logging); stubs use a single `debug!` entry, no exit log.

**A string argument counts as parsed input.** The entry log knows only the
handle, so a function that logs just that shows *which* call happened and not
what it asked for — precisely what you need when a client's metadata query comes
back empty. Every catalog function therefore logs its `parse_filter_param`
results. This is the rule most easily missed when a stub becomes a real
implementation, because the stub's single `debug!` looks like it already
complies.

### Named constants

ODBC attribute values, function IDs, and bitmap constants must use named `const` definitions. Never write raw integer literals for ODBC-spec-defined values. Name them after the ODBC spec name (e.g., `SQL_AUTOCOMMIT_ON`, `SQL_CUR_USE_DRIVER`, `SQL2_FREE_CONNECT`).

**This applies to tests too.** Test code is where raw literals creep back in most
easily, usually with the spec name relegated to a trailing comment. A comment is
not a constant:

```rust
// BAD — the value is unchecked and the name is only a comment
sql_bind_parameter::<B>(stmt, 1, 1 /* SQL_PARAM_INPUT */, ..., -5 /* SQL_BIGINT */, ...);

// GOOD — the compiler validates both
sql_bind_parameter::<B>(stmt, 1, ParamType::Input as i16, ..., SqlDataType::EXT_BIG_INT.0, ...);
```

Prefer the `odbc-sys` type over defining a new constant when one exists — most
spec values are already modelled:

| Value | Use |
|-------|-----|
| `SQL_PARAM_INPUT`, `SQL_PARAM_OUTPUT`, … | `ParamType::Input as i16` |
| `SQL_BIGINT`, `SQL_VARCHAR`, `SQL_INTEGER`, … | `SqlDataType::EXT_BIG_INT.0` (note the `.0`) |
| `SQL_C_SBIGINT`, `SQL_C_WCHAR`, … | `CDataType::SBigInt as i16` |
| `SQL_ATTR_*` | `StatementAttribute::*` / `ConnectionAttribute::*` |
| `SQL_HANDLE_*` | `HandleType::*` |

All are re-exported from `stackable_odbc_core::types`. Only define a new `const` in
`types/constants.rs` when `odbc-sys` genuinely lacks the value. Ordinals that
are not spec constants (a parameter number, a column index) are fine as
literals.

### Type cast safety

Use `T::try_from(x)` over bare `as T` when truncation is possible. For ODBC output parameters typed `*mut i16` (column counts, parameter counts): use `i16::try_from(n).unwrap_or_else(|_| { tracing::warn!(...); i16::MAX })`.

### Backend error mapping

Core never talks to a database; it only defines the trait boundary. A driver,
however, must route **every** error from its client library through a single
central mapping function — never hand-build an `OdbcError` at the call site. That
function is the one place that decides the SQLSTATE; bypassing it silently
degrades specific codes to `HY000`.

Hand-built errors are correct only for *internal* invariant violations that
never came from the client (e.g. "get_data called before fetch", a missing
runtime handle, a poisoned mutex), and for connection-setup failures where the
call-site context is more useful than a mapped variant.

### 08001 versus 08S01

`08001` ("client unable to establish connection") is only valid from the
connection functions. Once a connection exists, a failing link is `08S01`
("communication link failure") — that is the code the diagnostics tables of
`SQLExecute`, `SQLFetch`, `SQLGetInfo` and the rest actually list. A driver
whose `connect` performs no network I/O will only ever see post-connection
failures, and should map them to `08S01`; a driver that opens a real connection
in `connect` is where `08001` legitimately originates.

### A SQLSTATE only the data source can determine

Some states are the driver's to return by the spec's `(DM)` rules, yet core
cannot produce them, because the fact they assert lives at the data source.
`3D000` ("invalid catalog name") is the type case: `SQLSetConnectAttr`'s row
carries no `(DM)` marker, but only the data source knows which catalogs exist,
and the attribute's description has the driver *send* something to find out
("the driver sends a **USE** *database* statement"). Core's part is threefold —
name the state (`SqlState::invalid_catalog_name`), call the hook, and propagate
what it returns unchanged. A backend that maps "no such catalog" to a generic
`HY000` is the only reason an application would not see `3D000`.

Two consequences generalise to any hook of this shape:

- **A "not returned by this driver" doc line is a claim about the whole path,
  not about core's own code.** `3D000` was recorded that way while core was
  already propagating it, because whoever wrote the line was looking at core and
  the state comes from the backend.
- **A pending connection attribute moves the SQLSTATE to a different
  function.** `SQL_ATTR_CURRENT_CATALOG` and `SQL_ATTR_ACCESS_MODE` are settable
  either side of a connection, and the spec says interoperable applications set
  them *before* — so core applies them during `SQLDriverConnectW` and a hook
  failure surfaces there, carrying a state that function's own diagnostics table
  may not list (`3D000`, or `HYC00` from an unimplemented hook). Propagate it
  rather than degrading it; a connection that failed because the catalog does
  not exist should say so.

### Prompting the user: core decides whether, the driver decides how

A driver needing interactive authentication — an OAuth 2.0 external flow, say —
implements `prompt::Prompter` and returns it from the defaulted
`Backend::prompter`. It reads it back inside its own `connect`, from
`ConnectParams::prompter()`, and **never** by calling `Backend::prompter`
directly: that method is ungated and says what the driver *could* do, not what
this call is allowed to do.

The gate is `SQLDriverConnect`'s *DriverCompletion*, and it lives in exactly one
function, `prompter_for` in `ffi/connect.rs`. Three points that are easy to get
wrong in the other direction:

- **A withheld prompter is `None`, not an error.** Under `SQL_DRIVER_NOPROMPT`
  the backend is simply handed nothing to call, so the spec's "do not prompt"
  cannot be forgotten at a call site. A backend that finds `None` and needs a
  prompt fails the connect the way the spec's own `SQL_DRIVER_NOPROMPT` clause
  says: "otherwise, the driver returns SQL_ERROR."
- **`SQLConnect` and `SQLBrowseConnect` have no such argument, and absence
  permits prompting.** `SQLConnect` is the DSN path — `isql` and Excel — so
  those are the likeliest interactive callers of the whole driver. Reading the
  missing argument as `SQL_DRIVER_NOPROMPT` would lock DSN connections out of
  interactive authentication, and no spec text asks for it.
- **An unrecognised value is accepted.** `HY110` carries `(DM)` on *both* of its
  clauses, so core adds no check, and the fallback is the most permissive
  treatment rather than a driver-side error borrowed from a Driver-Manager row.

Core ships no `Prompter` implementation and must not gain a dependency for one:
every implementation it could offer needs a platform (a browser, a window
system) that the database-independent half of a driver has no business
choosing. A Windows dialog implementation would belong here, next to
`SQLDriverConnectW`, but it needs its own design and its own dependency; the
trait is shaped so it can arrive later without changing the backend-facing API.

## Adding a new ODBC function

1. **Read the ODBC spec first.** Every function has a spec page at `https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/<function-name>-function?view=sql-server-ver17` (e.g. `sqlallochandle-function`). Read it in detail before writing any code.
2. **Implement every check and constraint** from the spec. This includes:
   - All parameter validation (null checks, valid handle types, valid attribute values)
   - All required error returns and SQLSTATEs listed in the spec's "Diagnostics" table
   - All state transition rules (e.g. "cannot call X before Y")
   - Setting output parameters to defined values on error (e.g. `*OutputHandlePtr = SQL_NULL_HANDLE`)
   - If a spec requirement cannot be implemented, leave a `// TODO(spec):` comment explaining why, and flag it to the user
3. **Reference the spec URL** in the doc comment for the generic function in `src/ffi/`:

   ```rust
   /// Generic implementation of SQLAllocHandle.
   ///
   /// Spec: <https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqlallochandle-function>
   ```

   The doc comment's SQLSTATE list is checked against the spec's own
   Diagnostics table by `every_doc_comment_matches_the_spec_diagnostics_table`
   (`src/types/diagnostics_table.rs`), so a new function needs its table
   transcribed there before it will build. That module's docs give the four
   verdict phrasings the guard recognises, and state the one thing it does not
   check: whether the *reason* a row is not returned is true.

4. **Implement the generic function** in `src/ffi/` (in the appropriate module)
5. **Add a `Backend` (or `StatementBackend`) trait method** if the function needs database-specific logic. Prefer a defaulted method so existing drivers keep compiling.
6. **Each driver implements the new trait method** in its own backend.
7. Add one entry to the `forward_ffi!` macro in `src/forward_ffi.rs` — all drivers pick it up automatically.

## odbc-sys usage

`odbc-sys` is a minimal `-sys` crate for ODBC type definitions. It deliberately has no convenience methods (see [PR #47](https://github.com/pacman82/odbc-sys/pull/47) for rationale). `stackable-odbc-core` is the driver-side convenience layer on top of it.

- **Always use `odbc-sys` types** where they exist: `HandleType`, `SqlReturn`, `CDataType`, `SqlDataType`, `InfoType`, `Desc`, `FreeStmtOption`, `AttrOdbcVersion`, `EnvironmentAttribute`, `Len`, `Pointer`, `WChar`, etc. For primitive parameters where odbc-sys 0.29 removed the type aliases (e.g. the old `SmallInt`), use the Rust primitives directly (`i16`, `u16`, `i32`).
- **Never redefine** enums, structs, or constants that `odbc-sys` already provides. Before defining a new constant or enum, check `odbc-sys` first. If it's there, use it.
- **Add driver-side extensions** in `stackable-odbc-core` — since orphan rules prevent `impl TryFrom<i16> for odbc_sys::HandleType`, use standalone conversion functions like `fn handle_type_from_raw(v: i16) -> Option<HandleType>`
- **Keep our own types** only for things `odbc-sys` doesn't have: `ConnectParams`, `ColumnValue`, `ColumnDescriptor`, `FetchResult`, `InfoValue`, `SqlState`, `DiagnosticQueue`, `TypeInfoRow`, `OdbcError`
- **ODBC function IDs** (`SQL_API_*` values) are NOT in `odbc-sys`. They live in `src/function_id.rs` as the `FunctionId` enum (sourced from `/usr/include/sql.h` and `sqlext.h`). Always use `FunctionId::ExecDirect` etc. — never raw numeric IDs. Convert with `function_id_from_raw(u16) -> Option<FunctionId>`.

## Converting raw values to strongly typed enums

Raw integers from the ODBC C ABI must be converted to strongly typed Rust enums **as early as possible** — at the FFI boundary, before any logic runs.

```rust
// GOOD: fallible conversion, handles unknown values gracefully
let field = desc_from_raw(field_identifier).ok_or_else(|| {
    OdbcError::general(
        format!("Unknown descriptor field: {field_identifier}"),
        SqlState::optional_feature_not_implemented(),
    )
})?;
tracing::debug!("SQLColAttributeW(col={}, field={:?})", col, field);

// BAD: transmute on arbitrary u16 is UB if the value isn't a valid enum variant
let field: Desc = std::mem::transmute(field_identifier); // DON'T DO THIS

// BAD: passing raw u16 through multiple layers before converting
fn do_work(field_id: u16) { ... } // loses type safety and readable logging
```

Available conversion functions (all in `src/types/conversions.rs` unless noted):

- `handle_type_from_raw(i16) -> Option<HandleType>`
- `desc_from_raw(u16) -> Option<Desc>`
- `info_type_from_raw(u16) -> Option<InfoType>`
- `c_data_type_from_raw(i16) -> Option<CDataType>`
- `param_type_from_raw(i16) -> Option<ParamType>`
- `environment_attribute_from_raw(i32) -> Option<EnvironmentAttribute>`
- `attr_odbc_version_from_raw(i32) -> Option<AttrOdbcVersion>`
- `free_stmt_option_from_raw(u16) -> Option<FreeStmtOption>`
- `statement_attribute_from_raw(i32) -> Option<StatementAttribute>`
- `completion_type_from_raw(i16) -> Option<CompletionType>`
- `fetch_orientation_from_raw(i16) -> Option<FetchOrientation>`
- `identifier_type_from_raw(u16) -> Option<IdentifierType>`
- `nullable_from_raw(u16) -> Option<Nullable>`
- `scope_from_raw(u16) -> Option<Scope>`
- `bulk_operation_from_raw(i16) -> Option<BulkOperation>`
- `interval_from_raw(i16) -> Option<Interval>`
- `declared_odbc_version_from_raw(i32) -> Option<DeclaredOdbcVersion>`
- `driver_connect_option_from_raw(u16) -> Option<DriverConnectOption>`
- `function_id_from_raw(u16) -> Option<FunctionId>` (in `function_id.rs`)

If `odbc-sys` adds a new enum that we need to convert from raw values, add a `xxx_from_raw` function following the same pattern. Do not use `transmute`.

`SQLSetPos`'s `Operation` and `LockType` are the one documented exception, and
the reason generalises: an `odbc-sys` type is only usable here if the raw ABI
value can be recovered from it. `odbc_sys::Operation` and `odbc_sys::Lock` are
newtype structs over a **private** `i16` — no accessor, no `From`, and not a
`#[repr]` enum to cast through — so a converted value can be compared against
their associated constants and used for nothing else, and no caller or test can
name a valid input. Those two validate against `SQL_POSITION` / `SQL_LOCK_*` in
`types/constants.rs`, which is the exception that block's comment records.
Before adding a conversion, check that the target type can round-trip; if it
cannot, a named constant is the correct answer, not a worse conversion.

## Adding a new driver

1. Create a new crate that depends on `stackable-odbc-core`.
2. Implement `Backend` + `StatementBackend` for your backend type.
3. In `lib.rs`, invoke `stackable_odbc_core::forward_ffi!(crate::backend::YourBackend);` — no `ffi.rs` needed.

### The catalog functions: core owns the result set

The ten catalog `Backend` methods — `tables`, `columns`, `primary_keys`,
`foreign_keys`, `statistics`, `special_columns`, `procedures`,
`procedure_columns`, `column_privileges`, `table_privileges` — return **typed
row structs** (`TableRow`, `ColumnRow`, …), not a `Self::Statement`. Five
consequences for a driver author:

- **Return the rows in any order.** Core sorts each result set into the order
  its spec page mandates (`SQLTables` by `TABLE_TYPE, TABLE_CAT, TABLE_SCHEM,
  TABLE_NAME`, and so on), with NULL placement from `Backend::null_collation`.
  A driver needs no `ORDER BY` for ODBC compliance, and one added purely for it
  can be deleted.
- **Core owns the column layout.** A backend fills named fields, so it cannot
  get column order or count wrong, and a column added to a spec result set is a
  core-only change. That last part is what `#[non_exhaustive]` on all ten row
  types buys. Since that rules out a struct expression outside core — including
  `..Default::default()`, which Rust rejects cross-crate with `E0639` — each
  type carries one consuming setter per column, generated from the same field
  list by the `catalog_rows!` macro:

  ```rust
  let row = TableRow::default()
      .catalog(catalog)          // Option<String> column takes a bare String
      .name(name)
      .table_type("TABLE");      // String column takes a &str
  ```

  Setters take `impl Into<T>` and are named after their field, so adding a
  column adds a setter and breaks nothing. There is deliberately no positional
  `new(...)`: `ColumnRow` has eighteen columns and `ProcedureColumnRow`
  nineteen, so an argument list would reintroduce the ordering mistake named
  fields exist to prevent.
- **The `SQL_ALL_*` enumerations never reach these methods.** Core serves
  `SQL_ALL_CATALOGS`, `SQL_ALL_SCHEMAS` and `SQL_ALL_TABLE_TYPES` from
  `Backend::catalogs`, `Backend::schemas` and `Backend::table_types`, building
  the all-but-one-column-NULL rows itself. The first two are only called when
  `supports_catalogs`/`supports_schemas` already returned `true`.
- **`SQL_ATTR_METADATA_ID` is core's job.** When it is `SQL_TRUE`, core has
  already stripped delimiters, case-folded per `identifier_case` and escaped
  `%`/`_` per `search_pattern_escape` before calling the backend, so these
  methods always see ordinary pattern values. `SQLTables`' `TableType` is the
  one exemption in the family — the spec makes it a value list under both
  settings — and even that no longer reaches a driver as a raw string: core
  parses it and `tables` reads it back from `query.table_types()`.
- **The arguments arrive as a typed query object.** Each hook takes a single
  `&XxxQuery<'_>` (`TablesQuery`, `ForeignKeysQuery`, and so on) instead of five
  to eight positional arguments, read through accessors:

  ```rust
  fn tables(
      conn: &Self::Connection,
      cancel: &Self::CancelToken,
      query: &TablesQuery<'_>,
  ) -> Result<Vec<TableRow>, Self::Error> {
      let _ = (query.catalog(), query.schema(), query.table(), query.table_types());
      todo!()
  }
  ```

  These are sealed exactly as the row types are, so an argument added to a
  catalog hook is a source-compatible change for every driver. The second reason
  is `SQLForeignKeys`, which took six consecutive `Option<&str>`: crossing a
  primary-key argument with its foreign-key counterpart compiled without
  complaint, and `query.pk_table()` beside `query.fk_table()` cannot.

  Eight are built from `Default` plus `with_*` setters. The other two take the
  arguments that have no honest default through `new` instead:
  `StatisticsQuery::new(unique_only)`, because `false` means `SQL_INDEX_ALL`
  rather than "unspecified", and
  `SpecialColumnsQuery::new(identifier_type, scope, nullable)`, because no
  `Scope` or `IdentifierType` value is a defensible default and core does not
  invent one.

The last four — `procedures`, `procedure_columns`, `column_privileges`,
`table_privileges` — are **defaulted to `Ok(Vec::new())`**, not to
`NotImplemented` like `primary_keys` and its neighbours. Their FFI functions
returned an empty result set for every driver before the hooks existed, and a
data source with no stored procedures or no privilege metadata genuinely has
none to report, so erroring would turn a working call into a failure. An
existing driver is unaffected until it opts in by overriding one.

Their `HY009` handling is **not** uniform, and the difference is deliberate.
All four return it for the spec's `SQL_ATTR_METADATA_ID` + null-`CatalogName` +
catalogs-supported clause, which every one of the four pages states without a
`(DM)` marker. Only `SQLColumnPrivileges` additionally rejects a null
`TableName` unconditionally: it is the only one of the four whose page carries
that sentence unmarked. `SQLTablePrivileges`, `SQLProcedures` and
`SQLProcedureColumns` must **not** check it. This mirrors the split among the
first six, where `SQLStatistics` and `SQLSpecialColumns` check a null
`TableName` and `SQLPrimaryKeys` and `SQLForeignKeys` do not. Tests pin both
directions; do not "fix" any of it into consistency.

### Capability methods are required, not defaulted

Most of `Backend` is defaulted, so a driver implements only what it needs.
These deliberately are not:

| Method | States |
|--------|--------|
| `supports_catalogs` | whether the data source has ODBC catalogs |
| `supports_schemas` | whether it has ODBC schemas |
| `alter_table_support` | the `SQL_ALTER_TABLE` `SQL_AT_*` bitmask |
| `outer_join_capabilities` | the `SQL_OJ_CAPABILITIES` `SQL_OJ_*` bitmask |
| `default_txn_isolation` | `SQL_DEFAULT_TXN_ISOLATION` (`0` = no transactions) |
| `txn_isolation_options` | `SQL_TXN_ISOLATION_OPTION` (`0` = no transactions) |
| `group_by` | `SQL_GROUP_BY` (`0` = `GROUP BY` not supported) |
| `null_collation` | `SQL_NULL_COLLATION` (`0` = `SQL_NC_HIGH`) |
| `correlation_name` | `SQL_CORRELATION_NAME` (`0` = `SQL_CN_NONE`) |
| `non_nullable_columns` | `SQL_NON_NULLABLE_COLUMNS` (`0` = `SQL_NNC_NULL`) |
| `expressions_in_order_by` | `SQL_EXPRESSIONS_IN_ORDERBY` |
| `identifier_case` | `SQL_IDENTIFIER_CASE` (`SQL_IC_*`); `0` is not a legal value |
| `quoted_identifier_case` | `SQL_QUOTED_IDENTIFIER_CASE` (`SQL_IC_*`); independent of the unquoted rule |
| `txn_capable` | `SQL_TXN_CAPABLE` (`SQL_TC_*`); `0` = `SQL_TC_NONE`, contradicting any declared isolation level |
| `integrity` | `SQL_INTEGRITY` — whether the *data source* has the Integrity Enhancement Facility |
| `multiple_active_txn` | `SQL_MULTIPLE_ACTIVE_TXN` — whether two transactions can be live at once |
| `special_characters` | `SQL_SPECIAL_CHARACTERS`; an empty list is an answer, as with `keywords` |
| `accessible_procedures` | `SQL_ACCESSIBLE_PROCEDURES`, the counterpart of `accessible_tables` |
| `driver_name` / `driver_version` | `SQL_DRIVER_NAME` / `SQL_DRIVER_VER`; answered before a connection exists |
| `dbms_name` / `dbms_version` | `SQL_DBMS_NAME` / `SQL_DBMS_VER` — what this connection reached |
| `sql_conformance` | `SQL_SQL_CONFORMANCE` (`SQL_SC_*`) |
| `timedate_add_intervals` | `SQL_TIMEDATE_ADD_INTERVALS` (`SQL_FN_TSI_*`) |
| `timedate_diff_intervals` | `SQL_TIMEDATE_DIFF_INTERVALS` (`SQL_FN_TSI_*`) |
| `subqueries` | `SQL_SUBQUERIES` (`SQL_SQ_*`) |
| `column_alias` | `SQL_COLUMN_ALIAS` |
| `concat_null_behavior` | `SQL_CONCAT_NULL_BEHAVIOR` (`0` = `SQL_CB_NULL`) |
| `union_support` | `SQL_UNION` (`SQL_U_*`) |
| `convert_functions` | `SQL_CONVERT_FUNCTIONS` (`SQL_FN_CVT_*`) |
| `order_by_columns_in_select` | `SQL_ORDER_BY_COLUMNS_IN_SELECT` |
| `accessible_tables` | `SQL_ACCESSIBLE_TABLES` |
| `data_source_read_only` | `SQL_DATA_SOURCE_READ_ONLY` |
| `search_pattern_escape` | `SQL_SEARCH_PATTERN_ESCAPE` |
| `keywords` | the data source's own reserved words, *before* ODBC's are subtracted (`SQL_KEYWORDS`) |
| `table_types` | the data source's table types, for `SQLTables`' `SQL_ALL_TABLE_TYPES` enumeration |

Each states a **capability**, where any default is a claim the backend author
never made. `0` understates ("this data source cannot do this at all") and
`true` overstates, and a backend author is unlikely to notice a capability they
never wrote code for — so the compiler asks instead of core guessing. Any value
core invents for these is wrong for some real driver, and wrong silently: the
backend author never sees the question, and the application never sees anything
but a confident answer.

#### Attributes that reduce load at the data source are never emulated

`SQL_ATTR_QUERY_TIMEOUT`, `SQL_ATTR_MAX_ROWS` and `SQL_ATTR_MAX_LENGTH` share
one shape, in `offer_to_data_source` (`ffi/stmt_attr.rs`): offer the value to a
defaulted `Backend` hook, store it if the backend accepts, substitute the
spec's default with `01S02` if the hook is unimplemented, and propagate any
*other* error as-is. Core emulates none of them, and the spec is explicit about
why for two of the three — "a driver should not emulate SQL_ATTR_MAX_ROWS
behavior", and `SQL_ATTR_MAX_LENGTH` "should be supported only when the data
source (as opposed to the driver) ... can implement it". Each row states the
purpose that makes emulation pointless: "this attribute is intended to reduce
network traffic". Counting rows or bytes in the driver, after they have crossed
the wire, achieves nothing the application asked for.

`SQL_ATTR_QUERY_TIMEOUT` is the one with a core-side fallback, and it is
opt-in rather than automatic: `Backend::set_query_timeout` returns a
`QueryTimeout`, and only `CoreCancels` arms core's timer. Core cannot infer
that — every statement-producing `Backend` method is synchronous and blocks the
calling thread, so `Backend::cancel` is the only lever, and whether a backend
wired it up is not observable from Rust.

**The timer is armed at `SQLFetch` too, not only at the statement-producing
calls**, and the reason generalises to any driver whose data source streams.
`SQL_ATTR_QUERY_TIMEOUT` bounds *returning the result set*, and a data source is
free to answer with column metadata long before it has computed a row — Trino
does. Measured against a live coordinator under a two-second deadline, the
`SQLExecDirect` returned `SQL_SUCCESS` in 0.1 s and the following `SQLFetch`
took 24.6 s, so an execute-only timer expired on nothing and bounded nothing.
`SQLFetch` and `SQLFetchScroll` both carry `HYT00` with **no `(DM)` marker**,
naming this attribute directly.

`SQLGetData` is the boundary, and it is drawn by the spec rather than by
convenience: its diagnostics table carries `HYT01` and **no `HYT00` row at
all**, so it is deliberately unarmed. The bound-column reads that run *inside*
`SQLFetch` are a different thing and do fall under that call's deadline.
`SQLFetchScroll` needs no site of its own — every orientation but
`SQL_FETCH_NEXT` is rejected with `HY106`, and that one delegates to
`sql_fetch`. Before arming a further site, check the function's own table for
an `HYT00` row.

Before adding a fourth attribute of this kind, check the spec row for a stated
*purpose*. If the purpose is to reduce work at the data source, the answer is a
hook plus the `01S02` fallback, not an implementation in core.

#### Deciding whether a new info type belongs here

The test is one question: **is zero "unknown", or is zero an answer?**

- Zero means *unknown or no limit* → shared default in `default_get_info`.
  `SQL_MAX_ROW_SIZE`, `SQL_MAX_INDEX_SIZE`, `SQL_MAX_STATEMENT_LEN` and the
  `SQL_MAX_COLUMNS_IN_*` group are all of this kind: the spec explicitly
  defines `0` as "no specified limit or the limit is unknown", so a shared `0`
  asserts nothing.
- Zero is a *substantive claim* → required `Backend` method. Every enum in the
  table above has this shape. `SQL_NULL_COLLATION`'s zero is `SQL_NC_HIGH`,
  `SQL_CORRELATION_NAME`'s is `SQL_CN_NONE`, `SQL_NON_NULLABLE_COLUMNS`'s is
  `SQL_NNC_NULL` — each a specific, falsifiable statement about the data
  source that core has no way to know.

Two corollaries worth checking when adding an info type:

- **A Y/N string has no valid empty value.** The shape-aware fallback in
  `info_type_default_response` gives an unhandled `String`-shaped info type
  `""`, which is the right *shape* but is not in any Y/N value list. Such a
  type needs either a shared `"N"` arm in `default_get_info` or a hook.
- **An empty *list* is an answer too.** `SQL_KEYWORDS` reads as an empty
  string just like an unhandled `String`-shaped type, but it means "this data
  source reserves nothing beyond ODBC" — which applications act on when
  deciding what to quote. It is a `Backend::keywords` hook for that reason;
  core owns only the spec's subtraction of `ODBC_RESERVED_KEYWORDS`, which is
  the same for every backend.
- **Watch for info types that constrain each other.** `SQL_SQL_CONFORMANCE`
  fixes the value of `SQL_GROUP_BY`, `SQL_CORRELATION_NAME`,
  `SQL_NON_NULLABLE_COLUMNS`, `SQL_CONCAT_NULL_BEHAVIOR`, `SQL_SUBQUERIES`
  and `SQL_COLUMN_ALIAS` — the spec names what an entry-level driver returns
  for each of those six. `SQL_TIMEDATE_FUNCTIONS` claiming
  `SQL_FN_TD_TIMESTAMPADD` obliges `SQL_TIMEDATE_ADD_INTERVALS` to be
  non-zero. `SQL_CATALOG_NAME` drives the whole catalog group. Core supplying
  one side of such a pair while the backend supplies the other is how it ends
  up contradicting itself.
- **Prefer deriving over adding a hook when the fact is already declared.**
  `SQL_IDENTIFIER_QUOTE_CHAR` comes from `EscapeDialect::identifier_quotes`
  and `SQL_CURSOR_COMMIT_BEHAVIOR` from `Backend::cursor_commit_behavior`,
  because a second way to state the same fact is a second way to state it
  *differently*. Check whether an existing hook already answers the question
  before adding one.

#### The rule is enforced by a test, not by review

`default_get_info_answers_are_backend_derived_or_declared_core_facts`
(`src/backend.rs`) asks one question of every info type: **does the answer
move when the backend does?** It evaluates `default_get_info` for two mock
backends that share no capability declaration. An info type answering
identically for both is one core decided, and must appear in that test's
`CORE_FACTS` list together with the reason core is entitled to decide it —
a fact about core's own implementation (its fetch really is forward-only, its
`Backend` trait really is synchronous), a limit where the spec defines `0` as
"no limit or unknown", or driver-level identity with no per-backend answer.

So adding a hard-coded claim to `default_get_info` fails a test that names the
info type. If you find yourself adding an entry to `CORE_FACTS`, the reason
string is the test: if you cannot write one that is about *core* rather than
about the data source, the value belongs on a `Backend` method.

`supports_catalogs` and `supports_schemas` between them drive seven info types
(`SQL_CATALOG_NAME`, `SQL_CATALOG_TERM`, `SQL_CATALOG_NAME_SEPARATOR`,
`SQL_CATALOG_LOCATION`, `SQL_CATALOG_USAGE`, `SQL_SCHEMA_TERM`,
`SQL_SCHEMA_USAGE`), which the `SQLGetInfo` spec defines in terms of that one
fact. Note the asymmetry: core answers the whole group when the answer is
*no*, because the spec mandates the empty string or zero, but for
`SQL_CATALOG_LOCATION`, `SQL_CATALOG_USAGE` and `SQL_SCHEMA_USAGE` when the
answer is *yes* it returns `None` and leaves them to the backend rather than
inventing a value. A driver with catalogs still answers those three itself.

`Backend::set_txn_isolation` stays defaulted, and the default is only correct
for a data source with exactly one isolation level. A backend declaring more
than one bit in `txn_isolation_options` **must** override it, or
`SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` reports `NotImplemented` rather
than accepting a level it cannot apply.

### Windows Driver Manager compatibility checklist

The Windows DM is much stricter than unixODBC. These items are **required** for
a driver to work on Windows — omitting any one can cause silent crashes,
`IM001` errors, or blocked `SQLGetData` calls:

- **The pre-connect info group is core's job now, not a checklist item.** The
  Windows DM queries `SQL_DRIVER_ODBC_VER` (77) *before* `SQLDriverConnectW`,
  and on `SQL_ERROR` treats the driver as ODBC 2.x and blocks 3.x features like
  `SQL_C_SBIGINT`. Core answers the whole group without a connection —
  `SQL_DRIVER_NAME` and `SQL_DRIVER_VER` from the required `Backend::driver_name`
  and `Backend::driver_version`, and `SQL_DRIVER_ODBC_VER`,
  `SQL_ASYNC_DBC_FUNCTIONS` and `SQL_MAX_CONCURRENT_ACTIVITIES` from facts about
  itself. Declaring the two hooks is all a driver does; overriding
  `get_info_pre_connect` is only for a *further* info type it can answer before
  connecting, which is rare.

- **`get_functions`**: List **every** exported FFI function, not just
  query-related ones — and **nothing core does not export**. The Windows DM uses
  the 3.x bitmap (`func_id=999`) to build its dispatch table. Missing entries
  (e.g. `SetEnvAttr`, `GetStmtAttr`, `BindCol`) cause NULL function pointer
  crashes. Build the list from `CORE_EXPORTED_FUNCTIONS` and it cannot drift in
  either direction.

  The 2.x array (`func_id=0`) is a **different question with a different
  answer**, and this is the part most easily got wrong. It asks "can an ODBC 2.x
  application call this", so it reports the deprecated functions as supported
  even though core exports none of them — the Driver Manager's mapping is what
  makes that true. `stackable-odbc-core` derives those entries from their 3.x
  counterparts automatically. An entry there naming a `FunctionId` absent from
  `CORE_EXPORTED_FUNCTIONS` is correct and deliberate, not an oversight; psqlODBC
  ships the same combination (`pfExists[SQL_API_SQLERROR] = TRUE` beside a
  commented-out `;;SQLError` in its `.def`).

#### A 3.x driver does not export the deprecated 2.x functions

Appendix G, "Mapping Deprecated Functions": a 3.x driver "does not have to
implement the ODBC 2.x functions", and the mapping "is triggered when the driver
is an ODBC 3.x driver and **the driver does not support the function that is
being mapped**."

So exporting one does not *add* a capability — it **removes the Driver
Manager's**, which is usually better informed. unixODBC's `SQLSetScrollOptions`
mapping is 572 lines that check the requested concurrency against the driver's
own `SQLGetInfo` answers before setting anything; core's export was a bare
`SQL_ERROR` that replaced all of it. `SQLError`'s mapping routes to
`SQLGetDiagRec`, which core implements properly; core's export answered
`SQL_NO_DATA` and an ODBC 2.x application saw no diagnostics at all.

Core therefore exports none of Appendix G's seventeen, and psqlODBC comments out
every one of them in its `.def`. **One exception:** `SQLFreeStmt` is an ODBC 3.x
function in its own right, and the Windows DM passes its deprecated `SQL_DROP`
option through rather than mapping it.

The generalisable trap: "we export it, so we should make it work" is backwards
whenever the Driver Manager already maps the function. Check
`CORE_UNEXPORTED_FUNCTIONS` — each entry records which 3.x function the DM maps
it to — before implementing any deprecated entry point.

- **`get_type_info`**: Include **both** ANSI and Unicode type variants.
  pyodbc queries `SQLGetTypeInfo(SQL_VARCHAR=12)` and
  `SQLGetTypeInfo(SQL_CHAR=1)` — if only `SQL_WVARCHAR` (-9) and `SQL_WCHAR`
  (-8) are returned, pyodbc cannot perform type conversions and `SQLGetData`
  fails for numeric types.

- **`SQL_GETDATA_EXTENSIONS`**: Report exactly what the shared `stackable-odbc-core`
  fetch/bind implementation supports — do not reflexively return `0x0F`.
  `SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER | SQL_GD_BOUND` (`0x0B`) is correct
  for a forward-only driver: `sql_get_data`
  (`src/ffi/fetch.rs`) never checks column order or binding state,
  so any column, in any order, bound or not, can be read via `SQLGetData`.
  `SQL_GD_BLOCK` must **not** be included unless the driver implements block
  cursors: `SQLSetStmtAttrW` (`src/ffi/stmt_attr.rs`) rejects any
  `SQL_ATTR_ROW_ARRAY_SIZE` other than 1 (substituting 1 back with `01S02`), so
  a driver that inherits that behaviour can never produce a multi-row rowset for
  `SQL_GD_BLOCK` to describe.

- **Unknown `SQLGetInfoW` info types**: `stackable-odbc-core` returns `U32(0)` for
  unknown info types (returning `SQL_ERROR` corrupts the DM's internal state).
  For the genuine per-source-type `SQL_CONVERT_*` info types it returns
  `0xFFFFFFFF` ("all conversions supported") — returning 0 causes the DM to
  block `SQLGetData` with `HYC00`.

  That set is **53–71, 122–126 and 173**, not the contiguous 48–73 the numbering
  suggests. The gap matters: 48 is `SQL_CONVERT_FUNCTIONS`, a bitmask of whether
  `CAST`/`CONVERT` syntax is supported at all, and 49–52 are the
  numeric/string/system/timedate scalar-function bitmaps. Answering "all
  supported" for those claims scalar functions the backend may not have, which
  is how a BI tool comes to emit `{fn SOUNDEX(x)}` against a data source that
  rejects it. `info_type_default_response` classifies them individually against
  `sqlext.h` for exactly this reason.

## Testing

### Miri (undefined behaviour and leaks)

`stackable-odbc-core` is checked by Miri on every PR (the `miri` job in
`.github/workflows/build.yaml`). Run it locally the same way:

```bash
rustup +nightly component add miri
MIRIFLAGS="-Zmiri-disable-isolation" \
  cargo +nightly miri test -p stackable-odbc-core --lib -- --skip proptest
```

The non-proptest set is **1511 tests** today, twelve of which carry a `miri`
ignore, and it grows with every commit — take the count from
`cargo test --lib -- --skip proptest --list`, not from this sentence.

**The wall-clock figure that used to be here is deleted rather than updated,
because it could not be re-measured**: Miri needs to write `~/.cache/miri` to
build its sysroot, which the development sandbox denies, so the run fails before
it interprets anything. Measure it on a host where Miri can run and put the number
back with the date you took it. **Re-measure rather than trusting a figure here** —
the last one was "110 seconds
for 675 tests" for four days after it stopped being true, and the run had
reached 28.5 minutes against a `timeout-minutes: 30` budget before anyone
noticed. `-Z unstable-options --report-time` after the `--` gives the per-test
breakdown that tells you which test is responsible. Notes:

- **Nightly only.** Miri cannot run on the pinned stable toolchain.
- **Pure Rust.** All the raw-pointer marshalling lives in `stackable-odbc-core`, so
  it is where the undefined-behaviour risk lives and Miri earns its keep.
- **Proptests are skipped** — they take hours under Miri. They run on stable.
- **A test whose cost is algorithmic, not memory-safety-related, gets
  `#[cfg_attr(miri, ignore = "…")]`.** Miri's slowdown turns a large input into
  minutes or hours, and the `miri` CI job budgets 30. The precedent is
  `escape::tests::pathological_nesting_returns_an_error_rather_than_killing_the_process`:
  50 000 nesting levels over a 250 KB input cost **over 16 minutes** of
  interpreted execution on its own. Skipping it loses nothing, because
  `src/escape.rs` contains no `unsafe` for Miri to check and the neighbouring
  `MAX_ESCAPE_DEPTH ± 1` tests cover the limit on both recursion paths.
  Before adding a big-input test, ask whether the code under it is `unsafe` at
  all; if not, Miri is not the tool that should be paying for it.

  **A big input does not have to look big.** The two guards in
  `types/diagnostics_table.rs` are the case to learn from: they take no
  parameters and read no files at runtime, so nothing about them reads as
  expensive, and they finish in 0.018 s and 0.003 s natively. But they scan the
  1.86 MB of FFI source that module `include_str!`s, and Miri interprets that
  byte by byte — **553 s and 136 s**, together 68% of the whole run, on a module
  containing no `unsafe` whatsoever. The signal to watch for is `include_str!`,
  a full-`u16`-space scan, or any other input baked in at compile time rather
  than passed in; the native runtime will not warn you, because the ratio, not
  the absolute time, is what Miri multiplies. There are **three** such guards
  there now — the third scans function *bodies* for `SqlState::` factory calls
  — and each carries the same `#[cfg_attr(miri, ignore = …)]`. A fourth scanner
  added to that module needs one too, or it silently buys back the 68%.
- **Leak reporting is deliberately left on.** It is what catches a handle or
  descriptor allocation that a teardown path forgets to free. If you add a
  test that allocates handles, it must free them or the job goes red.
- **That figure assumes warm build artifacts.** A run after any source change
  rebuilds the crate under Miri first, which dominates and can take many
  minutes. Budget for that before assuming a run has hung.
- **`-Zmiri-disable-isolation` is required, not optional.**
  `column_value::current_utc_date` reads the wall clock, which the
  `SQL_TYPE_TIME` → `SQL_C_TYPE_TIMESTAMP` conversion needs ("the date fields
  of the timestamp structure are set to the current date"). Without the flag
  Miri refuses `SystemTime::now` as an unsupported operation and the test
  aborts rather than failing an assertion, which reads like a Miri bug rather
  than a missing flag.

### Alignment: what Miri does and does not catch

Every access through an application-supplied pointer must be
`read_unaligned` / `write_unaligned`, a byte-wise copy, or an element-wise
loop. ODBC applications using row-wise binding pass pointers at arbitrary
offsets into a packed buffer, so alignment is never guaranteed. Four operations
carry an alignment requirement, and all four have been the source of real bugs
here:

| Operation | Requirement |
|-----------|-------------|
| `*ptr = v` / `*ptr`, including `*(p as *mut T) = v` | aligned for `T` |
| `slice::from_raw_parts(_mut)` | aligned for the element type — **UB on construction, before anything is read** |
| `ptr::copy_nonoverlapping` | *both* pointers aligned for `T`; cast to `*mut u8` to avoid it |
| `&*(p as *const T)` | aligned for `T` |

`u8` pointers are exempt: `u8` has alignment 1.

Two things make this easy to get wrong when auditing:

- **Grep for the operation, not for `*ptr`.** A deref of a cast
  (`*(diag_info as *mut i32) = v`) and a multi-line `unsafe` block both evade
  the obvious pattern. `from_raw_parts` looks nothing like a deref at all.
- **A misaligned access is not reliably observable on x86-64.** In a *debug*
  build the standard library's precondition check fires, but it raises a
  **non-unwinding** panic, which `panic_safe`'s `catch_unwind` cannot contain —
  the host process aborts. In release it usually just works, until it does not.

**`-Zmiri-symbolic-alignment-check` is a manual tool, deliberately not in CI.**
Plain Miri checks alignment against the concrete address the allocator returned,
so a test can pass by luck: offsetting `+1` into a `Vec<u8>` is not reliably
misaligned, because a byte allocation has alignment 1 and may already start on
an odd address. The symbolic check ignores the concrete address and catches the
class regardless. It is slow enough that it was not worth a per-PR job, though
the incremental cost over plain Miri has not been measured separately from the
rebuild. Run it by hand when touching pointer marshalling:

```bash
MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-symbolic-alignment-check" \
  cargo +nightly miri test -p stackable-odbc-core --lib -- --skip proptest
```

To write a test that is misaligned on every platform, offset one byte into an
allocation of the *target* type, not into a byte buffer:

```rust
let mut arena = vec![0u16; 16];
let ptr = unsafe { arena.as_mut_ptr().cast::<u8>().add(1) }.cast::<u16>();
```

### Concurrency: the lock discipline

Handle contents are internally synchronised, per connection, not left to the
Driver Manager. `SQLAllocHandle`'s Comments section requires this: "Drivers
must therefore support safe, multithread access to this information." The
mechanism:

- **A lock group is per connection, shared with every statement (and
  descriptor) allocated on it.** One acquisition therefore covers a call that
  touches a statement and its parent connection, which is what removes any
  ordering to get wrong between the two. Groups are `GroupLock`
  (`src/handles/registry.rs`); which group a token belongs to is derived from
  the registry, not stored on the handle.
- **`HandleScope` is the only way to reach a handle's contents, and
  `panic_safe` (`src/panic.rs`) is the only way most code gets a
  `HandleScope`.** `panic_safe` locks the target's group before constructing
  the scope and ties the scope's lifetime to that lock, so "the group lock is
  held" is a fact the borrow checker enforces rather than a rule a comment
  states. The three other production callers of `HandleScope::new` are
  `HandleScope::with_child_group_in` (the nested-lock case below),
  `HandleScope::with_group` (`SQLCopyDesc` phase one, which takes the *source*'s
  group and materialises an owned snapshot), and `sql_cancel`, which builds one
  only on the branch where its own `try_lock` succeeded. Four in total, which is
  what `handles/scope.rs`'s own doc comment says; count them before repeating a
  number here.
- **A `Backend` method must never re-enter a `SQLxxx` entry point on the same
  connection.** Every `Backend` method, including `connect`, runs while
  `panic_safe` holds that connection's group lock, and the lock is not
  reentrant: calling back in, directly or through an application callback,
  deadlocks the calling thread with no diagnostic and no `SqlReturn`, because
  the thread never returns far enough to produce either. `Backend::cancel` is
  the one exception, covered separately below.
- **The one lock-ordering rule is environment before connection**, and
  `SQLEndTran(SQL_HANDLE_ENV)` is its only site: it holds the environment's
  group while walking that environment's connections via
  `HandleScope::with_child_group`. Do not acquire a connection's group first
  and then reach for its environment's; nothing else in the crate nests two
  groups at all.
- **`SQLCancel` is deliberately exempt.** It may run on a thread other than
  the one executing on the target statement, and taking that statement's
  group lock unconditionally would make cancelling a query wait for the query
  it was asked to cancel. Instead it clones the statement's cancel token out
  of the registry, then attempts the group with `try_lock`: on the branch
  where another thread holds it, cancel signals the backend's `CancelToken`
  and returns, touching no handle state and posting no diagnostic, per the
  spec's own carve-out for a function running on another thread. A
  `CancelToken` therefore carries the crate's one bounded exception to "core
  never touches a backend's state concurrently": it must be built eagerly,
  with the connection's real parameters in hand, at first use rather than
  lazily inside `cancel` (see `Backend::cancel_token`'s doc comment for the
  MariaDB ODBC-401 failure this rule exists to prevent), and if it aliases the
  connection rather than standing alone, it must keep its target alive
  through an `Arc` — core clones the token out before doing anything else, so
  it must survive a concurrent `SQLDisconnect`.

  Two consequences of running the cross-thread branch lock-free are worth
  knowing rather than rediscovering as a bug report: a `SQLGetDiagRecW` /
  `SQLGetDiagFieldW` immediately following such a cancel **blocks** until the
  cancelled call has unwound through the backend, because both of those take
  the connection's group and reading the diagnostic queue while another
  thread pushes to it is undefined behaviour — `SQLCancel` itself still
  returns promptly; the wait moves to whichever call reads diagnostics next.
  And `try_lock` cannot tell "a sibling statement on this connection is busy"
  apart from "my own statement is busy": either pushes `SQLCancel` onto the
  cross-thread branch, so a merely-idle statement's data-at-execution state
  is occasionally left uncleared where it strictly could have been —
  harmless, and explicitly spec-legal.
- **A cancelled call reports `HY008`, and the token is minted per execution.**
  `Backend::cancel` signals; `Backend::is_cancelled` observes. They are a pair —
  a backend implementing the first and not the second still cancels the work,
  but the application sees whatever SQLSTATE the driver's error mapping
  produced instead of "operation canceled". Core asks `is_cancelled` **only
  after a backend call returned an error**: the spec permits a cancelled
  execution to finish anyway ("it is possible for the execution to succeed and
  return SQL_SUCCESS while the cancel is also successful"), so `Ok` is never
  reclassified. The single implementation is `crate::cancel`.

  `mint_cancel_token` builds a **new** token at every statement-producing call,
  and the cursor-consuming calls read that execution's token rather than
  minting one. An earlier revision created one token per statement and never
  replaced it, which left a cancelled statement permanently unusable —
  `Backend::cancel` marks the token, and the next execution reused it. The spec
  requires the opposite ("After the statement has been canceled, the
  application can call SQLExecute or SQLExecDirect again"), and the outcome
  that rule was protecting against is itself spec-mandated: "a call to
  SQLCancel when no processing is being done on the statement ... has is [sic]
  no effect at all."
- **`SQLCancel` is not the only cross-thread caller of `Backend::cancel`.**
  `src/query_timer.rs` enforces `SQL_ATTR_QUERY_TIMEOUT` for a backend that
  answered `QueryTimeout::CoreCancels`, and it does so the only way a
  synchronous trait allows: a timer thread that calls `Backend::cancel` while
  the calling thread is still blocked inside the backend. It holds **no lock**
  — the same footing as `SQLCancel`'s cross-thread branch — so the rule that a
  `cancel` implementation must never block on this connection's lock covers it
  unchanged. It clones the token out of the registry for the same reason too:
  the token has to survive a statement freed while a timer is still armed.

  A timed-out call reports **`HYT00`**, not `HY008`. Both arrive through a
  signalled token, so the ordering in `QueryTimer::check` is load-bearing: the
  cancel pass runs first and would label it `HY008`, and the timeout pass runs
  second so the more specific state wins. An application that set a deadline is
  waiting to tell "my deadline passed" from "another thread cancelled me".

  **The attribution outlives the call that made it**, and has to. A deadline
  that expires as the backend call is returning cancels the token and leaves the
  call successful, so the failure it causes surfaces on a *later* call — whose
  own timer never fired. `cancel::CancelState` is therefore what the registry
  stores: the backend's token plus core's `timed_out` flag, one allocation, so
  the flag is minted per execution and survives a freed statement exactly as the
  token does. `QueryTimer::reclassify` reads it, which confines `HYT00` to the
  entry points that hold a timer. `cancel::reclassify_cancelled` deliberately
  does not read it: the four entry points that reach it without a timer —
  `SQLGetData`, `SQLDescribeParam`, `SQLDescribeCol`, `SQLColAttribute` — have
  no `HYT00` row between them, so those keep `HY008` on a timed-out cursor.

  Do not shorten that to "the timer-holding entry points are exactly the ones
  with an `HYT00` row". `SQLParamData` holds a timer and relabels, and its own
  table has no such row; it inherits one from the sentence after its table
  ("it can return any SQLSTATE that can be returned by the function called to
  execute the statement"), which is the same grant its doc comment already
  documents a dozen other inherited states under. Check a function's table
  *and* its surrounding prose before deciding either way.
- **Every lock in the crate is imported from `src/sync.rs`**, never directly
  from `std::sync`, so that building a test with `--cfg loom` swaps every one
  of them for loom's instrumented equivalent. A lock imported around that
  module would be invisible to loom and silently opt its code out of the
  interleaving proof.

  **Two documented exceptions.** First, `query_timer.rs`'s `Condvar` and its
  `Mutex` come from `std::sync`. loom's `Condvar` has no `wait_timeout_while`,
  and its `wait_timeout` ignores the duration outright — loom 0.7.2's source says
  "TODO: implement timing out" and always returns `WaitTimeoutResult(false)` —
  so an instrumented query timer could not model a timeout, which is the only
  thing about it worth modelling. No loom model reaches that code either.

  Second, `logging.rs` hands `std::sync::Mutex::new(file)` to
  `tracing_subscriber` as its writer. That one is not a preference:
  `tracing_subscriber` implements `MakeWriter` for `std::sync::Mutex<W>`
  specifically, and loom's `Mutex` is an unrelated type with no such impl, so the
  substitution would not compile. No loom model reaches logging either.

  The rule being enforced is "no lock silently opts itself out", so an exception
  stated at its site, in `sync.rs` and here does not break it; a quiet one would.
  Check first whether loom can model the primitive at all: if it can, import it
  from `sync.rs`.

**Loom models** the primitives this discipline is built from — `Registry`,
`GroupLock`, and the crate's own nested-lock path
(`HandleScope::with_child_group_in`) — in `src/handles/registry.rs`'s
`#[cfg(all(test, loom))] mod loom_tests`. Not the FFI entry points above them,
since `registry()` panics outside an active `loom::model` and cannot be called
from inside one either (loom replays the same closure many times to explore
interleavings, while a `static` only runs its initializer once).

**That constraint is why `with_child_group` has an `_in` variant taking a
`&Registry`**, and it generalises: when a model can only reach a function by
re-implementing what it does, the model proves a property of the test rather
than of the crate. `env_before_connection_cannot_deadlock` spent its life in
that state — it locked two `GroupLock`s of its own in the right order, so a
regression reversing the order in `with_child_group` would not have failed it.
Threading the registry through as a parameter was the whole fix. Before
accepting "the model cannot reach this", check whether a `&Registry` parameter
is all that stands in the way.

Run them with:

```bash
RUSTFLAGS="--cfg loom" cargo test --lib loom_tests
```

The `loom_tests` filter is required: every other unit test in the crate also
compiles under `--cfg loom` once it is set, and calls the process-wide
registry outside a model, which panics as soon as `Registry::new` resolves to
loom's `RwLock`. If a model runs long, lower `LOOM_MAX_PREEMPTIONS` (set to
`3` in CI) before simplifying the model itself — a smaller bound still proves
more than no model.

### Descriptors

A statement owns four descriptors — the ARD, APD, IRD and IPD — and ODBC makes
them the *definition* of a binding rather than a copy of one. `SQLBindCol`'s
page: "when `SQLBindCol` is called, the driver sets fields in the ARD." So
there is one storage, not a binding map beside a descriptor:

| Descriptor | Reached by | Records | What they are |
|---|---|---|---|
| ARD | `desc_of(stmt, Ard)` | `DescriptorRecord` | what `SQLBindCol` set |
| APD | `desc_of(stmt, Apd)` | `DescriptorRecord` | `SQLBindParameter`'s C-side buffer |
| IPD | `desc_of(stmt, Ipd)` | `DescriptorRecord` | `SQLBindParameter`'s declared SQL type |
| IRD | `desc_of(stmt, Ird)` | none stored | computed from `ColumnDescriptor` on read |

Each is its own registered allocation rather than a field of the statement, and
the two application descriptors may be replaced by one the application allocated
— see "Reaching a descriptor" below.

`Descriptor` carries a `role: DescriptorRole` rather than a type parameter,
because ODBC has one record shape and four *readings* of it: `SQLSetDescField`
accepts any field identifier against any descriptor and decides validity from
the role. Six points follow, and each of them has already been the wrong answer
once:

- **`SQLBindParameter` writes two descriptors.** The C-side fields are an APD
  record and the declared type is an IPD record, under the same key, removed
  together. One record spanning both is what makes `SQLSetDescField`
  unimplementable. Readers take `ParamRecord<'_>`, a borrowed view of both
  halves, from `ParamRecords::get`.
- **The IRD is a computed view, never stored state.** `SQLGetDescField` and
  `SQLGetDescRec` on the IRD delegate to `col_attr::get_column_attribute`, which
  is also `SQLColAttributeW`'s implementation — the two are spellings of one
  question, and answering them from two places is how they come to differ. A
  read before the statement has produced column metadata is `HY007`; the spec:
  "Until the IRD has been populated, any attempt to gain access to a field of an
  IRD will return an error." A write is `HY016`, except the two header fields
  that row exempts by name.
- **A binding is a non-null `SQL_DESC_DATA_PTR`, not a present key** — but that
  answers "is there a data buffer", not "is there a binding". A record exists as
  soon as any one field is set, so `records.contains_key` stopped answering
  either question; every site that needs the first calls
  `DescriptorRecord::is_bound`. The second has *two* pointers in it: the spec
  lets `SQLBindCol` unbind a column's data buffer while keeping its
  length/indicator buffer ("An application can unbind the data buffer for a
  column but still have a length/indicator buffer bound for the column"), so
  `collect_bindings` admits a record carrying either pointer and only a record
  carrying neither is skipped. The mature drivers split on this — MySQL
  Connector/ODBC keeps such a record, psqlODBC clears the whole binding — and
  core follows the spec sentence, which is unconditional. The visible half of
  getting this wrong is the *indicator*: `write_column_value` declines to write
  through a null target but writes the length indicator unconditionally, which
  is exactly what makes the indicator-only binding work and exactly what makes a
  stray record visible.
- **`set_concise_type` is the only writer of the type trio.** Setting
  `SQL_DESC_CONCISE_TYPE` also sets `SQL_DESC_TYPE` and
  `SQL_DESC_DATETIME_INTERVAL_CODE`, and the subcode is **not** the concise type
  — `SQL_TYPE_DATE` is 91 and `SQL_CODE_DATE` is 1. `col_attr` holds both
  mappings (`verbose_type` and `datetime_interval_subcode`) so the descriptor
  and `SQLColAttribute` cannot disagree about one column.
- **Eight statement attributes are descriptor header fields**, per
  `SQLSetStmtAttr`'s own mapping table, which says setting one sets the other.
  `HeaderOwner::of` names them and `HandleScope::attr_get`/`attr_set` is the only
  way to reach an attribute's storage, so `stmt.attrs` no longer holds those keys
  at all. `descriptor::header_attribute` is the same table read in the other
  direction, for `SQLGetDescField`. The four IRD- and IPD-side pairs
  (`SQL_ATTR_ROW_STATUS_PTR`, `SQL_ATTR_ROWS_FETCHED_PTR`,
  `SQL_ATTR_PARAM_STATUS_PTR`, `SQL_ATTR_PARAMS_PROCESSED_PTR`) stay on
  `stmt.attrs`, and `attr_get` routes them there so no caller needs to know.
  **The storage is keyed by the `SQL_DESC_*` field, not by the attribute**: the
  mapping is not one-to-one — `SQL_DESC_ARRAY_SIZE` is `SQL_ATTR_ROW_ARRAY_SIZE`
  on an ARD and `SQL_ATTR_PARAMSET_SIZE` on an APD — and one explicit descriptor
  may be the ARD of one statement and the APD of another, so two keys for one
  field would be two values for one field.
- **`odbc-sys` misspells one of the eight.** `SQL_ATTR_PARAM_OPERATION_PTR` is
  `StatementAttribute::ParamOpterationPtr` — transposed letters, upstream. A
  grep for the correct spelling finds nothing and reads as "core does not
  implement it", which is false.

#### The consistency check runs at all four sites

`descriptor::consistency_check` returns `HY021`, and `SQLSetDescRec`'s own
"Consistency Checks" section says when it runs: "This check is always performed
when **SQLBindParameter** or **SQLBindCol** is called or when **SQLSetDescRec**
is called for an APD, ARD, or IPD" — plus `SQLSetDescField` when it sets
`SQL_DESC_DATA_PTR`.

**So `SQLBindCol` and `SQLBindParameter` can now fail where they did not.** That
was taken deliberately; `CHANGELOG.md` carries it as a migration note. The
function's doc comment lists all five of the spec's clauses and states which
core reduces and why.

**Clause 5 is checked, and was not always.** It reads "if
`SQL_DESC_CONCISE_TYPE` is an interval type,
`SQL_DESC_DATETIME_INTERVAL_PRECISION` is a valid interval leading precision",
and its doc comment used to say it could not be enforced because core supported
no interval types. The *C to SQL: Numeric* table's interval row reads that
field, so it can be and is: a leading precision is a digit count and cannot be
negative, while zero passes and means the application declared none. That last
reading — zero as "unspecified" rather than as a literal limit — is the same one
`check_declared_decimal_size` gives a zero `ColumnSize`, and the conversion
relies on it. The clauses about interval *seconds* precision remain reduced.

The generalisable point: a doc comment saying a clause is unenforceable is a
claim about what core currently does, not about the spec. Check whether it is
still true before repeating it.

A value core cannot honour must be refused identically through both doors:
`SQL_DESC_ARRAY_SIZE` set through `SQLSetDescField` routes through the same
`01S02` substitution `SQLSetStmtAttr(SQL_ATTR_ROW_ARRAY_SIZE)` applies, because
they are one value.

#### Reaching a descriptor

**Every descriptor is its own registered allocation.** A statement holds four
tokens, not four `Box<Descriptor>` fields, plus two `Option` overrides for the
application descriptors. An explicit descriptor is parented to the **connection**
and an implicit one to its statement, and all of them join the connection's lock
group — the one every statement on it already shares — so a descriptor adds no
lock and no ordering rule.

Three rules follow, and each was the opposite while the four were fields of a
statement:

- **`Descriptor` has a `HasKind` impl.** It deliberately did not, on the grounds
  that `HandleScope::get` dispatches on `HandleKind` alone and all four of a
  statement's descriptors register as `HandleKind::Desc`. That held only while
  they were fields of one allocation: a token then named the *statement*. Now a
  token names exactly one descriptor and the struct at that address carries its
  own `role`, so `get` needs nothing the registry cannot check.
- **`HandleScope::stmt_with_desc` is sound**, on the same footing as
  `stmt_with_parent`: the statement holds opaque tokens the compiler cannot
  follow, and a `Descriptor` holds no back-pointer, so neither is reachable from
  the other. `stmt_with_parent_and_params` is the three-way form of the same
  argument, for the calls that need a statement, its connection and both
  parameter descriptors at once.
- **`Drop` does not reclaim them.** `free_statement_allocation` frees the four
  explicitly, `SQLFreeHandle` frees an explicit one, and `SQLDisconnect` frees any
  left on the connection. Miri's leak check is what enforces all three.

`HandleScope::desc_of` is the single door onto descriptor storage: it applies the
override, so no call site can read the implicit descriptor while the application
believes its own is in use. A site that already resolved the statement should copy
`descriptor_token(role)` out and use `HandleScope::descriptor` instead — going
through `desc_of` there resolves the statement a second time, which
`handle_lookup` measures.

The rule that remains is that a `Descriptor` is never reached by casting an
address; only through the registry, as every other handle kind is.

#### The explicit-descriptor rulings, and why

Four questions a future reader would otherwise relitigate:

- **`HY024` is core's; `HY017` is not.** `SQLSetStmtAttr`'s `HY024` row states
  the cross-connection descriptor case verbatim and closes with the general rule
  that makes it core's — "For all other connection and statement attributes, the
  driver must verify the value specified in *ValuePtr*". `HY017` is `(DM)` on
  *both* of its clauses, so core adds neither check; the second clause's "other
  than the handle originally allocated" implies the original *is* allowed, and it
  is accepted. The check core makes compares the parent **chain**, so a
  descriptor of this connection and one of this connection's statements both
  pass.
- **`SQLFreeHandle` answers `HY000` on the ownership branch, never `HY017`.**
  Routed by parentage rather than by alloc type: this function allocated the
  descriptors whose parent is a connection and only those, and retiring a
  statement's own slot would leave that statement pointing at nothing. The
  refusal is ownership, not a spec check, and borrows no `(DM)` code to say so —
  the same function already answers `HY000` for an unimplemented handle type,
  whose table lists no `HYC00` either. A token that is not a descriptor at all is
  `SQL_INVALID_HANDLE`, which is a different question.
- **`SQLCopyDesc` never holds two group locks.** The spec permits a copy across
  connections and even across environments, so source and target may be in two
  groups. Phase one takes the source's group through `HandleScope::with_group`
  and materialises an owned `DescriptorSnapshot`; that function's return type
  carries no guard, so the release before phase two is structural rather than
  remembered. Phase two is an ordinary `panic_safe` on the target, which is where
  every diagnostic belongs — including the `HY007` phase one decided.
  `opposite_direction_copies_cannot_deadlock` models it, and
  `the_set_of_group_lock_acquisition_sites_is_closed` records the site.
- **A shared descriptor means shared bindings.** Two statements pointed at one
  explicit ARD have one binding set between them, so `SQLFreeStmt(SQL_UNBIND)` on
  either clears both. That is spec-correct — the spec makes the descriptor *be*
  the binding — and has a test rather than a workaround.

Two smaller ones: `SQL_DESC_ALLOC_TYPE` follows the allocation and is the one
field `SQLCopyDesc` never copies; and `DescriptorSnapshot` carries neither it nor
the *source's* role, because the consistency check runs under the **target's**
role and a snapshot that remembered where it came from would invite a check
against the wrong one.

#### What descriptors now support

All five descriptor functions are implemented and reported by `SQLGetFunctions`:
`SQLGetDescFieldW`, `SQLSetDescFieldW`, `SQLGetDescRecW`, `SQLSetDescRec` and
`SQLCopyDesc`. `SQLAllocHandle(SQL_HANDLE_DESC)` and
`SQLFreeHandle(SQL_HANDLE_DESC)` work, an application descriptor can be swapped
in through `SQL_ATTR_APP_ROW_DESC` / `SQL_ATTR_APP_PARAM_DESC`, and one
descriptor may be shared across statements on a connection.

`DescriptorRole` has a fifth variant, `App`, for an explicitly allocated
descriptor whose role is not yet known — the spec: "it is not known whether an
explicitly allocated application descriptor is an APD or ARD until execute time".
`field_access(App, f)` is defined as the ARD's cell, and
`the_ard_and_apd_field_tables_agree_everywhere` is what makes that a derived fact
rather than a fourth hand transcription.

**`SQL_OIC_CORE` is satisfied.** Core-level conformance requires allocating and
freeing all handle types and manipulating descriptor fields through all five
functions, which is what the above closes.

Still out of scope, deliberately: bookmark records (record 0), and automatic
population of the IPD — `SQL_ATTR_AUTO_IPD` stays `SQL_FALSE`, so the five
footnote-[1] fields stay `Undefined` on the IPD.

### Fuzzing

`fuzz/` holds [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets for
the memory-marshalling hot paths (`write_column_value` and `utf16`). Each
allocates its output buffer at exactly the caller-declared length, so
AddressSanitizer catches any overrun that clippy cannot see. It is its own Cargo
workspace (libFuzzer needs nightly), so the root build ignores it. A short smoke
run of both targets also runs on every PR (the `fuzz` job in `build.yaml`).

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run utf16
cargo +nightly fuzz run column_value
```

See [`fuzz/README.md`](fuzz/README.md) for what is and is not worth fuzzing.

### Unit tests

- `stackable-odbc-core` tests use a shared `MockBackend` from `test_utils.rs`
  (the examples below are a sample, not an inventory — grep `struct Mock` for the
  current set)
  (connect/disconnect succeed, everything else returns `NotImplemented`), plus a
  family of purpose-built mocks for paths `MockBackend` cannot reach:
  `MockAltBackend` (declares a different value for every capability method, so
  the guard test can see an answer move with the backend), `MockNoCatalogBackend`,
  `MockTypeInfoBackend` and `MockFunctionsBackend` (declare real rows and a real
  function list, because a mock returning an empty slice makes a loop run zero
  times and the test pass vacuously), `MockFailingCloseBackend`, and the
  `mock_isolation_backend!` / `mock_txn_backend!` families.
- Run `cargo test` — must produce zero warnings.
- **Array fetch and batch parameter paths** (`SQL_ATTR_ROW_ARRAY_SIZE`,
  `SQL_ATTR_ROWS_FETCHED_PTR`, `SQL_ATTR_PARAMSET_SIZE`) are covered by direct C
  ABI calls with pre-allocated column/parameter buffers, which Rust handles
  cleanly without any external dependencies.
- A driver crate tests its `Backend` impl directly and adds FFI-level
  integration tests that call the generated C ABI entry points; those live in
  the driver's repository, not here.

### Benchmarks

Core has three Criterion benchmarks, all in `bench/`, its own detached crate,
for the same reason `fuzz/` does:

```bash
cd bench && cargo bench
```

- **`fetch_throughput`** — drives `SyntheticStatement::fetch()`/`get_data()`
  directly (in-memory, no backend) and never enters the FFI layer at all: no
  `panic_safe`, no handle registry lookup, no descriptor, no
  `write_column_value`. What it measures is `SyntheticStatement`'s own
  row-cloning and `ColumnValue` construction cost, not the marshalling into an
  application's buffer — that is `ffi_fetch`'s job, below. `BENCH_ROWS`
  overrides the row count.
- **`handle_lookup`** — goes through the FFI entry points, so it is the only one
  of the first two that sees the handle registry. Neither of its benchmarked
  functions ever opens a result set (`BenchBackend::exec_direct` is never
  called), so it measures `HandleScope::get`'s cost in isolation, with no
  binding, fetch, or data marshalling anywhere on the path. Two shapes,
  because the error path is not the success path scaled: `get` (one
  `HandleScope::get`, then trivial work) and `get_then_push_diagnostic` (the
  error path, where `panic_safe` also has to find the handle again).
- **`ffi_fetch`** — the one benchmark that goes through the FFI entry points
  *and* drives a real result set, closing the gap the other two leave: a
  connection is installed with the `test-support` feature's
  `attach_connection`, so `sql_bind_col`, `sql_fetch` and `sql_get_data` run
  for real against a backend-produced `StatementBackend`, with `panic_safe`,
  the handle registry, the ARD and `write_column_value` all on the measured
  path. Two groups: `ffi_fetch_bound` (`SQLBindCol` three columns — one
  `i64`, one 1 KiB string, one 1 KiB bytes — then loop `SQLFetch` over
  `BENCH_ROWS` rows) and `ffi_get_data_chunked` (`SQLFetch` one row, then
  drain a 64 KiB string column through a 512-byte `SQLGetData` buffer until
  `SQL_NO_DATA`, exercising the `GetDataCursor` chunking loop that a bound
  column never reaches).

Pick the one that can actually see what you changed. A registry or locking
change is invisible to `fetch_throughput`; a `ColumnValue` conversion inside
`SyntheticStatement` is invisible to `handle_lookup`; and anything in
`write_column_value`, the ARD, or the `SQLGetData` chunking cursor is invisible
to both — only `ffi_fetch` reaches those. If nothing covers it, that is a
reason to add a fourth rather than to quote a number from the wrong one.

Keeping it out of core's manifest is deliberate. A `[[bench]]` target that is
excluded from the published package makes `cargo package` warn on every
publish, and the alternatives are to ship a benchmark no consumer can run or to
leave a standing warning where it would mask the next real one. Off in its own
crate, core declares no bench target and `criterion` stays out of core's
dependency graph.

The directory is `bench/`, singular: cargo auto-discovers `benches/` as a
target directory and then insists on validating any manifest inside it, which
fails packaging.

## Architecture

The project uses a generic `Backend` trait in `stackable-odbc-core` with manual
forwarding stubs generated in each driver crate. This was chosen over
proc-macros (premature) and `dyn` trait (poor ergonomics for complex trait
hierarchies).

### How a function call flows

Example: `SQLDriverConnectW` (a fully implemented function), for a driver whose
backend type is `XyzBackend`:

```text
ODBC Application (e.g. isql)
  -> SQLDriverConnectW(...)                    # C ABI entry point generated by forward_ffi! in the driver's lib.rs
    -> ffi::connect::sql_driver_connect_w::<XyzBackend>(...)  # generic impl in stackable-odbc-core
      -> panic_safe::<B, _>(...)               # locks the target's group, builds a HandleScope, catches panics
        -> scope.get::<ConnectionHandle<B>>()  # validates the token against the handle registry, returns typed &mut
          -> handle.diagnostics.clear()        # spec: clear diagnostics at start of each call
          -> validation checks                 # 08002, HY090 -- per ODBC spec
          -> utf16_to_string(...)              # convert UTF-16 input to Rust String
          -> merge_dsn_params(...)             # parse "Key=Value;..."; if DSN= is present, resolve its keys from odbc.ini (explicit values win) and re-parse
          -> params.set_prompter(prompter_for::<B>(completion))  # B::prompter(), unless DriverCompletion is SQL_DRIVER_NOPROMPT
          -> B::connect(&params)               # Backend trait method (database-specific); runs under the connection's group lock, like every other Backend method
          -> handle.connection = Some(conn)    # store result in handle
          -> apply_pending_autocommit::<B>(..) # apply a SQL_ATTR_AUTOCOMMIT set before connect; tears the connection down on failure
          -> write_utf16(...)                  # echo connection string to output buffer
```

### Key design decisions

- **Two traits**: `Backend` (creates connections/statements) and `StatementBackend` (iterates results). Split to separate lifecycle from cursor operations.
- **Handle registry**: an application-facing `SQLHANDLE` is an opaque token
  packing a slot index and a generation counter, not an address. A driver-owned
  table holds `{ generation, kind, addr, group, parent, cancel }`, and
  `HandleScope::get<T>()` validates a token with a bounds check plus a
  generation and kind compare — **without dereferencing the pointer the
  application passed**. Freeing bumps the slot's generation, so every
  outstanding token for that slot is permanently rejected, which also closes
  the recycled-address double-free. This is the primary safety mechanism at
  the FFI boundary. Nothing may treat a `SQLHANDLE` as an address, or validate
  one by reading through it. See "Concurrency: the lock discipline" above for
  `group`, `parent` and `cancel`.
- **`panic_safe`**: Wraps every FFI function except the two below. Locks the
  target handle's group, builds the `HandleScope` the closure operates through,
  and uses `AssertUnwindSafe` + `catch_unwind`. On error, pushes to the handle's
  diagnostic queue and returns the appropriate `SqlReturn`.

  **The two exceptions both use `panic_safe_unlocked`, and both because they have
  no handle to work through** — which is the thing `panic_safe` needs, not merely
  a convenience it offers. `SQLCancel` must not touch the diagnostic state
  `panic_safe` clears and pushes, per the spec's carve-out for cancelling a call
  running on another thread. `ConfigDSNW` is handed no ODBC handle at all: its
  arguments are a window handle, a request code and two strings, so there is no
  token to lock a group by and no queue to push to. It is still an
  `extern "system"` boundary, and an unwind across it lands in the ODBC
  Administrator.

  **Every `extern "system"` export needs one of the two.** Neither is optional
  for a new entry point, and "it takes no handle" is a reason to reach for
  `panic_safe_unlocked`, not a reason to skip the guard — `ConfigDSNW` had no
  guard at all until 2026-07-30 on exactly that reasoning.

  **`SQLCopyDesc` is the one export a single guard cannot cover, because it is
  the one export that takes two lock phases rather than one** (see
  "Descriptors" → "The explicit-descriptor rulings" above for why). Phase two
  is an ordinary `panic_safe` on the target. Phase one holds only the
  *source*'s group — through `HandleScope::with_group`, which is a plain
  lock-then-call with no `catch_unwind` of its own — so a panic reaching
  `describe_col` through `snapshot_ird` had no guard at all until 2026-07-31,
  the same gap `ConfigDSNW` had. `panic::catch_panic_as_error` closes it:
  narrower than `panic_safe_unlocked`, because it does not itself sit at the
  FFI boundary — it converts the panic into the same `OdbcError` shape a
  non-panicking phase-one failure (`HY007`) already returns, and phase two's
  `panic_safe` posts it to the target's queue either way, which is where the
  whole call's diagnostics belong. So `sql_copy_desc` is fully guarded, just not by
  "one of the two" in the sense above — the property this bullet is
  asserting one export needs a third shape to satisfy.
- **W-only for string-bearing functions**: every ODBC function that takes or
  returns a string is exported only in its Wide (`W`-suffix) form; the Driver
  Manager translates an ANSI application's calls into those. Functions with no
  strings in their signature — `SQLAllocHandle`, `SQLFetch`, `SQLBindCol` and
  the rest — have one spelling and are exported unsuffixed.
  `CORE_EXPORTED_FUNCTIONS` in `src/function_id.rs` is the authoritative list,
  and a guard test pins every entry to a symbol that exists; a count written
  here would only go stale.
- **No async in the trait**: `Backend` is synchronous. A driver that wraps an async client library is expected to bridge to it internally (e.g. a current-thread tokio runtime + `block_on`).

### Crate layout

Generic framework. Zero database-specific code.

| Module | What it does |
|--------|-------------|
| `backend.rs` | `Backend` + `StatementBackend` trait definitions; `common_get_info_raw` and `default_get_info` shared helpers |
| `types/mod.rs` | `odbc-sys` re-exports, `InfoValue` enum, submodule declarations |
| `types/constants.rs` | All `SQL_*` named constants (spec-defined values not in `odbc-sys`) |
| `types/conversions.rs` | `*_from_raw()` conversion functions for all ODBC ABI types |
| `types/sql_state.rs` | `SqlState` — five-character ODBC diagnostic code and factory methods |
| `types/value.rs` | `ColumnValue`, `FetchResult`, `Nullable`, `TypeInfoRow`, `ColumnDescriptor` |
| `types/result_cols.rs` | `TablesResultCol`, `ColumnsResultCol`, `PrimaryKeysResultCol`, `ForeignKeysResultCol`, and `CatalogResultColumnWidths` — the per-backend widths those result sets declare |
| `types/connect_params.rs` | `ConnectParams` — ODBC connection string parser |
| `types/col_attr.rs` | `ColAttrValue` and column attribute logic for `SQLColAttributeW` |
| `types/cursor_behavior.rs` | `CursorBehavior` — the `SQL_CB_*` cursor behaviour `SQLEndTran` applies, declared by the backend and reported by `SQLGetInfoW` |
| `types/query_timeout.rs` | `QueryTimeout` — which side enforces `SQL_ATTR_QUERY_TIMEOUT`, declared by `Backend::set_query_timeout` |
| `types/column_size.rs` | Shared ODBC column-size formulas (`catalog_column_size`/`column_size`); keeps declared vs maximum precision distinct |
| `types/info_type_shape.rs` | The `SQLGetInfo` spec's per-`InfoType` return-value shape, transcribed for the conformance test |
| `types/version.rs` | Parsed data-source version numbers, for a backend gating capabilities on server version |
| `types/odbc_version.rs` | `DeclaredOdbcVersion` — the version an application declared through `SQL_ATTR_ODBC_VERSION`, and what it changes |
| `types/catalog_queries.rs` | The ten sealed `XxxQuery` argument objects the catalog hooks take |
| `types/diagnostics_table.rs` | Every function's spec Diagnostics table transcribed, plus the three guards that check the doc comments against it |
| `types/redacted.rs` | `Redacted<T>` — `Debug` wrapper that prints `*****` for sensitive fields (e.g. passwords) |
| `column_value.rs` | `write_column_value()` — core data marshalling for `SQLGetData` (NULL, truncation, type coercion) |
| `param_convert.rs` | `text_to_sql_type()` — the reverse direction: converts `SQL_C_CHAR`/`SQL_C_WCHAR` parameter text to the SQL type `SQLBindParameter` declared. The spec's "C to SQL: Character" table, transcribed. Also owns the size checks all three C-to-SQL tables share (`DecimalLiteral`, `check_declared_char_size`, `check_declared_decimal_size`, `check_declared_binary_size`) |
| `binary_convert.rs` | The spec's "C to SQL: Binary" table, transcribed. `SQL_C_BINARY` to the targets whose byte layout ODBC defines; refuses the rest at bind with `07006` |
| `numeric_convert.rs` | The spec's "C to SQL: Numeric" table, transcribed. Every numeric C type to any of its six target rows, including the interval row and footnote [b]'s optional `01S07`. `numeric_pairing_is_supported` is `SQLBindParameter`'s gate |
| `prompt.rs` | `Prompter` — the trait a driver implements to present a login URL to the user during a connect. Definition only: core ships no implementation and gains no dependency |
| `query_timer.rs` | `QueryTimer` — core-side `SQL_ATTR_QUERY_TIMEOUT` enforcement: a timer thread that calls `Backend::cancel` on expiry and relabels the resulting failure `HYT00` |
| `cancel.rs` | `CancelState` — a backend's cancel token plus core's `timed_out` flag, and the one implementation of "a cancelled call reports `HY008`" |
| `synthetic.rs` | `SyntheticStatement` — in-memory result set for `SQLGetTypeInfo` and catalog functions |
| `catalog_sort.rs` | Sorts a catalog result set into its spec-mandated order; NULL placement from `Backend::null_collation` |
| `catalog_ident.rs` | `SQL_ATTR_METADATA_ID` identifier normalisation and the `SQLTables` `TableType` value-list parser |
| `types/catalog_rows.rs` | The ten typed catalog row structs a `Backend` returns (`TableRow`, `ColumnRow`, `PrimaryKeyRow`, `ForeignKeyRow`, `StatisticsRow`, `SpecialColumnRow`, `ProcedureRow`, `ProcedureColumnRow`, `ColumnPrivilegeRow`, `TablePrivilegeRow`), and their spec-order conversion to `ColumnValue`s |
| `conformance.rs` | Shared support for the `SQLGetInfoW` info-type conformance test (return shape + Driver-Manager-safe value), reused by core and by driver test suites |
| `escape.rs` | ODBC escape-sequence translation (`{fn}`, `{d/t/ts}`, `{oj}`, `{escape}`); a shared scanner with a per-backend `EscapeDialect` |
| `errors.rs` | `OdbcError` with SQLSTATE mapping and `SqlReturn` conversion |
| `descriptor.rs` | `DescriptorRecord`, `DescriptorRole`, the per-role field tables (`field_access`, which decides `HY091` for every identifier naming a real field; one naming none is refused earlier by `ffi::desc::field_from_raw`), the header-field mapping, and the `HY021` consistency check. No FFI, no handles |
| `diagnostics.rs` | Per-handle diagnostic queue (`SQLGetDiagRecW` reads from here) |
| `handles/mod.rs` | `EnvironmentHandle<B>`, `ConnectionHandle<B>`, `StatementHandle<B>`, alloc/free (`pub(crate)`) |
| `handles/registry.rs` | The live-handle table (`Registry`, `Slot`), per-connection `GroupLock`s, cancel tokens, and the loom models (`#[cfg(all(test, loom))] mod loom_tests`) |
| `handles/scope.rs` | `HandleScope` — the only way to reach a handle's contents; token validation without dereferencing the application's pointer |
| `sync.rs` | The one import path for every lock in the crate; aliases to `loom`'s primitives under `#[cfg(all(loom, test))]`, `std::sync` otherwise |
| `utf16.rs` | `utf16_to_string`, `write_utf16` (ODBC uses UTF-16LE) |
| `panic.rs` | `panic_safe` (locks the target's group, builds a `HandleScope`, catches panics), `panic_safe_unlocked` (`SQLCancel`'s lock-free sibling), and `catch_panic_as_error` (`SQLCopyDesc` phase one's panic-to-`OdbcError` guard) |
| `logging.rs` | `init_logging()` via tracing, configured by `ODBC_LOG_LEVEL` / `ODBC_LOG_FILE` |
| `function_id.rs` | `FunctionId` enum + `function_id_from_raw()` for `SQL_API_*` constants |
| `test_support.rs` | `test-support`-feature-gated hooks a driver's test suite uses to put a connection into a handle without `SQLDriverConnectW` |
| `ffi/handle.rs` | `sql_alloc_handle<B>`, `sql_free_handle<B>`, `sql_free_stmt<B>` |
| `ffi/env.rs` | `sql_set_env_attr<B>`, `sql_get_env_attr<B>` |
| `ffi/connect.rs` | `sql_driver_connect_w<B>`, `sql_browse_connect_w<B>`, `sql_connect_w<B>`, `sql_disconnect<B>`, `sql_native_sql_w<B>`; `merge_dsn_params` (DSN resolution) |
| `ffi/connect_attr.rs` | `sql_set_connect_attr_w<B>`, `sql_get_connect_attr_w<B>` |
| `ffi/diag.rs` | `sql_get_diag_rec_w<B>`, `sql_get_diag_field_w<B>` |
| `ffi/cursor.rs` | `sql_num_result_cols<B>`, `sql_row_count<B>`, `sql_more_results<B>`, `sql_close_cursor<B>`, `sql_cancel<B>`, `sql_get_cursor_name_w<B>`, `sql_set_cursor_name_w<B>`, `sql_bulk_operations<B>`, `sql_set_pos<B>` |
| `ffi/execute.rs` | `sql_exec_direct_w<B>`, `sql_prepare_w<B>`, `sql_execute<B>` |
| `ffi/fetch.rs` | `sql_fetch<B>`, `sql_fetch_scroll<B>`, `sql_extended_fetch<B>`, `sql_get_data<B>` |
| `ffi/metadata.rs` | `sql_describe_col_w<B>`, `sql_col_attribute_w<B>`, `sql_tables_w<B>`, `sql_columns_w<B>`, `sql_primary_keys_w<B>`, `sql_foreign_keys_w<B>`, `sql_statistics_w<B>`, `sql_special_columns_w<B>`, `sql_procedures_w<B>`, `sql_procedure_columns_w<B>`, `sql_column_privileges_w<B>`, `sql_table_privileges_w<B>` |
| `ffi/params.rs` | `sql_bind_parameter<B>`, `sql_num_params<B>`, `sql_describe_param<B>`, `sql_put_data<B>`, `sql_param_data<B>` |
| `ffi/bind.rs` | `sql_bind_col<B>` |
| `ffi/desc.rs` | `sql_get_desc_field_w<B>`, `sql_set_desc_field_w<B>`, `sql_get_desc_rec_w<B>`, `sql_set_desc_rec<B>`, `sql_copy_desc<B>` — argument marshalling over `descriptor.rs`'s tables |
| `ffi/stmt_attr.rs` | `sql_set_stmt_attr_w<B>`, `sql_get_stmt_attr_w<B>` |
| `ffi/info.rs` | `sql_get_info_w<B>`, `sql_get_type_info<B>`, `sql_get_functions<B>` |
| `ffi/tran.rs` | `sql_end_tran<B>` |
| `ffi/setup.rs` | `config_dsn_w` (ODBC installer entry point) |
| `ffi/mod.rs` | `ffi` submodule declarations |
| `forward_ffi.rs` | `forward_ffi!` macro — generates the C ABI entry points for a backend (the `SQL*` functions, plus `ConfigDSNW` on Windows) |
| `test_utils.rs` | Shared test infrastructure (`MockBackend` and the purpose-built mocks listed under Testing) |

### What a driver crate contains

A driver built on core is typically laid out like this:

| File | What it does |
|------|-------------|
| `backend.rs` | Struct definitions (`XyzBackend`, `XyzConnection`, `XyzStatement`), `connect`, `disconnect`, `end_tran`, the thin `impl Backend` delegation layer, and the central error-mapping function |
| `backend/execute.rs` | `exec_direct`, `prepare`, `execute`; `impl StatementBackend for XyzStatement` |
| `backend/metadata.rs` | `tables`, `columns`, `primary_keys`, `foreign_keys`; private query helpers |
| `backend/info.rs` | `get_info`, `get_info_pre_connect`, `get_info_raw`, `get_functions`, `get_type_info` |
| `backend/params.rs` | `bind_parameter`, `num_params`, `describe_param` (if the backend supports server-side parameters) |
| `backend/types/connect_params.rs` | Driver-specific connection parameters parsed from the ODBC connection string |
| `lib.rs` | Invokes `stackable_odbc_core::forward_ffi!(crate::backend::XyzBackend)` — generates all the C ABI entry points |
| `type_conversion.rs` | Converts backend-native column values to `ColumnValue` |
| `escape_dialect.rs` | The backend's `EscapeDialect` for core's escape-sequence translator (identifier quoting, `{fn}` name mapping) |
| `ffi_integration_tests.rs` | FFI-level integration tests that call the C ABI entry points directly |
