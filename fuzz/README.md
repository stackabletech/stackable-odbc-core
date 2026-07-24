# Fuzz targets

Fuzz targets for `stackable-odbc-core`'s **`unsafe` pointer-marshalling** hot paths, where
AddressSanitizer earns its keep. Each allocates its output buffer at exactly the
size a correct application would (the `BufferLength` argument for variable-length
C targets; the C type's own size for fixed-length ones, which ignore
`BufferLength`), so ASAN reports any write past it — the A1 bug class, invisible
to clippy.

- `utf16` — `utf16_to_string` / `write_utf16`
- `column_value` — `write_column_value` across every marshallable value variant
  and C target type (the full coercion matrix)

The pure-safe parsers (`translate_escapes`, `ConnectParams::parse`, the drivers'
type-name parsers) contain no `unsafe`, so ASAN adds nothing over property tests.
They are covered by [`proptest`](https://docs.rs/proptest) suites co-located with
the code, which run on stable in the normal `cargo test` (never-panics plus
round-trip invariants).

Run with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (nightly +
libFuzzer):

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run utf16
cargo +nightly fuzz run column_value
```

If cargo-fuzz fails with "sanitizer is incompatible with statically linked
libc", it defaulted to a musl target; pin the gnu triple explicitly (this is
what CI does):

```bash
cargo +nightly fuzz run utf16 --target x86_64-unknown-linux-gnu
```

`cargo fuzz run` runs **indefinitely** until it finds a crash or you stop it. To
bound a run, pass a libFuzzer flag after `--`:

```bash
cargo +nightly fuzz run column_value -- -max_total_time=60   # stop after 60s
cargo +nightly fuzz run column_value -- -runs=1000000        # or a run count
```

This crate is its own Cargo workspace, so `cargo build` in the repository root
does not touch it (libFuzzer needs a nightly toolchain).
