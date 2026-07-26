# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial extraction of `stackable-odbc-core` into its own repository.
  Provides the database-independent ODBC framework:
  protocol logic, handle allocation and tag validation, UTF-16 marshalling,
  diagnostics, panic safety, and the generic implementations of the ODBC FFI
  entry points, exported for a concrete backend via the `forward_ffi!` macro.
- `CursorBehavior` and the `SQL_CB_DELETE` / `SQL_CB_CLOSE` / `SQL_CB_PRESERVE`
  constants, modelling what `SQLEndTran` does to open cursors.
- `Backend::cursor_commit_behavior` and `Backend::cursor_rollback_behavior`.
  Both default to `CursorBehavior::Preserve`. A backend that supports
  transactions should declare its data source's actual behaviour here; the
  value drives both `SQLGetInfoW` and what `SQLEndTran` does to statements.
- `SQL_CURSOR_COMMIT_BEHAVIOR` constant (23), derived from
  `odbc_sys::InfoType::CursorCommitBehaviour`.
- The `SQL_AT_*` bitmask constants for the `SQL_ALTER_TABLE` (86) info type, so
  a driver can describe its `ALTER TABLE` support by name instead of by raw
  literal. The set is now complete: `SQL_AT_ADD_COLUMN`, `SQL_AT_DROP_COLUMN`
  and `SQL_AT_ADD_CONSTRAINT` (defined in `sql.h`) and the four
  `SQL_AT_CONSTRAINT_INITIALLY_*` / `_DEFERRABLE` / `_NON_DEFERRABLE`
  deferrability bits were missing, so a driver still needed raw literals for
  them.
- The `SQL_NC_START` / `SQL_NC_END`, `SQL_CN_*`, `SQL_NNC_*`, `SQL_GB_COLLATE`,
  `SQL_SC_*` and `SQL_FN_TSI_*` constants, plus the `SQL_ROW_UPDATES` (11) and
  `SQL_PROCEDURES` (21) info type numbers, all asserted against
  `/usr/include/sql.h` and `sqlext.h` by
  `info_type_value_constants_match_sql_headers`. These are values where a typo
  cannot look empty, because zero is itself a valid claim for several of them.
- `EscapeDialect::rewrite_scalar_fn`, which receives a whole
  `{fn NAME(args)}` escape and returns the replacement text.
  `remap_scalar_fn` only swaps the identifier in front of the parentheses and
  never sees the arguments, so any scalar function whose ODBC form differs
  from the target dialect in *argument* syntax was untranslatable —
  `{fn LOCATE('b','ab')}` → `position('b' IN 'ab')`,
  `{fn TIMESTAMPADD(SQL_TSI_DAY, 1, t)}` → `date_add('day', 1, t)`, or a
  zero-argument call that must become a bare keyword with no trailing `()`
  such as `{fn CURDATE()}` → `current_date`. Because the `SQL_*_FUNCTIONS`
  bitmaps are defined in terms of the `{fn}` escape, a driver that could not
  translate one could not honestly advertise it.
  `args` arrives already escape-translated, so a nested `{fn}` or `{ts}` is
  resolved before the dialect sees it, and with string literals, quoted
  identifiers, comments and nested parentheses intact — core does not split
  on commas, which would corrupt `{fn LOCATE(',', x)}`. Splitting arguments is
  the dialect's job. `remap_scalar_fn` stays as the cheap path, and a dialect
  setting only it is unaffected.
- `Backend::set_txn_isolation`, for a backend that can switch isolation
  levels. Defaulted: a data source with exactly one level in
  `txn_isolation_options` needs no implementation, while one declaring several
  and not overriding this reports `NotImplemented` rather than accepting a
  level it would silently fail to apply.
- `StatementHandle::cursor_open`, plus the `set_result_set`,
  `set_prepared_statement` and `discard_result_set` helpers that maintain it.
  `StatementHandle::statement` no longer doubles as the answer to "is a cursor
  open?" — see the `Fixed` entry below.

