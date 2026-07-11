#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'oom-failpoint adapter takes no arguments'
FAILURE_BYTES=${THEKERNEL_NIGHTLY_OOM_FAILURE_BYTES:-67108864}
ci_require_positive_int oom_failure_bytes "$FAILURE_BYTES"

mkdir -p "$NIGHTLY_LOG_DIR"
support_image=$(nightly_prepare_support_image)
selected_arches=$(nightly_selected_arches) || exit $?

while IFS= read -r arch; do
    commands="$NIGHTLY_LOG_DIR/$arch.commands"
    run_dir="$NIGHTLY_LOG_DIR/$arch"
    printf '/opt/oscomp-support/bin/thekernel-nightly-oom-failpoint %s; exit\n' \
        "$FAILURE_BYTES" >"$commands"

    nightly_run_guest "$arch" "$commands" "$run_dir" "$support_image"
    nightly_validate_guest_log \
        "$run_dir/qemu.log" clean \
        CI_NIGHTLY_OOM_FAILPOINT_START \
        NIGHTLY_OOM_EXPECTED_ENOMEM \
        NIGHTLY_OOM_RECOVERY_MAPPING_OK \
        CI_NIGHTLY_OOM_FAILPOINT_PASS
done <<<"$selected_arches"

printf 'nightly OOM/failpoint adapter: PASS\n'
