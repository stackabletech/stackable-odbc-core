<!--
Delete any section that does not apply. The checklist is a reminder, not a
gate — CI enforces what it can, and the rest is what a reviewer would
otherwise have to ask for.
-->

## What this changes

<!-- What behaviour differs after this, and why. -->

## Spec basis

<!--
For anything touching an FFI entry point: link the function's page on Microsoft
Learn and quote the row or sentence this implements.

Two things worth stating explicitly, because both have caused rework here:
- whether a SQLSTATE's row carries a **(DM)** marker, which means the Driver
  Manager owes it and the driver must not return it; and
- what the attribute or info type's description says its *purpose* is, which
  has decided the design more than once.
-->

## Checklist

- [ ] `pre-commit run --all-files` passes — this is the single source of truth for what must pass.
- [ ] `CHANGELOG.md` has an entry under `## [Unreleased]`, if this is user-facing. Any change to a public type, trait method or exported FFI contract counts, since driver crates consume them.
- [ ] Doc comments on any FFI function touched list every SQLSTATE from its spec diagnostics table, saying for each whether the driver returns it or why not.
- [ ] New tests were checked by breaking the line they cover and watching them fail. A test that cannot fail reports coverage that does not exist.

### If it applies

- [ ] Miri, for anything touching raw pointers: `MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test -p stackable-odbc-core --lib -- --skip proptest`
- [ ] loom, for anything touching handle locking: `RUSTFLAGS="--cfg loom" cargo test --lib loom_tests`
- [ ] Breaking changes for driver crates are called out above, so the drivers can be updated alongside.

## Notes for the reviewer

<!--
Anything you decided rather than derived — a spec sentence you read two ways,
a case left unhandled on purpose, a TODO(spec) you added.
-->