### Changed

- `SQL_CURSOR_COMMIT_BEHAVIOR` and `SQL_CURSOR_ROLLBACK_BEHAVIOR` now report
  `SQL_CB_PRESERVE` instead of `SQL_CB_DELETE`, and are derived from
  `Backend::cursor_commit_behavior` / `Backend::cursor_rollback_behavior`
  rather than hard-coded. Core previously advertised `SQL_CB_DELETE` while
  `SQLEndTran` did nothing to cursors at all.
- The derivation now also covers backends that answer these two info types
  *nowhere*: `SQLGetInfoW`'s Driver-Manager-safe fallback reports the hooks
  instead of the generic shape default. Previously such a backend got
  `U16(0)` for `SQL_CURSOR_COMMIT_BEHAVIOR` and `U32(0)` for
  `SQL_CURSOR_ROLLBACK_BEHAVIOR` — both `SQL_CB_DELETE`, the second in the
  wrong shape — while `SQLEndTran` applied the declared behaviour.
- Under `SQL_CB_CLOSE`, `SQLEndTran` now closes each statement's cursor via
  `StatementBackend::close_cursor` and keeps the statement itself, instead of
  dropping it. The `SQLEndTran` statement transition table's footnote `[2]`
  leaves the prepared states S2/S3 unchanged; dropping the statement sent a
  prepared-but-never-executed statement back to the allocated state, so a
  subsequent `SQLNumResultCols` failed with `HY010` where the spec allows it.
  A backend declaring `CursorBehavior::Close` must implement `close_cursor`,
  which defaults to a no-op.
- Under `SQL_CB_DELETE`, `SQLEndTran` now also clears a pending
  data-at-execution sequence. It already cleared `param_count`, which
  `SQLParamData` uses to size its parameter vector, so a surviving sequence
  would have executed with zero parameters and silently discarded everything
  the application streamed via `SQLPutData`.
- **Breaking:** `default_get_info` and `common_get_info_raw` are now generic
  over the backend. Call them as `default_get_info::<Self>(info_type, widths)`
  and `common_get_info_raw::<Self>(info_type)` from a `Backend` impl.
- **Breaking:** new **required** `Backend` methods —
  `supports_catalogs`, `supports_schemas`, `alter_table_support`,
  `outer_join_capabilities`, `default_txn_isolation`,
  `txn_isolation_options`, `group_by`, `null_collation`, `correlation_name`,
  `non_nullable_columns`, `expressions_in_order_by`, `sql_conformance`,
  `timedate_add_intervals` and `timedate_diff_intervals`.
  They are required rather than defaulted on purpose:
  each states a *capability*, where a defaulted value is a claim the backend
  author never made and is unlikely to notice. A defaulted `0` understates and
  a defaulted `true` overstates; the compiler asking is what makes the fact
  explicit. Every one of them replaces a value core previously invented (see
  `Fixed`).
- `SQL_ALTER_TABLE` and `SQL_OJ_CAPABILITIES` now come from
  `Backend::alter_table_support` / `Backend::outer_join_capabilities` instead
  of defaulting to `0`. `0` remains the shared default for the surrounding
  *limits* (`SQL_MAX_INDEX_SIZE`, `SQL_MAX_ROW_SIZE`, `SQL_MAX_STATEMENT_LEN`,
  the `SQL_MAX_COLUMNS_IN_*` group), where the spec defines it as "no
  specified limit or the limit is unknown".
- `SQLSetConnectAttr(SQL_ATTR_TXN_ISOLATION)` now validates its value and
  applies it, where it previously stored any `usize` and echoed it back. A
  value that does not name exactly one isolation level, or names one outside
  `Backend::txn_isolation_options`, is rejected with `HY024` — the spec assigns
  this check to the driver, since the Driver Manager only validates attributes
  "that accept a discrete set of values". An accepted level is passed to the
  new `Backend::set_txn_isolation`, including one set before connecting, which
  is applied when the connection opens (the same deferral
  `SQL_ATTR_AUTOCOMMIT` already had).

