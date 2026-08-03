# Audit Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix every finding from the 2026-07-31 eight-dimension audit (bugs, ODBC compliance, docs, headers, assertions, test gaps, performance, security), with each step verified by a test where a test can express it, and the full suite green before every commit.

**Architecture:** No architectural change. Fixes are localised: the fetch-direction conversion layer (`src/column_value.rs`, `src/ffi/fetch.rs`), the descriptor/param offset plumbing, one panic-guard hole, doc-comment/markdown sweeps, and targeted test additions. Perf work adds caching to `GetDataCursor` and restructures `fetch_with_report`, guarded by a new FFI-level benchmark.

**Tech Stack:** Rust 1.95.0 (edition 2024), odbc-sys, snafu, tracing; nightly for Miri/fuzz; loom via `--cfg loom`; Criterion in the detached `bench/` workspace.

## Global Constraints

- **Every commit gate is:** `cargo test` (zero failures, zero warnings) **and** `pre-commit run --all-files`. Tasks below say "GATE" to mean exactly this.
- **Commits are made by Andrew, not the agent:** a hook blocks agent-initiated commits. Each "Commit" step means: stage nothing, present the proposed message plus `git diff --stat`, and wait for Andrew to review and commit. Do not proceed to the next task until the commit lands.
- **Miri runs are deferred to the end** (Andrew's instruction — they are slow). Ignore per-task "targeted Miri" steps during execution; instead run Task 7.1's full Miri pass (plain + symbolic alignment) once after the last code task, before the final sign-off. Exception: a test whose *red* step is only observable under Miri (Task 1.1's wild write) may substitute a plain-assertion red where possible and rely on 7.1 otherwise.
- **Read the spec page for every FFI function touched** before changing it, and re-verify its doc-comment SQLSTATE list against `src/types/diagnostics_table.rs` (project rule). The guard test `every_doc_comment_matches_the_spec_diagnostics_table` gates doc/table mismatches at build.
- **Named constants in tests** — no raw ODBC literals; use `odbc-sys` types / `types/constants.rs` (project rule).
- **When mature drivers disagree with the spec or each other, ask — do not pick one** (project rule; applies to Tasks 3.1–3.3).
- **User-facing changes get a CHANGELOG entry** under `## [Unreleased]` in the appropriate group, in the same commit as the change.
- **If the `Backend`/`StatementBackend` trait is touched** (no task here should need it): `(cd bench && cargo build --benches)` and `(cd fuzz && cargo +nightly build --target x86_64-unknown-linux-gnu)` before pushing.
- **Pointer-marshalling changes** (Tasks 1.1, 1.3, 2.x in column_value, 6.1–6.2) are covered by the deferred Miri pass (Task 7.1), not per-task runs.
- New tests that allocate handles must free them (Miri leak check is on).
- Several existing tests **pin wrong behaviour** and must be *changed in the same commit* as the fix (named per task). Red–green still applies: the new/changed test must fail against the unfixed code first.

---

## Phase 0 — Measurement harness

### Task 0.1: FFI-level fetch benchmark

**Files:**

- Create: `bench/benches/ffi_fetch.rs`
- Modify: `bench/Cargo.toml` (add `[[bench]] name = "ffi_fetch"`), `AGENTS.md` Benchmarks section (add the third benchmark and correct the false "fetch_throughput measures the marshalling path" claim)

**Interfaces:**

- Consumes: the C ABI entry points the way `bench/benches/handle_lookup.rs` already does (its `impl Backend` + `forward_ffi!` setup is the template); `test-support` feature hooks to install a connection.
- Produces: two Criterion groups later perf tasks cite: `ffi_fetch_bound` (SQLBindCol + SQLFetch loop over an N-row, mixed-type synthetic result set) and `ffi_get_data_chunked` (SQLFetch + per-cell SQLGetData with a small buffer over one long string column).

- [ ] **Step 1:** Copy `handle_lookup.rs`'s backend/setup scaffolding; add a `StatementBackend` returning `BENCH_ROWS` rows with one i64, one `String` (1 KiB), one `Bytes` (1 KiB) column.
- [ ] **Step 2:** Implement `ffi_fetch_bound`: bind all three columns via `sql_bind_col`-generated `SQLBindCol`, loop `SQLFetch` to `SQL_NO_DATA`, `black_box` the buffers.
- [ ] **Step 3:** Implement `ffi_get_data_chunked`: one 64 KiB string column, `SQLGetData` with a 512-byte buffer until `SQL_NO_DATA`.
- [ ] **Step 4:** Verify: `(cd bench && cargo bench --bench ffi_fetch -- --test)` runs both groups; record baseline numbers in the bench module doc comment.
- [ ] **Step 5:** GATE (root workspace untouched except AGENTS.md — pre-commit still runs). Commit: `bench: add FFI-level fetch and chunked SQLGetData benchmarks`

---

## Phase 1 — Critical safety bugs

### Task 1.1: Row bind offset must not resurrect null pointers (B1)

**Files:**

- Modify: `src/ffi/fetch.rs:415-427` (the `binding_info` map applying `wrapping_byte_add`)
- Test: `src/ffi/fetch.rs` tests mod

**Interfaces:** Produces: offset applied only when the pointer is non-null; the 22002 check and `write_column_value`'s null-target skip see genuine nulls again.

- [ ] **Step 1: Write failing tests** (both must fail against current code):
  - `row_bind_offset_does_not_offset_a_null_indicator_pointer`: bind one column with a valid data buffer and a **null** `StrLen_or_IndPtr`; set `SQL_ATTR_ROW_BIND_OFFSET_PTR` to a live `SQLULEN = 64`; fetch a row whose value is `ColumnValue::Null`. Expected after fix: `SQL_ERROR` with SQLSTATE `22002` (`SqlState::indicator_variable_required()`) — because the indicator is *really* absent. Against current code the offset makes the null "present" and the test sees a wrong return (and under Miri, a wild write).
  - `row_bind_offset_does_not_offset_a_null_data_pointer`: indicator-only binding (data ptr null, indicator bound), same offset, fetch a non-null value. Expected: indicator written at `base + offset`, `SQL_SUCCESS`, and no data write (assert the canary bytes at `offset` in a data-sized arena are untouched).
- [ ] **Step 2:** Run both, confirm FAIL. Run the first under Miri to demonstrate the wild write class.
- [ ] **Step 3:** Fix: in the `binding_info` closure, `if ptr.is_null() { ptr } else { ptr.wrapping_byte_add(bind_offset) }` for both pointers (small helper `offset_non_null`).
- [ ] **Step 4:** Run tests → PASS; targeted Miri with symbolic alignment on `ffi::fetch`.
- [ ] **Step 5:** GATE. CHANGELOG (Fixed). Commit: `fix: SQL_ATTR_ROW_BIND_OFFSET_PTR no longer offsets null data/indicator pointers`

### Task 1.2: Guard SQLCopyDesc phase one against backend panics (B4)

**Files:**

- Modify: `src/ffi/desc.rs:1229-1260` (`sql_copy_desc`), `src/panic.rs` only if a helper variant is needed
- Test: `src/ffi/desc.rs` tests mod; new mock in `src/test_utils.rs`

**Interfaces:** Consumes `panic_safe_unlocked` (`src/panic.rs`). Produces: `sql_copy_desc` whose phase one cannot unwind across `extern "system"`.

- [ ] **Step 1: Write failing test** `copy_desc_from_ird_with_panicking_describe_col_returns_error_not_abort`: add `MockPanickingDescribeBackend` to `test_utils.rs` whose `StatementBackend::describe_col` panics; execute a statement, obtain the IRD token via `SQL_ATTR_IMP_ROW_DESC`, allocate an explicit target descriptor, call `SQLCopyDesc(ird, target)`. Expected: `SQL_ERROR` returned (process alive). Current code: the panic escapes `sql_copy_desc` — under `cargo test` the unwind is caught by the test harness, so assert via `std::panic::catch_unwind` around the call: after the fix `catch_unwind` must return `Ok(SQL_ERROR)`, before it returns `Err`.
- [ ] **Step 2:** Run → FAIL (panic propagates).
- [ ] **Step 3:** Fix: wrap the whole `sql_copy_desc` body in `panic_safe_unlocked` (matching `sql_cancel`'s pattern; phase two's inner `panic_safe` still owns diagnostics). Update the function's doc comment and the AGENTS.md "two exceptions" sentence — it becomes three sites using `panic_safe_unlocked`, or restructure so the sentence stays true; whichever, the prose and code must agree.
- [ ] **Step 4:** Run → PASS. Free all handles in the test (Miri leak check).
- [ ] **Step 5:** GATE. CHANGELOG (Fixed). Commit: `fix: SQLCopyDesc phase one runs under a panic guard`

### Task 1.3: SQLGetData with a zero-length buffer must not consume the column (B2)

**Files:**

- Modify: `src/column_value.rs:898, 960, 1004` (`write_wchar`/`write_char`/`write_binary` shared branch), `src/ffi/fetch.rs` doc comment
- Test: `src/column_value.rs` + `src/ffi/fetch.rs` tests mods. **Changes pinned tests:** `wchar_zero_length_buffer_reports_size_and_writes_nothing` (column_value.rs:~1890) and its char/binary siblings.

**Interfaces:** Produces: non-null target + `buf_len == 0` (and `< 2` for wchar's terminator) → `(SUCCESS_WITH_INFO, 0)`; null target keeps `(SUCCESS, 0)`. `sql_get_data`'s existing `cursor.done = ... != SUCCESS_WITH_INFO` then keeps the cursor resumable with no change of its own.

- [ ] **Step 1: Write failing test** `get_data_length_probe_with_zero_buffer_keeps_the_column_readable`: fetch a row with a 32-char string; `SQLGetData(col, SQL_C_WCHAR, non_null_buf, 0, &ind)` → expect `SQL_SUCCESS_WITH_INFO`, `01004` diagnostic, `ind == 64` (bytes); then `SQLGetData` again with a full-size buffer → expect the complete value. Current code: second call returns `SQL_NO_DATA`. Add char and binary variants.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Fix the three writers: split `target_ptr.is_null()` (→ `SUCCESS`, length report only — the spec's null-target length query) from `buf_len <= 0` with a non-null target (→ `SUCCESS_WITH_INFO`, 0 written), exactly the split `write_utf16` (utf16.rs:172-181) documents. Update the three pinned tests to the new contract in the same commit; re-read the SQLGetData spec page and refresh the 01004 bullet.
- [ ] **Step 4:** Run → PASS, including the updated pinned tests failing against old code (red-green on the changed pins).
- [ ] **Step 5:** GATE + targeted Miri. CHANGELOG (Fixed). Commit: `fix: a zero-length SQLGetData buffer reports truncation instead of consuming the column`

### Task 1.4: Honour SQL_ATTR_PARAM_BIND_OFFSET_PTR (B3)

**Files:**

- Modify: `src/ffi/params.rs` (`read_param_value` ~:717, `find_data_at_exec_params`, `sql_param_data`'s `value_ptr_ptr` echo ~:1761)
- Test: `src/ffi/params.rs` tests mod

**Interfaces:** Consumes the APD header field `Desc::BindOffsetPtr` via the same read shape as `row_bind_offset` (`src/ffi/fetch.rs:86`) — extract that into a shared `bind_offset_of(desc)` helper in `src/descriptor.rs` or `src/ffi/params.rs` rather than duplicating. Null-pointer rule from Task 1.1 applies identically.

- [ ] **Step 1: Write failing tests:**
  - `param_bind_offset_moves_the_read_address`: bind an i64 parameter at `&arena[0]`, set `SQL_ATTR_PARAM_BIND_OFFSET_PTR` to a live `SQLULEN = 16`, place a distinct value at `arena[16..24]`, execute against a recording mock (`MockRecordingBackend` exists) → backend must receive the offset value.
  - `param_bind_offset_applies_to_the_indicator_pointer` (same shape, `SQL_NULL_DATA` indicator at offset).
  - `param_data_echoes_the_offset_data_at_exec_pointer` for the `SQLParamData` echo.
- [ ] **Step 2:** Run → FAIL (base-address value received).
- [ ] **Step 3:** Implement: apply the offset (non-null pointers only) at every APD pointer read; re-read the SQLBindParameter and SQLParamData spec pages for the offset wording; update the set-side comment in `stmt_attr.rs` that currently names only the row-side three.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** GATE. CHANGELOG (Fixed). Commit: `fix: SQL_ATTR_PARAM_BIND_OFFSET_PTR is applied when reading bound parameters`

---

## Phase 2 — Wrong-data conversion fixes (fetch direction)

Every task here: red test first, spec table row cited in the test comment, pinned-test updates in the same commit, GATE, CHANGELOG (Fixed), ask-then-commit. All in `src/column_value.rs` unless stated. Commit one task at a time.

### Task 2.1: SQL_C_BIT three-way conversion (B5)

Fix `write_numeric_pivot`'s two Bit arms (`:1180`, `:1284-1292`) to mirror `param_convert.rs::to_bit`: 0/1 → write, SUCCESS; fractional in (0,2) → write truncated 0/1, `OdbcError::FractionalTruncation` (01S07); `<0 || >=2` → `SqlState::numeric_value_out_of_range()` (22003), nothing written. Tests: `i64_five_to_bit_is_22003`, `i64_minus_one_to_bit_is_22003`, `f64_half_to_bit_writes_zero_with_01s07`, `f64_one_point_five_to_bit_writes_one_with_01s07`. Update pinned `float_nonzero_to_bit_is_one`. Commit: `fix: SQL_C_BIT fetch conversion follows the spec's three-way range rule`

### Task 2.2: Decimal/String → integer targets convert exactly (B6)

`parse_numeric_text` (`:1041-1047`) and the integer arms (`:1211-1224`): route text with a fractional part through `param_convert`'s `parse_numeric_literal`/`DecimalLiteral::to_integer()` (truncate toward zero, exact digits), falling back to f64 only for float *targets*. Tests: `decimal_text_above_2_pow_53_to_sbigint_is_exact` (`"9007199254740993.5"` → `9007199254740993` + 01S07), `decimal_text_fraction_to_slong_truncates_toward_zero_with_01s07` (`"-3.9"` → `-3`). Commit: `fix: text-to-integer fetch conversion is exact instead of via f64`

### Task 2.3: Float → integer targets: truncate-then-bound, 01S07 on fraction

`:1184-1224`: test `v.trunc()` against the target bounds; write the truncated value; `FractionalTruncation` when `v.trunc() != v` (pattern already at `:1167`). Tests: `f64_3_9_to_slong_is_3_with_01s07`, `f64_127_5_to_stinyint_is_127_with_01s07` (currently a false 22003), `f64_128_0_to_stinyint_is_22003`. Update pinned `f64_to_slong`. Commit: `fix: float-to-integer fetch conversion truncates before range-checking and reports 01S07`

### Task 2.4: Character → date/time/timestamp missing spec rows

`:255-272`: reuse the bind-side cascades (`param_convert::to_date`/`to_time`/`to_timestamp`, `:685-751`) so: timestamp text → DATE (zero time: clean; non-zero: 01S07), timestamp text → TIME (01S07 for non-zero fraction), time-only text → TIMESTAMP (current date via `current_utc_date`). Tests: one per row, e.g. `timestamp_text_with_zero_time_converts_to_date`, `timestamp_text_with_time_to_date_is_01s07`, `time_text_to_timestamp_gets_current_date`. Commit: `fix: character-to-datetime fetch conversion implements the spec's cross-form rows`

### Task 2.5: Reject impossible calendar dates

`parse_sql_date` (`:762`): add days-in-month with leap-year rule (Feb 29 valid in 2024, not 2023). Tests: `feb_30_is_rejected` (22007 per the module's documented convention — keep the documented deviation, do not switch to 22018 silently), `feb_29_2024_is_accepted`, `apr_31_is_rejected`. Applies to both directions since `to_date` shares the parser. Commit: `fix: date parsing rejects impossible calendar days`

### Task 2.6: Numeric → char whole-digit loss is 22003, not 01004

`:208-230`: when the target C buffer cannot hold all whole digits (sign included) of a numeric source, return `numeric_value_out_of_range()` (22003) instead of truncating with 01004; fractional-digit-only loss stays 01004. Test: `i64_123456_into_4_byte_char_buffer_is_22003`; `f64_1_25_into_buffer_holding_1_2_is_01004`. Commit: `fix: whole-digit truncation of numeric-to-character fetch is an error per the spec`

### Task 2.7: SQL_NTS scan cap is an error, not a silent prefix

`src/utf16.rs:49-61` (`utf16_to_string`) and `nts_utf16_len`/`nts_byte_len`: reaching `MAX_NTS_SCAN` without a terminator returns `Err(OdbcError::...)` (HY090, buffer length invalid — re-read the SQLExecDirect spec page for the right row) instead of the prefix. Callers in `execute.rs`/`params.rs` propagate. Test: `nts_input_longer_than_the_scan_cap_is_hy090_not_a_truncated_statement` (build a 40 000-unit terminator-less buffer; assert error, and that no backend call was made via `MockRecordingBackend`). Commit: `fix: an unterminated SQL_NTS input past the scan cap errors instead of executing a prefix`

### Task 2.8: Cap hostile exponents before allocation

`src/param_convert.rs:364, 452`: bound the expanded digit count (`|scale|` + digits) at a named constant (e.g. `MAX_DECIMAL_EXPANSION = 2 * 38 + 2`-ish — pick against the max ODBC precision and document); beyond it return 22003. Test: `huge_exponent_literal_is_22003_without_allocating` (`"1e2147483646"` as `SQL_C_CHAR`→`SQL_BIGINT`; assert 22003 — allocation absence is asserted by the test completing instantly under Miri, note in comment). Commit: `fix: pathological exponents in numeric literals are rejected before expansion`

### Task 2.9: Bounded NTS scan for SQL_C_CHAR parameters — DONE inside Task 2.7

Landed as part of Task 2.7, which found this call site while enumerating every
caller of the terminator-scanning helpers. `src/ffi/params.rs` now uses
`nts_byte_len(ptr)?` followed by `from_raw_parts`, at the exact site named
below; the review confirmed it covers this task's scope and introduces no
alignment risk (`u8`, alignment 1). One caveat recorded there: bounding makes
the output-only-binding over-read *bounded*, not absent — what closes that is
`collect_params` never reading such a binding. No separate commit.

`src/ffi/params.rs:790`: replace `CStr::from_ptr` with `nts_byte_len` (the helper's own docs name this exact call as the thing to replace). Test: behavioural parity test `char_param_with_nts_reads_to_the_terminator`; the unbounded-read half is un-assertable natively — cover with a Miri run over the params tests (an OOB read would fail Miri). Commit: `fix: SQL_C_CHAR/SQL_NTS parameters use the bounded terminator scan`

### Task 2.10: Column-count narrowing saturates up, not down

`src/ffi/metadata.rs:1594, 1823`: `u16::try_from(n).unwrap_or(u16::MAX)` with a `tracing::warn!`. Test: not practically constructible (needs >65 535 backend columns); verify by code review + the existing 07009 tests still passing. Commit: `fix: oversized backend column counts saturate to u16::MAX for the 07009 range check`

### Task 2.11: A late-firing timeout must not relabel later failures HY008

`src/query_timer.rs`, `src/cancel.rs:35`: record that a *core timer* signalled the token (flag alongside the token or on `FiredCancel`) so `reclassify_cancelled_opt` yields `HYT00` for timer-signalled tokens even when the observing call's own timer never fired. Also add the missing ordering test from the test-gap audit. Tests: `a_token_signalled_by_the_timer_reports_hyt00_on_the_next_failing_call`; `simultaneous_cancel_and_timeout_reports_hyt00` (signal token manually + fire timer, assert `QueryTimer::check` ordering — unit-testable per audit). Commit: `fix: timer-signalled cancellations report HYT00 on subsequent calls, not HY008`

### Task 2.13 (addendum): raise MAX_NTS_SCAN

**Not from the original audit** — the value question was exposed by Task 2.7 and
Andrew ruled: land 2.7 as-is, raise the cap separately.

Task 2.7 turned a silent truncation into a clean error, which is strictly
better for the same input. It did not make the *value* right.
`MAX_NTS_SCAN = 32 767` is `i16::MAX` — the width of ODBC's *name-length*
arguments — while `TextLength` is `SQLINTEGER`. Generated SQL routinely exceeds
32 767 characters, so core now refuses statements every other driver executes.

Evidence gathered during 2.7's review, from cloned source rather than docs:
psqlODBC's `ucs2strlen` (`win_unicode.c:125-131`) and `make_string`
(`misc.c:105-116`) are unbounded; MySQL Connector/ODBC's `sqlwcharlen`
(`util/stringutil.cc:713-719`) likewise, and its one `HY090` length limit
(`GET_NAME_LEN`, 192 bytes) is a post-hoc MySQL identifier check on catalog
arguments, never on SQL text; FreeTDS uses `strlen`/`wcslen`
(`src/odbc/odbc_util.c:60-105`). `strnlen`/`wcsnlen` appear in none of them.
Decisively, unixODBC forwards `SQL_NTS` **unchanged** for a Unicode
application talking to a Unicode driver (`SQLExecDirectW.c:312-315`,
`SQLDriverConnectW.c:777-781`) — it only resolves the length itself on the
ANSI-app path — so a W-only driver behind it sees raw `SQL_NTS` from every
Unicode application, and the refusal is reachable in the ordinary case.

Pick a value with a stated derivation rather than by feel, and say what the
bound is still *for* (it guards a missing terminator; nothing about memory
safety requires it to be small). Keep the error path and its tests — only the
constant and its doc move. Check whether the boundary tests, which allocate
buffers at exactly the cap, become expensive enough at the new value to matter
under Miri, and if so what to do about it. Commit:
`fix: MAX_NTS_SCAN no longer refuses statements every other driver executes`

### Task 2.12 (addendum): SQL_C_FLOAT overflow is 22003, not 01S07

**Not from the original audit** — found during Task 2.3's review, added on Andrew's instruction.

`src/column_value.rs:1346-1354` (the `SQL_C_FLOAT` arm of `write_numeric_pivot`): when `v as f32` overflows to ±infinity, the arm returns `01S07` with the infinity written. The *SQL to C: Numeric* row for `SQL_C_FLOAT`/`SQL_C_DOUBLE` has only **two** outcomes — in range → *Data* / `n/a`; out of range → ***Undefined*** / `22003` — and no `01S07` cell at all. So an overflowing narrow should be `22003` with nothing written.

Fetch the row and quote it before changing anything: if the row genuinely lacks an `01S07` cell, the current behaviour delivers an infinity an application did not ask for and calls it a warning. Check both directions of the narrowing (`+inf` and `-inf`), and check whether an `f64` that narrows to a *subnormal* or to zero is a different cell (underflow is not the same as overflow — decide it explicitly rather than by omission).

Note the interaction with Task 2.3: that task's doc-comment edit enumerates this site as one of the `01S07` producers, so fixing the behaviour means correcting that enumeration too — otherwise the comment blesses what the code no longer does. Tests: `f64_overflowing_f32_to_float_is_22003_with_nothing_written` (both signs, sentinel), plus whatever the underflow decision needs. Commit: `fix: a float narrowing that overflows reports 22003 instead of writing infinity`

### Task 2.14 (addendum): the two float-conversion claims Task 2.12 argued but did not act on

**Not from the original audit** — both were surfaced by Task 2.12's
implementer and sharpened by its reviewer, which recommended one task covering
the pair rather than two drive-by fixes. Added on Andrew's instruction
(2026-08-03). The reason they belong together: Task 2.12 argued the case for
both *in code comments* while changing neither, so the comments currently
describe a position the code does not hold.

Both parts are one question asked twice — **what does a float target report
when the value it was handed is not the value it delivers?** Decide them
together or they will disagree.

#### Part A — the *in-range* `01S07` in both float arms is core's invention

`src/column_value.rs`, `write_numeric_pivot`: the `F64`→`SQL_C_FLOAT` arm and
the `i64`→`SQL_C_FLOAT` arm both report `01S07` when the narrowing is inexact
but lands on a representable `f32`. The *SQL to C: Numeric* row for
`SQL_C_FLOAT`/`SQL_C_DOUBLE` has **two** cells — in range → *Data* / `n/a`;
out of range → *Undefined* / `22003` — and Task 2.12's reviewer independently
confirmed against the live page that the integer row above it and the
`SQL_C_BIT` row below it *do* carry `01S07`, so the float row's omission is a
distinction the table draws rather than an oversight. Neither psqlODBC
(`convert.c`, `case SQL_C_FLOAT`) nor MySQL Connector/ODBC
(`driver/results.cc`, `sql_get_data`) reports anything on this path.

So spec and drivers agree, which makes this a decision rather than an ask —
but it is a decision to **remove a diagnostic**, so:

- Fetch the row and quote it before changing anything, and check whether any
  footnote or the table's preamble grants `01S07` to a float target by another
  route. A blind removal is the failure mode here.
- One decision must cover **both** arms. Fixing the `F64` arm alone leaves the
  `i64` arm making the same claim the change just rejected.
- The severity moves from `SQL_SUCCESS_WITH_INFO` to `SQL_SUCCESS`, and the
  data delivered does not change. That is user-facing at all five
  `write_column_value` entry points (`SQLFetch`, `SQLFetchScroll`,
  `SQLGetData`, `SQLExecDirect`, `SQLExecute`) — an application watching for
  `01S07` to detect precision loss stops seeing it. Say so in the CHANGELOG.
- **Three tests pin the behaviour being removed** and must change in the same
  commit, red-green: `int_to_float_precision_loss_returns_01s07`,
  `f64_narrowed_to_f32_with_precision_loss_warns`, and Task 2.12's
  `f64_underflowing_to_zero_in_f32_is_01s07_with_zero_written`. The third is
  the one most easily mistaken for a regression, because Task 2.12 added it
  *as* characterisation of exactly this invented warning — its comment says so.
- Task 2.12's doc-comment enumeration of the `01S07` sites (itself a correction
  of Task 2.3's) must be corrected again, and this time the enumeration shrinks
  by two entries rather than changing one.

#### Part B — `parse_numeric_text` turns `"1e400"` into `Ok(inf)`

`src/column_value.rs`, `parse_numeric_text`: a character column holding a
literal beyond `f64` range parses to infinity and is then written with
`SQL_SUCCESS`. After Task 2.12, `F64(1e300)` into `SQL_C_FLOAT` errors with
`22003` while `String("1e400")` into the same target still delivers `+inf` and
calls it success — and Task 2.12's own justification ("delivering a number the
data source never held") applies to the second verbatim. This is the governing
table's *SQL to C: Character* row, not *Numeric*, so read that row rather than
reusing Part A's.

It affects `SQL_C_DOUBLE` as well as `SQL_C_FLOAT`, and the loss happens at
parse time, before the arm that would range-check it — so the fix site is
`parse_numeric_text`, not the pivot. Check whether the same literal reaches any
*other* caller of `parse_numeric_text` that would now start erroring, and name
them; a parse-site change has wider reach than an arm-site one, which is the
main risk in this part.

Tests: name them for the concluded contract, not for the current behaviour.
`a_character_literal_beyond_f64_range_is_22003_with_nothing_written` covers
Part B for both `SQL_C_FLOAT` and `SQL_C_DOUBLE`; Part A needs the three pinned
tests above inverted plus one asserting `SQL_SUCCESS` on a genuinely inexact
narrowing. Commit: `fix: a float target reports only the diagnostics its spec row defines`

**Outcome (2026-08-03).** Both parts done as argued; both rows were re-fetched
first and quoted in the code comments. Three things the task did not predict:

- **A fourth Part A test was needed.** A NaN reached the removed warning by the
  same equality test, so `f64_nan_to_float_is_written_without_a_diagnostic`
  pins the case that shows the `01S07` was *wrong* rather than merely
  unauthorised — a NaN never compares equal to its source, so nothing had been
  truncated.
- **Part B needed a three-way parse outcome, not a two-way one.**
  `parse_numeric_text` returned `Option`, whose `None` the call site read as
  `22018`; the new `NumericPivotError` separates that cell from the `22003` one
  and from the `07006` a non-numeric `ColumnValue` variant gets.
- **`parse_numeric_literal` is the wrong discriminator for Part B**, which is
  the near-miss worth recording: it parses the exponent as an `i32`, so
  `"1e2147483648"` is `None` there and would have slipped through as an
  infinity. The check is "does the text contain a digit" instead — among the
  strings Rust's float parser accepts, the only digitless ones are the
  `inf`/`infinity`/`nan` spellings. Both near-miss inputs are in the test.

Part B's reach is narrower than the task feared: `parse_numeric_text` has one
non-test caller (`column_value_as_numeric`), which has one caller (the numeric
arm of `write_column_value`). An integer target keeps the SQLSTATE it had —
`22003` either way, by the exact-numeric row — so only its diagnostic message
changes, and `SQL_C_BIT` likewise. Two stale CHANGELOG claims about this
behaviour were corrected in the same commit, one of them Task 2.12's own
deferral bullet.

---

## Phase 3 — Investigations (mature-driver checks; ask before changing)

Each produces either a fix commit or a documented decision. **If drivers disagree with the spec or each other, stop and ask Andrew** (project rule).

### Task 3.1: SQLGetDescRecW length semantics (bytes vs characters)

- [x] Read psqlODBC's and MySQL Connector/ODBC's `SQLGetDescRec`/W implementations (source, not docs) for `BufferLength`/`StringLengthPtr` units.
- [x] If they corroborate the spec (characters): fix `src/ffi/desc.rs:916, 920` (drop the `/2` and `*2`), update the doc comment **with the evidence found**, update the two pinned tests (`desc.rs:2291, 2452`) — red-green on the pins. CHANGELOG (Fixed). Commit: `fix: SQLGetDescRecW buffer and length are counted in characters per the spec`
- [x] If they contradict the spec: report both findings to Andrew and stop.

**Outcome (2026-08-03).** Characters, fixed. The drivers corroborate the spec —
FreeTDS via `odbc_set_dstr` → the character-counted `odbc_set_string` (it has a
byte-counted `odbc_set_string_oct` and uses it elsewhere, so the choice is
deliberate), psqlODBC via `*StringLength = nlen` from `utf8_to_ucs2_lf`. MySQL
Connector/ODBC abstains: `SQLGetDescRecW` is `NOT_IMPLEMENTED`.

Four corrections to what this task assumed:

- **The two named tests did not pin the units.** Both read an *unnamed* record
  and assert `name_len == 0`, which is true in either unit, so both passed
  before and after. The red test had to be written from scratch
  (`get_desc_rec_counts_the_name_buffer_in_characters_not_bytes`, a six-character
  IPD name read twice — once with room to spare, once through a four-character
  buffer). Their `BufferLength` arguments were still corrected from
  `name.len() * 2` to `name.len()`, which under the fixed reading overstated the
  buffer by 2×.
- **`SQLGetDescField` is the reason this is decidable at all**, and it is *not*
  part of the fix. Its page says "total number of bytes" and carries the "must
  be an even number" clause that `SQLGetDescRec`'s page lacks, so the two
  siblings differ by design and core's byte counts at `desc.rs:219-228` are
  correct. `SQLGetCursorNameW` was already character-counted. Core follows each
  page's own wording rather than one house rule — worth knowing before "fixing"
  the neighbours for consistency.
- **unixODBC's DM is self-inconsistent here** and was nearly read as a
  contradiction of the spec: its Unicode path passes both lengths to the driver
  untouched, while its ANSI path multiplies the driver's `*string_length` by
  `sizeof(SQLWCHAR)`. Only the second says anything about units, and it is about
  an *ANSI* driver, so it is not authority for this function. That asymmetry is
  recorded in the code comment and the CHANGELOG rather than acted on.
- **It is an ABI-visible change with no shim possible** — the two readings are
  indistinguishable at the call site. Flagged in the CHANGELOG.

### Task 3.2: METADATA_ID delimited identifiers with embedded quotes

- [x] Check psqlODBC/MySQL handling of doubled quotes inside delimited identifiers under `SQL_ATTR_METADATA_ID`.
- [ ] If un-doubling is the norm: fix `src/catalog_ident.rs:49-63` (`strip_delimiters` collapses `""` → `"`; a non-terminal closing quote means "not delimited"). Tests: `doubled_quote_inside_a_delimited_identifier_is_collapsed`, `identifier_with_interior_quote_is_not_treated_as_delimited`. Commit: `fix: METADATA_ID identifier stripping collapses doubled delimiters`
- [x] Otherwise: document the evidence in the function's comment; no behaviour change. Commit as `docs:`.

**Outcome (2026-08-03).** The "otherwise" branch: **there is no norm to follow**,
because neither driver strips delimiters at all, so neither reaches the doubling
question. Documented in `catalog_ident.rs` (module docs plus
`strip_delimiters`), no behaviour change.

- psqlODBC does not implement `SQL_ATTR_METADATA_ID`: absent from `options.c`,
  whose `default` arm answers "Unknown statement option", so it cannot be set.
- MySQL Connector/ODBC threads it into `add_name_condition_oa_id` /
  `add_name_condition_pv_id` (`driver/catalog.cc`) and uses it for one thing —
  `=` instead of `= BINARY`, i.e. a case-insensitive compare. No delimiter
  handling, and a `/* Need also code to remove trailing blanks */` TODO for
  another clause of the same paragraph.
- Neither *Quoted Identifiers* nor *Identifier Arguments* mentions escaping a
  delimiter inside a delimited identifier.

**The bigger finding, escalated rather than acted on:** *Identifier Arguments*
contradicts itself within one page. Its first paragraph states the rule core
implements ("if a string in an identifier argument is quoted, the driver removes
leading and trailing blanks and treats literally the string within the quotation
marks"); its third says identifiers "must not be quoted when passed as catalog
function arguments, because quote characters passed to catalog functions are
interpreted literally", with a worked `SQLTables` example. Both drivers are on
the side of the third paragraph or silent, so core's delimiter recognition is a
deviation from both. Left as it stands — it is the only way `SQL_ATTR_METADATA_ID`
can name a case-sensitive identifier on a `SQL_IC_UPPER` source, and the page's
fourth paragraph (pseudo-column disambiguation) needs quoting to be significant —
but the decision is Andrew's, and reversing it would be a behaviour change across
all ten catalog functions.

### Task 3.3: sql_bind_parameter's off-table HY024 + guard blind spot

- [x] First close the guard gap: extend `every_doc_comment_matches_the_spec_diagnostics_table` (or add a sibling test) to flag SQLSTATEs *returned by the function body* (grep for `SqlState::` factories per file, as the existing site-audit tests do) that are neither in the transcribed table nor documented with the "absent from this function's diagnostics table" phrasing. Verify the new guard fails on current `params.rs:169` (red).
- [x] Then decide the behaviour with Andrew: (a) return `07009`-style defensive `HY105` (matching how the function already treats other (DM) rows), or (b) keep HY024 with the house phrasing. Implement the chosen one; guard goes green. Tests: `bind_parameter_with_unknown_param_type_returns_<chosen state>` asserting the SQLSTATE via `first_sqlstate`. Commit: `fix: undocumented off-table SQLSTATEs are caught by the diagnostics guard`

**Outcome (2026-08-03).** Andrew chose (a), `HY105`. The live page confirms the
row is "(DM) The value specified for the argument *InputOutputType* was invalid"
and that `HY024` occurs **nowhere** on it. Guarded defensively for the reason the
neighbouring `07009` already gives, plus one this task did not anticipate: unlike
`SQLDriverConnect`'s `DriverCompletion`, where an unrecognised value has a
defensible most-permissive fallback, there is none here — defaulting would
silently mis-bind an output parameter as an input.

Notes for whoever extends the guard:

- **The guard found exactly one violation across all sixty functions**, so the
  blind spot was narrow. It sees only literal `SqlState::factory()` calls between
  a function's signature and its column-zero closing brace, and therefore misses
  a state produced in a helper, one carried by an `OdbcError` variant such as
  `FractionalTruncation`, and one propagated from a backend. Under-approximating
  is deliberate; the doc comments' prose reasons are still the only thing
  covering the rest.
- **Two vacuity floors were needed, not one.** The factory map is parsed out of
  `sql_state.rs` (47 factories) rather than restated, and the body scan sees ~90
  (function, state) pairs; both are asserted, because either a changed
  `pub fn`/`Self::new` shape or a changed function-signature shape would make the
  guard pass while checking nothing.
- **It is the third `include_str!` scanner in that module and carries the same
  `#[cfg_attr(miri, ignore)]`.** AGENTS.md's Miri section is updated to say three
  and to warn that a fourth needs the attribute too.

---

## Phase 4 — Compliance and documentation sweeps

Doc-only changes cannot be test-verified except via the doc guards and `pre-commit` (cargo-doc hook); each task's gate is the standard GATE. Group into four commits.

### Task 4.1: Diagnostics-table and doc-comment corrections (code-adjacent)

- [x] **Three catalog `HY090` rows are transcribed wrong — found during Task
  2.7's review, verified against the live pages.** `SQLColumns`,
  `SQLForeignKeys` and `SQLColumnPrivileges` are marked `DmMarking::All`, but
  each live page carries an unmarked "exceeded the maximum length value for the
  corresponding catalog or name" clause after the `(DM)` one — the same
  two-clause shape `SQLTablePrivileges` already has, correctly, as
  `Split("maximum length")` at `src/types/diagnostics_table.rs:1176`. So
  `SQLColumnPrivileges` at `:1146` sits beside its own counter-example. Change
  the three rows to `Split("maximum length")` and rewrite the three doc
  sentences at `src/ffi/metadata.rs:511-513`, `:903-905` and `:2504-2506`,
  which currently assert the opposite ("this one has only the
  name-length-below-zero sentence and no maximum-length sentence, so nothing in
  it is the driver's"). Note the existing bullets hyphenate "maximum-length",
  which would **not** satisfy a `Split("maximum length")` substring check — so
  the guard fails loudly until both halves are fixed, which is the desired
  behaviour. Verify each against the live spec page, not against the
  transcription: reading the transcription is how this slipped through in the
  first place.
- [x] `src/types/diagnostics_table.rs:699`: SQLGetData HY010 `All` → `Split("SQL_PARAM_DATA_AVAILABLE")`; update the `sql_get_data` doc bullet (`fetch.rs:911`) to answer the unmarked clause ("cannot arise — core never returns SQL_PARAM_DATA_AVAILABLE"). The guard test enforces the pairing (it will fail between the two edits — that is the red step).
- [x] Reword to the "guarded defensively"/"returned here as a defence-in-depth guard" verdicts: `fetch.rs:888-895` (24000), `metadata.rs:237-238` (SQLTables HY010 — and sweep the other 11 catalog functions for the same pattern), `fetch.rs:243-245` (HY010, align with the inline comment at :364), `connect.rs:57-58, 291-292` (08002 — copy browse_connect's wording), `connect_attr.rs:326-328` (HY090), `fetch.rs:229-231` (24000 S2/S3 extension is core's choice, not the spec's).
- [x] Delete the dangling "Deferred." at `connect_attr.rs:304-313`.
- [x] Commit: `docs: diagnostics doc comments state defensive guards honestly; SQLGetData HY010 is Split`


**Outcome (2026-08-03).** All four items done, verified against the pages'
**source markdown** (`raw.githubusercontent.com/MicrosoftDocs/sql-docs`) rather
than the rendered HTML — which is both faster to grep and the thing the rendering
is generated from. Five findings worth carrying forward:

- **The two-clause rows use two different encodings, and one is invisible to a
  grep for the SQLSTATE.** `SQLTablePrivileges` puts both clauses in one cell
  separated by `<br /><br />`; `SQLColumns`, `SQLForeignKeys` and
  `SQLColumnPrivileges` use a **continuation row** whose first two cells are
  empty (`|||The value of one of the name length arguments exceeded…`). A grep
  for `|HY090|` finds the first clause and silently misses the second, which is
  exactly how three rows came to be transcribed `All`. Grep with `-A 1`.
- **The guard could not have gone green on `SQL_PARAM_DATA_AVAILABLE`**: its
  `Split` check compared a raw needle against a lowercased haystack, so any
  needle containing a capital never matched and no doc edit could have cleared
  the failure. Every pre-existing needle happened to be lowercase. Both sides are
  lowercased now.
- **Two sites making the same "no maximum-length sentence" claim were correct and
  left alone.** `SQLDescribeCol` and `SQLColAttribute` really do have
  single-clause `(DM)`-only `HY090` rows. The three named in this task were the
  only wrong ones.
- **The catalog `HY010` sweep is exactly ten functions, not twelve.** The ten
  that call `SqlState::function_sequence_error()` are the ten catalog functions;
  `sql_describe_col_w` and `sql_col_attribute_w` do not return `HY010` at all, so
  their `(DM)`-only bullets were already true.
- **Every clause of those `HY010` rows is `(DM)`, and none of them is the
  condition core actually checks.** The rows enumerate async execution,
  `SQL_PARAM_DATA_AVAILABLE` and `SQL_NEED_DATA`; core returns `HY010` when the
  connection is not open, which no clause states. The bullets now say that rather
  than implying core answers one of the row's own clauses. The same correction
  applied to `SQLFetch` (all six clauses `(DM)`, including the one core acts on),
  `SQLGetData`'s `24000` (core's `cursor_open` test overlaps a `(DM)` clause and
  extends past the row for S2/S3), `SQLFetch`'s `24000` (unmarked, so core's — but
  the S2/S3 half is core's own extension), the three `08002` bullets in
  `connect.rs` (each of which said "not returned here" *and* "checked as
  defence-in-depth" in consecutive sentences), and `connect_attr.rs`'s `HY090`.

No CHANGELOG entry: doc-comment corrections with no behaviour change, matching
what previous `docs:` commits in this branch did.

### Task 4.2: The HYT01 cluster and rotted uniqueness claims

- [ ] Replace "HYT01: not implemented / not applicable" with "propagated from the backend unchanged" at `fetch.rs:263, 566, 732, 927`, `cursor.rs:243, 346`, `metadata.rs:1519, 1726`; same treatment for the reason-less "HY013: not returned" lines (`fetch.rs:246, 549, 715, 912`).
- [ ] Fix uniqueness claims: `col_attr.rs:483` + `descriptor.rs:667` (set_concise_type "only writer" — name the `Desc::Type`/`Desc::DatetimeIntervalCode` arms); `handles/mod.rs:918` (`implicit_descriptor_token` callers); `handles/mod.rs:1276` + AGENTS.md ("three callers of HandleScope::new" → four); `sync.rs:27` + `logging.rs:72` (record the tracing `MakeWriter` Mutex as the second stated exception, with the loom justification, in both files + AGENTS.md); `backend.rs:440, 878` ("nine statement-producing methods" → the actual count, or replace with "the methods taking `cancel:`" so it cannot rot); `descriptor.rs:382` + `desc.rs:25` (HY091 "sole authority" → name `field_from_raw`'s unknown-integer mint).
- [ ] Add the found evidence to the bare driver citations: `handle.rs:526, 1187`, `backend.rs:979`, `params.rs:1379` (look each up in the named driver's source while writing the citation; if unverifiable, soften the sentence instead).
- [ ] Commit: `docs: whole-path SQLSTATE claims, uniqueness claims and driver citations match reality`

### Task 4.3: Module headers and crate-level counts

- [ ] `src/ffi/desc.rs:28-33`: delete/rewrite "What is still missing" (remaining true gaps: bookmark records, auto-IPD).
- [ ] `src/lib.rs:8, 66-67`: replace the four counts (72/32/40/36) with count-free prose pointing at `CORE_EXPORTED_FUNCTIONS`.
- [ ] `src/ffi/params.rs:1` (name all five entry points), `src/ffi/fetch.rs:1` (+ `sql_extended_fetch`), `src/handles/mod.rs:1` (+ descriptor), `src/forward_ffi.rs` (add a `//!`), `src/errors.rs:223` ("~70" → "the"), `src/test_utils.rs:3` (drop the nonexistent `MockFailBackend` mention — superseded by Task 5.1 which adds it for real, so instead make the sentence true there).
- [ ] Commit: `docs: module headers and crate docs describe what the modules now contain`

### Task 4.4: Markdown drift (README, AGENTS, CHANGELOG)

- [ ] README: 4 associated types / 47 required methods (or drop the numbers); "deprecated ODBC 2.x left out (except SQLExtendedFetch, which the DM does not map)"; drop or caveat the "1345 unit tests" count.
- [ ] AGENTS.md: "six" → "ten" catalog row structs (:1339); add crate-layout rows for `cancel.rs`, `types/catalog_queries.rs`, `types/diagnostics_table.rs`, `types/odbc_version.rs`; add `sql_extended_fetch` and `sql_copy_desc` to their rows; fix the three `*_from_raw` signatures (`u16`) and add the three missing functions; "~1320 tests" → re-measured figure; `result_cols.rs` row mentions `CatalogResultColumnWidths`; mock list rephrased as examples; "1.72 MB" → current figure (also in `diagnostics_table.rs:2058, 2064, 2160`).
- [ ] CHANGELOG migration §6: `tables` takes `&TablesQuery<'_>`; re-check §§1-8 against the query-type signatures.
- [ ] Commit: `docs: README, AGENTS and CHANGELOG match the current code`

---

## Phase 5 — Test gaps

### Task 5.1: The connect-failure path runs for the first time

**Files:** `src/test_utils.rs` (new `MockFailBackend`: `connect` returns `Err` with SQLSTATE `08004`), `src/ffi/connect.rs` tests.

- [ ] Tests: `failed_connect_surfaces_the_backends_sqlstate` (SQLDriverConnectW → `SQL_ERROR`, `first_sqlstate == "08004"`), `failed_connect_leaves_the_handle_unconnected` (subsequent `SQLDisconnect` → `08003`), `pending_attr_failure_tears_the_connection_down` (backend connects OK but `set_current_catalog` fails → error propagates, connection torn down — exercises `connect.rs:221-230`).
- [ ] These are new tests over existing code — expected to pass; if any fails, that is a real bug: stop and report before fixing.
- [ ] GATE. Commit: `test: the Backend::connect failure path is exercised`

### Task 5.2: Misalignment tests for every marshalling family

**Files:** tests in `src/column_value.rs`, `src/ffi/fetch.rs`, `src/ffi/desc.rs`, `src/ffi/diag.rs`, `src/ffi/metadata.rs`.

- [ ] One arena+1 test per family, using the AGENTS.md pattern (offset one byte into an allocation of the *target* type): fixed-width `write_column_value` (i64, f64, `SQL_TIMESTAMP_STRUCT` targets), `SQLFetch` indicator (`isize`) writes, row-status `u16` array, `SQLGetDiagFieldW`/`SQLGetDescFieldW` integer outputs, `SQLDescribeColW` outputs.
- [ ] Verify the tests bite: run under `MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-symbolic-alignment-check"` — they pass; temporarily revert one `write_unaligned` to `write` locally and confirm Miri fails (do not commit the revert).
- [ ] Also fix the fuzz target's alignment blind spot: `fuzz/fuzz_targets/column_value.rs:144` — allocate the arena as the widest target type and offset, so ASan/Miri luck is not required.
- [ ] GATE (+ `(cd fuzz && cargo +nightly build ...)` since fuzz/ changed). Commit: `test: every marshalling family has a deliberately misaligned-buffer test`

### Task 5.3: Error tests assert their SQLSTATE

- [ ] Retrofit the ~15 return-code-only tests (list: `bind.rs:196, 447`; `connect.rs:1772, 1849`; `connect_attr.rs:2128` + 3 siblings; `params.rs:2984, 3006` + 3 siblings; `stmt_attr.rs:3313, 3337`; `tran.rs:591`; `fetch.rs:2216` + neighbours) with `first_sqlstate`-style assertions against the state their name claims. Where a file lacks the helper, copy the 6-line pattern from `execute.rs:1740`.
- [ ] Any retrofit that then FAILS reveals a wrong-state bug: stop, report, fix as its own red-green step within this task.
- [ ] GATE. Commit: `test: error-path tests assert the SQLSTATE their names claim`

### Task 5.4: Remaining coverage gaps

- [ ] `22002` assertions (both `sql_fetch` bound-column and `sql_get_data` paths — the fetch one lands with Task 1.1's test; add the get_data one).
- [ ] One `01004` test asserting the diagnostic *record* content, not just SUCCESS_WITH_INFO.
- [ ] Spec-order tests for `ColumnRow` (18 columns) and `ProcedureColumnRow` (19) in `types/catalog_rows.rs`, mirroring `table_row_converts_in_spec_column_order`.
- [ ] Huge-ordinal tests: `column_number = u16::MAX` → `07009` for `sql_get_data`, `sql_bind_col`, `sql_bind_parameter`, `sql_col_attribute_w`.
- [ ] GATE. Commit: `test: 22002, 01004 record content, wide catalog rows and huge ordinals are pinned`

---

## Phase 6 — Performance (each verified by the Task 0.1 benchmark; correctness by existing tests)

### Task 6.1: Cache the conversion in GetDataCursor (P1/S1)

**Files:** `src/handles/mod.rs:576` (`GetDataCursor` gains a cached payload, e.g. `enum CachedChunkSource { Utf16(Vec<u16>), Bytes(Vec<u8>) }` + total length), `src/ffi/fetch.rs:1046-1071`, `src/column_value.rs` chunk writers take the cached slice.

- [ ] Red: capture the `ffi_get_data_chunked` baseline; write `chunked_get_data_converts_the_value_once` — instrument via a counting mock whose `get_data` increments a counter; assert one backend materialisation per column per row, not one per chunk (fails now).
- [ ] Implement; all existing chunking tests (interleaved columns, restart-on-new-column, indicator remaining-length) must stay green — they define the contract.
- [ ] Verify: benchmark shows the quadratic gone (record numbers in the commit message). This also closes security finding S1. GATE + targeted Miri. CHANGELOG (Fixed — DoS class). Commit: `perf: chunked SQLGetData converts each value once instead of per chunk`

### Task 6.2: Encode strings directly into the target buffer (P2)

`src/column_value.rs` `write_wchar`: bounded `encode_utf16()` write via per-unit `write_unaligned`, `count()` for the remainder — no intermediate `Vec` on the single-shot path (the Task 6.1 cache covers chunked). Numeric→char: format into a stack buffer. Red/green via `ffi_fetch_bound` benchmark delta + existing conversion tests. Commit: `perf: string and numeric fetch conversions stop allocating per value`

### Task 6.3: One binding collection per fetch (P4)

`src/ffi/fetch.rs:319-322, 408-427`: fuse `collect_bindings` and `binding_info` (apply `c_type_of` + offset inside the first pass); sort by column number for deterministic write order. Existing fetch tests green; benchmark delta. Commit: `perf: sql_fetch builds its binding list once, in column order`

### Task 6.4: Fewer registry lookups per fetch/get_data (P5)

Restructure `fetch_with_report` around one `stmt_with_desc` resolution per AGENTS.md's own `descriptor_token` guidance (~6 → ~4 acquisitions); same for `sql_get_data`'s 3. `handle_lookup` + `ffi_fetch_bound` show the delta; loom models unaffected (no new acquisition *sites* — the site-closure guard `the_set_of_group_lock_acquisition_sites_is_closed` must stay green unmodified, which is the real test here). Commit: `perf: fetch resolves the statement once per call`

### Task 6.5: Persistent query-timer thread (P3)

`src/query_timer.rs`: one lazily-started deadline thread (register/deregister via the existing std Condvar — the documented loom exception carries over; update the exception comments). All existing timer tests (HYT00 at execute and fetch, disarm, poisoning) are the contract; add `arming_twice_reuses_the_thread` via a thread-count probe if cheaply assertable, else rely on the existing behavioural suite. Benchmark: timed-fetch variant added to `ffi_fetch_bound`. Commit: `perf: SQL_ATTR_QUERY_TIMEOUT uses one timer thread, not one per fetch`

### Task 6.6: Small wins + logging hardening (LOW batch, one commit)

- [ ] `write_utf16` null-buffer branch uses `count()` (no `Vec`) — existing length-probe tests green.
- [ ] `translate_escapes` fast path: `if !sql.contains('{')` return owned input — escape tests green; add `text_without_braces_is_returned_unchanged`.
- [ ] `catalog_rows.rs`: add consuming `into_values(self)`; switch the ten `metadata.rs` call sites.
- [ ] `logging.rs` (S2): document the symlink-follow behaviour and same-user threat model in the module header; optionally add CRLF-stripping for backend-originated text in log lines (decide with Andrew — it changes log fidelity).
- [ ] GATE. Commit: `perf: small allocation wins on cold paths; logging threat model documented`

---

## Phase 7 — Final verification

### Task 7.1: Deferred Miri pass and full-suite sign-off

- [ ] `MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test -p stackable-odbc-core --lib -- --skip proptest` — zero failures, zero leaks. Budget for a cold rebuild (AGENTS.md: the warm run is ~5-6 min; a rebuild dominates).
- [ ] `MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-symbolic-alignment-check" cargo +nightly miri test -p stackable-odbc-core --lib -- --skip proptest` — the alignment class regardless of allocator luck (mandatory: this work touched pointer marshalling).
- [ ] `RUSTFLAGS="--cfg loom" cargo test --lib loom_tests` — the lock-discipline models still hold (Tasks 6.4/6.5 are the risk).
- [ ] `(cd bench && cargo build --benches)` and `(cd fuzz && cargo +nightly build --target x86_64-unknown-linux-gnu)` — the detached workspaces still compile (5.2 touched fuzz/, 0.1 touched bench/).
- [ ] `cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings` — the `#[cfg(windows)]` half.
- [ ] Any failure: fix red-green as its own step, then re-run the failed check; the fixes join the nearest pending commit or a dedicated `fix:` commit for Andrew.

## Known limitations — found during this work, deliberately not fixed

Defects surfaced by implementers and reviewers while executing the plan, which
Andrew has ruled are recorded rather than fixed here. Each is real, each has a
code comment at its site, and none is a regression introduced by this work.
Listed so a future reader finds them in one place instead of rediscovering them
one at a time.

- **More than nine fractional-second digits truncate silently**
  (`src/column_value.rs`, `parse_time_fields`; the comment sits at the
  `(String, TypeTimestamp)` arm and carries no `TODO` marker, because it
  records a decision rather than an intention). The *SQL to C: Character* row's
  "fractional seconds portion truncated → `01S07`" cell is not reported: the
  parser pads or truncates to nanoseconds without a diagnostic. Identical on
  the timestamp-value and time-value paths, so the two cannot disagree. Data
  still arrives; only the warning is missing. *Ruled known 2026-08-01.*
- **`SQL_C_NUMERIC` has no arm in `write_column_value` at all**, though it
  appears in the exact-numeric row of both governing tables. A fetch into it
  answers `HY003` rather than converting. Found during Tasks 2.2 and 2.3.
- **`SQL_C_BIT` from *character* sources still routes through `f64`.** The
  worst case is not a wrong diagnostic but lost data: `"1.9999999999999999999"`
  rounds to exactly `2.0` in `f64` and so answers `22003` with nothing written,
  where the table's second row converts it to `1` with `01S07`. Found during
  Task 2.2's review.
- **A conversion warning inside `SQLFetch`'s bound-column loop skips the
  remaining columns** (`src/ffi/fetch.rs`, the `?` in the bound-column loop).
  A `01S07` on one column abandons the rest of that row's bindings and skips
  `report_rows_fetched`, while `panic_safe` still returns
  `SQL_SUCCESS_WITH_INFO`. Pre-existing and accepted when only `i64`→`f32`
  precision loss could produce it; Tasks 2.1 and 2.3 make it far easier to
  reach. **The largest of these — worth its own task if any of them is.**
- **The expansion invariant test covers `to_decimal_string` only.**
  `to_integer`'s `"0".repeat` site rests on reasoning plus two behavioural
  tests rather than the shared-expression assertion. Found during Task 2.8's
  final review.
- **`"2026-07-21T"` is now accepted as midnight** on the DATE path, a trivial
  widening from Task 2.4's shared parser. Consistent with what the timestamp
  path already did.
- **A negative `RecNumber` aliases to record 0 for two descriptor roles.**
  `src/ffi/desc.rs:312` (`read_desc_field`) and `:603` (`write_desc_field`)
  narrow with `u16::try_from(record_number).unwrap_or(0)`. `Ard` and `Apd` are
  already protected by the negative check at `:272` / `:542`; the gap is
  `DescriptorRole::App` — the explicit descriptor whose role is not yet
  resolved, omitted from that guard — and `DescriptorRole::Ipd`, where only
  `== 0` is checked so a negative non-zero value slips through. The result is
  a missing `07009`, a spec-compliance gap rather than a safety one. Low
  severity: IPD record 0 is never consulted by real parameter binding
  (parameters start at 1), and bookmark records are deliberately out of scope
  per AGENTS.md with no path reading them for real data today. Found during
  Task 2.10's survey; the scoping above is the *reviewer's* correction — the
  original survey named the two roles that are already guarded, listed a safe
  site (`:389`, which rejects the aliased 0 unconditionally) and a dead one
  (`:1121`, where `:1074` has already refused negatives for all four roles).
  Promote to a task if the missing `07009` is wanted before 0.1.0.

## Execution order and dependencies

```
0.1 ──────────────────────────────► 6.1..6.5 (need the benchmark)
1.1 → 1.4 (shared offset helper)    2.1..2.10 independent of each other
1.3 → 6.1 (same writers/cursor)     2.11 independent
2.12 → 2.14 (2.14 corrects 2.12's own doc enumeration and inverts a test it added)
3.1/3.2/3.3 independent, may end in "ask"
4.x after phases 1–3 (doc claims must describe the *fixed* code)
5.1..5.4 independent of each other; 5.2 after 1.1/1.3 (same test arenas)
```

## Self-review notes

- Spec coverage: all 8 audit dimensions mapped — bugs (Phases 1–2), compliance (4.1, 3.3), docs/drift (4.3–4.4), headers (4.3), assertions (4.2), test gaps (Phase 5 + red tests throughout), performance (Phase 6, gated on 0.1), security (S1 via 6.1, S2 via 6.6; the rest of the security audit was clean).
- Known ask-points: 3.1, 3.2, 3.3 (behaviour choice), 6.6 (log CRLF stripping), and every commit (user rule).
- Doc-only tasks are gated by the doc guards + pre-commit rather than new tests; that is the "wherever possible" boundary.
