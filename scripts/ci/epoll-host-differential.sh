#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
WORKDIR="$REPO_ROOT/.state/ci/epoll-host-differential"
MANIFEST="$REPO_ROOT/scripts/ci/differential/manifests/epoll.markers"

usage() {
    cat <<'EOF'
Usage: scripts/ci/epoll-host-differential.sh [OPTIONS]

Options:
  --workdir DIR                Artifact directory

Builds and runs the portable epoll edge-semantics smoke test against the host
Linux kernel, verifies the full marker manifest, and writes result.txt plus a
thekernel-differential-receipt-v0 receipt.json.
EOF
}

while (($#)); do
    case "$1" in
        --workdir)
            if (($# < 2)) || [ -z "$2" ] || [[ "$2" == -* ]]; then
                printf '%s\n' 'epoll-host-differential: --workdir requires a path' >&2
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
            printf 'epoll-host-differential: unknown argument: %s\n' "$1" >&2
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

BINARY="$WORKDIR/epoll-smoke"
LOG="$WORKDIR/epoll-smoke.log"
RESULT="$WORKDIR/result.txt"
RECEIPT="$WORKDIR/receipt.json"
rm -f -- "$BINARY" "$LOG" "$RESULT" "$RECEIPT"

if [ ! -f "$MANIFEST" ]; then
    printf 'epoll-host-differential: missing marker manifest: %s\n' \
        "$MANIFEST" >&2
    exit 1
fi

cc -O2 -std=c11 -Wall -Wextra -Werror -pthread \
    "$REPO_ROOT/tests/guest/tools/epoll-smoke.c" \
    -o "$BINARY"
command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' 'epoll-host-differential: timeout command is required' >&2
    exit 1
}

json_escape() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

markers_expected=0
markers_matched=0

write_receipt() {
    local result="$1"
    local git_rev kernel_release version_line cc_line
    git_rev=$(git -C "$REPO_ROOT" rev-parse HEAD)
    kernel_release=$(uname -r)
    version_line=$(head -n 1 /proc/version)
    cc_line=$(cc --version | head -n 1)
    cat >"$RECEIPT" <<EOF
{
  "schema": "thekernel-differential-receipt-v0",
  "case": "epoll",
  "git_rev": "$(json_escape "$git_rev")",
  "reference": {
    "kind": "host-linux",
    "kernel_release": "$(json_escape "$kernel_release")",
    "kernel_version_line": "$(json_escape "$version_line")"
  },
  "toolchain": {"cc": "$(json_escape "$cc_line")"},
  "markers_expected": $markers_expected,
  "markers_matched": $markers_matched,
  "allowlist_applied": [],
  "result": "$result"
}
EOF
}

fail_run() {
    printf 'epoll-host-differential: FAIL %s\n' "$1" | tee "$RESULT" >&2
    write_receipt fail
    exit 1
}

set +e
timeout --kill-after=5s 60s "$BINARY" >"$LOG" 2>&1
smoke_status=$?
set -e
if [ "$smoke_status" -ne 0 ]; then
    fail_run "smoke_exit=$smoke_status timeout_secs=60"
fi

missing_marker=''
while IFS= read -r marker_line; do
    [ -n "$marker_line" ] || continue
    markers_expected=$((markers_expected + 1))
    if grep -Fqx -- "$marker_line" "$LOG"; then
        markers_matched=$((markers_matched + 1))
    elif [ -z "$missing_marker" ]; then
        missing_marker=$marker_line
    fi
done <"$MANIFEST"
if [ -n "$missing_marker" ]; then
    fail_run "missing_marker=$missing_marker"
fi
if grep -Fq 'THEKERNEL_EPOLL_FAIL' "$LOG"; then
    fail_run 'smoke reported a failure marker'
fi
if [ "$(tail -n 1 "$LOG")" != 'THEKERNEL_EPOLL_OK' ]; then
    fail_run 'final marker is not the last output line'
fi

write_receipt pass
printf 'epoll-host-differential: PASS markers=%s/%s\n' \
    "$markers_matched" "$markers_expected" | tee "$RESULT"
