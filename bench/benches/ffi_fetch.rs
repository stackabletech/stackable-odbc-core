//! FFI-level fetch benchmarks: `SQLBindCol` + `SQLFetch`, and chunked `SQLGetData`.
//!
//! `fetch_throughput` drives `SyntheticStatement::fetch()`/`get_data()` directly
//! and never enters the FFI layer at all -- no `panic_safe`, no handle
//! registry lookup, no descriptor, no `write_column_value`. `handle_lookup`
//! goes through the FFI entry points but never reaches a result set: its
//! `BenchBackend::exec_direct`/`prepare` are never even called by either of its
//! two benchmarked functions. This file closes the gap between them: both
//! groups run the real `SQLBindCol`/`SQLFetch`/`SQLGetData` C ABI entry points
//! (`stackable_odbc_core::ffi::bind::sql_bind_col`,
//! `stackable_odbc_core::ffi::fetch::sql_fetch`,
//! `stackable_odbc_core::ffi::fetch::sql_get_data`) against a connected
//! statement handle, so `panic_safe`, the handle registry, the ARD and
//! `write_column_value` are all on the measured path.
//!
//! The connection is installed with the `test-support` feature's
//! [`attach_connection`], the same shortcut a driver's own test suite uses to
//! reach the connected path without a real data source -- `Backend::connect`
//! is never called by this benchmark, only `Backend::exec_direct`.
//!
//! Two groups:
//!
//! - `ffi_fetch_bound` -- `SQLBindCol` three columns (one `i64`, one 1 KiB
//!   `SQL_C_CHAR` string, one 1 KiB `SQL_C_BINARY` blob), then loop `SQLFetch`
//!   over `BENCH_ROWS` rows to `SQL_NO_DATA`. Every row's string and bytes
//!   value is freshly allocated inside `BoundColumnsStatement::get_data`,
//!   which is what a backend that does not cache its whole result set in
//!   memory actually does -- `fetch_throughput`'s `SyntheticStatement` clones
//!   out of a `Vec<Vec<ColumnValue>>` it built once, so it cannot see that
//!   per-row allocation cost, or the ARD lookup, bind-offset arithmetic and
//!   `write_column_value` call that turn each `ColumnValue` into bytes in the
//!   application's buffer.
//! - `ffi_get_data_chunked` -- one row, one 64 KiB string column, read back
//!   with `SQLFetch` then repeated `SQLGetData` calls through a 512-byte
//!   buffer until `SQL_NO_DATA`. Exercises the `GetDataCursor` chunking loop
//!   in `sql_get_data` (`cursor.delivered`/`cursor.done`) that
//!   `ffi_fetch_bound`'s bound-column path never reaches at all, since a bound
//!   column is written by `SQLFetch` in one call regardless of size.
//!
//! `BENCH_ROWS` overrides `ffi_fetch_bound`'s row count (default 100,000),
//! matching `fetch_throughput`'s env var so the two can be compared at the
//! same N. `ffi_get_data_chunked`'s row count (1) and column width (64 KiB)
//! are fixed, since the group's variable is chunk count, not throughput.
//!
//! Run with:
//!   (cd bench && cargo bench --bench ffi_fetch)
//!   (cd bench && cargo bench --bench ffi_fetch -- --test)   # smoke test only
//!
//! ## Baseline (recorded 2026-07-31, `BENCH_ROWS=5000`, warm build, this host)
//!
//! ```text
//! ffi_fetch_bound/5000_rows                     time:   [1.5347 ms 1.5542 ms 1.5795 ms]
//!                                                thrpt:  [3.1655 Melem/s 3.2171 Melem/s 3.2579 Melem/s]
//! ffi_get_data_chunked/64KiB_over_512B_chunks    time:   [235.41 us 242.26 us 248.92 us]
//!                                                thrpt:  [251.08 MiB/s 257.99 MiB/s 265.50 MiB/s]
//! ```
//!
//! Re-measure rather than trusting these numbers on a different host or after
//! a source change that touches the fetch path -- see `AGENTS.md`'s Miri
//! section for why a stale recorded figure is worse than none.

