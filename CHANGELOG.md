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
