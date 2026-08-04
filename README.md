<!-- markdownlint-disable MD041 MD033 -->

<p align="center">
  <img width="150" src="./.readme/static/borrowed/Icon_Stackable.svg" alt="Stackable Logo"/>
</p>

<h1 align="center">Stackable ODBC Core</h1>

<p align="center"><em>The database-independent half of an ODBC driver, in Rust.</em></p>

[![Build and Test](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/build.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/build.yaml)
[![Security Audit](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/security_audit.yaml/badge.svg)](https://github.com/stackabletech/stackable-odbc-core/actions/workflows/security_audit.yaml)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-green.svg)](CONTRIBUTING.md)
[![Apache License 2.0](https://img.shields.io/badge/license-Apache--2.0-green)](./LICENSE)
[![ODBC 3.80 Core](https://img.shields.io/badge/ODBC-3.80%20Core-blue)](#conformance)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20Windows-blue)](#conformance)

[Stackable Data Platform](https://stackable.tech/) | [Platform Docs](https://docs.stackable.tech/) | [Discussions](https://github.com/orgs/stackabletech/discussions) | [Discord](https://discord.gg/7kZ3BNnCAF)

## What is this?

ODBC is the standard way desktop tools talk to a database. Excel, Tableau,
Power BI and Python's pyodbc all speak it. Each database needs its own *driver*,
a shared library the tool loads that translates those standard calls into
whatever the database actually speaks.

Writing one is a large job, and most of it has nothing to do with your database.
The driver has to hand out and validate handles, convert every string to and
from UTF-16, report errors in the exact format the standard demands, copy values
into buffers the application supplied, and not crash when the application lies
about how big those buffers are.

`stackable-odbc-core` is that shared part, written once. You supply only what is
actually about your database, which is how to connect, how to run a query and
how to read a row back. One macro then generates the C entry points the standard
requires.

This is a library rather than a driver you can load on its own. A working driver
is this crate plus a backend, and
[stackable-odbc-sqlite](https://github.com/stackabletech/stackable-odbc-sqlite)
is the smallest complete example of one.

## What you get

- **A database backend is two traits and one macro.** Implement `Backend` and
  `StatementBackend`, then call `forward_ffi!`. The compiler names everything
  still missing, so there is no list to work through by hand. Core holds no
  database-specific code, so a driver never forks or patches it.

- **A handle is a ticket number, not a memory address.** ODBC hands the
  application a `SQLHANDLE` that refers to a connection or a running query. The
  obvious implementation is a raw pointer, and then an application that frees a
  handle twice, or uses one after freeing it, corrupts the driver's memory.
  That is undefined behaviour, so the program may crash, or may quietly return a
  wrong answer.

  Here a handle is a slot number plus a counter. The driver looks it up in its
  own table and never follows the pointer the application passed. Freeing bumps
  that slot's counter, so every ticket still referring to it stops matching.
  Use-after-free and double-free become a clean "invalid handle" error rather
  than memory corruption.

- **Two threads can share one connection safely.** The standard requires it,
  because "drivers must therefore support safe, multithread access to this
  information", and many drivers leave it to the Driver Manager instead. Each
  connection here has one lock, shared with every query started on it, so a call
  touching both a query and its connection takes a single lock. That leaves no
  lock ordering to get wrong, which is the usual way a driver deadlocks.
  `SQLCancel` takes no lock at all, because cancelling a slow query must not
  wait for the query it is cancelling.

- **The query timeout covers waiting for rows, not just sending the query.** An
  application sets `SQL_ATTR_QUERY_TIMEOUT` to say "give up after N seconds".
  Most drivers run that clock only while the query is being submitted, but a
  database can answer with the column names immediately and then take much
  longer to produce the first row. A timer covering only submission bounds
  nothing, so this one runs during `SQLFetch` as well.

- **Core builds the catalog answers.** For "what tables exist?" and its
  relatives, the standard dictates the exact columns, their order, and how the
  rows are sorted. A backend returns ordinary Rust structs with named fields,
  and core puts the columns in order, sorts the rows and normalises identifier
  case. You cannot get the column order or count wrong because you never write
  them, and a column added to one of those result sets is a change in core
  alone.

- **Value conversion is already done.** When an application supplies a parameter
  as text and asks for it to be treated as a number, the standard has three
  large tables saying exactly what each conversion does, down to which warning
  to raise when precision is lost. All three are implemented: character, binary
  and numeric, including the interval rows and the optional `01S07` warning for
  fractional seconds that were rounded away.

- **Windows is a first-class target.** Its Driver Manager is stricter than
  unixODBC and it fails quietly, so missing one requirement stops a feature
  working with no error to explain why. The known traps are handled: answering
  the version query it makes before connecting, reporting the complete function
  list it uses to build its dispatch table, and not exporting the deprecated
  ODBC 2.x functions, because exporting one replaces the Driver Manager's own
  better implementation with yours.

- **Checked by more than unit tests.** Three tools cover what ordinary tests
  cannot. Miri runs the code in an interpreter that detects undefined behaviour
  and leaked handles, loom re-runs the locking code under every thread
  interleaving rather than the one that happened to occur, and cargo-fuzz throws
  random input at the buffer-copying code under AddressSanitizer. All three run
  on every pull request, alongside the unit tests on Linux and Windows.

## Writing a driver

```bash
cargo new --lib stackable-odbc-xyz
cargo add stackable-odbc-core
```

Implement `Backend` and `StatementBackend` for your database, then generate the
C ABI in `lib.rs`:

```rust,ignore
stackable_odbc_core::forward_ffi!(crate::backend::XyzBackend);
```

That one line expands to every exported `SQL*` entry point, plus `ConfigDSNW` on
Windows, each forwarding to the generic implementation in this crate.

`Backend` has four associated types and a body of required methods, but most of
them are one-line capability declarations such as `supports_catalogs`,
`identifier_case` and `sql_conformance`, each answering a single question about
your database. They are required rather than defaulted on purpose: any default
core supplied would be a claim about your database that nobody ever checked, and
a wrong one is invisible, because the driver would confidently tell applications
something untrue and nothing would complain. `StatementBackend` is the opposite,
with one associated type and no required methods, so you override only what your
backend supports.

In practice you do not look the list up. Write the four associated types, run
`cargo check`, and the compiler names what is still missing.

[AGENTS.md](https://github.com/stackabletech/stackable-odbc-core/blob/main/AGENTS.md)
has the full walkthrough: how a call flows through the layers, what each
capability method means, the catalog and descriptor rules, and the Windows
Driver Manager checklist.

## Conformance

This implements ODBC 3.80 at the `SQL_OIC_CORE` level, the base of the
standard's three interface-conformance levels and the one an application may
assume of any driver. All four handle types can be allocated and freed, and all
five descriptor functions work. Descriptors are the standard's own way of
describing a bound column or parameter, and one can be shared between queries on
a connection.

`CORE_EXPORTED_FUNCTIONS` in `src/function_id.rs` is the authoritative list of
what is exported, and a guard test pins every entry to a symbol that exists. The
deprecated ODBC 2.x functions are left out, because the Driver Manager already
emulates them on top of the modern ones and usually does it better than a driver
would, so exporting your own version switches that off rather than adding
anything. `SQLExtendedFetch` is the exception the Driver Manager does not map,
so core exports it.

## Limits

Each of these is reported to the application as unsupported rather than quietly
ignored, so a tool can react instead of trusting a wrong answer.

- **Results are read front to back only** (`SQL_SO_FORWARD_ONLY`), so there is
  no jumping to a row and no going backwards. `SQLFetchScroll` accepts
  `SQL_FETCH_NEXT` and rejects every other direction with `HY106`.
- **One row at a time.** There are no block cursors, so
  `SQL_ATTR_ROW_ARRAY_SIZE` is fixed at 1. Asking for more returns 1 with an
  `01S02` warning, and `SQL_GD_BLOCK` is never reported.
- **No bookmarks**, which are saved row positions an application can return to
  later, and no automatic population of parameter metadata, so
  `SQL_ATTR_AUTO_IPD` stays `SQL_FALSE`.
- **No async.** `Backend` is synchronous. A driver built on an async client
  library bridges to it internally, for example with a current-thread tokio
  runtime and `block_on`.

## Drivers built on this crate

Each driver is a separate crate supplying only its `Backend` and
`StatementBackend` implementation.

- [stackable-odbc-trino](https://github.com/stackabletech/stackable-odbc-trino),
  an ODBC driver for [Trino](https://trino.io/).
- [stackable-odbc-sqlite](https://github.com/stackabletech/stackable-odbc-sqlite),
  a SQLite driver, used as a worked example and as the test driver for the
  framework itself.

## Resources

- [ODBC API reference](https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/odbc-api-reference?view=sql-server-ver16),
  the authoritative specification. It is the most detailed source and still not
  an easy read.
- [Header files](https://github.com/microsoft/ODBC-Specification/blob/master/Windows/inc/sql.h)
  for the unreleased ODBC 4 standard, mostly valid for the older ones too.
- [odbc-sys](https://github.com/pacman82/odbc-sys), the ODBC type definitions
  this crate builds on.

## Getting help

- [GitHub Discussions](https://github.com/orgs/stackabletech/discussions) for
  questions
- [Discord](https://discord.gg/7kZ3BNnCAF) to talk to us
- [Issues](https://github.com/stackabletech/stackable-odbc-core/issues) for
  bugs, and [SECURITY.md](SECURITY.md) for anything security-related

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for building from source, running the
tests, and how the repository is laid out. [CHANGELOG.md](CHANGELOG.md) records
what changed in each release.

## License

Apache-2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
