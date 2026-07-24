//! Benchmarks for the result-set hot path: fetch + get_data.
//!
//! Two workload shapes:
//!   * Shape A — mixed columns (50% i64, 40% short string, 10% decimal-as-string).
//!     Defaults to BENCH_ROWS=100_000 × BENCH_COLS=20.
//!   * Shape B — 5 columns × wide strings.
//!     Defaults to BENCH_WIDE_ROWS=10_000 × BENCH_WIDE_STR_LEN=1024.
//!
//! Three scenarios (where supported by the harness):
//!   * `late_binding` — SQLFetch + per-cell SQLGetData. Default ODBC pattern.
//!   * `bound_columns` — SQLBindCol + SQLFetch writes directly to client buffers.
//!     Not exercised here; only the FFI-end-to-end driver benches cover it.
//!   * `repeat_get_data` — SQLGetData called BENCH_REPEAT_GET_DATA times per cell.
//!     Surfaces per-call clone cost.
//!
//! Run with:
//!   cargo bench -p stackable-odbc-core
//!   BENCH_ROWS=1000000 cargo bench -p stackable-odbc-core      # huge-data manual run

use criterion::{Criterion, criterion_group, criterion_main};
use odbc_sys::{CDataType, SqlDataType};
use stackable_odbc_core::backend::StatementBackend;
use stackable_odbc_core::synthetic::SyntheticStatement;
use stackable_odbc_core::types::{ColumnDescriptor, ColumnValue, FetchResult};
use std::hint::black_box;
use std::time::Duration;

