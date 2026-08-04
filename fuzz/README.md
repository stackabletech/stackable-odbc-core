# Fuzz targets

These targets cover the `unsafe` pointer-marshalling paths in
`stackable-odbc-core`, which is where AddressSanitizer catches what clippy
cannot see. Each one allocates its output buffer at exactly the size a correct
application would, so any write past the end is a reported error rather than a
silent overrun into neighbouring memory.

That size is the `BufferLength` argument for a variable-length C target, and the
C type's own size for a fixed-length one, which ignores `BufferLength`
altogether.

- `utf16` covers `utf16_to_string` and `write_utf16`.
- `column_value` covers `write_column_value` across every marshallable value
  variant and C target type, which is the full coercion matrix.

The pure-safe parsers, `translate_escapes`, `ConnectParams::parse` and the
drivers' own type-name parsers, contain no `unsafe`, so AddressSanitizer adds
nothing over property tests. They are covered by
[`proptest`](https://docs.rs/proptest) suites next to the code, which run on
stable under an ordinary `cargo test` and assert both never-panics and
round-trip invariants.

## Running

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) needs nightly, because
libFuzzer does.

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run utf16
cargo +nightly fuzz run column_value
```

If cargo-fuzz fails with "sanitizer is incompatible with statically linked
libc", it picked a musl target. Pin the gnu triple explicitly, which is what CI
does:

```bash
cargo +nightly fuzz run utf16 --target x86_64-unknown-linux-gnu
```

`cargo fuzz run` runs until it finds a crash or you stop it. To bound a run,
pass a libFuzzer flag after `--`:

```bash
cargo +nightly fuzz run column_value -- -max_total_time=60   # stop after 60s
cargo +nightly fuzz run column_value -- -runs=1000000        # or a run count
```

## Workspace

This is its own Cargo workspace, so `cargo build` in the repository root does
not touch it and no `pre-commit` hook compiles it. Anything that changes the
`Backend` or `StatementBackend` trait can break it while every root check still
passes, so build it by hand after such a change.
