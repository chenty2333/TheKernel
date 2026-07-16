#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'mm-performance adapter takes no arguments'

MM_PERF_CPUS=${THEKERNEL_MM_PERF_CPUS:-"4 8"}
MM_PERF_ITERATIONS=${THEKERNEL_MM_PERF_ITERATIONS:-256}
MM_PERF_VMAS=${THEKERNEL_MM_PERF_VMAS:-512}
MM_PERF_PIN_ITERATIONS=${THEKERNEL_MM_PERF_PIN_ITERATIONS:-64}

ci_require_positive_int mm_perf_iterations "$MM_PERF_ITERATIONS"
ci_require_positive_int mm_perf_vmas "$MM_PERF_VMAS"
ci_require_positive_int mm_perf_pin_iterations "$MM_PERF_PIN_ITERATIONS"
[ "$MM_PERF_ITERATIONS" -le 100000 ] \
    || nightly_fail 'THEKERNEL_MM_PERF_ITERATIONS must not exceed 100000'
[ "$MM_PERF_VMAS" -le 16384 ] \
    || nightly_fail 'THEKERNEL_MM_PERF_VMAS must not exceed 16384'
[ "$MM_PERF_PIN_ITERATIONS" -le 10000 ] \
    || nightly_fail 'THEKERNEL_MM_PERF_PIN_ITERATIONS must not exceed 10000'

read -r -a cpu_counts <<<"$MM_PERF_CPUS"
[ "${#cpu_counts[@]}" -gt 0 ] || nightly_fail 'THEKERNEL_MM_PERF_CPUS is empty'
seen_cpus=' '
for cpus in "${cpu_counts[@]}"; do
    case "$cpus" in
        ''|*[!0-9]*)
            nightly_fail "THEKERNEL_MM_PERF_CPUS contains a non-integer: $cpus"
            ;;
    esac
    [ "$cpus" -gt 0 ] && [ "$cpus" -le 64 ] \
        || nightly_fail "THEKERNEL_MM_PERF_CPUS must contain values from 1 to 64"
    case "$seen_cpus" in
        *" $cpus "*) nightly_fail "duplicate CPU count in THEKERNEL_MM_PERF_CPUS: $cpus" ;;
    esac
    seen_cpus="$seen_cpus$cpus "
done

mkdir -p "$NIGHTLY_LOG_DIR"
matrix="$NIGHTLY_LOG_DIR/mm-performance.tsv"
manifest="$NIGHTLY_LOG_DIR/mm-performance-manifest.tsv"
rm -f "$matrix"
rm -f "$manifest"
selected_arches=$(nightly_selected_arches) || exit $?
first_artifact=1

repo_commit=$(git -C "$REPO_ROOT" rev-parse --verify HEAD) \
    || nightly_fail 'cannot resolve TheKernel HEAD'
if [ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all)" ]; then
    nightly_fail 'exact-HEAD MM evidence requires a clean TheKernel worktree'
fi
ax_repo=${THEKERNEL_AX_REPO:-$REPO_ROOT/../thekernel-ax}
linux_abi_repo=${THEKERNEL_LINUX_ABI_REPO:-$REPO_ROOT/../thekernel-linux-abi}
for dependency in "$ax_repo" "$linux_abi_repo"; do
    [ -d "$dependency" ] || nightly_fail "missing maintained dependency: $dependency"
    if [ -n "$(git -C "$dependency" status --porcelain --untracked-files=all)" ]; then
        nightly_fail "exact-HEAD MM evidence requires a clean dependency: $dependency"
    fi
done
ax_commit=$(git -C "$ax_repo" rev-parse --verify HEAD) \
    || nightly_fail 'cannot resolve thekernel-ax HEAD'
linux_abi_commit=$(git -C "$linux_abi_repo" rev-parse --verify HEAD) \
    || nightly_fail 'cannot resolve thekernel-linux-abi HEAD'
printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    thekernel_commit thekernel_ax_commit thekernel_linux_abi_commit \
    arch requested_cpus online_cpus kernel_sha256 rootfs_sha256 \
    qemu_binary qemu_version metrics_artifact qemu_log >"$manifest"

while IFS= read -r arch; do
    for cpus in "${cpu_counts[@]}"; do
        run_name="${arch}-${cpus}cpu"
        commands="$NIGHTLY_LOG_DIR/$run_name.commands"
        run_dir="$NIGHTLY_LOG_DIR/$run_name"
        artifact="$run_dir/mm-performance.tsv"

        printf '%s %s %s %s %s %s %s %s %s; exit\n' \
            /opt/thekernel-tests/bin/thekernel-mm-performance \
            --iterations "$MM_PERF_ITERATIONS" \
            --vmas "$MM_PERF_VMAS" \
            --pin-iterations "$MM_PERF_PIN_ITERATIONS" \
            --pin-workers "$cpus" \
            >"$commands"

        (
            export THEKERNEL_QEMU_CPUS=$cpus
            export THEKERNEL_KERNEL_CPUS=$cpus
            export SMP=$cpus
            export THEKERNEL_NIGHTLY_REBUILD_KERNELS=1
            nightly_run_guest "$arch" "$commands" "$run_dir"
        )
        nightly_validate_guest_log \
            "$run_dir/qemu.log" clean \
            'MM_PERF_SEMANTICS status=ok' \
            'MM_PERF_DONE status=ok'
        python3 "$CI_SCRIPT_DIR/parse-mm-performance.py" \
            "$run_dir/qemu.log" --arch "$arch" --cpus "$cpus" \
            --output "$artifact"

        if [ "$first_artifact" -eq 1 ]; then
            cp "$artifact" "$matrix"
            first_artifact=0
        else
            tail -n +2 "$artifact" >>"$matrix"
        fi

        kernel=$(nightly_kernel_path "$arch") \
            || nightly_fail "invalid architecture after run: $arch"
        rootfs=$(nightly_rootfs_path "$arch") \
            || nightly_fail "invalid architecture after run: $arch"
        qemu=$(nightly_qemu_binary "$arch") \
            || nightly_fail "invalid QEMU architecture after run: $arch"
        online_cpus=$(awk -F '\t' 'NR == 2 { print $3 }' "$artifact")
        [ "$online_cpus" = "$cpus" ] \
            || nightly_fail "parsed topology drift for $run_name: $online_cpus"
        kernel_sha256=$(sha256sum "$kernel" | awk '{ print $1 }')
        rootfs_sha256=$(sha256sum "$rootfs" | awk '{ print $1 }')
        qemu_version=$("$qemu" --version | sed -n '1{s/[[:space:]]\+/ /g;p;}')
        qemu_version=${qemu_version//$'\t'/ }
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$repo_commit" "$ax_commit" "$linux_abi_commit" \
            "$arch" "$cpus" "$online_cpus" "$kernel_sha256" \
            "$rootfs_sha256" "$(command -v "$qemu")" "$qemu_version" \
            "$artifact" "$run_dir/qemu.log" >>"$manifest"
    done
done <<<"$selected_arches"

missing_count=$(
    awk -F '\t' \
        'NR > 1 && $5 == "missing" { count += 1 } END { print count + 0 }' \
        "$matrix"
)
printf 'nightly MM performance evidence: COMPLETE matrix=%s manifest=%s explicit_missing=%s\n' \
    "$matrix" "$manifest" "$missing_count"
