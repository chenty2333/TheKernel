#!/usr/bin/env bash
set -euo pipefail

MINIMUM=

usage() {
    cat <<'EOF'
Usage: scripts/ci/rust-full-test-gate.sh --minimum N -- COMMAND [ARGS...]

Runs COMMAND with no test filter and the harness' default parallelism, then
refuses an unexpectedly small executed test set.

This complements `rust-filtered-test-gate.sh` rather than replacing it. The
filtered gates prove that each named subsystem still executes a known number of
tests; they cannot prove anything about tests no filter names, and because they
pin `--test-threads=1` they cannot observe interference between tests that
share kernel globals.

Both properties have failed before. Four tests were reachable by no filter at
all and failed on the default branch unnoticed, and twenty-one more passed
under isolation while failing whenever the suite ran as one binary, because a
per-module scheduler bootstrap treated an already-initialized scheduler as
fatal. Running the whole suite, in parallel, is what makes either visible.

The minimum count is a floor on the total, so a filter typo, a `#[cfg]` that
silently drops a module, or a deleted test file cannot shrink the suite
without failing this gate.
EOF
}

while (($#)); do
    case "$1" in
        --minimum) MINIMUM=${2:-}; shift 2 ;;
        --) shift; break ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'rust-full-test-gate: unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

case "$MINIMUM" in
    ''|*[!0-9]*|0)
        printf 'rust-full-test-gate: invalid minimum: %s\n' "$MINIMUM" >&2
        exit 2
        ;;
esac
(($# > 0)) || {
    printf '%s\n' 'rust-full-test-gate: command must not be empty' >&2
    exit 2
}
command -v rustc >/dev/null 2>&1 || {
    printf '%s\n' 'rust-full-test-gate: rustc is required' >&2
    exit 2
}
printf 'rust-full-test-gate: toolchain=%s\n' "$(rustc --version)"

output=$(mktemp)
trap 'rm -f -- "$output"' EXIT
if ! "$@" 2>&1 | tee "$output"; then
    printf '%s\n' 'rust-full-test-gate: unfiltered execution failed' >&2
    exit 1
fi
executed=$(awk '
    { sub(/\r$/, "", $0) }
    /^running [0-9]+ tests?$/ { count += $2; reports += 1 }
    END {
        if (reports == 0) exit 1
        print count + 0
    }
' "$output") || {
    printf '%s\n' 'rust-full-test-gate: no Rust harness count reported' >&2
    exit 1
}
if ((executed < MINIMUM)); then
    printf 'rust-full-test-gate: executed %s tests; require at least %s\n' \
        "$executed" "$MINIMUM" >&2
    exit 1
fi
printf 'rust-full-test-gate: executed %s tests (minimum %s)\n' \
    "$executed" "$MINIMUM"
