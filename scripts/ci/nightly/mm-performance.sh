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
MM_PERF_BASELINE_BUNDLE=${THEKERNEL_MM_PERF_BASELINE_BUNDLE:-}
MM_PERF_POLICY=${THEKERNEL_MM_PERF_POLICY:-$SCRIPT_DIR/mm-performance-regression-policy.json}
MM_PERF_REQUIRE_BASELINE=${THEKERNEL_MM_PERF_REQUIRE_BASELINE:-0}

case "$MM_PERF_REQUIRE_BASELINE" in
    1|y|Y|yes|YES|true|TRUE|on|ON) MM_PERF_REQUIRE_BASELINE=1 ;;
    0|n|N|no|NO|false|FALSE|off|OFF) MM_PERF_REQUIRE_BASELINE=0 ;;
    *) nightly_fail \
        "THEKERNEL_MM_PERF_REQUIRE_BASELINE must be a boolean: $MM_PERF_REQUIRE_BASELINE" ;;
esac
[ "$MM_PERF_REQUIRE_BASELINE" -eq 0 ] || [ -n "$MM_PERF_BASELINE_BUNDLE" ] \
    || nightly_fail \
        'THEKERNEL_MM_PERF_REQUIRE_BASELINE is set but no baseline bundle was provided'

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
regression_report="$NIGHTLY_LOG_DIR/mm-performance-regression.tsv"
rm -f "$matrix"
rm -f "$manifest"
rm -f "$regression_report"
selected_arches=$(nightly_selected_arches) || exit $?
first_artifact=1

