# Contributing

Thanks for considering a contribution. Reports of a spec rule this crate gets
wrong, a SQLSTATE it should return and does not, or an application that will not
work against a driver built on it are all useful.

- **Questions and ideas:** [GitHub Discussions](https://github.com/orgs/stackabletech/discussions)
  or [Discord](https://discord.gg/7kZ3BNnCAF).
- **Bugs:** open an issue. Please say which platform, which Driver Manager
  (unixODBC or the Windows one), which driver built on this crate, and which
  application. A driver log helps most of all: set `ODBC_LOG_FILE` and
  `ODBC_LOG_LEVEL=debug` and attach the result, with any passwords removed.
- **Security problems:** do not open an issue. See [SECURITY.md](SECURITY.md).

## Building

You need the unixODBC development libraries, because `odbc-sys` links against
them. You do not need a DSN or a running Driver Manager to build or to run the
tests.

```bash
sudo apt-get install unixodbc-dev   # Debian/Ubuntu
```

```bash
git clone https://github.com/stackabletech/stackable-odbc-core
cd stackable-odbc-core
cargo build
```

The toolchain version is pinned in `rust-toolchain.toml`, so rustup fetches the
right one on the first build.

This crate is a library. To see it working end to end you need a driver on top
of it, and
[stackable-odbc-sqlite](https://github.com/stackabletech/stackable-odbc-sqlite)
is the smallest one.

### Windows code, from Linux

`#[cfg(windows)]` code compiles from Linux, and should be compiled before it is
pushed. A plain `cargo check` does not look at it at all, so `ffi/setup.rs` and
`ConfigDSNW` can reach a state that builds and tests clean locally and fails on
the Windows runner.

```bash
rustup target add x86_64-pc-windows-msvc          # once
cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings
```

This links nothing and needs no Windows host, because `raw-dylib` resolves
`odbccp32` at link time and a `clippy` run never reaches it. It is not a
substitute for running the code, which needs a Windows host with a Driver
Manager, but it closes the compile-and-lint half where the regressions actually
happen.

## Testing

```bash
cargo test                                   # unit and FFI tests
cargo clippy --all-targets -- -D warnings
```

`cargo test` must produce zero warnings.

All the raw-pointer marshalling lives in this crate, so three further tools run
on every pull request and are worth running by hand when you touch that code.

```bash
# undefined behaviour and leaked handles
MIRIFLAGS="-Zmiri-disable-isolation" \
  cargo +nightly miri test -p stackable-odbc-core --lib -- --skip proptest

# every thread interleaving of the locking code, not just the one that occurred
RUSTFLAGS="--cfg loom" cargo test --lib loom_tests

# random input at the buffer-copying paths, under AddressSanitizer
cargo install cargo-fuzz
cargo +nightly fuzz run utf16
cargo +nightly fuzz run column_value
```

`bench/` and `fuzz/` are **separate Cargo workspaces**, so nothing at the repo
root compiles them. Not `cargo test`, not `cargo clippy --all-targets`, and not
a single `pre-commit` hook. `bench/benches/handle_lookup.rs` contains a full
`impl Backend`, so any change to the `Backend` or `StatementBackend` trait
breaks it while every local check still passes and CI fails. After touching
either trait:

```bash
(cd bench && cargo build --benches)
(cd fuzz && cargo +nightly build --target x86_64-unknown-linux-gnu)
```

## Before you commit

```bash
pre-commit run --all-files
```

That is the gate, and the single source of truth for what must pass. It runs
rustfmt, clippy, `cargo test`, `cargo doc` with warnings denied so a broken
intra-doc link fails the commit, `cargo sort`, `cargo deny`, markdownlint and a
secret scan.

Two of those are not cargo built-ins, so install them once:

```bash
cargo install cargo-deny cargo-sort
```

A change usually needs one more thing: **a changelog entry**, under
`## [Unreleased]` in [`CHANGELOG.md`](CHANGELOG.md), if a driver author or an
ODBC application can observe the difference. Because this crate is a library
that driver crates build on, treat any change to a public type, a trait method,
or an exported FFI contract as observable.

## Where things live

[`AGENTS.md`](AGENTS.md) is the working reference: the architecture, how a call
flows through the layers, the descriptor and catalog rules, the lock discipline,
and the spec evidence behind each design decision. Read the section covering
whatever you are about to change. It is written for AI coding agents and human
contributors alike.

Four rules are worth stating here, because they are the ones a reasonable-looking
change breaks most easily.

- **Read the ODBC spec page for every function you touch.** Every FFI function's
  doc comment lists each SQLSTATE from that function's spec diagnostics table
  and says whether this crate returns it, or why not. A guard test in
  `src/types/diagnostics_table.rs` checks the doc comments against a
  transcription of those tables, so an incomplete one fails the build.
- **Watch the `(DM)` annotations.** A SQLSTATE marked that way is the Driver
  Manager's to return, not the driver's, so adding a driver-side check for one
  is wrong even though it looks like extra safety.
- **Convert raw integers to typed enums at the FFI boundary**, using the
  `xxx_from_raw` functions in `src/types/conversions.rs`. Never `transmute`: an
  arbitrary integer is not necessarily a valid enum variant, and transmuting one
  is undefined behaviour.
- **Import every lock from `src/sync.rs`**, never from `std::sync` directly. That
  is what lets a `--cfg loom` build swap them for loom's instrumented versions.
  A lock imported around that module is invisible to loom and silently opts its
  code out of the interleaving proof.

## License

By contributing you agree that your contribution is licensed under
[Apache-2.0](LICENSE).
