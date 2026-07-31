<!-- markdownlint-disable MD041 MD033 -->

<p align="center">
  <img width="150" src="./.readme/static/borrowed/Icon_Stackable.svg" alt="Stackable Logo"/>
</p>

<h1 align="center">Stackable ODBC Core</h1>

<p align="center"><em>The database-independent half of an ODBC driver, in Rust.</em></p>

[![Build and Test](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/build.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/build.yaml)
[![Security Audit](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/security_audit.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/security_audit.yaml)
[![Maintained](https://img.shields.io/badge/Maintained%3F-yes-green.svg)](https://github.com/stackabletech/stackable-odbc-core/graphs/commit-activity)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-green.svg)](https://docs.stackable.tech/home/stable/contributor/index.html)
[![Apache License 2.0](https://img.shields.io/badge/license-Apache--2.0-green)](./LICENSE)
[![ODBC 3.80 Core](https://img.shields.io/badge/ODBC-3.80%20Core-blue)](#conformance-and-scope)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-blue)](#conformance-and-scope)

[Stackable Data Platform](https://stackable.tech/) | [Platform Docs](https://docs.stackable.tech/) | [Discussions](https://github.com/orgs/stackabletech/discussions) | [Discord](https://discord.gg/7kZ3BNnCAF)

`stackable-odbc-core` provides ODBC protocol logic, handle allocation and token
validation, UTF-16 marshalling, diagnostics, panic safety, and the generic
implementations of the ODBC FFI entry points. A concrete driver crate implements
the `Backend` and `StatementBackend` traits, then calls the `forward_ffi!` macro
to export the `SQL*` C ABI entry points, plus `ConfigDSNW` on Windows.

This is a library crate, not a loadable ODBC driver on its own. For the
architecture, the call-flow walkthrough and the spec-compliance rules, see
[AGENTS.md](AGENTS.md).

## Highlights

- **A backend is a trait, not a fork.** Implement `Backend` and
  `StatementBackend`, then one `forward_ffi!` call exports all 59 C ABI entry
  points. Core holds zero database-specific code.

- **Handles are tokens, not pointers.** An application-facing `SQLHANDLE` is a
  generation-tagged slot index, validated against a driver-owned registry
  without ever dereferencing the pointer the application passed. Freeing bumps
  the slot's generation, so a use-after-free or a double-free is *rejected*
  rather than undefined.

- **Thread-safe by construction.** Handle contents are guarded by
  per-connection lock groups, so one acquisition covers a statement and its
  parent connection and there is no ordering to get wrong. `SQLAllocHandle`
  requires this ("drivers must therefore support safe, multithread access to
  this information"); it is usually left to the Driver Manager. `SQLCancel` is
  deliberately lock-free, so cancelling a query never waits on the query it was
  asked to cancel.

- **A query timeout that bounds the fetch, not just the execute.**
  `SQL_ATTR_QUERY_TIMEOUT` is armed at `SQLFetch` as well, because a data source
  is free to answer with column metadata long before it has computed a row.
  Measured against a live Trino coordinator under a two-second deadline: the
  `SQLExecDirect` returned in 0.1 s and the following `SQLFetch` took 24.6 s, so
  an execute-only timer bounded nothing.

- **Core owns the catalog result sets.** The ten catalog hooks return typed row
  structs, and core applies the spec-mandated ordering, the column layout and
  the `SQL_ATTR_METADATA_ID` identifier normalisation. A backend fills named
  fields, so it cannot get column order or count wrong, and a column added to a
  spec result set is a core-only change.

- **All three C-to-SQL conversion tables transcribed.** Character, binary and
  numeric, including the interval row and its optional `01S07` fractional
  truncation warning.

- **Verified, not just tested.** 1335 unit tests, plus Miri for undefined
  behaviour and leaked handles, loom for the lock discipline under every
  interleaving, and cargo-fuzz under AddressSanitizer on the marshalling hot
  paths. All four run on every pull request.

- **Windows is a real target.** `extern "system"` resolves to the correct ABI on
  each platform, the ODBC installer entry point `ConfigDSNW` is exported behind
  `#[cfg(windows)]`, and the Windows Driver Manager's stricter requirements are
  handled explicitly: the pre-connect `SQL_DRIVER_ODBC_VER` query, the complete
  3.x function bitmap, and *not* exporting the deprecated ODBC 2.x functions, so
  the Driver Manager's better-informed mapping wins. See the [Windows Driver
  Manager compatibility
  checklist](AGENTS.md#windows-driver-manager-compatibility-checklist).

## Conformance and scope

ODBC 3.80, reporting `SQL_OIC_CORE` interface conformance. All four handle types
allocate and free. All five descriptor functions are implemented, including
explicitly allocated descriptors shared across statements on a connection. 59 C
ABI entry points are exported; Appendix G's deprecated ODBC 2.x functions are
deliberately not among them, because exporting one suppresses the Driver
Manager's own mapping rather than adding a capability.

Deliberately out of scope:

- **Forward-only cursors** (`SQL_SO_FORWARD_ONLY`). `SQLFetchScroll` accepts
  `SQL_FETCH_NEXT` and rejects every other orientation with `HY106`.
- **No block cursors.** `SQL_ATTR_ROW_ARRAY_SIZE` is fixed at 1 (a larger value
  is substituted back with `01S02`), so `SQL_GD_BLOCK` is never reported.
- **No bookmark records**, and no automatic population of the IPD:
  `SQL_ATTR_AUTO_IPD` is `SQL_FALSE`.
- **No async execution.** `Backend` is synchronous; a driver wrapping an async
  client library bridges to it internally.

## Creating a new driver

Adding a new database backend requires three steps:

1. **Create the crate** and add `stackable-odbc-core` as a dependency:

   ```toml
   [dependencies]
   stackable-odbc-core = "0.0.1"
   ```

2. **Implement the `Backend` and `StatementBackend` traits** in `backend.rs`:

   ```rust,ignore
   use stackable_odbc_core::backend::{Backend, StatementBackend};
   use stackable_odbc_core::errors::OdbcError;
   use stackable_odbc_core::types::ConnectParams;

   pub struct XyzBackend;
   pub struct XyzConnection;
   pub struct XyzStatement;

   #[derive(Debug, snafu::Snafu)]
   pub enum XyzError { /* ... */ }

   // `Backend::Error` needs both directions: core converts a backend error into
   // a diagnostic, and a defaulted trait body constructs one and names
   // `Self::Error`.
   impl From<XyzError> for OdbcError { /* map to a SQLSTATE */ }
   impl From<OdbcError> for XyzError { /* wrap */ }

   impl Backend for XyzBackend {
       type Connection = XyzConnection;
       type Statement = XyzStatement;
       type Error = XyzError;

       fn connect(params: &ConnectParams) -> Result<XyzConnection, XyzError> { todo!() }
       // ...and the rest of the required items: see below.
   }

   impl StatementBackend for XyzStatement {
       type Error = XyzError;
       // every method is defaulted; override the ones this backend supports
   }
   ```

   The sketch above is deliberately incomplete — `Backend` has 3 associated
   types and 35 required methods, most of them one-line *capability
   declarations* (`supports_catalogs`, `identifier_case`, `sql_conformance`, …).
   They are required rather than defaulted because each states a falsifiable
   fact about the data source that core cannot know, and a wrong default is
   invisible: the compiler asking is the point. `StatementBackend` has one
   associated type and no required methods.

   The compiler lists exactly what is missing, so the practical route is to
   write the three associated types and let `cargo check` drive the rest.

3. **Generate the FFI entry points** in `lib.rs` using the `forward_ffi!` macro:

   ```rust
   stackable_odbc_core::forward_ffi!(crate::backend::XyzBackend);
   ```

   This single line expands to every `#[unsafe(no_mangle)] pub unsafe extern
   "system"` C ABI entry point (`SQL*` functions), plus `ConfigDSNW` on
   Windows, each
   forwarding to the corresponding generic implementation in `stackable-odbc-core`.

When adding a new ODBC function to the framework later, add one entry to
`src/forward_ffi.rs` and every driver automatically exports it. See
[AGENTS.md](AGENTS.md#adding-a-new-odbc-function) for the full checklist.

## Testing

```bash
cargo test           # unit tests (no running Driver Manager needed)
cd bench && cargo bench   # Criterion fetch-throughput benchmark (own crate)
```

`stackable-odbc-core` links against unixODBC through
[odbc-sys](https://github.com/pacman82/odbc-sys), so the dev libraries must be
installed to build and test (no DSN or running Driver Manager is required):

```bash
sudo apt-get install unixodbc-dev   # Debian/Ubuntu
```

`stackable-odbc-core` holds all the raw-pointer marshalling, so it is checked by
Miri (undefined behaviour + leaks) and cargo-fuzz (AddressSanitizer) on every PR:

```bash
MIRIFLAGS="-Zmiri-disable-isolation" \
  cargo +nightly miri test -p stackable-odbc-core --lib -- --skip proptest

cargo install cargo-fuzz
cargo +nightly fuzz run utf16
cargo +nightly fuzz run column_value
```

The lock discipline is modelled with [loom](https://github.com/tokio-rs/loom),
which explores the thread interleavings a test run happens not to produce:

```bash
RUSTFLAGS="--cfg loom" cargo test --lib loom_tests
```

See [AGENTS.md](AGENTS.md#testing) and [`fuzz/README.md`](fuzz/README.md) for details.

Every pull request runs the unit tests on Linux and Windows, plus Miri, loom and
the fuzz smoke targets.

## Drivers built on this crate

Drivers are published as separate crates, each supplying only its
`Backend`/`StatementBackend` implementation:

- [stackable-odbc-trino](https://github.com/stackabletech/stackable-odbc-trino)
  — an ODBC driver for [Trino](https://trino.io/).
- [stackable-odbc-sqlite](https://github.com/stackabletech/stackable-odbc-sqlite)
  — a SQLite driver, used as a worked example and as the test driver for the
  framework itself.

## Resources

- [ODBC API / Documentation](https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/odbc-api-reference?view=sql-server-ver16):
  the authoritative reference; the most detailed, still not easy to read.
- [Header files](https://github.com/microsoft/ODBC-Specification/blob/master/Windows/inc/sql.h):
  for the unreleased ODBC 4 standard, but mostly valid for older ones too.
- [odbc-sys](https://github.com/pacman82/odbc-sys): ODBC definitions in Rust.

## License

Apache-2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
