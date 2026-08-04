# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

First release, so this section describes what the crate offers rather than what
changed.

### Added

**The framework.** The database-independent half of an ODBC 3.80 driver, at the
`SQL_OIC_CORE` conformance level, on Linux and Windows. Implement the `Backend`
and `StatementBackend` traits and invoke `forward_ffi!` once, and the crate
generates every C entry point the standard requires. Core carries no
database-specific code, so a driver never has to fork or patch it.

**A handle is a ticket, not an address.** A `SQLHANDLE` is a slot index plus a
generation counter, looked up in a driver-owned table rather than dereferenced.
Freeing bumps that slot's generation, so every token still naming it stops
matching, and use after free becomes a clean `SQL_INVALID_HANDLE` rather than
memory corruption.

**Two threads can share one connection.** Each connection owns a lock group that
its statements and descriptors join, so a call touching a statement and its
connection takes a single lock and no ordering rule is left to get wrong.
`SQLCancel` takes no lock at all, because cancelling a slow query must not wait
for the query it is cancelling.

**Connections and transactions.** The three connect functions parse the
connection string, resolve a `DSN=` key against `odbc.ini` with explicit values
winning, and hand `Backend::connect` a typed `ConnectParams`. Attributes set
before connecting reach that call, because the spec calls setting them early the
interoperable choice. `SQLEndTran` commits or rolls back and applies the cursor
behaviour the backend declares. A driver needing an interactive login implements
`prompt::Prompter`, and core decides whether prompting is permitted from
`SQLDriverConnect`'s DriverCompletion argument.

**Statements and result sets.** Direct and prepared execution, row counts,
multiple result sets, cursor names, and `SQLFetch` over bound columns or
`SQLGetData` chunked through a buffer of any size. The ODBC escape sequences
`{fn}`, `{d}`, `{t}`, `{ts}`, `{oj}` and `{escape}` are translated by a shared
scanner driven by a per-backend `EscapeDialect`, which can rewrite a whole
scalar-function call rather than only its name, so a function whose argument
syntax differs from ODBC's stays translatable. `SQLDescribeParam` answers from
`Backend::describe_param` where a backend can ask the data source, so a client
sizing its buffers from the answer is not left guessing at `VARCHAR`.

**Value conversion is done for you.** `SQLBindParameter` records the C-side
buffer in the APD and the declared SQL type in the IPD, and core converts
between them with all three of the spec's C-to-SQL tables, character, binary
and numeric. The other direction covers the C types the standard defines, and
reports truncation as `01004`, an out-of-range value as `22003` and dropped
precision as `01S07`. Every access through an application pointer is unaligned,
because row-wise binding hands out pointers at arbitrary offsets into a packed
buffer.

**Catalog metadata.** The ten catalog functions are implemented with core owning
the result set: a backend returns typed row structs with named fields, and core
puts the columns in spec order, sorts the rows as each page mandates and places
NULLs per `Backend::null_collation`. `SQL_ATTR_METADATA_ID` normalisation and
the `SQL_ALL_*` enumerations are core's too, so a driver writes no code for
either and cannot get a column order wrong.

**Descriptors.** All five descriptor functions work over the four descriptors a
statement owns, an application can install its own through
`SQL_ATTR_APP_ROW_DESC` or `SQL_ATTR_APP_PARAM_DESC`, and one descriptor can be
shared across statements on a connection. ODBC makes a descriptor *be* the
binding, so a binding assembled through `SQLSetDescField` fetches exactly like
one made with `SQLBindCol`.

**Diagnostics.** Every handle carries its own queue, read through
`SQLGetDiagRec` and `SQLGetDiagField`, and a backend error keeps its native
error code and its causal chain rather than a flattened message string. Each FFI
function's doc comment lists every SQLSTATE in its spec diagnostics table with a
verdict, and guard tests check those lists against the transcribed tables, so a
missing or invented state fails the build.

**Cancellation and query timeouts.** `SQLCancel` on another thread signals the
backend's cancel token and returns without waiting, and a call stopped that way
reports `HY008` rather than whatever the driver's error mapping produced. A
fresh token is minted per execution, so a cancelled statement is usable again.
`SQL_ATTR_QUERY_TIMEOUT` goes to the data source first, and a backend that can
only be cancelled gets a core-side timer reporting `HYT00`, armed at `SQLFetch`
as well as at execution, because a data source may answer with column metadata
long before it has a row.

**Capability reporting.** `SQLGetInfo` answers from required `Backend` methods
wherever the answer is a claim about the data source, and from core only where
the fact is core's own or the spec defines zero as "unknown". A guard test
evaluates the defaults against two backends sharing no declaration and fails on
any answer that does not move, so a hard-coded claim about somebody else's
database cannot slip in.

**Windows is a first-class target.** The info group its Driver Manager queries
before `SQLDriverConnect` is answered without a connection, the function bitmap
is built from the exported-function list so the two cannot drift apart, and an
unknown info type gets a Driver-Manager-safe value rather than the `SQL_ERROR`
that corrupts its internal state. `ConfigDSNW` is exported for the ODBC
Administrator, with a hook so a driver's own dialog supplies the keywords.

**A conformance harness drivers can run.** The `test-support` feature exposes
the `conformance` module, so a driver's own test suite can drive core's shared
`SQLGetInfo` checks against its real backend and catch an info type whose value
contradicts another one it declared.

**Checked by more than unit tests.** Miri runs the pointer marshalling in an
interpreter that detects undefined behaviour and leaked handles, loom explores
the thread interleavings a test run happens not to produce, and cargo-fuzz
throws random input at the buffer copying under AddressSanitizer. Every
`extern "system"` entry point catches unwinds, because a panic crossing the C
boundary is itself undefined behaviour.

### Known limitations

- Results are read front to back only (`SQL_SO_FORWARD_ONLY`). `SQLFetchScroll`
  takes `SQL_FETCH_NEXT` and rejects every other orientation with `HY106`.
- No block cursors and no parameter arrays. `SQL_ATTR_ROW_ARRAY_SIZE` and
  `SQL_ATTR_PARAMSET_SIZE` are fixed at 1, and asking for more returns 1 with an
  `01S02` warning, so `SQL_GD_BLOCK` is never reported.
- No bookmarks and no positioned updates: `SQLSetPos` and `SQLBulkOperations`
  validate their arguments and then report `HYC00`.
- `SQL_ATTR_AUTO_IPD` is `SQL_FALSE`, so parameter metadata is never populated
  automatically. Bind the parameter or set the IPD fields yourself.
- No async. `Backend` is synchronous, so a driver over an async client library
  bridges internally, for example with a current-thread tokio runtime.
- The deprecated ODBC 2.x functions are not exported, because the Driver Manager
  already maps them onto the modern ones and usually does it better.
  `SQLExtendedFetch` is the exception, since it is not mapped.
- `SQL_ATTR_MAX_ROWS` and `SQL_ATTR_MAX_LENGTH` are offered to the data source
  and otherwise capped to 0 with `01S02`, never emulated, because counting rows
  in the driver after they have crossed the wire saves nothing.
- Core ships no `Prompter` implementation, because any one it could offer needs
  a browser or a window system a database-independent crate cannot choose.

[Unreleased]: https://github.com/stackabletech/stackable-odbc-core/commits/HEAD
