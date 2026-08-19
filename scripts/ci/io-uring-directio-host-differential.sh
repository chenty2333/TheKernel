#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/ci/differential/lib.sh
. "$SCRIPT_DIR/differential/lib.sh"

CASE=io-uring-directio
WORKDIR="$REPO_ROOT/.state/ci/io-uring-directio-host-differential"
SOURCE_REL=tests/guest/tools/io-uring-directio-differential.c
MANIFEST_REL=scripts/ci/differential/manifests/io-uring-directio.markers

usage() {
    cat <<'EOF'
Usage: scripts/ci/io-uring-directio-host-differential.sh [OPTIONS]

Options:
  --workdir DIR                Artifact directory

Builds and runs the portable registered-buffer/O_DIRECT helper against the
host Linux kernel.  The workdir is also the fixture filesystem; the default
is under the repository's btrfs-backed state tree so the alignment observation
is reproducible.  The helper emits precise CQE results
for alignment, EOF, short-read, sparse-hole, fragmented-data, fixed-buffer
subrange/range, invalid-slot, and unregister-after-admission cases.
Fragmentation is marked as verified only when FIEMAP reports multiple data
extents; unsupported FIEMAP is explicit.
EOF
}

while (($#)); do
    case "$1" in
        --workdir)
            WORKDIR=$(differential_workdir_value \
                io-uring-directio-host-differential "$@") || exit $?
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *)
            printf 'io-uring-directio-host-differential: unknown argument: %s\n' \
                "$1" >&2
            exit 2
            ;;
    esac
done

WORKDIR=$(differential_resolve_workdir "$REPO_ROOT" "$WORKDIR")
SOURCE="$REPO_ROOT/$SOURCE_REL"
MANIFEST="$REPO_ROOT/$MANIFEST_REL"
BINARY="$WORKDIR/io-uring-directio-differential"
LOG="$WORKDIR/io-uring-directio.log"
RESULT="$WORKDIR/result.txt"
RECEIPT="$WORKDIR/receipt.json"
APPLIED="$WORKDIR/allowlist-applied.jsonl"
rm -f -- "$BINARY" "$LOG" "$RESULT" "$RECEIPT" "$APPLIED"

differential_build_smoke "$BINARY" "$SOURCE" \
    -O2 -std=c11 -Wall -Wextra -Werror -pthread
command -v timeout >/dev/null 2>&1 || {
    printf '%s\n' \
        'io-uring-directio-host-differential: timeout command is required' >&2
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
    printf 'io-uring-directio-host-differential: FAIL %s\n' "$1" \
        | tee "$RESULT" >&2
    exit 1
}

smoke_status=0
# Keep the fixture on the workdir filesystem.  /tmp may be tmpfs, where Linux
# is permitted to transparently fall back for misaligned O_DIRECT requests;
# the helper records that as a filesystem observation rather than treating it
# as a cross-system expectation.
fixture="$WORKDIR/thekernel-io-uring-directio-fixture"
THEKERNEL_DIRECTIO_PATH="$fixture" \
    differential_run_bounded "$LOG" 60s 5s -- "$BINARY" --linux-host \
    || smoke_status=$?
missing_total=$(differential_missing_markers "$LOG" "$MANIFEST" once || true)
missing=$missing_total
if [ "$smoke_status" -ne 0 ]; then
    if grep -Fq \
        'THEKERNEL_IO_URING_DIRECTIO_UNREGISTER_ADMITTED_UNSUPPORTED' "$LOG"; then
        fail_run 'unregister-admitted-inflight-unsupported=1'
    fi
    fail_run "smoke_exit=$smoke_status timeout_secs=60"
fi
if [ -n "$missing" ]; then
    fail_run "missing_marker=$(printf '%s\n' "$missing" | sed -n 1p)"
fi
if grep -Fq 'THEKERNEL_IO_URING_DIRECTIO_FAIL' "$LOG"; then
    fail_run 'smoke_reported_failure=1'
fi
for boundary in alignment_address alignment_length alignment_offset eof_exact eof_past \
    short_read write_fixed fixed_subrange fixed_range_efault invalid_fixed_slot \
    unregister_admitted close_pending; do
    if [ "$(grep -c "^io_uring_directio: linux_host=1 ${boundary} " "$LOG" || true)" -ne 1 ]; then
        fail_run "boundary_count_mismatch=${boundary}"
    fi
done
short_tail_total=$(grep -c \
    '^io_uring_directio: linux_host=1 short_read_tail=' "$LOG" || true)
short_tail_valid=$(grep -Ec \
    '^io_uring_directio: linux_host=1 short_read_tail=(preserved|zeroed) bytes=[0-9]+$' \
    "$LOG" || true)
if [ "$short_tail_total" -ne 1 ] || [ "$short_tail_valid" -ne 1 ]; then
    fail_run 'short-read-tail-oracle-missing-or-invalid=1'
fi
fragmented_physical=$(grep -c \
    '^THEKERNEL_IO_URING_DIRECTIO_FRAGMENTED_EXTENT_PHYSICAL_SG_OK$' "$LOG" || true)
fragmented_unsupported=$(grep -c \
    '^THEKERNEL_IO_URING_DIRECTIO_FRAGMENTED_EXTENT_UNSUPPORTED$' "$LOG" || true)
if [ "$((fragmented_physical + fragmented_unsupported))" -ne 1 ]; then
    fail_run 'fragmented-extent-oracle-missing-or-duplicated=1'
fi
if [ "$(tail -n 1 "$LOG")" != 'THEKERNEL_IO_URING_DIRECTIO_OK' ]; then
    fail_run 'final_marker_not_last=1'
fi

emit_receipt pass
printf 'io-uring-directio-host-differential: PASS markers_expected=%s evidence=%s\n' \
    "$markers_expected" "$WORKDIR" | tee "$RESULT"
