# stackable-odbc-core

The database-independent half of an ODBC driver. `stackable-odbc-core` provides ODBC
protocol logic, handle allocation and tag validation, UTF-16 marshalling,
diagnostics, panic safety, and the generic implementations of the ODBC FFI
entry points. A concrete driver crate implements the `Backend` and
`StatementBackend` traits, then calls the `forward_ffi!` macro to export all 73
C ABI entry points.

This is a library crate, not a loadable ODBC driver on its own. Drivers built on
it are published as separate crates, for example `stackable-odbc-trino` (an ODBC
driver for [Trino](https://trino.io/)) and `stackable-odbc-sqlite` (a SQLite
driver used for development and testing).

For architecture, the call-flow walkthrough, and the spec-compliance rules, see
[AGENTS.md](AGENTS.md).

## Creating a new driver

Adding a new database backend requires three steps:

1. **Create the crate** and add `stackable-odbc-core` as a dependency:

   ```toml
   [dependencies]
   stackable-odbc-core = "0.0.1"
   ```

2. **Implement the `Backend` trait** in `backend.rs`:

   ```rust
   use stackable_odbc_core::backend::{Backend, StatementBackend};

   pub struct XyzBackend;

   impl Backend for XyzBackend {
       // implement connect, disconnect, etc.
   }
   ```

3. **Generate the FFI entry points** in `lib.rs` using the `forward_ffi!` macro:

   ```rust
   stackable_odbc_core::forward_ffi!(crate::backend::XyzBackend);
   ```

   This single line expands to 73 `#[unsafe(no_mangle)] pub unsafe extern
   "system"` C ABI entry points (72 `SQL*` functions plus `ConfigDSNW`), each
   forwarding to the corresponding generic implementation in `stackable-odbc-core`.

When adding a new ODBC function to the framework later, add one entry to
`src/forward_ffi.rs` and every driver automatically exports it. See
[AGENTS.md](AGENTS.md#adding-a-new-odbc-function) for the full checklist.

## Testing

```bash
cargo test           # unit tests (no running Driver Manager needed)
cargo bench          # Criterion fetch-throughput benchmark
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

## Resources

- [ODBC API / Documentation](https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/odbc-api-reference?view=sql-server-ver16):
  the authoritative reference; the most detailed, still not easy to read.
- [Header files](https://github.com/microsoft/ODBC-Specification/blob/master/Windows/inc/sql.h):
  for the unreleased ODBC 4 standard, but mostly valid for older ones too.
- [odbc-sys](https://github.com/pacman82/odbc-sys): ODBC definitions in Rust.

## License

Apache-2.0
