#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/ci/differential/lib.sh
. "$SCRIPT_DIR/differential/lib.sh"

CASE=vfork
WORKDIR="$REPO_ROOT/.state/ci/vfork-host-differential"
SOURCE_REL=tests/guest/tools/vfork-smoke.c
MANIFEST_REL=scripts/ci/differential/manifests/vfork.markers
ALLOWLIST="$SCRIPT_DIR/differential/allowlist/vfork.json"

usage() {
    cat <<'EOF'
Usage: scripts/ci/vfork-host-differential.sh [OPTIONS]

Options:
  --workdir DIR                Artifact directory

Builds and runs the portable vfork exit/signal/exec-release smoke test
against the host Linux kernel 200 times. Each run checks the same-CPU phase-3
exit proof, fatal-signal release, and post-exec identity, then records hashes
of the working-tree inputs.
EOF
}

while (($#)); do
    case "$1" in
        --workdir)
            WORKDIR=$(differential_workdir_value \
                vfork-host-differential "$@") || exit $?
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *)
            printf 'vfork-host-differential: unknown argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done

WORKDIR=$(differential_resolve_workdir "$REPO_ROOT" "$WORKDIR")
SOURCE="$REPO_ROOT/$SOURCE_REL"
MANIFEST="$REPO_ROOT/$MANIFEST_REL"
BINARY="$WORKDIR/vfork-smoke"
LOG="$WORKDIR/vfork-smoke.log"
RESULT="$WORKDIR/result.txt"
RECEIPT="$WORKDIR/receipt.json"
APPLIED="$WORKDIR/allowlist-applied.jsonl"
rm -f -- "$BINARY" "$LOG" "$RESULT" "$RECEIPT" "$APPLIED"

differential_build_smoke "$BINARY" "$SOURCE" \
    -O2 -std=c11 -Wall -Wextra -Werror
command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' 'vfork-host-differential: timeout command is required' >&2
    exit 1
}
exec_target=$(command -v busybox 2>/dev/null || command -v sleep 2>/dev/null || true)
if [ -z "$exec_target" ]; then
    printf '%s\n' 'vfork-host-differential: busybox or sleep target is required' >&2
    exit 1
fi

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
    printf 'vfork-host-differential: FAIL %s\n' "$1" | tee "$RESULT" >&2
    exit 1
}

smoke_status=0
RUNS=200
: >"$LOG"
for iteration in $(seq 1 "$RUNS"); do
    iteration_log="$WORKDIR/vfork-smoke-run-$iteration.log"
    rm -f -- "$iteration_log"
    THEKERNEL_VFORK_EXEC_TARGET="$exec_target" \
        differential_run_bounded "$iteration_log" 60s 5s -- "$BINARY" ||
        smoke_status=$?
    cat "$iteration_log" >>"$LOG"
    if [ "$smoke_status" -ne 0 ]; then
        fail_run "smoke_exit=$smoke_status timeout_secs=60 iteration=$iteration"
    fi
    if grep -Fq 'THEKERNEL_VFORK_FAIL' "$iteration_log"; then
        fail_run "smoke_reported_failure=1 iteration=$iteration"
    fi
    if ! grep -Fq 'THEKERNEL_VFORK_EXIT_PHASE3_OK' "$iteration_log"; then
        fail_run "exit_phase3_observation_missing=1 iteration=$iteration"
    fi
    if ! grep -Fq 'THEKERNEL_VFORK_SIGNAL_KILL_OK' "$iteration_log"; then
        fail_run "signal_kill_observation_missing=1 iteration=$iteration"
    fi
    if [ "$(tail -n 1 "$iteration_log")" != 'THEKERNEL_VFORK_OK' ]; then
        fail_run "final_marker_not_last=1 iteration=$iteration"
    fi
    if ! grep -Fq 'thekernel_vfork: exec_identity target=' "$iteration_log"; then
        fail_run "exec_identity_observation_missing=1 iteration=$iteration"
    fi
    smoke_status=0
done
missing_total=$(differential_missing_markers "$LOG" "$MANIFEST" present || true)
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
if [ "$(tail -n 1 "$LOG")" != 'THEKERNEL_VFORK_OK' ]; then
    fail_run 'final_marker_not_last=1'
fi

emit_receipt pass
printf 'vfork-host-differential: PASS markers_expected=%s evidence=%s\n' \
    "$markers_expected" "$WORKDIR" | tee "$RESULT"