#[derive(Clone, Copy)]
struct BenchConfig {
    rows: usize,
    cols: usize,
    wide_rows: usize,
    wide_str_len: usize,
    repeat_get_data: usize,
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn bench_config() -> BenchConfig {
    BenchConfig {
        rows: env_or("BENCH_ROWS", 100_000),
        cols: env_or("BENCH_COLS", 20),
        wide_rows: env_or("BENCH_WIDE_ROWS", 10_000),
        wide_str_len: env_or("BENCH_WIDE_STR_LEN", 1024),
        repeat_get_data: env_or("BENCH_REPEAT_GET_DATA", 3),
    }
}

/// Apply the large-run sample-size heuristic: fewer samples when each iteration
/// is expected to take seconds (rows > 250k).
fn configure_for_size(c: Criterion, rows: usize) -> Criterion {
    if rows > 250_000 {
        c.sample_size(20)
            .measurement_time(Duration::from_secs(30))
            .warm_up_time(Duration::from_secs(3))
    } else {
        c
    }
}

fn string_rows(n_rows: usize, n_cols: usize, s_len: usize) -> Vec<Vec<ColumnValue>> {
    let s = "x".repeat(s_len);
    (0..n_rows)
        .map(|_| {
            (0..n_cols)
                .map(|_| ColumnValue::String(s.clone()))
                .collect()
        })
        .collect()
}

fn int_rows(n_rows: usize, n_cols: usize) -> Vec<Vec<ColumnValue>> {
    (0..n_rows)
        .map(|row| {
            (0..n_cols)
                .map(|col| ColumnValue::I64(row as i64 * n_cols as i64 + col as i64))
                .collect()
        })
        .collect()
}

/// Compute the (i64, string, decimal) column split for Shape A based on `n_cols`.
/// Ratio is 50/40/10, with any rounding remainder going to i64.
fn shape_a_split(n_cols: usize) -> (usize, usize, usize) {
    let s = (n_cols * 4) / 10;
    let d = n_cols / 10;
    let i = n_cols - s - d;
    debug_assert_eq!(i + s + d, n_cols, "shape_a_split must total n_cols");
    (i, s, d)
}

/// Shape A: mixed columns at 50% i64, 40% short string (32 chars), 10% decimal-as-string.
fn shape_a_rows(n_rows: usize, n_cols: usize) -> Vec<Vec<ColumnValue>> {
    let (n_i, n_s, n_d) = shape_a_split(n_cols);
    let short = "x".repeat(32);
    let decimal = String::from("12345678.90");
    (0..n_rows)
        .map(|row| {
            let mut cells = Vec::with_capacity(n_cols);
            for col in 0..n_i {
                cells.push(ColumnValue::I64(row as i64 * n_cols as i64 + col as i64));
            }
            for _ in 0..n_s {
                cells.push(ColumnValue::String(short.clone()));
            }
            for _ in 0..n_d {
                cells.push(ColumnValue::String(decimal.clone()));
            }
            cells
        })
        .collect()
}

/// Shape B: wide-string rows — 5 columns × `s_len` characters each.
fn shape_b_rows(n_rows: usize, s_len: usize) -> Vec<Vec<ColumnValue>> {
    let s = "x".repeat(s_len);
    (0..n_rows)
        .map(|_| (0..5).map(|_| ColumnValue::String(s.clone())).collect())
        .collect()
}

fn columns(n: usize) -> Vec<ColumnDescriptor> {
    (0..n)
        .map(|i| ColumnDescriptor {
            name: format!("col{i}"),
            type_name: String::new(),
            sql_type: SqlDataType::EXT_W_VARCHAR,
            precision: 255,
            scale: 0,
            nullable: true,
        })
        .collect()
}

/// Drain a SyntheticStatement by calling fetch+get_data on every column.
/// Returns the total number of values read so the caller can pass it through `black_box`.
fn drain(stmt: &mut SyntheticStatement, n_cols: usize) -> usize {
    let mut count = 0usize;
    while let FetchResult::Row = stmt.fetch().expect("fetch") {
        for col in 1..=(n_cols as u16) {
            stmt.get_data(col, CDataType::Default).expect("get_data");
            count += 1;
        }
    }
    count
}

fn drain_repeat(stmt: &mut SyntheticStatement, n_cols: usize, repeats: usize) -> usize {
    let mut count = 0usize;
    while let FetchResult::Row = stmt.fetch().expect("fetch") {
        for col in 1..=(n_cols as u16) {
            for _ in 0..repeats {
                stmt.get_data(col, CDataType::Default).expect("get_data");
                count += 1;
            }
        }
    }
    count
}

fn bench_get_data_string(c: &mut Criterion) {
    use criterion::{BatchSize, Throughput};

    let n_rows = 1_000;
    let n_cols = 10;
    let cols = columns(n_cols);
    let rows = string_rows(n_rows, n_cols, 64);

    let mut group = c.benchmark_group("get_data_string");
    group.throughput(Throughput::Elements((n_rows * n_cols) as u64));
    group.bench_function("1000x10_len64", |b| {
        b.iter_batched(
            || SyntheticStatement::new(cols.clone(), rows.clone()),
            |mut stmt| {
                black_box(drain(&mut stmt, n_cols));
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_get_data_int(c: &mut Criterion) {
    use criterion::{BatchSize, Throughput};

    let n_rows = 1_000;
    let n_cols = 10;
    let cols = columns(n_cols);
    let rows = int_rows(n_rows, n_cols);

    let mut group = c.benchmark_group("get_data_i64");
    group.throughput(Throughput::Elements((n_rows * n_cols) as u64));
    group.bench_function("1000x10", |b| {
        b.iter_batched(
            || SyntheticStatement::new(cols.clone(), rows.clone()),
            |mut stmt| {
                black_box(drain(&mut stmt, n_cols));
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_shape_a(c: &mut Criterion) {
    use criterion::{BatchSize, BenchmarkId, Throughput};

    let cfg = bench_config();
    let cols = columns(cfg.cols);
    let rows = shape_a_rows(cfg.rows, cfg.cols);
    let label = format!("{}x{}", cfg.rows, cfg.cols);

    let mut group = c.benchmark_group("shape_a");
    group.throughput(Throughput::Elements((cfg.rows * cfg.cols) as u64));

    group.bench_function(BenchmarkId::new("late_binding", &label), |b| {
        b.iter_batched(
            || SyntheticStatement::new(cols.clone(), rows.clone()),
            |mut stmt| {
                black_box(drain(&mut stmt, cfg.cols));
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function(
        BenchmarkId::new(format!("repeat_get_data_x{}", cfg.repeat_get_data), &label),
        |b| {
            b.iter_batched(
                || SyntheticStatement::new(cols.clone(), rows.clone()),
                |mut stmt| {
                    black_box(drain_repeat(&mut stmt, cfg.cols, cfg.repeat_get_data));
                },
                BatchSize::LargeInput,
            );
        },
    );

    group.finish();
}

fn bench_shape_b(c: &mut Criterion) {
    use criterion::{BatchSize, BenchmarkId, Throughput};

    let cfg = bench_config();
    let n_cols = 5;
    let cols = columns(n_cols);
    let rows = shape_b_rows(cfg.wide_rows, cfg.wide_str_len);
    let label = format!("{}x5_len{}", cfg.wide_rows, cfg.wide_str_len);

    let mut group = c.benchmark_group("shape_b");
    group.throughput(Throughput::Elements((cfg.wide_rows * n_cols) as u64));

    group.bench_function(BenchmarkId::new("late_binding", &label), |b| {
        b.iter_batched(
            || SyntheticStatement::new(cols.clone(), rows.clone()),
            |mut stmt| {
                black_box(drain(&mut stmt, n_cols));
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function(
        BenchmarkId::new(format!("repeat_get_data_x{}", cfg.repeat_get_data), &label),
        |b| {
            b.iter_batched(
                || SyntheticStatement::new(cols.clone(), rows.clone()),
                |mut stmt| {
                    black_box(drain_repeat(&mut stmt, n_cols, cfg.repeat_get_data));
                },
                BatchSize::LargeInput,
            );
        },
    );

    group.finish();
}

fn benches() -> Criterion {
    configure_for_size(Criterion::default(), bench_config().rows)
}

criterion_group! {
    name = benches_group;
    config = benches();
    targets =
        bench_get_data_string,
        bench_get_data_int,
        bench_shape_a,
        bench_shape_b
}
criterion_main!(benches_group);
