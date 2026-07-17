#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'smp-tlb-shootdown adapter takes no arguments'

SMP_TLB_CPUS=${THEKERNEL_SMP_TLB_CPUS:-"4 8"}

read -r -a cpu_counts <<<"$SMP_TLB_CPUS"
[ "${#cpu_counts[@]}" -gt 0 ] || nightly_fail 'THEKERNEL_SMP_TLB_CPUS is empty'
seen_cpus=' '
for cpus in "${cpu_counts[@]}"; do
    case "$cpus" in
        ''|*[!0-9]*)
            nightly_fail "THEKERNEL_SMP_TLB_CPUS contains a non-integer: $cpus"
            ;;
    esac
    [ "$cpus" -ge 2 ] && [ "$cpus" -le 64 ] \
        || nightly_fail 'THEKERNEL_SMP_TLB_CPUS must contain values from 2 to 64'
    case "$seen_cpus" in
        *" $cpus "*) nightly_fail "duplicate CPU count in THEKERNEL_SMP_TLB_CPUS: $cpus" ;;
    esac
    seen_cpus="$seen_cpus$cpus "
done

mkdir -p "$NIGHTLY_LOG_DIR"
manifest="$NIGHTLY_LOG_DIR/smp-tlb-shootdown-manifest.tsv"
rm -f "$manifest"
selected_arches=$(nightly_selected_arches) || exit $?

repo_commit=$(git -C "$REPO_ROOT" rev-parse --verify HEAD) \
    || nightly_fail 'cannot resolve TheKernel HEAD'
if [ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all)" ]; then
    nightly_fail 'exact-HEAD SMP TLB evidence requires a clean TheKernel worktree'
fi
cargo_ax_repo=$(cd -- "$REPO_ROOT/../thekernel-ax" && pwd -P) \
    || nightly_fail 'missing Cargo path dependency: ../thekernel-ax'
cargo_linux_abi_repo=$(cd -- "$REPO_ROOT/../thekernel-linux-abi" && pwd -P) \
    || nightly_fail 'missing Cargo path dependency: ../thekernel-linux-abi'
ax_repo=${THEKERNEL_AX_REPO:-$cargo_ax_repo}
linux_abi_repo=${THEKERNEL_LINUX_ABI_REPO:-$cargo_linux_abi_repo}
ax_repo=$(cd -- "$ax_repo" && pwd -P) \
    || nightly_fail "missing maintained dependency: $ax_repo"
linux_abi_repo=$(cd -- "$linux_abi_repo" && pwd -P) \
    || nightly_fail "missing maintained dependency: $linux_abi_repo"
[ "$ax_repo" = "$cargo_ax_repo" ] \
    || nightly_fail 'THEKERNEL_AX_REPO does not match the Cargo path dependency'
[ "$linux_abi_repo" = "$cargo_linux_abi_repo" ] \
    || nightly_fail \
        'THEKERNEL_LINUX_ABI_REPO does not match the Cargo path dependency'
for dependency in "$ax_repo" "$linux_abi_repo"; do
    [ -d "$dependency" ] || nightly_fail "missing maintained dependency: $dependency"
    if [ -n "$(git -C "$dependency" status --porcelain --untracked-files=all)" ]; then
        nightly_fail "exact-HEAD SMP TLB evidence requires a clean dependency: $dependency"
    fi
done
ax_commit=$(git -C "$ax_repo" rev-parse --verify HEAD) \
    || nightly_fail 'cannot resolve thekernel-ax HEAD'
linux_abi_commit=$(git -C "$linux_abi_repo" rev-parse --verify HEAD) \
    || nightly_fail 'cannot resolve thekernel-linux-abi HEAD'

printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    thekernel_commit thekernel_ax_commit thekernel_linux_abi_commit \
    arch requested_cpus online_cpus control_cpu worker_count worker_cpus \
    kernel_sha256 rootfs_sha256 qemu_binary qemu_version kernel_artifact \
    commands_artifact qemu_log >"$manifest"

