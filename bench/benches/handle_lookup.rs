//! Registry-lookup cost on the FFI entry path.
//!
//! The sibling `fetch_throughput` benchmark drives `SyntheticStatement`
//! directly and never touches the handle registry, so it cannot see this at
//! all. Every `SQLxxx` entry point in the crate reaches its handle through
//! `HandleScope::get`, which is the one place that cost is paid, and paid on
//! every call whatever the function goes on to do.
//!
//! Two shapes, because the error path is not the success path scaled:
//!
//! - `get` — one `scope.get`, then trivial work. `SQLFreeStmt(SQL_UNBIND)`
//!   clears a binding vector that is already empty, so essentially all of what
//!   is measured is the lookup.
//! - `get_then_push_diagnostic` — the error path, where `panic_safe` also has
//!   to find the handle again to post a diagnostic against it.
//!   `SQLNumResultCols` on a statement that was never executed is the cheapest
//!   way to reach it.

use std::borrow::Cow;
use std::ffi::c_void;

use criterion::{Criterion, criterion_group, criterion_main};
use stackable_odbc_core::backend::Backend;
use stackable_odbc_core::errors::OdbcError;
use stackable_odbc_core::function_id::FunctionId;
use stackable_odbc_core::types::{ConnectParams, InfoValue, InfoType, SqlReturn, TypeInfoRow};
use stackable_odbc_core::odbc_sys::{FreeStmtOption, HandleType};

/// The smallest backend the trait allows. Nothing here is ever called: the
/// benchmarked entry points resolve a handle and return, without reaching a
/// connection.
struct BenchBackend;

struct BenchConnection;
struct BenchStatement;

impl stackable_odbc_core::backend::StatementBackend for BenchStatement {
    type Error = OdbcError;
}

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
    fn get_functions() -> Cow<'static, [FunctionId]> {
        Cow::Borrowed(&[])
    }
    fn get_type_info(_: &Self::Connection) -> Cow<'static, [TypeInfoRow]> {
        Cow::Borrowed(&[])
    }
    fn table_types(_: &Self::Connection) -> Vec<Cow<'static, str>> {
        Vec::new()
    }
    fn cancel_token(_: &Self::Connection) -> Self::CancelToken {}
    fn get_info(_: &Self::Connection, _: InfoType) -> Result<InfoValue, Self::Error> {
        Err(OdbcError::NotImplemented {
            feature: "get_info".into(),
        })
    }
    fn exec_direct(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<Self::Statement, Self::Error> {
        Ok(BenchStatement)
    }
    fn prepare(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &str,
    ) -> Result<Self::Statement, Self::Error> {
        Ok(BenchStatement)
    }
    fn execute(
        _: &Self::Connection,
        _: &Self::CancelToken,
        _: &mut Self::Statement,
        _: &[stackable_odbc_core::types::ColumnValue],
    ) -> Result<stackable_odbc_core::types::ExecuteOutcome, Self::Error> {
        Ok(stackable_odbc_core::types::ExecuteOutcome::default())
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

/// Allocate env → connection → statement, all unconnected.
///
/// No connection is attached: neither benchmarked entry point reaches one, and
/// leaving it off keeps the measurement to the registry.
fn alloc() -> (*mut c_void, *mut c_void, *mut c_void) {
    use stackable_odbc_core::ffi::handle::sql_alloc_handle;
    unsafe {
        let mut env: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sql_alloc_handle::<BenchBackend>(HandleType::Env as i16, std::ptr::null_mut(), &mut env),
            SqlReturn::SUCCESS,
        );
        let mut conn: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sql_alloc_handle::<BenchBackend>(HandleType::Dbc as i16, env, &mut conn),
            SqlReturn::SUCCESS,
        );
        let mut stmt: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sql_alloc_handle::<BenchBackend>(HandleType::Stmt as i16, conn, &mut stmt),
            SqlReturn::SUCCESS,
        );
        (env, conn, stmt)
    }
}

fn free(env: *mut c_void, conn: *mut c_void, stmt: *mut c_void) {
    use stackable_odbc_core::ffi::handle::sql_free_handle;
    unsafe {
        let _ = sql_free_handle::<BenchBackend>(HandleType::Stmt as i16, stmt);
        let _ = sql_free_handle::<BenchBackend>(HandleType::Dbc as i16, conn);
        let _ = sql_free_handle::<BenchBackend>(HandleType::Env as i16, env);
    }
}

fn bench_get(c: &mut Criterion) {
    let (env, conn, stmt) = alloc();
    let mut group = c.benchmark_group("handle_scope");

    // One `scope.get`, then a no-op: the bindings vector is already empty.
    group.bench_function("get", |b| {
        b.iter(|| unsafe {
            stackable_odbc_core::ffi::handle::sql_free_stmt::<BenchBackend>(
                std::hint::black_box(stmt),
                FreeStmtOption::Unbind as u16,
            )
        });
    });

    // The error path: `scope.get`, then `panic_safe` finding the handle again
    // to post the diagnostic against it.
    group.bench_function("get_then_push_diagnostic", |b| {
        b.iter(|| unsafe {
            let mut count: i16 = 0;
            stackable_odbc_core::ffi::cursor::sql_num_result_cols::<BenchBackend>(
                std::hint::black_box(stmt),
                &mut count,
            )
        });
    });

    group.finish();
    free(env, conn, stmt);
}

criterion_group!(benches, bench_get);
criterion_main!(benches);
