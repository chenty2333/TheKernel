#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/ci/differential/lib.sh
. "$SCRIPT_DIR/differential/lib.sh"

CASE=futex2-waitv-signal
WORKDIR="$REPO_ROOT/.state/ci/futex2-waitv-signal-host-differential"
SOURCE_REL=tests/guest/tools/futex2-waitv-signal-differential.c
MANIFEST_REL=scripts/ci/differential/manifests/futex2-waitv-signal.markers
ALLOWLIST="$SCRIPT_DIR/differential/allowlist/futex2-waitv-signal.json"

usage() {
    cat <<'EOF'
Usage: scripts/ci/futex2-waitv-signal-host-differential.sh [OPTIONS]

Options:
  --workdir DIR                Artifact directory

Builds and runs the portable futex2 FUTEX_WAITV signal-interruption helper
against host Linux. A host without the futex_waitv syscall is reported as
UNSUPPORTED (exit 78); it is never accepted as an EINTR result.
EOF
}

while (($#)); do
    case "$1" in
        --workdir)
            WORKDIR=$(differential_workdir_value \
                futex2-waitv-signal-host-differential "$@") || exit $?
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *)
            printf 'futex2-waitv-signal-host-differential: unknown argument: %s\n' \
                "$1" >&2
            exit 2
            ;;
    esac
done

WORKDIR=$(differential_resolve_workdir "$REPO_ROOT" "$WORKDIR")
SOURCE="$REPO_ROOT/$SOURCE_REL"
MANIFEST="$REPO_ROOT/$MANIFEST_REL"
BINARY="$WORKDIR/futex2-waitv-signal-differential"
LOG="$WORKDIR/futex2-waitv-signal.log"
RESULT="$WORKDIR/result.txt"
RECEIPT="$WORKDIR/receipt.json"
APPLIED="$WORKDIR/allowlist-applied.jsonl"
rm -f -- "$BINARY" "$LOG" "$RESULT" "$RECEIPT" "$APPLIED"

differential_build_smoke "$BINARY" "$SOURCE" \
    -O2 -std=c11 -Wall -Wextra -Werror -pthread
command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' \
        'futex2-waitv-signal-host-differential: timeout command is required' >&2
    exit 1
}

markers_expected=$(differential_manifest_count "$MANIFEST")
missing_total=
count_missing() {
    if [ -z "$missing_total" ]; then
        printf '0\n'
    else
        printf '%s\n' "$missing_total" | wc -l
    fi
}
emit_receipt() {
    local result=$1
    local markers_matched
    markers_matched=$((markers_expected - $(count_missing)))
    differential_write_receipt "$RECEIPT" "$CASE" "$REPO_ROOT" \
        "$markers_expected" "$markers_matched" "$APPLIED" "$result" \
        "$SOURCE" "$MANIFEST"
}
fail_run() {
    emit_receipt fail
    printf 'futex2-waitv-signal-host-differential: FAIL %s\n' "$1" \
        | tee "$RESULT" >&2
    exit 1
}

smoke_status=0
differential_run_bounded "$LOG" 60s 5s -- "$BINARY" || smoke_status=$?
if [ "$smoke_status" -ne 0 ]; then
    fail_run "smoke_exit=$smoke_status timeout_secs=60"
fi

unsupported=$(grep -m1 '^THEKERNEL_FUTEX2_WAITV_SIGNAL_UNSUPPORTED ' \
    "$LOG" || true)
if [ -n "$unsupported" ]; then
    printf 'futex2-waitv-signal-host-differential: UNSUPPORTED %s\n' \
        "$unsupported" | tee "$RESULT"
    exit 78
fi

missing_total=$(differential_missing_markers "$LOG" "$MANIFEST" once || true)
missing=$missing_total
if [ -n "$missing" ]; then
    missing=$(printf '%s\n' "$missing" |
        differential_apply_allowlist "$ALLOWLIST" "$(uname -r)" "$APPLIED")
fi
if [ -n "$missing" ]; then
    fail_run "missing_marker=$(printf '%s\n' "$missing" | sed -n 1p)"
fi
if grep -Fq 'THEKERNEL_FUTEX2_WAITV_SIGNAL_FAIL' "$LOG"; then
    fail_run 'smoke_reported_failure=1'
fi
if [ "$(tail -n 1 "$LOG")" != 'THEKERNEL_FUTEX2_WAITV_SIGNAL_OK' ]; then
    fail_run 'final_marker_not_last=1'
fi

emit_receipt pass
printf 'futex2-waitv-signal-host-differential: PASS markers_expected=%s evidence=%s\n' \
    "$markers_expected" "$WORKDIR" | tee "$RESULT"