run_count=0
while IFS= read -r arch; do
    for cpus in "${cpu_counts[@]}"; do
        run_name="${arch}-${cpus}cpu"
        commands="$NIGHTLY_LOG_DIR/$run_name.commands.input"
        run_dir="$NIGHTLY_LOG_DIR/$run_name"

        printf '%s --expect-cpus %s; exit\n' \
            /opt/thekernel-tests/bin/thekernel-smp-tlb-shootdown "$cpus" \
            >"$commands"
        (
            export THEKERNEL_QEMU_CPUS=$cpus
            export THEKERNEL_KERNEL_CPUS=$cpus
            export SMP=$cpus
            export THEKERNEL_NIGHTLY_REBUILD_KERNELS=1
            export THEKERNEL_NIGHTLY_REBUILD_ROOTFS=1
            nightly_run_guest "$arch" "$commands" "$run_dir"
        )
        nightly_validate_guest_log \
            "$run_dir/qemu.log" clean \
            'SMP_TLB_GATE status=ok stale_count=0'
        if grep -Eq \
            '^SMP_TLB_GATE status=fail|^SMP_TLB_CASE .* status=stale |^SMP_TLB_GATE .*stale_count=[1-9]' \
            "$run_dir/qemu.log"; then
            nightly_fail "stale translation or failure marker found in $run_dir/qemu.log"
        fi
        "$CI_SCRIPT_DIR/validate-smp-tlb-log.sh" \
            "$run_dir/qemu.log" "$cpus" >/dev/null \
            || nightly_fail "invalid SMP TLB guest evidence: $run_dir/qemu.log"

        topology=$(awk '/^SMP_TLB_TOPOLOGY / { sub(/\r$/, ""); print; exit }' \
            "$run_dir/qemu.log")
        read -r _ online_field control_field worker_count_field worker_cpus_field \
            <<<"$topology"
        online_cpus=${online_field#online_cpus=}
        control_cpu=${control_field#control_cpu=}
        worker_count=${worker_count_field#worker_count=}
        worker_cpus=${worker_cpus_field#worker_cpus=}

        kernel=$(nightly_kernel_path "$arch") \
            || nightly_fail "invalid architecture after run: $arch"
        rootfs=$(nightly_rootfs_path "$arch") \
            || nightly_fail "invalid architecture after run: $arch"
        qemu=$(nightly_qemu_binary "$arch") \
            || nightly_fail "invalid QEMU architecture after run: $arch"
        kernel_artifact="$run_dir/kernel"
        commands_artifact="$run_dir/commands"
        cp -- "$kernel" "$kernel_artifact"
        cp -- "$commands" "$commands_artifact"
        kernel_sha256=$(sha256sum "$kernel_artifact" | awk '{ print $1 }')
        rootfs_sha256=$(sha256sum "$rootfs" | awk '{ print $1 }')
        qemu_version=$("$qemu" --version | sed -n '1{s/[[:space:]]\+/ /g;p;}')
        qemu_version=${qemu_version//$'\t'/ }
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$repo_commit" "$ax_commit" "$linux_abi_commit" \
            "$arch" "$cpus" "$online_cpus" "$control_cpu" \
            "$worker_count" "$worker_cpus" "$kernel_sha256" \
            "$rootfs_sha256" "$(command -v "$qemu")" "$qemu_version" \
            "$kernel_artifact" "$commands_artifact" "$run_dir/qemu.log" \
            >>"$manifest"
        run_count=$((run_count + 1))
    done
done <<<"$selected_arches"

manifest_rows=$(awk 'END { print NR - 1 }' "$manifest")
[ "$manifest_rows" -eq "$run_count" ] \
    || nightly_fail "SMP TLB manifest row-count drift: $manifest_rows"
for state in \
    "$REPO_ROOT:$repo_commit:TheKernel" \
    "$ax_repo:$ax_commit:thekernel-ax" \
    "$linux_abi_repo:$linux_abi_commit:thekernel-linux-abi"; do
    IFS=: read -r dependency expected_commit label <<<"$state"
    [ "$(git -C "$dependency" rev-parse --verify HEAD)" = "$expected_commit" ] \
        || nightly_fail "$label HEAD changed during SMP TLB evidence capture"
    [ -z "$(git -C "$dependency" status --porcelain --untracked-files=all)" ] \
        || nightly_fail "$label worktree changed during SMP TLB evidence capture"
done
printf 'nightly SMP TLB shootdown evidence: COMPLETE manifest=%s runs=%s\n' \
    "$manifest" "$run_count"
