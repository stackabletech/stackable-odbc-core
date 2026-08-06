# Fuzz targets

These targets cover the `unsafe` raw-pointer paths in `stackable-odbc-core`,
which is where AddressSanitizer catches what clippy cannot see.

The two marshalling targets allocate their output buffer at exactly the size a
correct application would, so any write past the end is a reported error rather
than a silent overrun into neighbouring memory. That size is the `BufferLength`
argument for a variable-length C target, and the C type's own size for a
fixed-length one, which ignores `BufferLength` altogether.

The third goes the other way: it hands a parser an *input* buffer sized exactly
to its contents, so a read that runs past the terminator is reported rather than
finding more of the same allocation.

- `utf16` covers `utf16_to_string` and `write_utf16`.
- `column_value` covers `write_column_value` across every marshallable value
  variant and C target type, which is the full coercion matrix.
- `parse_attributes` covers `ffi::setup::parse_attributes_w`, the attribute-list
  walk behind `ConfigDSNW`, over aligned, deliberately misaligned and
  overlong-segment buffers.

## What belongs here, and what does not

The line is `unsafe`, not importance. A target earns its nightly toolchain and
its ASAN build when the failure it is hunting is a read or write outside an
allocation. Where the code is safe Rust, the worst outcome is a panic or a wrong
answer, and a property test finds both on stable, in a fraction of the CPU time,
on every PR rather than in a 30-second smoke run.

So the pure-safe parsers are covered by [`proptest`](https://docs.rs/proptest)
suites next to the code instead, asserting never-panics *and* an oracle wherever
one exists:

| Module | What the properties assert |
| --- | --- |
| `escape` | A grammar of escape-ish tokens against four dialects; an unknown escape and its surroundings survive byte for byte |
| `types::connect_params` | Every pair survives `parse` ∘ `to_connection_string`, and no keyword is injected by a value containing `}` |
| `param_convert` | Rendering never expands past `MAX_DECIMAL_EXPANSION_DIGITS`; rendering round-trips; `to_integer` agrees with `i128::from_str`; a `SQL_NUMERIC_STRUCT` reconstructs its literal |
| `numeric_convert` | The whole *C to SQL: Numeric* table is total; integers reach their target exactly when they are in range |
| `types::conversions` | Every `*_from_raw` swept across its entire 16-bit domain, round-tripping |

`parse_attributes` is on this side of the line because it steps a raw `*const
u16` looking for a terminator, and because the Driver Manager gives it no
alignment guarantee.

### Terminate the buffer

The parser's safety contract is that the pointer is null or double-null
terminated. A fuzz target that hands it an unterminated buffer will get an ASAN
report, and the report will be about the target: the read past the end is the
caller breaking a contract, not the parser exceeding one. `parse_attributes`
appends the terminator itself and fuzzes what comes before it, and reaches the
per-segment scan limit with a run that is long but still inside a real
allocation.

## Running

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) needs nightly, because
libFuzzer does.

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run utf16
cargo +nightly fuzz run column_value
cargo +nightly fuzz run parse_attributes
```

`parse_attributes` reaches `parse_attributes_w`, which is `pub(crate)`, through
`test_support::parse_attributes_summary_w`. That wrapper is gated behind the
default-off `test-support` feature, which this crate enables on its dependency,
so a shipped driver never exports it.

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
