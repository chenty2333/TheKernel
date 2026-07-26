#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/ci/differential/lib.sh
. "$SCRIPT_DIR/differential/lib.sh"

CASE=signal-order
WORKDIR="$REPO_ROOT/.state/ci/signal-order-host-differential"
SOURCE_REL=tests/guest/tools/signal-order-smoke.c
MANIFEST_REL=scripts/ci/differential/manifests/signal-order.markers
ALLOWLIST="$SCRIPT_DIR/differential/allowlist/signal-order.json"

usage() {
    cat <<'EOF'
Usage: scripts/ci/signal-order-host-differential.sh [OPTIONS]

Options:
  --workdir DIR                Artifact directory

Builds and runs the portable signal delivery-order/queueing smoke against the
host Linux kernel and writes source-exact differential evidence. Inherited
pending signals or a non-empty blocked mask invalidate the reference baseline.
EOF
}

while (($#)); do
    case "$1" in
        --workdir)
            WORKDIR=$(differential_workdir_value \
                signal-order-host-differential "$@") || exit $?
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *)
            printf 'signal-order-host-differential: unknown argument: %s\n' \
                "$1" >&2
            exit 2
            ;;
    esac
done

differential_require_clean_repo "$REPO_ROOT"
source_head=$(git -C "$REPO_ROOT" rev-parse HEAD)
source_tree=$(git -C "$REPO_ROOT" rev-parse 'HEAD^{tree}')
WORKDIR=$(differential_resolve_workdir "$REPO_ROOT" "$WORKDIR")
SOURCE="$WORKDIR/input/signal-order-smoke.c"
MANIFEST="$WORKDIR/input/signal-order.markers"
BINARY="$WORKDIR/signal-order-smoke"
LOG="$WORKDIR/signal-order-smoke.log"
RESULT="$WORKDIR/result.txt"
RECEIPT="$WORKDIR/receipt.json"
APPLIED="$WORKDIR/allowlist-applied.jsonl"
rm -f -- "$SOURCE" "$MANIFEST" "$BINARY" "$LOG" "$RESULT" \
    "$RECEIPT" "$APPLIED"
differential_materialize_committed_input \
    "$REPO_ROOT" "$source_head" "$SOURCE_REL" "$SOURCE"
differential_materialize_committed_input \
    "$REPO_ROOT" "$source_head" "$MANIFEST_REL" "$MANIFEST"

differential_build_smoke "$BINARY" "$SOURCE" \
    -O2 -std=c11 -Wall -Wextra -Werror
command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' 'signal-order-host-differential: timeout command is required' >&2
    exit 1
}

for field in SigPnd ShdPnd SigBlk; do
    field_value=$(awk -v field="$field:" \
        '$1 == field { print $2; found = 1; exit }
         END { if (!found) exit 1 }' /proc/self/status) || {
        printf 'signal-order-host-differential: cannot read %s from /proc/self/status\n' \
            "$field" >&2
        exit 1
    }
    if [ "$field_value" != 0000000000000000 ]; then
        printf 'signal-order-host-differential: inherited %s=%s invalidates the Linux baseline\n' \
            "$field" "$field_value" >&2
        exit 1
    fi
done

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
    differential_revalidate_clean_repo \
        "$REPO_ROOT" "$source_head" "$source_tree"
    differential_write_receipt "$RECEIPT" "$CASE" "$REPO_ROOT" \
        "$markers_expected" "$markers_matched" "$APPLIED" "$result"
}
fail_run() {
    emit_receipt fail
    printf 'signal-order-host-differential: FAIL %s\n' "$1" \
        | tee "$RESULT" >&2
    exit 1
}

smoke_status=0
differential_run_bounded "$LOG" 60s 5s -- "$BINARY" || smoke_status=$?
missing_total=$(differential_missing_markers "$LOG" "$MANIFEST" once || true)
missing=$missing_total
if [ "$smoke_status" -ne 0 ]; then
    fail_run "smoke_exit=$smoke_status timeout_secs=60"
fi
if [ -n "$missing" ]; then
    missing=$(printf '%s\n' "$missing" |
        differential_apply_allowlist "$ALLOWLIST" "$(uname -r)" "$APPLIED")
fi
if [ -n "$missing" ]; then
    fail_run "missing_marker=$(printf '%s\n' "$missing" | sed -n 1p)"
fi
if grep -Fq 'THEKERNEL_SIGORDER_FAIL' "$LOG"; then
    fail_run 'smoke_reported_failure=1'
fi
if [ "$(tail -n 1 "$LOG")" != 'THEKERNEL_SIGORDER_OK' ]; then
    fail_run 'final_marker_not_last=1'
fi

emit_receipt pass
printf 'signal-order-host-differential: PASS markers_expected=%s evidence=%s\n' \
    "$markers_expected" "$WORKDIR" | tee "$RESULT"
