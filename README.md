# stackable-odbc-core

The database-independent half of an ODBC driver. `stackable-odbc-core` provides ODBC
protocol logic, handle allocation and token validation, UTF-16 marshalling,
diagnostics, panic safety, and the generic implementations of the ODBC FFI
entry points. A concrete driver crate implements the `Backend` and
`StatementBackend` traits, then calls the `forward_ffi!` macro to export the
`SQL*` C ABI entry points, plus `ConfigDSNW` on Windows.

This is a library crate, not a loadable ODBC driver on its own.

Both Linux and Windows are first-class targets. `extern "system"` resolves to the
correct ABI on each, the Windows-only ODBC installer entry point (`ConfigDSNW`)
is exported behind `#[cfg(windows)]`, and the Windows Driver Manager's stricter
requirements — the pre-connect `SQL_DRIVER_ODBC_VER` query, the 3.x function
bitmap, `SQL_DROP` passthrough — are handled explicitly rather than left to
chance. See the [Windows Driver Manager compatibility
checklist](AGENTS.md#windows-driver-manager-compatibility-checklist).

For architecture, the call-flow walkthrough, and the spec-compliance rules, see
[AGENTS.md](AGENTS.md).

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

See [AGENTS.md](AGENTS.md#testing) and [`fuzz/README.md`](fuzz/README.md) for details.

Every pull request runs the unit tests, Miri and the fuzz smoke targets.

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

Apache-2.0
