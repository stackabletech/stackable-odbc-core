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

This is a reusable crate to help develop ODBC drivers in Rust.

If you don't know what ODBC is, then this repo/crate will probably not be of interest to you.

There are other libraries for ODBC in Rust like [odbc-sys](https://crates.io/crates/odbc-sys) but they all focus on ODBC clients.
This library is exclusively meant to fill the gap to write actual _ODBC drivers_.
Writing a driver is obviously very specific to your target datastore but there is a lot of common stuff that this library handles:

- Hands out and validates handles
- Converts every string to and from UTF-16
- Reports errors in the exact format the standard demands
- Copies values into buffers the application supplied (and handles errors gracefully)
- ...and more

`stackable-odbc-core` is that shared part.
You just need to supply the part that is really about your database:

- How to connect and authenticate
- How to run a query, read rows back, map types onto ODBC's and so on...

A macro then generates all the necessary C plumbing the standard requires.

This is a library rather than a driver you can load on its own.
A working driver is this crate plus a backend, and [stackable-odbc-sqlite](https://github.com/stackabletech/stackable-odbc-sqlite) is the smallest complete example of one.

## What you get

- **We handle (sic!) the Handles.**
  If you don't know what a Handle is in ODBC-land, you're lucky.
  A driver gets handed various Handles (e.g. `SQLHANDLE`) which are basically just pointers to memory holding its state.
  Ours is a slot number plus a counter looked up in our own table, so the pointer the application passed is never followed and a double free is a clean error instead of memory corruption.

- **Two threads can share one connection safely.**
  The standard requires it and many drivers leave it to the Driver Manager instead.
  This crate handles it correctly by using one lock per connection, shared with its queries, so there is no lock ordering left to get wrong.
  `SQLCancel` takes no lock at all, because cancelling a slow query must not wait for the query it is cancelling.

- **The query timeout covers waiting for rows, not just sending the query.**
  A database can answer with the column names immediately and take much longer to produce the first row, so `SQL_ATTR_QUERY_TIMEOUT` runs during `SQLFetch` too.

- **Core builds the catalog answers.**
  Return ordinary Rust structs with named fields and core puts the columns in the order the standard dictates, sorts the rows and normalises identifier case.
  You cannot get the column order or count wrong.

- **Value conversion is already done.**
  All three of the standard's conversion tables are implemented: character, binary and numeric, down to the interval rows and the optional `01S07` warning for rounded-away fractional seconds.

- **Windows is a first-class target.**
  Its Driver Manager is stricter than unixODBC and it fails quietly.
  Getting this correct is annoying.
  The known traps are handled and [AGENTS.md](https://github.com/stackabletech/stackable-odbc-core/blob/main/AGENTS.md) has the checklist.

- **Checked by more than unit tests.**
  Miri catches undefined behaviour and leaked handles, loom re-runs the locking code under every thread interleaving, and cargo-fuzz throws random input at the buffer copies under AddressSanitizer.
  All three run on every pull request, alongside the unit tests on Linux and Windows.

## Writing a driver

```bash
cargo new --lib stackable-odbc-xyz
cargo add stackable-odbc-core
```

Implement `Backend` and `StatementBackend` for your database, then generate the C ABI in `lib.rs`:

```rust,ignore
stackable_odbc_core::forward_ffi!(crate::backend::XyzBackend);
```

That one line expands to every exported `SQL*` entry point, plus `ConfigDSNW` on Windows.

Most of `Backend` is one-line capability declarations such as `supports_catalogs` and `identifier_case`.
None of them are defaulted, because a default would be a claim about your database that nobody ever checked, and a wrong one is invisible.
`StatementBackend` is the opposite and has no required methods, so you override only what your backend supports.

Don't look the list up.
Write the four associated types, run `cargo check`, and the compiler names what is still missing.

[AGENTS.md](https://github.com/stackabletech/stackable-odbc-core/blob/main/AGENTS.md) has the full walkthrough: how a call flows through the layers, what each capability method means, and the catalog, descriptor and Windows rules.

## Conformance

ODBC 3.80 is implemented at the `SQL_OIC_CORE` level, which is the base of the standard's three interface-conformance levels and the one an application may assume of any driver.
All four handle types and all five descriptor functions work, and a descriptor can be shared between queries on a connection.

This is a Unicode driver, so anything taking or returning a string is exported only in its wide form (`SQLConnectW`) and the Driver Manager translates for ANSI applications.
Functions with no strings in their signature, such as `SQLFetch`, have one spelling and are exported unsuffixed.

`CORE_EXPORTED_FUNCTIONS` in `src/function_id.rs` is the authoritative list, pinned by a guard test.
The deprecated ODBC 2.x functions are absent on purpose: the Driver Manager already emulates them on top of the modern ones.
`SQLExtendedFetch` is the one the Driver Manager does not map, so core exports it.

## Limits

Each of these limits is actually reported back to an application that tries to use one of these features so they can react to it.

| Not supported | What the application sees |
|---|---|
| Scrollable cursors | `SQL_SO_FORWARD_ONLY`; `SQLFetchScroll` takes `SQL_FETCH_NEXT` and rejects the rest with `HY106` |
| Block cursors | `SQL_ATTR_ROW_ARRAY_SIZE` fixed at 1, returning 1 with an `01S02` warning; `SQL_GD_BLOCK` never reported |
| Bookmarks, automatic parameter metadata | `SQL_ATTR_AUTO_IPD` stays `SQL_FALSE` |
| Async | `SQL_AM_NONE`; `SQL_ATTR_ASYNC_ENABLE` is refused, not ignored |

Async here means the calling thread, not the shape of the results.
Rows still arrive one `SQLFetch` at a time, the query timeout still bounds a slow query, and `SQLCancel` still interrupts one.
`Backend` is synchronous too, so a driver built on an async client library bridges to it internally, for example with a current-thread tokio runtime and `block_on`.

## Drivers built on this crate

Each is a separate crate supplying only its `Backend` and `StatementBackend` implementation.

- [stackable-odbc-trino](https://github.com/stackabletech/stackable-odbc-trino), an ODBC driver for [Trino](https://trino.io/).
- [stackable-odbc-sqlite](https://github.com/stackabletech/stackable-odbc-sqlite), a SQLite driver, used as a worked example and as the test driver for the framework itself.

## Resources

- [ODBC API reference](https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/odbc-api-reference?view=sql-server-ver16), the authoritative specification.
  It is the most detailed source and still not an easy read.
- [Header files](https://github.com/microsoft/ODBC-Specification/blob/master/Windows/inc/sql.h) for the unreleased ODBC 4 standard, mostly valid for the older ones too.
- [odbc-sys](https://github.com/pacman82/odbc-sys), the ODBC type definitions this crate builds on. Thank you!

## Getting help

- [GitHub Discussions](https://github.com/orgs/stackabletech/discussions) for questions
- [Discord](https://discord.gg/7kZ3BNnCAF) to talk to us
- [Issues](https://github.com/stackabletech/stackable-odbc-core/issues) for bugs, and [SECURITY.md](SECURITY.md) for anything security-related

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for building from source, running the tests, and how the repository is laid out.
[CHANGELOG.md](CHANGELOG.md) records what changed in each release.

## License

Apache-2.0.
See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