use std::borrow::Cow;
use std::ffi::c_void;
use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use stackable_odbc_core::backend::{Backend, StatementBackend};
use stackable_odbc_core::errors::OdbcError;
use stackable_odbc_core::function_id::FunctionId;
use stackable_odbc_core::odbc_sys::{FreeStmtOption, HandleType};
use stackable_odbc_core::test_support::{attach_connection, detach_connection};
use stackable_odbc_core::types::{
    CDataType, ColumnValue, ConnectParams, ExecuteOutcome, FetchResult, InfoType, InfoValue,
    SQL_NTS, SqlReturn, SqlState, TypeInfoRow,
};

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn bench_rows() -> u64 {
    env_or("BENCH_ROWS", 100_000)
}

/// Width of the string and bytes columns `ffi_fetch_bound` binds.
const ONE_KIB: usize = 1024;

/// Width of the string column `ffi_get_data_chunked` reads back in parts.
const CHUNKED_STRING_LEN: usize = 64 * 1024;

/// The `SQLGetData` target buffer size for `ffi_get_data_chunked` -- small
/// enough against a 64 KiB value that draining it takes many calls.
const CHUNK_BUFFER_LEN: usize = 512;

/// How many `SQLGetData` calls draining [`CHUNKED_STRING_LEN`] bytes through a
/// [`CHUNK_BUFFER_LEN`]-byte `SQL_C_CHAR` buffer takes: `write_char`
/// (`src/column_value.rs`) reserves the buffer's last byte for the null
/// terminator, so each call before the last delivers `CHUNK_BUFFER_LEN - 1`
/// bytes.
const EXPECTED_CHUNKS: u64 =
    ((CHUNKED_STRING_LEN + CHUNK_BUFFER_LEN - 2) / (CHUNK_BUFFER_LEN - 1)) as u64;

/// The exact SQL text `ffi_fetch_bound`'s setup sends. `BenchBackend` matches
/// on it to decide which shape of statement to hand back, since
/// `Backend::exec_direct` has no other per-call argument that could carry that
/// choice -- this is a benchmark's own fixed request, not data an application
/// chose, so matching on it is safe.
const BOUND_SQL: &str = "SELECT id, payload_text, payload_bytes FROM bound_bench";

/// The exact SQL text `ffi_get_data_chunked`'s setup sends. See [`BOUND_SQL`].
const CHUNKED_SQL: &str = "SELECT wide_text FROM chunked_bench";

// ---------------------------------------------------------------------------
// Statement shapes
// ---------------------------------------------------------------------------

/// `BENCH_ROWS` rows of `(i64, 1 KiB string, 1 KiB bytes)`, for
/// `ffi_fetch_bound`.
///
/// `get_data` allocates a fresh `String`/`Vec<u8>` on every call rather than
/// cloning out of a pre-built row list, because that allocation is exactly
/// the cost `fetch_throughput`'s in-memory `SyntheticStatement` cannot show.
struct BoundColumnsStatement {
    rows_left: u64,
}

impl BoundColumnsStatement {
    fn new(rows: u64) -> Self {
        Self { rows_left: rows }
    }
}

impl StatementBackend for BoundColumnsStatement {
    type Error = OdbcError;

    fn column_count(&self) -> i16 {
        3
    }

    fn fetch(&mut self) -> Result<FetchResult, Self::Error> {
        if self.rows_left == 0 {
            return Ok(FetchResult::NoData);
        }
        self.rows_left -= 1;
        Ok(FetchResult::Row)
    }

