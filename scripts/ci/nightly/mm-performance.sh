#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
# shellcheck source=mm-performance-boundary.sh
source "$SCRIPT_DIR/mm-performance-boundary.sh"

[ "$#" -eq 0 ] || nightly_fail 'mm-performance adapter takes no arguments'

MM_PERF_CPUS=${THEKERNEL_MM_PERF_CPUS:-"4 8"}
MM_PERF_ITERATIONS=${THEKERNEL_MM_PERF_ITERATIONS:-256}
MM_PERF_VMAS=${THEKERNEL_MM_PERF_VMAS:-512}
MM_PERF_PIN_ITERATIONS=${THEKERNEL_MM_PERF_PIN_ITERATIONS:-64}
MM_PERF_HOST_CPUS=${THEKERNEL_MM_PERF_HOST_CPUS:-}
MM_PERF_SETTLE_SECS=${THEKERNEL_MM_PERF_SETTLE_SECS:-5}
MM_PERF_MEASUREMENT_MODE=${THEKERNEL_MM_PERF_MEASUREMENT_MODE:-product}

MM_PERF_KERNEL_PROFILE=$(mm_perf_kernel_profile_for_mode "$MM_PERF_MEASUREMENT_MODE") \
    || nightly_fail \
        'THEKERNEL_MM_PERF_MEASUREMENT_MODE must be product or diagnostic'
if [ -n "${THEKERNEL_NIGHTLY_KERNEL_PROFILE:-}" ] && \
    [ "$THEKERNEL_NIGHTLY_KERNEL_PROFILE" != "$MM_PERF_KERNEL_PROFILE" ]; then
    nightly_fail \
        "THEKERNEL_NIGHTLY_KERNEL_PROFILE conflicts with MM measurement mode: mode=$MM_PERF_MEASUREMENT_MODE expected=$MM_PERF_KERNEL_PROFILE actual=$THEKERNEL_NIGHTLY_KERNEL_PROFILE"
fi
export THEKERNEL_NIGHTLY_KERNEL_PROFILE=$MM_PERF_KERNEL_PROFILE

ci_require_positive_int mm_perf_iterations "$MM_PERF_ITERATIONS"
ci_require_positive_int mm_perf_vmas "$MM_PERF_VMAS"
ci_require_positive_int mm_perf_pin_iterations "$MM_PERF_PIN_ITERATIONS"
[ "$MM_PERF_ITERATIONS" -le 100000 ] \
    || nightly_fail 'THEKERNEL_MM_PERF_ITERATIONS must not exceed 100000'
[ "$MM_PERF_VMAS" -le 16384 ] \
    || nightly_fail 'THEKERNEL_MM_PERF_VMAS must not exceed 16384'
[ "$MM_PERF_PIN_ITERATIONS" -le 10000 ] \
    || nightly_fail 'THEKERNEL_MM_PERF_PIN_ITERATIONS must not exceed 10000'
mm_perf_validate_settle_seconds "$MM_PERF_SETTLE_SECS" \
    || nightly_fail \
        "THEKERNEL_MM_PERF_SETTLE_SECS must be an integer from 0 to $MM_PERF_MAX_SETTLE_SECS"

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
host_cpu_matrix="$NIGHTLY_LOG_DIR/mm-performance-host-cpus.tsv"
rm -f "$matrix"
rm -f "$manifest"
rm -f "$host_cpu_matrix"
selected_arches=$(nightly_selected_arches) || exit $?
first_artifact=1

command -v taskset >/dev/null 2>&1 \
    || nightly_unsupported 'missing taskset for inherited host CPU affinity'
selector_args=(--counts "${cpu_counts[@]}" --output "$host_cpu_matrix")
if [ -n "$MM_PERF_HOST_CPUS" ]; then
    selector_args+=(--explicit "$MM_PERF_HOST_CPUS")
fi
set +e
python3 "$CI_SCRIPT_DIR/select-mm-performance-cpus.py" "${selector_args[@]}"
selector_status=$?
set -e
case "$selector_status" in
    0) ;;
    78) nightly_unsupported 'no homogeneous host CPU class can hold the MM matrix' ;;
    *) nightly_fail 'invalid MM host CPU affinity selection' ;;
esac

