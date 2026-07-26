#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
WORKDIR="$REPO_ROOT/.state/ci/futex-host-differential"

usage() {
    cat <<'EOF'
Usage: scripts/ci/futex-host-differential.sh [OPTIONS]

Options:
  --workdir DIR                Artifact directory

Builds and runs the portable futex smoke test against the host Linux kernel,
verifies the expected marker manifest, and writes result.txt plus a
thekernel-differential-receipt-v0 receipt.json into the artifact directory.
EOF
}

while (($#)); do
    case "$1" in
        --workdir)
            if (($# < 2)) || [ -z "$2" ] || [[ "$2" == -* ]]; then
                printf '%s\n' 'futex-host-differential: --workdir requires a path' >&2
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
            printf 'futex-host-differential: unknown argument: %s\n' "$1" >&2
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

BINARY="$WORKDIR/futex-smoke"
LOG="$WORKDIR/futex-smoke.log"
RESULT="$WORKDIR/result.txt"
RECEIPT="$WORKDIR/receipt.json"
MANIFEST="$REPO_ROOT/scripts/ci/differential/manifests/futex.markers"
rm -f -- "$BINARY" "$LOG" "$RESULT" "$RECEIPT"

if [ ! -f "$MANIFEST" ]; then
    printf 'futex-host-differential: missing marker manifest: %s\n' \
        "$MANIFEST" >&2
    exit 1
fi

json_escape() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

GIT_REV=$(git -C "$REPO_ROOT" rev-parse HEAD)
KERNEL_RELEASE=$(uname -r)
KERNEL_VERSION_LINE=$(head -n 1 /proc/version)
CC_VERSION=$(cc --version | head -n 1)
MARKERS_EXPECTED=$(grep -c . "$MANIFEST")

write_receipt() {
    local markers_matched=$1
    local result=$2
    cat >"$RECEIPT" <<EOF
{
  "schema": "thekernel-differential-receipt-v0",
  "case": "futex",
  "git_rev": "$(json_escape "$GIT_REV")",
  "reference": {
    "kind": "host-linux",
    "kernel_release": "$(json_escape "$KERNEL_RELEASE")",
    "kernel_version_line": "$(json_escape "$KERNEL_VERSION_LINE")"
  },
  "toolchain": {"cc": "$(json_escape "$CC_VERSION")"},
  "markers_expected": $MARKERS_EXPECTED,
  "markers_matched": $markers_matched,
  "allowlist_applied": [],
  "result": "$result"
}
EOF
}

cc -O2 -std=c11 -Wall -Wextra -Werror -pthread \
    "$REPO_ROOT/tests/guest/tools/futex-smoke.c" \
    -o "$BINARY"
command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' 'futex-host-differential: timeout command is required' >&2
    exit 1
}

set +e
timeout --kill-after=5s 60s "$BINARY" >"$LOG" 2>&1
smoke_status=$?
set -e
if [ "$smoke_status" -ne 0 ]; then
    write_receipt 0 fail
    printf 'futex-host-differential: FAIL smoke_exit=%s timeout_secs=60\n' \
        "$smoke_status" | tee "$RESULT" >&2
    exit 1
fi

markers_matched=0
while IFS= read -r marker_line; do
    [ -n "$marker_line" ] || continue
    if ! grep -Fqx -- "$marker_line" "$LOG"; then
        write_receipt "$markers_matched" fail
        printf 'futex-host-differential: FAIL missing_marker=%s\n' \
            "$marker_line" | tee "$RESULT" >&2
        exit 1
    fi
    markers_matched=$((markers_matched + 1))
done <"$MANIFEST"
if grep -Fq 'THEKERNEL_FUTEX_FAIL' "$LOG"; then
    write_receipt "$markers_matched" fail
    printf '%s\n' 'futex-host-differential: portable smoke reported a failure' >&2
    printf 'futex-host-differential: FAIL smoke_reported_failure=1\n' \
        | tee "$RESULT" >&2
    exit 1
fi

write_receipt "$markers_matched" pass
printf 'futex-host-differential: PASS markers_matched=%s\n' \
    "$markers_matched" | tee "$RESULT"