    fn get_data(
        &mut self,
        col: u16,
        _target_type: CDataType,
    ) -> Result<Cow<'_, ColumnValue>, Self::Error> {
        match col {
            1 => Ok(Cow::Owned(ColumnValue::I64(self.rows_left as i64))),
            2 => Ok(Cow::Owned(ColumnValue::String("x".repeat(ONE_KIB)))),
            3 => Ok(Cow::Owned(ColumnValue::Bytes(vec![0xABu8; ONE_KIB]))),
            other => Err(OdbcError::general(
                format!("BoundColumnsStatement has no column {other}"),
                SqlState::invalid_descriptor_index(),
            )),
        }
    }
}

/// One row, one 64 KiB string column, for `ffi_get_data_chunked`.
///
/// `get_data` returns the whole string on every call rather than a slice
/// starting at some offset -- exactly like `MockLongDataStatement` in core's
/// own test suite (`src/test_utils.rs`) -- because chunking across several
/// `SQLGetData` calls is `write_column_value_at`'s job, not the backend's; a
/// backend that pre-sliced would only be testing itself.
struct ChunkedStatement {
    rows_left: u8,
}

impl ChunkedStatement {
    fn new() -> Self {
        Self { rows_left: 1 }
    }
}

impl StatementBackend for ChunkedStatement {
    type Error = OdbcError;

    fn column_count(&self) -> i16 {
        1
    }

    fn fetch(&mut self) -> Result<FetchResult, Self::Error> {
        if self.rows_left == 0 {
            return Ok(FetchResult::NoData);
        }
        self.rows_left -= 1;
        Ok(FetchResult::Row)
    }

    fn get_data(
        &mut self,
        col: u16,
        _target_type: CDataType,
    ) -> Result<Cow<'_, ColumnValue>, Self::Error> {
        match col {
            1 => Ok(Cow::Owned(ColumnValue::String(
                "y".repeat(CHUNKED_STRING_LEN),
            ))),
            other => Err(OdbcError::general(
                format!("ChunkedStatement has no column {other}"),
                SqlState::invalid_descriptor_index(),
            )),
        }
    }
}

/// The one `Backend::Statement` type both benchmarked SQL texts produce.
enum BenchStatement {
    Bound(BoundColumnsStatement),
    Chunked(ChunkedStatement),
}

impl StatementBackend for BenchStatement {
    type Error = OdbcError;

    fn column_count(&self) -> i16 {
        match self {
            Self::Bound(s) => s.column_count(),
            Self::Chunked(s) => s.column_count(),
        }
    }

    fn fetch(&mut self) -> Result<FetchResult, Self::Error> {
        match self {
            Self::Bound(s) => s.fetch(),
            Self::Chunked(s) => s.fetch(),
        }
    }

    fn get_data(
        &mut self,
        col: u16,
        target_type: CDataType,
    ) -> Result<Cow<'_, ColumnValue>, Self::Error> {
        match self {
            Self::Bound(s) => s.get_data(col, target_type),
            Self::Chunked(s) => s.get_data(col, target_type),
        }
    }
}

struct BenchConnection;

/// The backend both benchmarks in this file drive.
///
/// `connect` is never called: `attach_connection` (behind the `test-support`
/// feature) puts a `BenchConnection` into the handle directly, so the
/// benchmark never has to invent a connection string.
struct BenchBackend;

impl Backend for BenchBackend {
    type Connection = BenchConnection;
    type Statement = BenchStatement;
    type Error = OdbcError;
    type CancelToken = ();

