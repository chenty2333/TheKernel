#!/usr/bin/env bash
set -euo pipefail

MINIMUM=
FILTER=

usage() {
    cat <<'EOF'
Usage: scripts/ci/rust-filtered-test-gate.sh --minimum N --filter TEXT -- COMMAND [ARGS...]

Runs COMMAND once with a Rust test-harness filter and refuses an empty or
unexpectedly small executed test set. Counting the harness report from the
same successful invocation keeps source, dependency resolution, and execution
inside one focused workspace.
EOF
}

while (($#)); do
    case "$1" in
        --minimum) MINIMUM=${2:-}; shift 2 ;;
        --filter) FILTER=${2:-}; shift 2 ;;
        --) shift; break ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'rust-filtered-test-gate: unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

case "$MINIMUM" in
    ''|*[!0-9]*|0)
        printf 'rust-filtered-test-gate: invalid minimum: %s\n' "$MINIMUM" >&2
        exit 2
        ;;
esac
[ -n "$FILTER" ] || {
    printf '%s\n' 'rust-filtered-test-gate: filter must not be empty' >&2
    exit 2
}
(($# > 0)) || {
    printf '%s\n' 'rust-filtered-test-gate: command must not be empty' >&2
    exit 2
}
command -v rustc >/dev/null 2>&1 || {
    printf '%s\n' 'rust-filtered-test-gate: rustc is required' >&2
    exit 2
}
toolchain=$(rustc --version)
printf 'rust-filtered-test-gate: toolchain=%s\n' "$toolchain"

output=$(mktemp)
trap 'rm -f -- "$output"' EXIT
if ! "$@" "$FILTER" -- --test-threads=1 2>&1 | tee "$output"; then
    printf 'rust-filtered-test-gate: filtered execution failed: %s\n' \
        "$FILTER" >&2
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
    printf 'rust-filtered-test-gate: no Rust harness count for filter %s\n' \
        "$FILTER" >&2
    exit 1
}
if ((executed < MINIMUM)); then
    printf 'rust-filtered-test-gate: filter %s executed %s tests; require at least %s\n' \
        "$FILTER" "$executed" "$MINIMUM" >&2
    exit 1
fi
printf 'rust-filtered-test-gate: PASS executed=%s minimum=%s filter=%s toolchain=%s\n' \
    "$executed" "$MINIMUM" "$FILTER" "$toolchain"
