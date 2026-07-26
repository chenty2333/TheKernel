#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
WORKDIR="$REPO_ROOT/.state/ci/signal-order-host-differential"
MANIFEST="$SCRIPT_DIR/differential/manifests/signal-order.markers"

usage() {
    cat <<'EOF'
Usage: scripts/ci/signal-order-host-differential.sh [OPTIONS]

Options:
  --workdir DIR                Artifact directory

Builds and runs the portable signal delivery-order/queueing smoke test
against the host Linux kernel. Pending signals inherited across exec or a
non-empty inherited blocked mask would invalidate the delivery-order
baseline, so both are rejected before execution.
EOF
}

while (($#)); do
    case "$1" in
        --workdir)
            if (($# < 2)) || [ -z "$2" ] || [[ "$2" == -* ]]; then
                printf '%s\n' 'signal-order-host-differential: --workdir requires a path' >&2
                exit 2
            fi
            WORKDIR=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'signal-order-host-differential: unknown argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done

case "$WORKDIR" in
    /*) ;;
    *) WORKDIR="$REPO_ROOT/$WORKDIR" ;;
esac
mkdir -p -- "$WORKDIR"
WORKDIR=$(cd -- "$WORKDIR" && pwd -P)

BINARY="$WORKDIR/signal-order-smoke"
LOG="$WORKDIR/signal-order-smoke.log"
RESULT="$WORKDIR/result.txt"
RECEIPT="$WORKDIR/receipt.json"
rm -f -- "$BINARY" "$LOG" "$RESULT" "$RECEIPT"

cc -O2 -std=c11 -Wall -Wextra -Werror \
    "$REPO_ROOT/tests/guest/tools/signal-order-smoke.c" \
    -o "$BINARY"
command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' 'signal-order-host-differential: timeout command is required' >&2
    exit 1
}
[ -r "$MANIFEST" ] || {
    printf 'signal-order-host-differential: missing marker manifest: %s\n' \
        "$MANIFEST" >&2
    exit 1
}

for field in SigPnd ShdPnd SigBlk; do
    field_value=$(awk -v field="$field:" '$1 == field { print $2; found = 1; exit }
                                          END { if (!found) exit 1 }' \
        /proc/self/status) || {
        printf 'signal-order-host-differential: cannot read %s from /proc/self/status\n' \
            "$field" >&2
        exit 1
    }
    if [ "$field_value" != "0000000000000000" ]; then
        printf 'signal-order-host-differential: inherited %s=%s invalidates the Linux baseline\n' \
            "$field" "$field_value" >&2
        exit 1
    fi
done

json_escape() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

write_receipt() {
    local result_value=$1
    local matched=$2
    local expected=$3
    cat >"$RECEIPT" <<EOF
{
  "schema": "thekernel-differential-receipt-v0",
  "case": "signal-order",
  "git_rev": "$(json_escape "$(git -C "$REPO_ROOT" rev-parse HEAD)")",
  "reference": {
    "kind": "host-linux",
    "kernel_release": "$(json_escape "$(uname -r)")",
    "kernel_version_line": "$(json_escape "$(head -1 /proc/version)")"
  },
  "toolchain": {"cc": "$(json_escape "$(cc --version | head -1)")"},
  "markers_expected": $expected,
  "markers_matched": $matched,
  "allowlist_applied": [],
  "result": "$result_value"
}
EOF
}

markers_expected=$(grep -c '' "$MANIFEST")

set +e
timeout --kill-after=5s 60s "$BINARY" >"$LOG" 2>&1
smoke_status=$?
set -e
if [ "$smoke_status" -ne 0 ]; then
    write_receipt fail 0 "$markers_expected"
    printf 'signal-order-host-differential: FAIL smoke_exit=%s timeout_secs=60\n' \
        "$smoke_status" | tee "$RESULT" >&2
    exit 1
fi

markers_matched=0
while IFS= read -r expected_marker; do
    [ -n "$expected_marker" ] || continue
    if ! grep -Fqx -- "$expected_marker" "$LOG"; then
        write_receipt fail "$markers_matched" "$markers_expected"
        printf 'signal-order-host-differential: missing marker: %s\n' \
            "$expected_marker" >&2
        printf 'signal-order-host-differential: FAIL missing_marker=1\n' \
            | tee "$RESULT" >&2
        exit 1
    fi
    markers_matched=$((markers_matched + 1))
done <"$MANIFEST"
if grep -Fq 'THEKERNEL_SIGORDER_FAIL' "$LOG"; then
    write_receipt fail "$markers_matched" "$markers_expected"
    printf '%s\n' 'signal-order-host-differential: smoke reported a failure' >&2
    exit 1
fi
if [ "$(tail -1 "$LOG")" != 'THEKERNEL_SIGORDER_OK' ]; then
    write_receipt fail "$markers_matched" "$markers_expected"
    printf '%s\n' 'signal-order-host-differential: final marker is not the last output line' >&2
    exit 1
fi

write_receipt pass "$markers_matched" "$markers_expected"
printf 'signal-order-host-differential: PASS markers_matched=%s\n' \
    "$markers_matched" | tee "$RESULT"