runner_contract_sha256=$(
    {
        {
            printf '%s\n' \
                scripts/ci/boot-shell-runner.sh \
                scripts/ci/nightly/mm-performance.sh \
                scripts/ci/nightly/mm-performance-boundary.sh \
                scripts/ci/mm_performance_host.py \
                scripts/ci/mm_performance_schema.py \
                scripts/ci/capture-mm-performance-host.py \
                scripts/ci/compare-mm-performance.py \
                scripts/ci/nightly/lib.sh \
                scripts/ci/parse-mm-lock-diagnostics.py \
                scripts/ci/parse-mm-performance.py \
                scripts/ci/select-mm-performance-cpus.py \
                scripts/ci/validate-qemu-receipt.py \
                scripts/ci/nightly/mm-performance-regression-policy.json \
                scripts/ci/nightly/mm-performance-stability-policy.json \
                scripts/build-rootfs.sh \
                scripts/create-rootfs-image.sh \
                tests/guest/tools/mm-performance.c
            find "$REPO_ROOT/tools/qemu_runner" -maxdepth 1 -type f -name '*.py' \
                -printf 'tools/qemu_runner/%f\n'
        } | sort -u | while IFS= read -r relative; do
            [ -f "$REPO_ROOT/$relative" ] \
                || nightly_fail "runner contract input is missing: $relative"
            printf '%s\t%s\n' "$relative" \
                "$(sha256sum "$REPO_ROOT/$relative" | awk '{ print $1 }')"
        done
        printf 'setting\tmm_perf_settle_seconds=%s\n' "$MM_PERF_SETTLE_SECS"
        printf 'setting\tmm_perf_measurement_mode=%s\n' "$MM_PERF_MEASUREMENT_MODE"
        printf 'setting\tmm_perf_kernel_profile=%s\n' "$MM_PERF_KERNEL_PROFILE"
        printf 'setting\tmm_perf_diagnostic_off_retries=%s\n' \
            "$MM_PERF_DIAGNOSTIC_OFF_RETRIES"
    } | sha256sum | awk '{ print $1 }'
)

runner_id=${THEKERNEL_MM_PERF_RUNNER_ID:-}
case "$runner_id" in
    *$'\t'*|*$'\n'*)
        nightly_fail 'THEKERNEL_MM_PERF_RUNNER_ID must not contain tabs or newlines'
        ;;
esac
if [ -n "$runner_id" ]; then
    runner_fingerprint="declared-sha256:$(printf '%s' "$runner_id" | sha256sum | awk '{ print $1 }')"
else
    runner_host_sha256=$(
        {
            printf 'fingerprint_schema=thekernel-mm-performance-runner-v1\n'
            printf 'uname=%s\n' "$(uname -srm)"
            printf 'processors_online=%s\n' "$(getconf _NPROCESSORS_ONLN)"
            if [ -f /etc/os-release ]; then
                printf 'os_release_sha256=%s\n' \
                    "$(sha256sum /etc/os-release | awk '{ print $1 }')"
            else
                printf 'os_release_sha256=missing\n'
            fi
            for host_file in \
                /sys/devices/system/cpu/online \
                /sys/fs/cgroup/cpuset.cpus.effective \
                /sys/fs/cgroup/cpu.max \
                /sys/fs/cgroup/cpu.weight; do
                if [ -f "$host_file" ]; then
                    printf '%s=%s\n' "$host_file" "$(tr -d '\r\n' <"$host_file")"
                else
                    printf '%s=missing\n' "$host_file"
                fi
            done
            if [ -f /proc/cpuinfo ]; then
                awk -F : '
                    $1 ~ /^[[:space:]]*(vendor_id|cpu family|model|model name|stepping|microcode|cache size|physical id|siblings|core id|cpu cores|flags|bugs|address sizes|isa|uarch)[[:space:]]*$/ {
                        key = $1
                        value = $2
                        gsub(/^[[:space:]]+|[[:space:]]+$/, "", key)
                        gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
                        print "cpu." key "=" value
                    }
                ' /proc/cpuinfo | sort -u
            fi
        } | sha256sum | awk '{ print $1 }'
    )
    runner_fingerprint="auto-sha256:$runner_host_sha256"
fi

repo_commit=$(git -C "$REPO_ROOT" rev-parse --verify HEAD) \
    || nightly_fail 'cannot resolve TheKernel HEAD'
if [ -n "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all)" ]; then
    nightly_fail 'exact-HEAD MM evidence requires a clean TheKernel worktree'
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
        nightly_fail "exact-HEAD MM evidence requires a clean dependency: $dependency"
    fi
done
ax_commit=$(git -C "$ax_repo" rev-parse --verify HEAD) \
    || nightly_fail 'cannot resolve thekernel-ax HEAD'
