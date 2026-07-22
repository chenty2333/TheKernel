#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

[ "$#" -eq 0 ] || nightly_fail 'smp-tlb-shootdown adapter takes no arguments'

guest_input_value() {
    local receipt=$1
    local key=$2
    awk -F '\t' -v expected="$key" '
        $1 == expected {
            count += 1
            value = $2
        }
        END {
            if (count != 1 || value == "") {
                exit 1
            }
            print value
        }
    ' "$receipt"
}

write_tsv_row() {
    [ "$#" -gt 0 ] || nightly_fail 'cannot write an empty TSV row'
    printf '%s' "$1"
    shift
    printf '\t%s' "$@"
    printf '\n'
}

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
provenance="$NIGHTLY_LOG_DIR/smp-tlb-shootdown-provenance.tsv"
rm -f "$manifest"
rm -f "$provenance"
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

printf '%s\t%s\t%s\t%s\n' \
    phase thekernel_commit thekernel_ax_commit thekernel_linux_abi_commit \
    >"$provenance"
printf '%s\t%s\t%s\t%s\n' \
    preflight "$repo_commit" "$ax_commit" "$linux_abi_commit" \
    >>"$provenance"

write_tsv_row \
    thekernel_commit thekernel_ax_commit thekernel_linux_abi_commit \
    arch requested_cpus online_cpus control_cpu worker_count worker_cpus \
    kernel_sha256 commands_sha256 rootfs_sha256 qemu_binary qemu_sha256 \
    qemu_version cross_compiler cross_compiler_sha256 cross_compiler_version \
    rustc_version cargo_version kernel_artifact commands_artifact \
    rootfs_digest_artifact inputs_artifact qemu_receipt qemu_log >"$manifest"