### Fixed

- Every `SQLGetInfo` type the spec declares as a character string but which
  `odbc_sys::InfoType` has no variant for is answered as a string, instead of
  falling through to the unnamed-raw default `U32(0)`. An application reading
  one into a character buffer got four bytes of binary zero with
  `StringLength = 4`. Found by sweeping every info-type number in
  `sql.h`/`sqlext.h` against `info_type_from_raw`, rather than one at a time:
  `SQL_ROW_UPDATES` (11) `"N"`, `SQL_PROCEDURES` (21) `"N"`,
  `SQL_MULTIPLE_ACTIVE_TXN` (37) `"N"`, `SQL_DATABASE_NAME` (16) `""`,
  `SQL_PROCEDURE_TERM` (40) `""` (consistent with `SQL_PROCEDURES = "N"`),
  `SQL_TABLE_TERM` (45) `"table"`, and `SQL_KEYWORDS` (89) `""`.
  All come from `common_get_info_raw`, and core's own fallback consults that
  helper, so a backend whose `get_info_raw` does not delegate to it gets the
  same answer. That last change also fixes `SQL_QUOTED_IDENTIFIER_CASE` for
  such a backend, which previously defaulted to `U16(0)` — not one of the four
  `SQL_IC_*` values.
- `SQL_MULT_RESULT_SETS`, `SQL_MAX_ROW_SIZE_INCLUDES_LONG` and
  `SQL_NEED_LONG_DATA_LEN` now default to `"N"` instead of `""`. They had no
  arm in `default_get_info`, so the shape-aware fallback gave them the empty
  string: the right shape, but not a value in any of their value lists.
- `SQL_GROUP_BY`, `SQL_NULL_COLLATION`, `SQL_CORRELATION_NAME` and
  `SQL_NON_NULLABLE_COLUMNS` are stated by the backend rather than invented by
  core. For all four, zero is a *substantive answer* — `SQL_GB_NOT_SUPPORTED`,
  `SQL_NC_HIGH`, `SQL_CN_NONE`, `SQL_NNC_NULL` — so the shape default was
  handing every backend a specific spec claim it never made. `SQL_GROUP_BY`
  was worse than a default: core actively reported `SQL_GB_NO_RELATION`.
- `SQL_SQL_CONFORMANCE` comes from the backend instead of a hard-coded
  `SQL_SC_SQL92_ENTRY`. Core claimed entry level while separately supplying
  `SQL_CORRELATION_NAME`, `SQL_NON_NULLABLE_COLUMNS` and `SQL_GROUP_BY` values
  the spec says an entry-level driver never returns, so every backend built on
  core inherited a contradiction it could not see.
- `SQL_EXPRESSIONS_IN_ORDERBY` is stated by the backend. It previously fell to
  `""`, which reads as "no" to a tool deciding whether to push an expression
  into `ORDER BY`.
- `SQL_TIMEDATE_ADD_INTERVALS` and `SQL_TIMEDATE_DIFF_INTERVALS` are stated by
  the backend instead of defaulting to `0`. The spec defines them as the units
  `TIMESTAMPADD` / `TIMESTAMPDIFF` accept, so `0` alongside a
  `SQL_FN_TD_TIMESTAMPADD` claim in `SQL_TIMEDATE_FUNCTIONS` says the function
  exists but takes no units.
- `SQL_CATALOG_TERM`, `SQL_SCHEMA_TERM` and `SQL_CATALOG_NAME_SEPARATOR` no
  longer name catalogs and schemas that the data source does not have. The
  `SQLGetInfo` spec defines the whole group — those three plus
  `SQL_CATALOG_NAME`, `SQL_CATALOG_LOCATION`, `SQL_CATALOG_USAGE` and
  `SQL_SCHEMA_USAGE` — in terms of whether catalogs (resp. schemas) exist at
  all, and mandates an empty string or zero when they do not. Core hard-coded
  `"catalog"`, `"schema"` and `"."`, so a backend reporting
  `SQL_CATALOG_NAME = "N"` and letting the rest fall through contradicted
  itself. All seven are now derived from `Backend::supports_catalogs` /
  `Backend::supports_schemas`. Where the spec only mandates the *zero*, core
  asserts only that: `SQL_CATALOG_LOCATION`, `SQL_CATALOG_USAGE` and
  `SQL_SCHEMA_USAGE` are left to the backend once catalogs or schemas exist,
  rather than core inventing a value it cannot know.