runner_contract_sha256=$(
    {
        printf '%s\n' \
            scripts/ci/boot-shell-runner.sh \
            scripts/ci/nightly/mm-performance.sh \
            scripts/ci/mm_performance_schema.py \
            scripts/ci/parse-mm-performance.py \
            tests/guest/tools/mm-performance.c
        find "$REPO_ROOT/tools/qemu_runner" -maxdepth 1 -type f -name '*.py' \
            -printf 'tools/qemu_runner/%f\n'
    } | sort -u | while IFS= read -r relative; do
        [ -f "$REPO_ROOT/$relative" ] \
            || nightly_fail "runner contract input is missing: $relative"
        printf '%s\t%s\n' "$relative" \
            "$(sha256sum "$REPO_ROOT/$relative" | awk '{ print $1 }')"
    done | sha256sum | awk '{ print $1 }'
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

if [ -n "$MM_PERF_BASELINE_BUNDLE" ]; then
    [ -d "$MM_PERF_BASELINE_BUNDLE" ] \
        || nightly_fail "MM baseline bundle is not a directory: $MM_PERF_BASELINE_BUNDLE"
    [ -f "$MM_PERF_POLICY" ] \
        || nightly_fail "MM regression policy is missing: $MM_PERF_POLICY"
    baseline_root=$(cd -- "$MM_PERF_BASELINE_BUNDLE" && pwd -P)
    candidate_root=$(cd -- "$NIGHTLY_LOG_DIR" && pwd -P)
    [ "$baseline_root" != "$candidate_root" ] \
        || nightly_fail 'MM baseline bundle and candidate output directory must differ'
    baseline_validation=$(mktemp "$NIGHTLY_LOG_DIR/.mm-performance-baseline.XXXXXX")
    set +e
    python3 "$CI_SCRIPT_DIR/compare-mm-performance.py" \
        --baseline "$baseline_root" \
        --candidate "$baseline_root" \
        --policy "$MM_PERF_POLICY" \
        --output "$baseline_validation" >/dev/null
    baseline_status=$?
    set -e
    rm -f "$baseline_validation"
    case "$baseline_status" in
        0|1) ;;
        *) nightly_fail 'MM baseline bundle or regression policy is invalid' ;;
    esac
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
    kernel_artifact
    metrics_artifact
    metrics_sha256
    metrics_size_bytes
    commands
    commands_sha256
    commands_size_bytes
    qemu_log
    qemu_log_sha256
    qemu_log_size_bytes
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
        commands_relative="$run_name.commands"
        qemu_log_relative="$run_name/qemu.log"

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
            # The helper binary is part of the evidence contract. Re-enter the
            # content-addressed rootfs builder instead of trusting a materialized
            # image left by an older checkout.
            export THEKERNEL_NIGHTLY_REBUILD_ROOTFS=1
            nightly_run_guest "$arch" "$commands" "$run_dir"
        )
        nightly_validate_guest_log \
            "$run_dir/qemu.log" clean \
            'MM_PERF_SEMANTICS status=ok' \
            'MM_PERF_DONE status=ok'
        python3 "$CI_SCRIPT_DIR/parse-mm-performance.py" \
            "$run_dir/qemu.log" --arch "$arch" --cpus "$cpus" \
            --iterations "$MM_PERF_ITERATIONS" \
            --pin-iterations "$MM_PERF_PIN_ITERATIONS" \
            --pin-workers "$cpus" \
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
        qemu_binary=$(nightly_qemu_binary "$arch") \
            || nightly_fail "invalid QEMU architecture after run: $arch"
        qemu=$(command -v "$qemu_binary") \
            || nightly_fail "QEMU binary disappeared after run: $qemu_binary"
        kernel_artifact="$run_dir/kernel"
        cp -- "$kernel" "$kernel_artifact"
        online_cpus=$(awk -F '\t' 'NR == 2 { print $3 }' "$artifact")
        [ "$online_cpus" = "$cpus" ] \
            || nightly_fail "parsed topology drift for $run_name: $online_cpus"
        kernel_sha256=$(sha256sum "$kernel_artifact" | awk '{ print $1 }')
        kernel_size_bytes=$(stat -c '%s' "$kernel_artifact")
        rootfs_sha256=$(sha256sum "$rootfs" | awk '{ print $1 }')
        qemu_sha256=$(sha256sum "$qemu" | awk '{ print $1 }')
        qemu_version=$("$qemu" --version | sed -n '1{s/[[:space:]]\+/ /g;p;}')
        qemu_version=${qemu_version//$'\t'/ }
        metrics_sha256=$(sha256sum "$artifact" | awk '{ print $1 }')
        metrics_size_bytes=$(stat -c '%s' "$artifact")
        commands_sha256=$(sha256sum "$commands" | awk '{ print $1 }')
        commands_size_bytes=$(stat -c '%s' "$commands")
        qemu_log_sha256=$(sha256sum "$run_dir/qemu.log" | awk '{ print $1 }')
        qemu_log_size_bytes=$(stat -c '%s' "$run_dir/qemu.log")
        manifest_row=(
            thekernel-mm-performance-bundle-v2
            "$repo_commit"
            "$ax_commit"
            "$linux_abi_commit"
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
            "$kernel_relative"
            "$metrics_relative"
            "$metrics_sha256"
            "$metrics_size_bytes"
            "$commands_relative"
            "$commands_sha256"
            "$commands_size_bytes"
            "$qemu_log_relative"
            "$qemu_log_sha256"
            "$qemu_log_size_bytes"
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
[ "$metric_rows" -eq $((run_count * 5)) ] \
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

regression_result=not-compared
regression_report_display=-
if [ -n "$MM_PERF_BASELINE_BUNDLE" ]; then
    set +e
    python3 "$CI_SCRIPT_DIR/compare-mm-performance.py" \
        --baseline "$baseline_root" \
        --candidate "$NIGHTLY_LOG_DIR" \
        --policy "$MM_PERF_POLICY" \
        --output "$regression_report"
    regression_status=$?
    set -e
    case "$regression_status" in
        0)
            regression_result=pass
            regression_report_display=$regression_report
            ;;
        1) nightly_fail \
            "MM performance regression exceeded policy; report=$regression_report" ;;
        *) nightly_fail 'MM performance baseline and candidate are not comparable' ;;
    esac
fi
printf '%s\n' \
    "nightly MM performance evidence: COMPLETE matrix=$matrix manifest=$manifest" \
    "explicit_missing=$missing_count regression=$regression_result report=$regression_report_display"