    fn connect(_: &ConnectParams) -> Result<Self::Connection, Self::Error> {
        Ok(BenchConnection)
    }
    fn disconnect(_: &mut Self::Connection) -> Result<(), Self::Error> {
        Ok(())
    }
    fn cancel_token(_: &Self::Connection) -> Self::CancelToken {}
    fn get_functions() -> Cow<'static, [FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn table_types(_: &Self::Connection) -> Vec<Cow<'static, str>> {
        Vec::new()
    }
    fn get_info(_: &Self::Connection, _: InfoType) -> Result<InfoValue, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "get_info".into(),
        })
    }
    fn exec_direct(
        _: &Self::Connection,
        _: &Self::CancelToken,
        sql: &str,
    ) -> Result<Self::Statement, Self::Error> {
        match sql {
            BOUND_SQL => Ok(BenchStatement::Bound(BoundColumnsStatement::new(
                bench_rows(),
            ))),
            CHUNKED_SQL => Ok(BenchStatement::Chunked(ChunkedStatement::new())),
            other => Err(OdbcError::NotImplemented {
                feature: format!("BenchBackend received unexpected SQL: {other}"),
            }),
        }
    }
    fn prepare(
        conn: &Self::Connection,
        cancel: &Self::CancelToken,
        sql: &str,
    ) -> Result<Self::Statement, Self::Error> {
        Self::exec_direct(conn, cancel, sql)
    }
    fn execute(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &mut Self::Statement,
        _: &[ColumnValue],
    ) -> Result<ExecuteOutcome, Self::Error> {
        Ok(ExecuteOutcome::default())
    }
    fn tables(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &stackable_odbc_core::types::TablesQuery<'_>,
    ) -> Result<Vec<stackable_odbc_core::types::TableRow>, Self::Error> {
        Ok(Vec::new())
    }
    fn columns(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &stackable_odbc_core::types::ColumnsQuery<'_>,
    ) -> Result<Vec<stackable_odbc_core::types::ColumnRow>, Self::Error> {
        Ok(Vec::new())
    }

    fn supports_catalogs(_: &Self::Connection) -> bool {
        false
    }
    fn supports_schemas(_: &Self::Connection) -> bool {
        false
    }
    fn alter_table_support(_: &Self::Connection) -> u32 {
        0
    }
    fn outer_join_capabilities(_: &Self::Connection) -> u32 {
        0
    }
    fn group_by(_: &Self::Connection) -> u16 {
        0
    }
    fn null_collation(_: &Self::Connection) -> u16 {
        0
    }
    fn identifier_case(_: &Self::Connection) -> u16 {
        stackable_odbc_core::types::SQL_IC_SENSITIVE
    }
    fn quoted_identifier_case(_: &Self::Connection) -> u16 {
        stackable_odbc_core::types::SQL_IC_SENSITIVE
    }
    fn txn_capable(_: &Self::Connection) -> u16 {
        0
    }
    fn integrity(_: &Self::Connection) -> bool {
        false
    }
    fn multiple_active_txn(_: &Self::Connection) -> bool {
        false
    }
    fn special_characters(_: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("")
    }
    fn accessible_procedures(_: &Self::Connection) -> bool {
        false
    }
    fn driver_name() -> Cow<'static, str> {
        Cow::Borrowed("bench")
    }
    fn driver_version() -> Cow<'static, str> {
        Cow::Borrowed("00.00.0000")
    }
    fn dbms_name(_: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("bench")
    }
    fn dbms_version(_: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("00.00.0000")
    }
    fn correlation_name(_: &Self::Connection) -> u16 {
        0
    }
    fn non_nullable_columns(_: &Self::Connection) -> u16 {
        0
    }
    fn expressions_in_order_by(_: &Self::Connection) -> bool {
        false
    }
    fn sql_conformance(_: &Self::Connection) -> u32 {
        0
    }
    fn subqueries(_: &Self::Connection) -> u32 {
        0
    }
    fn column_alias(_: &Self::Connection) -> bool {
        false
    }
    fn concat_null_behavior(_: &Self::Connection) -> u16 {
        0
    }
    fn union_support(_: &Self::Connection) -> u32 {
        0
    }
    fn convert_functions(_: &Self::Connection) -> u32 {
        0
    }
    fn order_by_columns_in_select(_: &Self::Connection) -> bool {
        false
    }
    fn accessible_tables(_: &Self::Connection) -> bool {
        false
    }
    fn data_source_read_only(_: &Self::Connection) -> bool {
        false
    }
    fn search_pattern_escape(_: &Self::Connection) -> Cow<'static, str> {
        Cow::Borrowed("")
    }
    fn keywords(_: &Self::Connection) -> Cow<'static, [Cow<'static, str>]> {
        Cow::Borrowed(&[])
    }
    fn timedate_add_intervals(_: &Self::Connection) -> u32 {
        0
    }
    fn timedate_diff_intervals(_: &Self::Connection) -> u32 {
        0
    }
    fn default_txn_isolation(_: &Self::Connection) -> u32 {
        0
    }
    fn txn_isolation_options(_: &Self::Connection) -> u32 {
        0
    }
}

