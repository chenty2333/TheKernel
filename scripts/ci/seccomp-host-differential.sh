#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/ci/differential/lib.sh
. "$SCRIPT_DIR/differential/lib.sh"
WORKDIR="$REPO_ROOT/.state/ci/seccomp-host-differential"
ALLOW_INHERITED=0
CASE=seccomp
MANIFEST="$SCRIPT_DIR/differential/manifests/seccomp.markers"
ALLOWLIST="$SCRIPT_DIR/differential/allowlist/seccomp.json"

usage() {
    cat <<'EOF'
Usage: scripts/ci/seccomp-host-differential.sh [OPTIONS]

Options:
  --workdir DIR                Artifact directory
  --allow-inherited-profile    Compile but explicitly skip execution when the
                               caller already has a seccomp profile

Builds and runs the portable seccomp smoke test against the host Linux kernel.
An inherited filter changes strict-mode, filter-count, and path-limit semantics,
so execution is rejected unless the caller explicitly requests a compile-only
skip. The canonical host differential job does not permit that skip.
EOF
}

while (($#)); do
    case "$1" in
        --workdir)
            if (($# < 2)) || [ -z "$2" ] || [[ "$2" == -* ]]; then
                printf '%s\n' 'seccomp-host-differential: --workdir requires a path' >&2
                exit 2
            fi
            WORKDIR=$2
            shift 2
            ;;
        --allow-inherited-profile)
            ALLOW_INHERITED=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'seccomp-host-differential: unknown argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done

WORKDIR=$(differential_resolve_workdir "$REPO_ROOT" "$WORKDIR")

BINARY="$WORKDIR/seccomp-smoke"
LOG="$WORKDIR/seccomp-smoke.log"
RESULT="$WORKDIR/result.txt"
RECEIPT="$WORKDIR/receipt.json"
APPLIED="$WORKDIR/allowlist-applied.jsonl"
rm -f -- "$BINARY" "$LOG" "$RESULT" "$RECEIPT" "$APPLIED"

differential_build_smoke "$BINARY" "$REPO_ROOT/tests/guest/tools/seccomp-smoke.c" \
    -O2 -std=c11 -Wall -Wextra -Werror -pthread
command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' 'seccomp-host-differential: timeout command is required' >&2
    exit 1
}

seccomp_mode=$(awk '$1 == "Seccomp:" { print $2; found = 1; exit }
                    END { if (!found) exit 1 }' /proc/self/status) || {
    printf '%s\n' 'seccomp-host-differential: cannot read initial seccomp mode' >&2
    exit 1
}
case "$seccomp_mode" in
    0) ;;
    1|2)
        if [ "$ALLOW_INHERITED" -eq 1 ]; then
            printf 'seccomp-host-differential: SKIP inherited_seccomp_mode=%s compile=ok\n' \
                "$seccomp_mode" | tee "$RESULT"
            exit 0
        fi
        printf 'seccomp-host-differential: inherited seccomp mode %s invalidates the Linux baseline\n' \
            "$seccomp_mode" >&2
        exit 1
        ;;
    *)
        printf 'seccomp-host-differential: invalid initial seccomp mode: %s\n' \
            "$seccomp_mode" >&2
        exit 1
        ;;
esac

markers_expected=$(differential_manifest_count "$MANIFEST")
missing_total=

count_lines() {
    if [ -z "$1" ]; then
        printf '0\n'
    else
        printf '%s\n' "$1" | wc -l
    fi
}

emit_receipt() {
    local matched
    matched=$((markers_expected - $(count_lines "$missing_total")))
    differential_write_receipt "$RECEIPT" "$CASE" "$REPO_ROOT" \
        "$markers_expected" "$matched" "$APPLIED" "$1"
}

smoke_status=0
differential_run_bounded "$LOG" 60s 5s -- "$BINARY" || smoke_status=$?
missing_total=$(differential_missing_markers "$LOG" "$MANIFEST" || true)
missing=$missing_total
if [ "$smoke_status" -ne 0 ]; then
    emit_receipt fail
    printf 'seccomp-host-differential: FAIL smoke_exit=%s timeout_secs=60\n' \
        "$smoke_status" | tee "$RESULT" >&2
    exit 1
fi
if [ -n "$missing" ]; then
    missing=$(printf '%s\n' "$missing" \
        | differential_apply_allowlist "$ALLOWLIST" "$(uname -r)" "$APPLIED")
fi
if [ -n "$missing" ]; then
    emit_receipt fail
    printf 'seccomp-host-differential: FAIL missing_marker=%s\n' \
        "$(printf '%s\n' "$missing" | sed -n 1p)" | tee "$RESULT" >&2
    exit 1
fi
if grep -Fq 'THEKERNEL_SECCOMP_FAIL' "$LOG"; then
    emit_receipt fail
    printf '%s\n' 'seccomp-host-differential: portable smoke reported a failure' >&2
    exit 1
fi

emit_receipt pass
printf '%s\n' 'seccomp-host-differential: PASS initial_seccomp_mode=0' \
    | tee "$RESULT"