linux_abi_commit=$(git -C "$linux_abi_repo" rev-parse --verify HEAD) \
    || nightly_fail 'cannot resolve thekernel-linux-abi HEAD'
manifest_columns=(
    bundle_schema
    thekernel_commit
    thekernel_ax_commit
    thekernel_linux_abi_commit
    measurement_mode
    kernel_profile
    arch
    requested_cpus
    online_cpus
    iterations
    live_vmas
    pin_iterations
    pin_workers
    kernel_sha256
    kernel_size_bytes
    rootfs_sha256
    qemu_binary
    qemu_version
    qemu_sha256
    runner_fingerprint
    runner_contract_sha256
    host_cpu_set
    host_cpu_selection
    host_cpu_class
    platform_class
    pmu_source
    cpu_model
    firmware_version
    cpu_freq_policy
    kernel_artifact
    metrics_artifact
    metrics_sha256
    metrics_size_bytes
    mm_lock_diagnostics_artifact
    mm_lock_diagnostics_sha256
    mm_lock_diagnostics_size_bytes
    commands
    commands_sha256
    commands_size_bytes
    guest_inputs
    guest_inputs_sha256
    guest_inputs_size_bytes
    qemu_receipt
    qemu_receipt_sha256
    qemu_receipt_size_bytes
    qemu_log
    qemu_log_sha256
    qemu_log_size_bytes
    host_diagnostics_pre
    host_diagnostics_pre_sha256
    host_diagnostics_pre_size_bytes
    host_diagnostics_post
    host_diagnostics_post_sha256
    host_diagnostics_post_size_bytes
)
(IFS=$'\t'; printf '%s\n' "${manifest_columns[*]}") >"$manifest"

