#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'pressure adapter takes no arguments'
PRESSURE_ITERATIONS=${THEKERNEL_NIGHTLY_PRESSURE_ITERATIONS:-16}
ci_require_positive_int pressure_iterations "$PRESSURE_ITERATIONS"

mkdir -p "$NIGHTLY_LOG_DIR"
selected_arches=$(nightly_selected_arches) || exit $?

while IFS= read -r arch; do
    commands="$NIGHTLY_LOG_DIR/$arch.commands"
    run_dir="$NIGHTLY_LOG_DIR/$arch"
    printf '/opt/thekernel-tests/bin/thekernel-nightly-pressure %s; exit\n' \
        "$PRESSURE_ITERATIONS" >"$commands"

    nightly_run_guest "$arch" "$commands" "$run_dir"
    nightly_validate_guest_log \
        "$run_dir/qemu.log" clean \
        CI_NIGHTLY_PRESSURE_START CI_NIGHTLY_PRESSURE_PASS
done <<<"$selected_arches"

printf 'nightly pressure adapter: PASS\n'