run_count=0
while IFS= read -r arch; do
    for cpus in "${cpu_counts[@]}"; do
        run_name="${arch}-${cpus}cpu"
        commands="$NIGHTLY_LOG_DIR/$run_name.commands.input"
        run_dir="$NIGHTLY_LOG_DIR/$run_name"

        printf '%s --expect-cpus %s && %s --expect-cpus %s; exit\n' \
            /opt/thekernel-tests/bin/thekernel-wait-boundary "$cpus" \
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
        case "$arch" in
            rv)
                setrlimit_precedence_marker='CI_WAIT_BOUNDARY_SETRLIMIT_PRECEDENCE_OK bad_new=EFAULT'
                ;;
            la)
                setrlimit_precedence_marker='CI_WAIT_BOUNDARY_SETRLIMIT_PRECEDENCE_NA syscall=absent'
                ;;
            *)
                nightly_fail "unsupported architecture in wait-boundary gate: $arch"
                ;;
        esac
        nightly_validate_guest_log \
            "$run_dir/qemu.log" clean \
            "CI_WAIT_BOUNDARY_CLOCK_PERCPU_OK online_cpus=$cpus" \
            'CI_WAIT_BOUNDARY_TIMERFD_CANCEL_OK' \
            'CI_WAIT_BOUNDARY_ITIMER_PERIODIC_OK min_hits=3' \
            'CI_WAIT_BOUNDARY_ITIMER_CPU_OK no_syscall_loop=1' \
            'CI_WAIT_BOUNDARY_RLIMIT_CPU_ESCALATION_OK soft_after_signal=2 hard_signal=SIGKILL' \
            'CI_WAIT_BOUNDARY_RLIMIT_CPU_HARD_ONLY_OK signal=SIGKILL sigxcpu=0' \
            'CI_WAIT_BOUNDARY_PRLIMIT_PRECEDENCE_OK bad_new=EFAULT bad_pid_before_resource=ESRCH' \
            'CI_WAIT_BOUNDARY_PRLIMIT_TRANSACTION_OK old_new=atomic invalid=rollback copyout_fault=committed' \
            "$setrlimit_precedence_marker" \
            'CI_WAIT_BOUNDARY_SETITIMER_PRECEDENCE_OK bad_new=EFAULT' \
            'CI_WAIT_BOUNDARY_FUTEX_WAKE_OK' \
            'CI_WAIT_BOUNDARY_FUTEX_TIMEOUT_OK' \
            'CI_WAIT_BOUNDARY_FUTEX_WAITV_OK' \
            'CI_WAIT_BOUNDARY_PASS' \
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

        rootfs=$(nightly_rootfs_path "$arch") \
            || nightly_fail "invalid architecture after run: $arch"
        kernel_artifact="$run_dir/kernel"
        commands_artifact="$run_dir/commands"
        rootfs_digest_artifact="$run_dir/rootfs.sha256"
        inputs_artifact="$run_dir/guest-inputs.tsv"
        qemu_receipt="$run_dir/qemu-runner-receipt.json"
        for artifact in \
            "$kernel_artifact" "$commands_artifact" "$rootfs_digest_artifact" \
            "$inputs_artifact" "$qemu_receipt"; do
            [ -s "$artifact" ] || nightly_fail "missing pre-run evidence artifact: $artifact"
        done

        kernel_sha256=$(guest_input_value "$inputs_artifact" kernel_sha256) \
            || nightly_fail "invalid kernel receipt: $inputs_artifact"
        commands_sha256=$(guest_input_value "$inputs_artifact" commands_sha256) \
            || nightly_fail "invalid commands receipt: $inputs_artifact"
        rootfs_sha256=$(guest_input_value "$inputs_artifact" rootfs_sha256) \
            || nightly_fail "invalid rootfs receipt: $inputs_artifact"
        qemu_binary=$(guest_input_value "$inputs_artifact" qemu_binary) \
            || nightly_fail "invalid QEMU receipt input: $inputs_artifact"
        qemu_sha256=$(guest_input_value "$inputs_artifact" qemu_sha256) \
            || nightly_fail "invalid QEMU hash receipt: $inputs_artifact"
        qemu_version=$(guest_input_value "$inputs_artifact" qemu_version) \
            || nightly_fail "invalid QEMU version receipt: $inputs_artifact"
        cross_compiler=$(guest_input_value "$inputs_artifact" cross_compiler) \
            || nightly_fail "invalid compiler receipt: $inputs_artifact"
        cross_compiler_sha256=$(guest_input_value "$inputs_artifact" cross_compiler_sha256) \
            || nightly_fail "invalid compiler hash receipt: $inputs_artifact"
        cross_compiler_version=$(guest_input_value "$inputs_artifact" cross_compiler_version) \
            || nightly_fail "invalid compiler version receipt: $inputs_artifact"
        rustc_version=$(guest_input_value "$inputs_artifact" rustc_version) \
            || nightly_fail "invalid rustc receipt: $inputs_artifact"
        cargo_version=$(guest_input_value "$inputs_artifact" cargo_version) \
            || nightly_fail "invalid cargo receipt: $inputs_artifact"

        [ "$(sha256sum "$kernel_artifact" | awk '{ print $1 }')" = "$kernel_sha256" ] \
            || nightly_fail "staged kernel hash drift: $kernel_artifact"
        [ "$(sha256sum "$commands_artifact" | awk '{ print $1 }')" = "$commands_sha256" ] \
            || nightly_fail "staged command hash drift: $commands_artifact"
        [ "$(sha256sum "$rootfs" | awk '{ print $1 }')" = "$rootfs_sha256" ] \
            || nightly_fail "rootfs hash drift after guest run: $rootfs"
        [ "$(sha256sum "$qemu_binary" | awk '{ print $1 }')" = "$qemu_sha256" ] \
            || nightly_fail "QEMU binary hash drift: $qemu_binary"
        [ "$(sha256sum "$cross_compiler" | awk '{ print $1 }')" = "$cross_compiler_sha256" ] \
            || nightly_fail "cross compiler hash drift: $cross_compiler"
        [ "$(awk '{ print $1; exit }' "$rootfs_digest_artifact")" = "$rootfs_sha256" ] \
            || nightly_fail "rootfs digest sidecar drift: $rootfs_digest_artifact"
        python3 "$CI_SCRIPT_DIR/validate-qemu-receipt.py" \
            --receipt "$qemu_receipt" --arch "$arch" --cpus "$cpus" \
            --kernel "$kernel_artifact" --rootfs "$rootfs" \
            --rootfs-mode snapshot --log "$run_dir/qemu.log" \
            --qemu-binary "$qemu_binary" --commands "$commands_artifact" >/dev/null \
            || nightly_fail "invalid QEMU lifecycle receipt: $qemu_receipt"

        write_tsv_row \
            "$repo_commit" "$ax_commit" "$linux_abi_commit" \
            "$arch" "$cpus" "$online_cpus" "$control_cpu" \
            "$worker_count" "$worker_cpus" "$kernel_sha256" \
            "$commands_sha256" "$rootfs_sha256" "$qemu_binary" \
            "$qemu_sha256" "$qemu_version" "$cross_compiler" \
            "$cross_compiler_sha256" "$cross_compiler_version" \
            "$rustc_version" "$cargo_version" "$kernel_artifact" \
            "$commands_artifact" "$rootfs_digest_artifact" "$inputs_artifact" \
            "$qemu_receipt" "$run_dir/qemu.log" \
            >>"$manifest"
        run_count=$((run_count + 1))
    done
done <<<"$selected_arches"

manifest_rows=$(awk 'END { print NR - 1 }' "$manifest")
[ "$manifest_rows" -eq "$run_count" ] \
    || nightly_fail "SMP TLB manifest row-count drift: $manifest_rows"
awk -F '\t' 'NF != 26 { exit 1 }' "$manifest" \
    || nightly_fail 'SMP TLB manifest column-count drift'
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
printf '%s\t%s\t%s\t%s\n' \
    finalize "$repo_commit" "$ax_commit" "$linux_abi_commit" \
    >>"$provenance"
[ "$(awk 'END { print NR - 1 }' "$provenance")" -eq 2 ] \
    || nightly_fail 'SMP TLB provenance phase-count drift'
printf 'nightly SMP TLB shootdown evidence: COMPLETE manifest=%s provenance=%s runs=%s\n' \
    "$manifest" "$provenance" "$run_count"