// ---------------------------------------------------------------------------
// Handle setup, shared by both groups
// ---------------------------------------------------------------------------

/// Allocate env -> connection -> statement, and attach a `BenchConnection` to
/// the connection handle through the `test-support` feature's
/// `attach_connection` -- the same shortcut a driver's own test suite uses to
/// reach the connected path without a real data source.
fn alloc_and_connect() -> (*mut c_void, *mut c_void, *mut c_void) {
    use stackable_odbc_core::ffi::handle::sql_alloc_handle;
    unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sql_alloc_handle::<BenchBackend>(
                HandleType::Env as i16,
                std::ptr::null_mut(),
                &mut env
            ),
            SqlReturn::SUCCESS,
        );
        let mut conn: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sql_alloc_handle::<BenchBackend>(HandleType::Dbc as i16, env, &mut conn),
            SqlReturn::SUCCESS,
        );
        attach_connection::<BenchBackend>(conn, BenchConnection).expect("attach_connection");
        let mut stmt: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sql_alloc_handle::<BenchBackend>(HandleType::Stmt as i16, conn, &mut stmt),
            SqlReturn::SUCCESS,
        );
        (env, conn, stmt)
    }
}

/// Detach the connection and free all three handles, mirroring the teardown
/// order `SQLDisconnect`/`SQLFreeHandle` would use.
fn free_all(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
    use stackable_odbc_core::ffi::handle::sql_free_handle;
    unsafe {
        assert_eq!(
            sql_free_handle::<BenchBackend>(HandleType::Stmt as i16, stmt),
            SqlReturn::SUCCESS,
        );
        // SQLFreeHandle(SQL_HANDLE_DBC) refuses a connection still holding a
        // connection (HY010); detach_connection takes it back out without
        // calling Backend::disconnect, which is what an offline benchmark
        // (no real data source to disconnect from) wants.
        detach_connection::<BenchBackend>(conn).expect("detach_connection");
        assert_eq!(
            sql_free_handle::<BenchBackend>(HandleType::Dbc as i16, conn),
            SqlReturn::SUCCESS,
        );
        assert_eq!(
            sql_free_handle::<BenchBackend>(HandleType::Env as i16, env),
            SqlReturn::SUCCESS,
        );
    }
}

