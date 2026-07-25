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

### Changed

- `SQL_CURSOR_COMMIT_BEHAVIOR` and `SQL_CURSOR_ROLLBACK_BEHAVIOR` now report
  `SQL_CB_PRESERVE` instead of `SQL_CB_DELETE`, and are derived from
  `Backend::cursor_commit_behavior` / `Backend::cursor_rollback_behavior`
  rather than hard-coded. Core previously advertised `SQL_CB_DELETE` while
  `SQLEndTran` did nothing to cursors at all.
- **Breaking:** `default_get_info` and `common_get_info_raw` are now generic
  over the backend. Call them as `default_get_info::<Self>(info_type, widths)`
  and `common_get_info_raw::<Self>(info_type)` from a `Backend` impl.

### Fixed

- `SQLEndTran` now applies the cursor behaviour the driver advertises. It
  previously reported `SQL_CB_DELETE` and left every cursor and prepared
  statement untouched, so an application that trusted the reported value
  operated on cursors it had been told were gone.
- `SQLEndTran` with `SQL_HANDLE_ENV` now attempts every connected connection
  on the environment instead of stopping at the first failure, and records a
  diagnostic on each failing connection so `SQLGetDiagRec` can identify it.
  A failure on one connection previously left every later connection holding
  an open transaction the application had asked to commit or roll back.
