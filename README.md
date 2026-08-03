<!-- markdownlint-disable MD041 MD033 -->

<p align="center">
  <img width="150" src="./.readme/static/borrowed/Icon_Stackable.svg" alt="Stackable Logo"/>
</p>

<h1 align="center">Stackable ODBC Core</h1>

<p align="center"><em>The database-independent half of an ODBC driver, in Rust.</em></p>

[![Build and Test](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/build.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/build.yaml)
[![Security Audit](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/security_audit.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/security_audit.yaml)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-green.svg)](https://docs.stackable.tech/home/stable/contributor/index.html)
[![Apache License 2.0](https://img.shields.io/badge/license-Apache--2.0-green)](./LICENSE)
[![ODBC 3.80 Core](https://img.shields.io/badge/ODBC-3.80%20Core-blue)](#conformance-and-scope)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-blue)](#conformance-and-scope)

[Stackable Data Platform](https://stackable.tech/) | [Platform Docs](https://docs.stackable.tech/) | [Discussions](https://github.com/orgs/stackabletech/discussions) | [Discord](https://discord.gg/7kZ3BNnCAF)

**What is this?** ODBC is the standard way desktop tools — Excel, Tableau,
Power BI, Python's pyodbc — talk to a database. Each database needs its own
*driver*: a shared library the tool loads, which translates those standard calls
into whatever that database actually speaks.

Most of a driver is the same work every time, and none of it is about your
database: handing out and validating handles, converting strings to and from
UTF-16, reporting errors in the exact format the standard demands, copying
values into buffers the application supplied, and not crashing when it lies
about their size. `stackable-odbc-core` is that shared part, written once.

You write only the part that *is* about your database — how to connect, how to
run a query, how to read a row — by implementing two Rust traits. One macro then
generates the 59 C functions the standard requires.

This is a library, not a driver you can load on its own. For the architecture,
a walkthrough of how one call flows through the layers, and the
spec-compliance rules, see [AGENTS.md](AGENTS.md).

## Highlights

- **Adding a database means filling in two traits.** Implement `Backend` and
  `StatementBackend`; the compiler tells you exactly what is still missing until
  you are done. Then one line — `forward_ffi!` — generates all 59 C entry points
  the standard requires. Core contains no database-specific code at all, so you
  never fork it or patch it.

- **A handle is a ticket number, not a memory address.** ODBC gives the
  application a `SQLHANDLE` to refer to a connection or a running query. The
  obvious way to build one is a raw pointer — and then an application that uses
  a handle after freeing it, or frees it twice, corrupts the driver's memory.
  That is *undefined behaviour*: the program may crash, or may quietly return a
  wrong answer, which is worse.

  Here a handle is a ticket: a slot number plus a counter. The driver looks the
  ticket up in its own table and never follows the pointer the application
  passed. Freeing bumps that slot's counter, so every ticket still referring to
  it stops matching. Use-after-free and double-free become a clean "invalid
  handle" error instead of memory corruption.

- **Two threads can share one connection safely.** ODBC requires this — "drivers
  must therefore support safe, multithread access to this information" — and
  most drivers leave it to the Driver Manager instead. Here each connection has
  one lock, shared with every query started on it, so a call touching both takes
  a single lock and there is no ordering rule to get wrong. (Getting lock
  ordering wrong is how programs deadlock.) `SQLCancel` deliberately takes no
  lock at all: cancelling a slow query must not wait for the very query it is
  cancelling.

- **The query timeout covers waiting for rows, not just sending the query.** An
  application sets `SQL_ATTR_QUERY_TIMEOUT` to say "give up after N seconds".
  Most drivers run that clock only while the query is being submitted — but a
  database may reply "here are the column names" instantly and then take half a
  minute to produce the first row. Measured against a real Trino server with a
  two-second deadline: submitting returned in 0.1 s, and fetching the first row
  took 24.6 s. A timer covering only submission bounds nothing, so this one runs
  during `SQLFetch` too.

- **Core builds the "what tables exist?" answers.** For questions like that, the
  standard dictates the exact columns, their order, and how the rows must be
  sorted. Your backend returns ordinary Rust structs with named fields; core
  puts the columns in order, sorts the rows, and normalises identifier case. You
  cannot get the column order or count wrong, because you never write them — and
  when a column is added to one of those result sets, only core changes.

- **Value conversion is already done.** When an application hands over a
  parameter as text and says "treat this as a number", the standard has three
  large tables specifying exactly what each conversion does, down to which
  warning to raise when precision is lost. All three are implemented — character,
  binary and numeric — including the awkward interval row and its optional
  `01S07` "your fractional seconds were rounded" warning.

- **Checked by more than unit tests.** Fifteen hundred-odd unit tests — run
  `cargo test` for today's figure rather than trusting this sentence — plus three
  tools that
  catch what ordinary tests cannot: Miri runs the code in an interpreter that
  detects undefined behaviour and leaked handles, loom re-runs the locking code
  under every possible thread interleaving rather than the one that happened to
  occur, and cargo-fuzz throws random input at the buffer-copying code under
  AddressSanitizer. All four run on every pull request.

- **Windows is a first-class target.** Its Driver Manager is much stricter than
  Linux's unixODBC, and it fails *quietly*: miss one requirement and a feature
  simply stops working with no error to explain why. The known traps are handled
  — answering the version query it makes *before* connecting, reporting the
  complete function list it uses to build its dispatch table, and deliberately
  *not* exporting the old ODBC 2.x functions, since exporting one replaces the
  Driver Manager's own better implementation with yours. See the [Windows Driver
  Manager compatibility
  checklist](AGENTS.md#windows-driver-manager-compatibility-checklist).

## Conformance and scope

This implements ODBC 3.80 at the `SQL_OIC_CORE` level — the base of the
standard's three interface-conformance levels, and the one applications can
assume of any driver. All four handle types can be allocated and freed, and all
five *descriptor* functions work (descriptors are the standard's own way of
describing a bound column or parameter, and can be shared between queries on one
connection).

Every entry point in `CORE_EXPORTED_FUNCTIONS` (`src/function_id.rs`) is exported —
read that list rather than a count, which is what a guard test checks against. The
deprecated ODBC 2.x functions are deliberately left out: the Driver Manager already
emulates them on top of the modern ones, usually better than a driver would, and
exporting your own version switches that off rather than adding anything. The one
exception is `SQLExtendedFetch`, which the Driver Manager does **not** map, so core
exports it.

Deliberately out of scope:

- **Results are read front to back only** (`SQL_SO_FORWARD_ONLY`) — no jumping
  to a row or going backwards. `SQLFetchScroll` accepts `SQL_FETCH_NEXT` and
  rejects every other direction with `HY106`.
- **One row at a time.** No block cursors: `SQL_ATTR_ROW_ARRAY_SIZE` is fixed at
  1 (ask for more and you get 1 back with an `01S02` warning), so `SQL_GD_BLOCK`
  is never reported.
- **No bookmarks** — saved row positions you can return to later — and no
  automatic filling-in of parameter metadata (`SQL_ATTR_AUTO_IPD` is
  `SQL_FALSE`).
- **No async.** `Backend` is synchronous. A driver built on an async client
  library bridges to it internally, for example with a current-thread tokio
  runtime and `block_on`.

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

   The sketch above is deliberately incomplete. `Backend` has 4 associated types
   and 47 required methods, but most are one-line *capability declarations* —
   `supports_catalogs`, `identifier_case`, `sql_conformance` — each answering a
   single yes/no or pick-a-value question about your database.

   They are required rather than optional on purpose. Any default core supplied
   would be a claim about *your* database that nobody ever checked, and a wrong
   one is invisible: your driver would confidently tell applications something
   untrue, and nothing would ever complain. Making the compiler ask is the point.
   `StatementBackend`, by contrast, has one associated type and no required
   methods at all — override only what your backend supports.

   In practice you do not look this list up. Write the four associated types,
   run `cargo check`, and the compiler names exactly what is still missing.

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