- `SQLGetConnectAttr(SQL_ATTR_TXN_ISOLATION)` on a connection where the
  application has not set the attribute now reports
  `Backend::default_txn_isolation` instead of a hard-coded
  `SQL_TXN_READ_COMMITTED`. `SQL_DEFAULT_TXN_ISOLATION` is derived from the
  same hook, so the info type and the connection attribute can no longer report
  two different levels for one connection — a driver declaring
  `SQL_TXN_SERIALIZABLE` previously disagreed with itself.
- Cursor state is now tracked explicitly, by `StatementHandle::cursor_open`,
  instead of being inferred from `StatementHandle::statement`. Every `24000`
  check reads it: `SQLExecDirect`, `SQLGetData`, `SQLCloseCursor`, all ten
  catalog functions and `SQLSetStmtAttr` for
  `SQL_ATTR_CURSOR_TYPE` / `SQL_ATTR_CONCURRENCY` / `SQL_ATTR_SIMULATE_CURSOR` /
  `SQL_ATTR_USE_BOOKMARKS` / `SQL_ATTR_ROW_NUMBER`. Three behaviour changes
  follow:
  - After `SQLEndTran` under `SQL_CB_CLOSE`, `SQLExecDirect`, the catalog
    functions and those `SQLSetStmtAttr` attributes are accepted instead of
    returning `24000`. The statement transition table makes them legal
    (S4→S2, S5-S7→S3), but the statement is kept under `SQL_CB_CLOSE` and the
    old `statement.is_some()` guard read that as an open cursor.
  - `SQLCloseCursor` and `SQLGetData` now return `24000` on a statement that is
    only prepared, that executed without producing a result set, or whose
    cursor `SQLEndTran` closed under `SQL_CB_CLOSE`. They previously accepted
    all three, `SQLCloseCursor` reporting success with no cursor to close.
  - An execution that produces no result set (an `UPDATE`, say) no longer
    counts as an open cursor. A backend must report
    `StatementBackend::column_count` accurately as soon as
    `execute`/`exec_direct` returns — the same point `SQLNumResultCols` already
    reads it.
- `SQLEndTran` now applies the cursor behaviour the driver advertises. It
  previously reported `SQL_CB_DELETE` and left every cursor and prepared
  statement untouched, so an application that trusted the reported value
  operated on cursors it had been told were gone.
- `SQLEndTran` with `SQL_HANDLE_ENV` now attempts every connected connection
  on the environment instead of stopping at the first failure, and records a
  diagnostic on each failing connection so `SQLGetDiagRec` can identify it.
  A failure on one connection previously left every later connection holding
  an open transaction the application had asked to commit or roll back.
- `SQLEndTran` now clears diagnostics at entry — on the handle it was given,
  and on each connection it visits in the `SQL_HANDLE_ENV` loop. Without this,
  a record left by an earlier failed call on a connection that then committed
  fine made `SQLGetDiagRec` blame the wrong connection, defeating the
  per-connection diagnostics the spec tells applications to read.
- A corrupt entry in an environment's connection list no longer makes
  `SQLEndTran(SQL_HANDLE_ENV)` return `SQL_INVALID_HANDLE` for a call whose
  input handle was valid (and with no diagnostic record, since none is pushed
  for that variant). It now records a general error, so a genuinely failing
  connection's own SQLSTATE is no longer suppressed by a corrupt entry
  encountered before it.

[Unreleased]: https://github.com/stackabletech/stackable-odbc-core/commits/HEAD