run_count=0
while IFS= read -r arch; do
    for cpus in "${cpu_counts[@]}"; do
        run_name="${arch}-${cpus}cpu"
        commands="$NIGHTLY_LOG_DIR/$run_name.commands"
        run_dir="$NIGHTLY_LOG_DIR/$run_name"
        artifact="$run_dir/mm-performance.tsv"
        kernel_relative="$run_name/kernel"
        metrics_relative="$run_name/mm-performance.tsv"
        diagnostics_artifact="$run_dir/mm-lock-diagnostics.tsv"
        commands_relative="$run_name/commands"
        guest_inputs_relative="$run_name/guest-inputs.tsv"
        qemu_receipt_relative="$run_name/qemu-runner-receipt.json"
        qemu_log_relative="$run_name/qemu.log"
        host_pre_relative="$run_name/host-pre.tsv"
        host_post_relative="$run_name/host-post.tsv"
        selection_row=$(awk -F '\t' -v requested="$cpus" \
            'NR > 1 && $1 == requested { print; count += 1 } END { exit count != 1 }' \
            "$host_cpu_matrix") \
            || nightly_fail "missing unique host CPU selection for $cpus CPUs"
        IFS=$'\t' read -r selected_count host_cpu_set \
            host_cpu_selection host_cpu_class <<<"$selection_row"
        [ "$selected_count" = "$cpus" ] \
            || nightly_fail "host CPU selection count drift for $run_name"
        mm_perf_write_guest_commands \
            "$MM_PERF_MEASUREMENT_MODE" "$commands" \
            "$MM_PERF_ITERATIONS" "$MM_PERF_VMAS" \
            "$MM_PERF_PIN_ITERATIONS" "$cpus" \
            || nightly_fail "cannot materialize MM guest commands for $run_name"

        (
            taskset --pid --cpu-list "$host_cpu_set" "$BASHPID" >/dev/null \
                || nightly_fail "cannot apply host CPU affinity $host_cpu_set"
            mm_perf_capture_prepared_run \
                "$arch" "$cpus" "$commands" "$run_dir" \
                "$host_cpu_set" "$host_cpu_selection" "$host_cpu_class" \
                "$MM_PERF_SETTLE_SECS"
        )
        qemu_binary=$(nightly_qemu_binary "$arch") \
            || nightly_fail "invalid QEMU architecture after run: $arch"
        qemu=$(command -v "$qemu_binary") \
            || nightly_fail "QEMU binary disappeared after run: $qemu_binary"
        rootfs=$(nightly_rootfs_path "$arch") \
            || nightly_fail "invalid rootfs architecture after run: $arch"
        python3 "$CI_SCRIPT_DIR/validate-qemu-receipt.py" \
            --receipt "$run_dir/qemu-runner-receipt.json" \
            --arch "$arch" --cpus "$cpus" --kernel "$run_dir/kernel" \
            --rootfs "$rootfs" --rootfs-mode snapshot \
            --log "$run_dir/qemu.log" --qemu-binary "$qemu" \
            --commands "$run_dir/commands" >/dev/null \
            || nightly_fail \
                "invalid QEMU command-stream receipt for $run_name"
        nightly_validate_guest_log \
            "$run_dir/qemu.log" clean \
            'MM_PERF_SEMANTICS status=ok' \
            'MM_PERF_DONE status=ok'
        python3 "$CI_SCRIPT_DIR/parse-mm-performance.py" \
            "$run_dir/qemu.log" --arch "$arch" --cpus "$cpus" \
            --iterations "$MM_PERF_ITERATIONS" \
            --vmas "$MM_PERF_VMAS" \
            --pin-iterations "$MM_PERF_PIN_ITERATIONS" \
            --pin-workers "$cpus" \
            --output "$artifact"

        receipt_kernel_profile=$(mm_perf_receipt_value \
            "$run_dir/guest-inputs.tsv" kernel_profile) \
            || nightly_fail "missing prepared kernel profile for $run_name"
        [ "$receipt_kernel_profile" = "$MM_PERF_KERNEL_PROFILE" ] \
            || nightly_fail \
                "prepared kernel profile drift for $run_name: expected=$MM_PERF_KERNEL_PROFILE actual=$receipt_kernel_profile"
        case "$MM_PERF_MEASUREMENT_MODE" in
            product)
                if grep -Eq '^MM_LOCK_' "$run_dir/qemu.log"; then
                    nightly_fail \
                        "product MM run emitted lock diagnostics: $run_name"
                fi
                [ ! -e "$diagnostics_artifact" ] \
                    || nightly_fail \
                        "product MM run created a diagnostics artifact: $run_name"
                diagnostics_relative=$MM_PERF_DIAGNOSTIC_SENTINEL
                diagnostics_sha256=$MM_PERF_DIAGNOSTIC_SENTINEL
                diagnostics_size_bytes=$MM_PERF_DIAGNOSTIC_SENTINEL
                ;;
            diagnostic)
                diagnostics_relative="$run_name/mm-lock-diagnostics.tsv"
                python3 "$CI_SCRIPT_DIR/parse-mm-lock-diagnostics.py" \
                    "$run_dir/qemu.log" --output "$diagnostics_artifact"
                [ -s "$diagnostics_artifact" ] \
                    || nightly_fail \
                        "diagnostic MM run produced an empty lock artifact: $run_name"
                diagnostics_sha256=$(sha256sum "$diagnostics_artifact" | awk '{ print $1 }')
                diagnostics_size_bytes=$(stat -c '%s' "$diagnostics_artifact")
                ;;
        esac

        if [ "$first_artifact" -eq 1 ]; then
            cp "$artifact" "$matrix"
            first_artifact=0
        else
            tail -n +2 "$artifact" >>"$matrix"
        fi

        kernel_artifact="$run_dir/kernel"
        online_cpus=$(awk -F '\t' 'NR == 2 { print $3 }' "$artifact")
        [ "$online_cpus" = "$cpus" ] \
            || nightly_fail "parsed topology drift for $run_name: $online_cpus"
        kernel_sha256=$(sha256sum "$kernel_artifact" | awk '{ print $1 }')
        kernel_size_bytes=$(stat -c '%s' "$kernel_artifact")
        rootfs_sha256=$(mm_perf_receipt_value \
            "$run_dir/guest-inputs.tsv" rootfs_sha256) \
            || nightly_fail "missing prepared rootfs receipt for $run_name"
        qemu_sha256=$(sha256sum "$qemu" | awk '{ print $1 }')
        qemu_version=$("$qemu" --version | sed -n '1{s/[[:space:]]\+/ /g;p;}')
        qemu_version=${qemu_version//$'\t'/ }
        metrics_sha256=$(sha256sum "$artifact" | awk '{ print $1 }')
        metrics_size_bytes=$(stat -c '%s' "$artifact")
        commands_sha256=$(sha256sum "$run_dir/commands" | awk '{ print $1 }')
        commands_size_bytes=$(stat -c '%s' "$run_dir/commands")
        guest_inputs_sha256=$(sha256sum "$run_dir/guest-inputs.tsv" | awk '{ print $1 }')
        guest_inputs_size_bytes=$(stat -c '%s' "$run_dir/guest-inputs.tsv")
        qemu_receipt_sha256=$(sha256sum \
            "$run_dir/qemu-runner-receipt.json" | awk '{ print $1 }')
        qemu_receipt_size_bytes=$(stat -c '%s' \
            "$run_dir/qemu-runner-receipt.json")
        qemu_log_sha256=$(sha256sum "$run_dir/qemu.log" | awk '{ print $1 }')
        qemu_log_size_bytes=$(stat -c '%s' "$run_dir/qemu.log")
        host_pre_sha256=$(sha256sum "$run_dir/host-pre.tsv" | awk '{ print $1 }')
        host_pre_size_bytes=$(stat -c '%s' "$run_dir/host-pre.tsv")
        host_post_sha256=$(sha256sum "$run_dir/host-post.tsv" | awk '{ print $1 }')
        host_post_size_bytes=$(stat -c '%s' "$run_dir/host-post.tsv")
        manifest_row=(
            thekernel-mm-performance-bundle-v9
            "$repo_commit"
            "$ax_commit"
            "$linux_abi_commit"
            "$MM_PERF_MEASUREMENT_MODE"
            "$MM_PERF_KERNEL_PROFILE"
            "$arch"
            "$cpus"
            "$online_cpus"
            "$MM_PERF_ITERATIONS"
            "$MM_PERF_VMAS"
            "$MM_PERF_PIN_ITERATIONS"
            "$cpus"
            "$kernel_sha256"
            "$kernel_size_bytes"
            "$rootfs_sha256"
            "$qemu_binary"
            "$qemu_version"
            "$qemu_sha256"
            "$runner_fingerprint"
            "$runner_contract_sha256"
            "$host_cpu_set"
            "$host_cpu_selection"
            "$host_cpu_class"
            qemu-tcg
            none
            not-applicable
            not-applicable
            not-applicable
            "$kernel_relative"
            "$metrics_relative"
            "$metrics_sha256"
            "$metrics_size_bytes"
            "$diagnostics_relative"
            "$diagnostics_sha256"
            "$diagnostics_size_bytes"
            "$commands_relative"
            "$commands_sha256"
            "$commands_size_bytes"
            "$guest_inputs_relative"
            "$guest_inputs_sha256"
            "$guest_inputs_size_bytes"
            "$qemu_receipt_relative"
            "$qemu_receipt_sha256"
            "$qemu_receipt_size_bytes"
            "$qemu_log_relative"
            "$qemu_log_sha256"
            "$qemu_log_size_bytes"
            "$host_pre_relative"
            "$host_pre_sha256"
            "$host_pre_size_bytes"
            "$host_post_relative"
            "$host_post_sha256"
            "$host_post_size_bytes"
        )
        (IFS=$'\t'; printf '%s\n' "${manifest_row[*]}") >>"$manifest"
        run_count=$((run_count + 1))
    done
done <<<"$selected_arches"

missing_count=$(
    awk -F '\t' \
        'NR > 1 && $5 == "missing" { count += 1 } END { print count + 0 }' \
        "$matrix"
)
[ "$missing_count" -eq 0 ] \
    || nightly_fail "MM performance evidence contains $missing_count missing metrics"
metric_rows=$(awk 'END { print NR - 1 }' "$matrix")
manifest_rows=$(awk 'END { print NR - 1 }' "$manifest")
[ "$metric_rows" -eq $((run_count * 10)) ] \
    || nightly_fail "MM performance matrix row-count drift: $metric_rows"
[ "$manifest_rows" -eq "$run_count" ] \
    || nightly_fail "MM performance manifest row-count drift: $manifest_rows"

for state in \
    "$REPO_ROOT:$repo_commit:TheKernel" \
    "$ax_repo:$ax_commit:thekernel-ax" \
    "$linux_abi_repo:$linux_abi_commit:thekernel-linux-abi"; do
    IFS=: read -r dependency expected_commit label <<<"$state"
    [ "$(git -C "$dependency" rev-parse --verify HEAD)" = "$expected_commit" ] \
        || nightly_fail "$label HEAD changed during MM evidence capture"
    [ -z "$(git -C "$dependency" status --porcelain --untracked-files=all)" ] \
        || nightly_fail "$label worktree changed during MM evidence capture"
done

printf '%s\n' \
    "nightly MM performance evidence: COMPLETE mode=$MM_PERF_MEASUREMENT_MODE matrix=$matrix manifest=$manifest" \
    "explicit_missing=$missing_count host_cpu_matrix=$host_cpu_matrix"
