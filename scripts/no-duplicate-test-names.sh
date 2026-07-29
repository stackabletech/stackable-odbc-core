#!/usr/bin/env bash
# Fails when the test binary registers the same test name twice.
#
# A function with two `#[test]` attributes is registered twice, and the
# function whose attribute was absorbed stops running altogether. Both leave
# `cargo test` green — the count moves, but nothing fails — so the registered
# names are the only place either shows up.
set -euo pipefail

duplicates=$(cargo test --locked --lib -- --list 2>/dev/null | grep ': test$' | sort | uniq -d)

if [[ -n "$duplicates" ]]; then
    echo "These test names are registered more than once:" >&2
    echo "$duplicates" >&2
    echo >&2
    echo "A duplicate almost always means one function carries two #[test]" >&2
    echo "attributes, having absorbed the attribute of the function above or" >&2
    echo "below it — which is no longer running." >&2
    exit 1
fi
