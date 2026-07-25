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
| [Architecture](#architecture) | Understanding call flow or crate layout |

## Conventions

- Edition 2024, resolver 3, Rust 1.95.0
- `snafu` for errors (the `unwrap_used`, `unwrap_in_result` and `panic` clippy
  lints are denied outside tests)
- `tracing` for logging (not `println!` or `log`)
- `#[repr(C)]` on all handle structs (required for tag-based validation)
- `extern "system"` on all FFI exports (resolves to correct ABI on both Windows and Linux)
- `odbc-sys` links against `libodbc`/`libodbcinst`, so building or testing needs
  the unixODBC dev libraries installed (`unixodbc-dev` on Debian/Ubuntu). No DSN
  or running Driver Manager is required. Miri is the exception — it interprets
  rather than links, so it needs no system libraries.

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

// OR if parsing raw integers to enums:
// 1a. TRACE: raw inputs before parse
tracing::trace!("SQLFunctionW(handle={:?}, raw={})", handle, raw_int);
// 1b. DEBUG: parsed/typed values after
tracing::debug!("SQLFunctionW: attr={:?}", parsed_attr);

// 2. WARN: intentional spec deviations (silent accepts, ignored features)
tracing::warn!("SQLFunctionW: accepting unrecognized X (DM compatibility)");

// 3. DEBUG: return value — always; requires let ret = unsafe { panic_safe(...) }; pattern
tracing::debug!("SQLFunctionW -> {:?}", ret);
```

Rules: no passwords or connection string content; `error!` only for validation failures expressed via `OdbcError` (avoid double-logging); stubs use a single `debug!` entry, no exit log.

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

4. **Implement the generic function** in `src/ffi/` (in the appropriate module)
5. **Add a `Backend` (or `StatementBackend`) trait method** if the function needs database-specific logic. Prefer a defaulted method so existing drivers keep compiling.
6. **Each driver implements the new trait method** in its own backend.
7. Add one entry to the `forward_ffi!` macro in `src/forward_ffi.rs` — all drivers pick it up automatically.

## odbc-sys usage

`odbc-sys` is a minimal `-sys` crate for ODBC type definitions. It deliberately has no convenience methods (see [PR #47](https://github.com/pacman82/odbc-sys/pull/47) for rationale). `stackable-odbc-core` is the driver-side convenience layer on top of it.

- **Always use `odbc-sys` types** where they exist: `HandleType`, `SqlReturn`, `CDataType`, `SqlDataType`, `InfoType`, `Desc`, `FreeStmtOption`, `AttrOdbcVersion`, `EnvironmentAttribute`, `Len`, `Pointer`, `WChar`, etc. For primitive parameters where odbc-sys 0.29 removed the type aliases (e.g. the old `SmallInt`), use the Rust primitives directly (`i16`, `u16`, `i32`).
- **Never redefine** enums, structs, or constants that `odbc-sys` already provides. Before defining a new constant or enum, check `odbc-sys` first. If it's there, use it.
- **Add driver-side extensions** in `stackable-odbc-core` -- since orphan rules prevent `impl TryFrom<i16> for odbc_sys::HandleType`, use standalone conversion functions like `fn handle_type_from_raw(v: i16) -> Option<HandleType>`
- **Keep our own types** only for things `odbc-sys` doesn't have: `ConnectParams`, `ColumnValue`, `ColumnDescriptor`, `FetchResult`, `InfoValue`, `SqlState`, `DiagnosticQueue`, `TypeInfoRow`, `OdbcError`
- **ODBC function IDs** (`SQL_API_*` values) are NOT in `odbc-sys`. They live in `src/function_id.rs` as the `FunctionId` enum (sourced from `/usr/include/sql.h` and `sqlext.h`). Always use `FunctionId::ExecDirect` etc. -- never raw numeric IDs. Convert with `function_id_from_raw(u16) -> Option<FunctionId>`.

## Converting raw values to strongly typed enums

Raw integers from the ODBC C ABI must be converted to strongly typed Rust enums **as early as possible** -- at the FFI boundary, before any logic runs.

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
- `function_id_from_raw(u16) -> Option<FunctionId>` (in `function_id.rs`)

If `odbc-sys` adds a new enum that we need to convert from raw values, add a `xxx_from_raw` function following the same pattern. Do not use `transmute`.

## Adding a new driver

1. Create a new crate that depends on `stackable-odbc-core`.
2. Implement `Backend` + `StatementBackend` for your backend type.
3. In `lib.rs`, invoke `stackable_odbc_core::forward_ffi!(crate::backend::YourBackend);` — no `ffi.rs` needed.

### Windows Driver Manager compatibility checklist

The Windows DM is much stricter than unixODBC. These items are **required** for
a driver to work on Windows — omitting any one can cause silent crashes,
`IM001` errors, or blocked `SQLGetData` calls:

- **`get_info_pre_connect`**: Override this in your `Backend` impl. The Windows DM
  queries `SQL_DRIVER_ODBC_VER` (77) *before* `SQLDriverConnectW`. If it gets
  `SQL_ERROR`, the DM treats the driver as ODBC 2.x and blocks 3.x features
  like `SQL_C_SBIGINT`. At minimum, return `DriverOdbcVer`, `DriverName`,
  `DriverVer`, `AsyncDbcFunctions`, and `MaxConcurrentActivities`. Delegate to
  the same handler as the connected path so the two never drift.

- **`get_functions`**: List **every** exported FFI function, not just
  query-related ones. The Windows DM uses the 3.x bitmap (`func_id=999`) to
  build its dispatch table. Missing entries (e.g. `SetEnvAttr`, `GetStmtAttr`,
  `BindCol`) cause NULL function pointer crashes. The 2.x array (`func_id=0`)
  also needs correct entries — `stackable-odbc-core` maps 3.x IDs to their deprecated
  2.x equivalents automatically, but only for IDs present in the list.

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
  For `SQL_CONVERT_*` info types (48–73) specifically, it returns `0xFFFFFFFF`
  ("all conversions supported") — returning 0 causes the DM to block
  `SQLGetData` with `HYC00`.

## Testing

### Miri (undefined behaviour and leaks)

`stackable-odbc-core` is checked by Miri on every PR (the `miri` job in
`.github/workflows/build.yaml`). Run it locally the same way:

```bash
rustup +nightly component add miri
MIRIFLAGS="-Zmiri-disable-isolation" \
  cargo +nightly miri test -p stackable-odbc-core --lib -- --skip proptest
```

Takes about 35 seconds. Notes:

- **Nightly only.** Miri cannot run on the pinned stable toolchain.
- **Pure Rust.** All the raw-pointer marshalling lives in `stackable-odbc-core`, so
  it is where the undefined-behaviour risk lives and Miri earns its keep.
- **Proptests are skipped** — they take hours under Miri. They run on stable.
- **Leak reporting is deliberately left on.** It is what catches a handle or
  descriptor allocation that a teardown path forgets to free. If you add a
  test that allocates handles, it must free them or the job goes red.
- Writes through application-supplied pointers must use `write_unaligned` /
  byte-wise copies. ODBC applications using row-wise binding pass pointers at
  arbitrary offsets into a packed buffer, so alignment is never guaranteed.

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
  (connect/disconnect succeed, everything else returns `NotImplemented`), plus
  `MockFailBackend` for error paths.
- Run `cargo test` — must produce zero warnings.
- **Array fetch and batch parameter paths** (`SQL_ATTR_ROW_ARRAY_SIZE`,
  `SQL_ATTR_ROWS_FETCHED_PTR`, `SQL_ATTR_PARAMSET_SIZE`) are covered by direct C
  ABI calls with pre-allocated column/parameter buffers, which Rust handles
  cleanly without any external dependencies.
- A driver crate tests its `Backend` impl directly and adds FFI-level
  integration tests that call the generated C ABI entry points; those live in
  the driver's repository, not here.

### Benchmarks

Core has a Criterion fetch-throughput benchmark in `benches/fetch_throughput.rs`
(in-memory, no backend):

```bash
cargo bench
```

`BENCH_ROWS` overrides the row count.

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
      -> panic_safe() wrapper                  # catches panics, manages diagnostics
        -> as_handle_ref::<ConnectionHandle>() # validates handle tag, returns typed &mut
          -> handle.diagnostics.clear()        # spec: clear diagnostics at start of each call
          -> validation checks                 # 08002, HY090 -- per ODBC spec
          -> utf16_to_string(...)              # convert UTF-16 input to Rust String
          -> merge_dsn_params(...)             # parse "Key=Value;..."; if DSN= is present, resolve its keys from odbc.ini (explicit values win) and re-parse
          -> B::connect(&params)               # Backend trait method (database-specific)
          -> handle.connection = Some(conn)    # store result in handle
          -> apply_pending_autocommit::<B>(..) # apply a SQL_ATTR_AUTOCOMMIT set before connect; tears the connection down on failure
          -> write_utf16(...)                  # echo connection string to output buffer
```

### Key design decisions

- **Two traits**: `Backend` (creates connections/statements) and `StatementBackend` (iterates results). Split to separate lifecycle from cursor operations.
- **Handle tags**: Every handle has a `#[repr(C)] HandleHeader { tag: u32 }` as its first field. `as_handle_ref<T>()` checks the tag before casting raw pointers. This is the primary safety mechanism at the FFI boundary.
- **`panic_safe`**: Wraps every FFI function. Uses `AssertUnwindSafe` + `catch_unwind`. On error, pushes to the handle's diagnostic queue and returns the appropriate `SqlReturn`.
- **W-only exports**: Only Unicode (W-suffix) ODBC functions are exported. The Driver Manager translates ANSI calls automatically.
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
| `types/result_cols.rs` | `TablesResultCol`, `ColumnsResultCol`, `PrimaryKeysResultCol`, `ForeignKeysResultCol` |
| `types/connect_params.rs` | `ConnectParams` — ODBC connection string parser |
| `types/col_attr.rs` | `ColAttrValue` and column attribute logic for `SQLColAttributeW` |
| `types/cursor_behavior.rs` | `CursorBehavior` — the `SQL_CB_*` cursor behaviour `SQLEndTran` applies, declared by the backend and reported by `SQLGetInfoW` |
| `types/column_size.rs` | Shared ODBC column-size formulas (`catalog_column_size`/`column_size`); keeps declared vs maximum precision distinct |
| `types/info_type_shape.rs` | The `SQLGetInfo` spec's per-`InfoType` return-value shape, transcribed for the conformance test |
| `types/redacted.rs` | `Redacted<T>` — `Debug` wrapper that prints `*****` for sensitive fields (e.g. passwords) |
| `column_value.rs` | `write_column_value()` — core data marshalling for `SQLGetData` (NULL, truncation, type coercion) |
| `synthetic.rs` | `SyntheticStatement` — in-memory result set for `SQLGetTypeInfo` and catalog functions |
| `conformance.rs` | Shared support for the `SQLGetInfoW` info-type conformance test (return shape + Driver-Manager-safe value), reused by core and by driver test suites |
| `escape.rs` | ODBC escape-sequence translation (`{fn}`, `{d/t/ts}`, `{oj}`, `{escape}`); a shared scanner with a per-backend `EscapeDialect` |
| `errors.rs` | `OdbcError` with SQLSTATE mapping and `SqlReturn` conversion |
| `diagnostics.rs` | Per-handle diagnostic queue (`SQLGetDiagRecW` reads from here) |
| `handles.rs` | `EnvironmentHandle<B>`, `ConnectionHandle<B>`, `StatementHandle<B>`, alloc/free, tag validation |
| `utf16.rs` | `utf16_to_string`, `write_utf16` (ODBC uses UTF-16LE) |
| `panic.rs` | `panic_safe` wrapper |
| `logging.rs` | `init_logging()` via tracing, configured by `ODBC_LOG_LEVEL` / `ODBC_LOG_FILE` |
| `function_id.rs` | `FunctionId` enum + `function_id_from_raw()` for `SQL_API_*` constants |
| `ffi/handle.rs` | `sql_alloc_handle<B>`, `sql_free_handle<B>`, `sql_free_stmt<B>` |
| `ffi/env.rs` | `sql_set_env_attr<B>`, `sql_get_env_attr<B>` |
| `ffi/connect.rs` | `sql_driver_connect_w<B>`, `sql_browse_connect_w<B>`, `sql_connect_w<B>`, `sql_disconnect<B>`, `sql_native_sql_w<B>`; `merge_dsn_params` (DSN resolution) |
| `ffi/connect_attr.rs` | `sql_set_connect_attr_w<B>`, `sql_get_connect_attr_w<B>` |
| `ffi/diag.rs` | `sql_get_diag_rec_w<B>`, `sql_get_diag_field_w<B>` |
| `ffi/cursor.rs` | `sql_num_result_cols<B>`, `sql_row_count<B>`, `sql_more_results<B>`, `sql_close_cursor<B>`, `sql_cancel<B>`, `sql_get_cursor_name_w<B>`, `sql_set_cursor_name_w<B>`, `sql_bulk_operations<B>`, `sql_set_pos<B>` |
| `ffi/execute.rs` | `sql_exec_direct_w<B>`, `sql_prepare_w<B>`, `sql_execute<B>` |
| `ffi/fetch.rs` | `sql_fetch<B>`, `sql_fetch_scroll<B>`, `sql_get_data<B>` |
| `ffi/metadata.rs` | `sql_describe_col_w<B>`, `sql_col_attribute_w<B>`, `sql_tables_w<B>`, `sql_columns_w<B>`, `sql_primary_keys_w<B>`, `sql_foreign_keys_w<B>`, `sql_statistics_w<B>`, `sql_special_columns_w<B>`, `sql_procedures_w<B>`, `sql_procedure_columns_w<B>`, `sql_column_privileges_w<B>`, `sql_table_privileges_w<B>` |
| `ffi/params.rs` | `sql_bind_parameter<B>`, `sql_num_params<B>`, `sql_describe_param<B>`, `sql_put_data<B>`, `sql_param_data<B>` |
| `ffi/bind.rs` | `sql_bind_col<B>` |
| `ffi/stmt_attr.rs` | `sql_set_stmt_attr_w<B>`, `sql_get_stmt_attr_w<B>` |
| `ffi/info.rs` | `sql_get_info_w<B>`, `sql_get_type_info<B>`, `sql_get_functions<B>` |
| `ffi/tran.rs` | `sql_end_tran<B>` |
| `ffi/setup.rs` | `config_dsn_w` (ODBC installer entry point) |
| `ffi/mod.rs` | `ffi` submodule declarations |
| `forward_ffi.rs` | `forward_ffi!` macro — generates the 73 C ABI entry points for a backend |
| `test_utils.rs` | Shared test infrastructure (`MockBackend`, `MockFailBackend`) |

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
| `lib.rs` | Invokes `stackable_odbc_core::forward_ffi!(crate::backend::XyzBackend)` — generates all 73 C ABI entry points |
| `type_conversion.rs` | Converts backend-native column values to `ColumnValue` |
| `escape_dialect.rs` | The backend's `EscapeDialect` for core's escape-sequence translator (identifier quoting, `{fn}` name mapping) |
| `ffi_integration_tests.rs` | FFI-level integration tests that call the C ABI entry points directly |