fn utf16_nts(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reset `stmt` for another execution of `sql` and run it, asserting success.
///
/// `SQLFreeStmt(SQL_CLOSE)` rather than `SQLCloseCursor`: the spec makes the
/// former a no-op when no cursor is open, so it works both for the very first
/// execution (no cursor yet) and every re-execution after (`ffi/handle.rs`'s
/// `sql_free_stmt`, `FreeStmtOption::Close`), where `SQLCloseCursor` would
/// return `24000` on the first call.
fn reexecute(stmt: *mut c_void, sql: &str) {
    use stackable_odbc_core::ffi::execute::sql_exec_direct_w;
    use stackable_odbc_core::ffi::handle::sql_free_stmt;
    let text = utf16_nts(sql);
    unsafe {
        assert_eq!(
            sql_free_stmt::<BenchBackend>(stmt, FreeStmtOption::Close as u16),
            SqlReturn::SUCCESS,
        );
        assert_eq!(
            sql_exec_direct_w::<BenchBackend>(stmt, text.as_ptr(), SQL_NTS),
            SqlReturn::SUCCESS,
        );
    }
}

// ---------------------------------------------------------------------------
// ffi_fetch_bound
// ---------------------------------------------------------------------------

/// `SQLBindCol` three columns, then loop `SQLFetch` over `BENCH_ROWS` rows.
///
/// Bound once, outside the measured loop: ARD bindings are their own
/// descriptor storage, independent of the backend statement, so they survive
/// `SQLFreeStmt(SQL_CLOSE)` and re-`SQLExecDirectW` across iterations. Only
/// `reexecute` (producing a fresh `BENCH_ROWS`-row statement) and the
/// `SQLFetch` loop are inside `iter_batched`'s timed routine.
fn ffi_fetch_bound(c: &mut Criterion) {
    use stackable_odbc_core::ffi::bind::sql_bind_col;
    use stackable_odbc_core::ffi::fetch::sql_fetch;

    let rows = bench_rows();
    let (env, conn, stmt) = alloc_and_connect();

    let mut id_buf: i64 = 0;
    let mut id_ind: isize = 0;
    let mut text_buf = [0u8; ONE_KIB + 1]; // +1: SQL_C_CHAR null terminator
    let mut text_ind: isize = 0;
    let mut bytes_buf = [0u8; ONE_KIB];
    let mut bytes_ind: isize = 0;

    unsafe {
        assert_eq!(
            sql_bind_col::<BenchBackend>(
                stmt,
                1,
                CDataType::SBigInt as i16,
                std::ptr::from_mut(&mut id_buf).cast::<c_void>(),
                isize::try_from(std::mem::size_of::<i64>()).expect("size_of::<i64> fits isize"),
                std::ptr::from_mut(&mut id_ind),
            ),
            SqlReturn::SUCCESS,
        );
        assert_eq!(
            sql_bind_col::<BenchBackend>(
                stmt,
                2,
                CDataType::Char as i16,
                text_buf.as_mut_ptr().cast::<c_void>(),
                isize::try_from(text_buf.len()).expect("text_buf.len() fits isize"),
                std::ptr::from_mut(&mut text_ind),
            ),
            SqlReturn::SUCCESS,
        );
        assert_eq!(
            sql_bind_col::<BenchBackend>(
                stmt,
                3,
                CDataType::Binary as i16,
                bytes_buf.as_mut_ptr().cast::<c_void>(),
                isize::try_from(bytes_buf.len()).expect("bytes_buf.len() fits isize"),
                std::ptr::from_mut(&mut bytes_ind),
            ),
            SqlReturn::SUCCESS,
        );
    }

    let mut group = c.benchmark_group("ffi_fetch_bound");
    group.throughput(Throughput::Elements(rows));
    group.bench_function(format!("{rows}_rows"), |b| {
        b.iter_batched(
            || reexecute(stmt, BOUND_SQL),
            |()| unsafe {
                let mut count = 0u64;
                loop {
                    match sql_fetch::<BenchBackend>(stmt) {
                        SqlReturn::SUCCESS => {
                            count += 1;
                            black_box(id_buf);
                            black_box(&text_buf);
                            black_box(&bytes_buf);
                        }
                        SqlReturn::NO_DATA => break,
                        other => panic!("sql_fetch returned {other:?}"),
                    }
                }
                // Catches the PerIteration/batching mistake documented below
                // even if it recurs in a different shape: a routine that
                // silently fetched zero or partial rows would otherwise still
                // report a (meaningless) time.
                assert_eq!(count, rows, "did not fetch exactly BENCH_ROWS rows");
                black_box(count);
            },
            // PerIteration, not LargeInput/SmallInput: those batch several
            // setup() calls ahead of the timed routine() calls they belong
            // to, and setup here mutates the one shared `stmt` handle rather
            // than returning an independent value per call. Under batching,
            // only the last setup() in a batch actually leaves a fresh
            // BENCH_ROWS-row statement behind, so every routine() after the
            // first in that batch fetches against an already-exhausted one
            // and returns SQL_NO_DATA immediately -- silently, since nothing
            // here asserts the loop actually ran BENCH_ROWS times.
            // PerIteration forces exactly one setup() before each timed
            // routine(), which is the only ordering this shared-handle
            // design is correct under.
            BatchSize::PerIteration,
        );
    });
    group.finish();

    free_all(env, conn, stmt);
}

// ---------------------------------------------------------------------------
// ffi_get_data_chunked
// ---------------------------------------------------------------------------

/// `SQLFetch` the one row, then drain its 64 KiB string column through a
/// 512-byte `SQLGetData` buffer until `SQL_NO_DATA`.
///
/// No `SQLBindCol` here: this group is late binding by construction, so every
/// byte crosses through `sql_get_data`'s `GetDataCursor` chunking loop
/// (`cursor.delivered` / `cursor.done`), which `ffi_fetch_bound`'s bound
/// columns never touch at all.
fn ffi_get_data_chunked(c: &mut Criterion) {
    use stackable_odbc_core::ffi::fetch::{sql_fetch, sql_get_data};

    let (env, conn, stmt) = alloc_and_connect();

    let mut group = c.benchmark_group("ffi_get_data_chunked");
    group.throughput(Throughput::Bytes(CHUNKED_STRING_LEN as u64));
    group.bench_function(format!("64KiB_over_{CHUNK_BUFFER_LEN}B_chunks"), |b| {
        b.iter_batched(
            || reexecute(stmt, CHUNKED_SQL),
            |()| unsafe {
                assert_eq!(
                    sql_fetch::<BenchBackend>(stmt),
                    SqlReturn::SUCCESS,
                    "sql_fetch"
                );
                let mut buf = [0u8; CHUNK_BUFFER_LEN];
                let mut ind: isize = 0;
                let mut chunks = 0u64;
                loop {
                    let ret = sql_get_data::<BenchBackend>(
                        stmt,
                        1,
                        CDataType::Char as i16,
                        buf.as_mut_ptr().cast::<c_void>(),
                        isize::try_from(buf.len()).expect("buf.len() fits isize"),
                        std::ptr::from_mut(&mut ind),
                    );
                    match ret {
                        SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO => {
                            chunks += 1;
                            black_box(&buf);
                        }
                        SqlReturn::NO_DATA => break,
                        other => panic!("sql_get_data returned {other:?}"),
                    }
                }
                // Same reasoning as ffi_fetch_bound's row-count assert:
                // a routine that silently drained zero or a partial value
                // would otherwise still report a (meaningless) time.
                assert_eq!(
                    chunks, EXPECTED_CHUNKS,
                    "did not drain the full 64 KiB value"
                );
                black_box(chunks);
            },
            // PerIteration: see the comment on the same choice in
            // ffi_fetch_bound. This group's routine is fast enough that
            // SmallInput's batching is exactly where the bug that choice
            // avoids was first caught -- reexecute's shared `stmt` handle
            // got reset once per batch instead of once per iteration, so
            // every fetch after the batch's first returned SQL_NO_DATA.
            BatchSize::PerIteration,
        );
    });
    group.finish();

    free_all(env, conn, stmt);
}

fn benches_config() -> Criterion {
    // Mirrors fetch_throughput's configure_for_size heuristic: a row count
    // large enough to make one iteration take seconds gets fewer samples and
    // a longer measurement window, so the whole run stays bounded.
    if bench_rows() > 20_000 {
        Criterion::default()
            .sample_size(20)
            .measurement_time(Duration::from_secs(15))
            .warm_up_time(Duration::from_secs(2))
    } else {
        Criterion::default()
    }
}

criterion_group! {
    name = benches;
    config = benches_config();
    targets = ffi_fetch_bound, ffi_get_data_chunked
}
criterion_main!(benches);
