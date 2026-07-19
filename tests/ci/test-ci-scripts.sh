#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
CI_DIR="$REPO_ROOT/scripts/ci"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

while IFS= read -r script; do
    bash -n "$script"
done < <(find "$CI_DIR" -type f -name '*.sh' -print | sort)
for script in "$REPO_ROOT"/tests/guest/nightly/*; do
    sh -n "$script"
done
bash -n "$0"

python3 "$REPO_ROOT/tests/ci/test_vendor_provenance.py"
python3 "$REPO_ROOT/tests/ci/test_mm_performance_parser.py"
"$SCRIPT_DIR/test-release-consumer-gate.sh"

# The developer container must see maintained sibling checkouts at the exact
# absolute paths produced by Cargo's ../thekernel-* patch dependencies. Keep
# this test Docker-free by capturing the final compose invocation.
dev_fixture="$tmp/dev-fixture"
mkdir -p \
    "$dev_fixture/scripts" \
    "$dev_fixture/dev-env" \
    "$dev_fixture/fake-bin" \
    "$tmp/dev-ax" \
    "$tmp/dev-linux-abi-primary" \
    "$tmp/dev-rootfs"
cp "$REPO_ROOT/scripts/dev-shell.sh" "$dev_fixture/scripts/"
: >"$dev_fixture/dev-env/versions.env"
: >"$dev_fixture/dev-env/compose.yaml"
: >"$tmp/dev-ax/Cargo.toml"
git -C "$dev_fixture" init --quiet
: >"$tmp/dev-linux-abi-primary/Cargo.toml"
git -C "$tmp/dev-linux-abi-primary" init --quiet
git -C "$tmp/dev-linux-abi-primary" add Cargo.toml
git -C "$tmp/dev-linux-abi-primary" \
    -c user.name=CI -c user.email=ci@example.invalid \
    commit --quiet -m fixture
git -C "$tmp/dev-linux-abi-primary" worktree add --quiet --detach \
    "$tmp/dev-linux-abi" HEAD
# The linked checkout must carry the package manifest expected by dev-shell.
[ -f "$tmp/dev-linux-abi/Cargo.toml" ]
cat >"$dev_fixture/fake-bin/docker" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$DEV_SHELL_DOCKER_ARGS"
EOF
chmod +x "$dev_fixture/fake-bin/docker"
env \
    PATH="$dev_fixture/fake-bin:$PATH" \
    DEV_SHELL_DOCKER_ARGS="$tmp/dev-shell.args" \
    THEKERNEL_ROOTFS_HOST_DIR="$tmp/dev-rootfs" \
    THEKERNEL_AX_REPO="$tmp/dev-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/dev-linux-abi" \
    "$dev_fixture/scripts/dev-shell.sh" -- make kernel-rv
grep -Fxq -- "$tmp/dev-ax:/thekernel-ax:ro,z" "$tmp/dev-shell.args"
grep -Fxq -- "$tmp/dev-linux-abi:/thekernel-linux-abi:ro,z" \
    "$tmp/dev-shell.args"
grep -Fxq -- \
    "$tmp/dev-linux-abi-primary/.git:$tmp/dev-linux-abi-primary/.git:ro,z" \
    "$tmp/dev-shell.args"
grep -Fxq -- 'make' "$tmp/dev-shell.args"
grep -Fxq -- 'kernel-rv' "$tmp/dev-shell.args"
if env \
    PATH="$dev_fixture/fake-bin:$PATH" \
    DEV_SHELL_DOCKER_ARGS="$tmp/dev-shell-missing.args" \
    THEKERNEL_ROOTFS_HOST_DIR="$tmp/dev-rootfs" \
    THEKERNEL_AX_REPO="$tmp/missing-dev-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/dev-linux-abi" \
    "$dev_fixture/scripts/dev-shell.sh" -- true >/dev/null 2>&1; then
    printf 'test-ci-scripts: dev shell accepted a missing maintained sibling\n' >&2
    exit 1
fi
[ ! -e "$tmp/dev-shell-missing.args" ]

# Hosted CI must consume exact, retrievable sibling commits without mutating an
# arbitrary pre-existing checkout. Local repositories keep this deterministic
# and exercise the same fetch-by-commit path as the hosted jobs.
make_sibling_fixture() {
    local path=$1
    local package=$2
    mkdir -p "$path/src"
    git -C "$path" init --quiet
    printf '[package]\nname = "%s"\nversion = "0.1.0"\n' "$package" \
        >"$path/Cargo.toml"
    printf '#![no_std]\n' >"$path/src/lib.rs"
    git -C "$path" add Cargo.toml src/lib.rs
    git -C "$path" -c user.name=CI -c user.email=ci@example.invalid \
        commit --quiet -m fixture
    git -C "$path" rev-parse HEAD
}

ax_fixture_head=$(make_sibling_fixture "$tmp/source-ax" thekernel-ax-fixture)
linux_abi_fixture_head=$(
    make_sibling_fixture "$tmp/source-linux-abi" thekernel-linux-abi-fixture
)
provision_env=(
    THEKERNEL_AX_REPOSITORY="$tmp/source-ax"
    THEKERNEL_AX_REF="$ax_fixture_head"
    THEKERNEL_LINUX_ABI_REPOSITORY="$tmp/source-linux-abi"
    THEKERNEL_LINUX_ABI_REF="$linux_abi_fixture_head"
    THEKERNEL_AX_REPO="$tmp/checkouts/thekernel-ax"
    THEKERNEL_LINUX_ABI_REPO="$tmp/checkouts/thekernel-linux-abi"
)
env "${provision_env[@]}" \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null
env "${provision_env[@]}" \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null
[ "$(git -C "$tmp/checkouts/thekernel-ax" rev-parse HEAD)" = "$ax_fixture_head" ]
[ "$(git -C "$tmp/checkouts/thekernel-linux-abi" rev-parse HEAD)" = \
    "$linux_abi_fixture_head" ]
[ -z "$(git -C "$tmp/checkouts/thekernel-ax" status --porcelain=v1)" ]

if env "${provision_env[@]}" THEKERNEL_AX_REF=main \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null 2>&1; then
    printf 'test-ci-scripts: non-commit sibling ref was accepted\n' >&2
    exit 1
fi
mkdir -p "$tmp/unmanaged-linux-abi"
if env "${provision_env[@]}" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/unmanaged-linux-abi" \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null 2>&1; then
    printf 'test-ci-scripts: unmanaged sibling checkout was changed\n' >&2
    exit 1
fi
if THEKERNEL_AX_REPOSITORY= THEKERNEL_AX_REF="$ax_fixture_head" \
    THEKERNEL_LINUX_ABI_REPOSITORY="$tmp/source-linux-abi" \
    THEKERNEL_LINUX_ABI_REF="$linux_abi_fixture_head" \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null 2>&1; then
    printf 'test-ci-scripts: missing sibling repository was accepted\n' >&2
    exit 1
fi

# The PR source-build branch must audit exact sibling release artifacts before
# invoking make, preserve the release set under its normal artifact log tree,
# and skip both operations when --skip-build is requested.
pr_fixture="$tmp/pr-fixture"
mkdir -p "$pr_fixture/scripts/ci" "$pr_fixture/fake-bin"
cp "$CI_DIR/pr-gate.sh" "$CI_DIR/lib.sh" "$pr_fixture/scripts/ci/"
cat >"$pr_fixture/scripts/ci/release-consumer-gate.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'release-consumer %s\n' "$*" >>"$PR_FIXTURE_TRACE"
output=
while (($#)); do
    case "$1" in
        --output-release-set) output=${2:-}; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$output" ]
mkdir -p "$(dirname -- "$output")"
printf 'package\tversion\tsha256\trepository_head\n' >"$output"
EOF
cat >"$pr_fixture/scripts/ci/boot-shell-gate.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'boot %s\n' "$*" >>"$PR_FIXTURE_TRACE"
EOF
cat >"$pr_fixture/scripts/system-test.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'system %s\n' "$*" >>"$PR_FIXTURE_TRACE"
EOF
cat >"$pr_fixture/fake-bin/make" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'make %s\n' "$*" >>"$PR_FIXTURE_TRACE"
case "$*" in
    kernels) printf fixture >kernel-rv; printf fixture >kernel-la ;;
    'kernel-rv-shell kernel-la-shell rootfs')
        mkdir -p .state/shell .state/rootfs
        printf fixture >.state/shell/kernel-rv
        printf fixture >.state/shell/kernel-la
        printf fixture >.state/rootfs/rootfs-rv.img
        printf fixture >.state/rootfs/rootfs-la.img
        ;;
esac
EOF
chmod +x \
    "$pr_fixture/scripts/ci/release-consumer-gate.sh" \
    "$pr_fixture/scripts/ci/boot-shell-gate.sh" \
    "$pr_fixture/scripts/system-test.sh" \
    "$pr_fixture/fake-bin/make"

pr_trace="$tmp/pr-gate.trace"
ax_exact=1111111111111111111111111111111111111111
linux_abi_exact=2222222222222222222222222222222222222222
env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REF="$ax_exact" \
    THEKERNEL_LINUX_ABI_REF="$linux_abi_exact" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --log-dir "$tmp/pr-gate-logs" >/dev/null
grep -Fxq \
    "release-consumer --arch both --ax-head $ax_exact --linux-abi-head $linux_abi_exact --output-release-set $tmp/pr-gate-logs/release-consumer/release-set.tsv" \
    "$pr_trace"
[ "$(sed -n '1p' "$pr_trace")" = \
    "release-consumer --arch both --ax-head $ax_exact --linux-abi-head $linux_abi_exact --output-release-set $tmp/pr-gate-logs/release-consumer/release-set.tsv" ]
[ "$(sed -n '2p' "$pr_trace")" = 'make kernels' ]
[ "$(sed -n '3p' "$pr_trace")" = 'make kernel-rv-shell kernel-la-shell rootfs' ]
grep -Fq 'boot --arch both --skip-build' "$pr_trace"
grep -Fq \
    "system --arch rv --skip-build --timeout 300 --workdir $tmp/pr-gate-logs/system/rv" \
    "$pr_trace"
grep -Fq \
    "system --arch la --skip-build --timeout 300 --workdir $tmp/pr-gate-logs/system/la" \
    "$pr_trace"
[ -s "$tmp/pr-gate-logs/release-consumer/release-set.tsv" ]
grep -q $'^release-consumer\tpass\t0\t' "$tmp/pr-gate-logs/status.tsv"

: >"$pr_trace"
if env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REF=main \
    THEKERNEL_LINUX_ABI_REF="$linux_abi_exact" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --log-dir "$tmp/pr-gate-invalid-ref-logs" >/dev/null 2>&1; then
    printf 'test-ci-scripts: PR gate accepted a non-exact sibling ref\n' >&2
    exit 1
fi
[ ! -s "$pr_trace" ]

env -u THEKERNEL_AX_REF -u THEKERNEL_LINUX_ABI_REF \
    PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-gate-skip-logs" >/dev/null
[ "$(wc -l <"$pr_trace")" -eq 3 ]
grep -Fq 'boot --arch both --skip-build' "$pr_trace"
grep -Fq 'system --arch rv --skip-build' "$pr_trace"
grep -Fq 'system --arch la --skip-build' "$pr_trace"
if grep -Eq '^(release-consumer|make) ' "$pr_trace"; then
    printf 'test-ci-scripts: --skip-build ran release or source build\n' >&2
    exit 1
fi

# The semantic system gate accepts the runner's intentional-stop status only
# after the exact final marker is written, then validates the complete marker
# sequence from the captured console log.
system_fixture="$tmp/system-fixture"
mkdir -p \
    "$system_fixture/scripts" \
    "$system_fixture/fake-bin" \
    "$system_fixture/.state/rootfs"
cp "$REPO_ROOT/scripts/system-test.sh" "$system_fixture/scripts/"
printf fixture >"$system_fixture/kernel-rv"
printf fixture >"$system_fixture/.state/rootfs/rootfs-rv.img"
cat >"$system_fixture/fake-bin/python3" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$@" >"$FAKE_SYSTEM_RUNNER_ARGS"
workdir=
while (($#)); do
    case "$1" in
        --workdir) workdir=${2:-}; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$workdir" ]
mkdir -p "$workdir"
cat >"$workdir/console.log" <<'MARKERS'
THEKERNEL_SYSTEM_TEST_INIT_EXEC_1_OK
THEKERNEL_SYSTEM_TEST_INIT_EXEC_2_OK
THEKERNEL_SYSTEM_TEST_START
THEKERNEL_SYSTEM_TEST_MOUNTS_OK
THEKERNEL_SYSTEM_TEST_ROOTFS_OK
THEKERNEL_SYSTEM_TEST_TMPFS_OK
THEKERNEL_SYSTEM_TEST_PROCFS_OK
THEKERNEL_SYSTEM_TEST_PROCESS_OK
THEKERNEL_EXEC_SMOKE_OK
THEKERNEL_SYSTEM_TEST_EXEC_OK
CI_SIGNAL_WAIT_BOUNDARY_PASS
THEKERNEL_SYSTEM_TEST_SIGNAL_WAIT_OK
CI_WAIT_BOUNDARY_CLOCK_PERCPU_OK online_cpus=1
CI_WAIT_BOUNDARY_TIMERFD_CANCEL_OK
CI_WAIT_BOUNDARY_ITIMER_PERIODIC_OK min_hits=3
CI_WAIT_BOUNDARY_ITIMER_CPU_OK no_syscall_loop=1
CI_WAIT_BOUNDARY_RLIMIT_CPU_ESCALATION_OK soft_after_signal=2 hard_signal=SIGKILL
CI_WAIT_BOUNDARY_RLIMIT_CPU_HARD_ONLY_OK signal=SIGKILL sigxcpu=0
CI_WAIT_BOUNDARY_PRLIMIT_PRECEDENCE_OK bad_new=EFAULT bad_pid_before_resource=ESRCH
CI_WAIT_BOUNDARY_PRLIMIT_TRANSACTION_OK old_new=atomic invalid=rollback copyout_fault=committed
CI_WAIT_BOUNDARY_LEGACY_PRECEDENCE_OK setrlimit_bad_new=EFAULT setitimer_bad_new=EFAULT
CI_WAIT_BOUNDARY_FUTEX_WAKE_OK
CI_WAIT_BOUNDARY_FUTEX_TIMEOUT_OK
CI_WAIT_BOUNDARY_FUTEX_WAITV_OK
CI_WAIT_BOUNDARY_PASS
THEKERNEL_SYSTEM_TEST_WAIT_BOUNDARY_OK
THEKERNEL_IO_URING_OK
THEKERNEL_SYSTEM_TEST_IO_URING_OK
THEKERNEL_SYSTEM_TEST_PASS
MARKERS
exit "${FAKE_SYSTEM_RUNNER_STATUS:-0}"
EOF
chmod +x "$system_fixture/fake-bin/python3"
system_args="$tmp/system-runner.args"
env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$system_args" \
    FAKE_SYSTEM_RUNNER_STATUS=75 \
    "$system_fixture/scripts/system-test.sh" \
    --arch rv --skip-build --workdir "$tmp/system-run" >/dev/null
grep -Fxq -- '--stop-after-marker' "$system_args"
grep -Fxq -- 'THEKERNEL_SYSTEM_TEST_PASS' "$system_args"
set +e
env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$system_args" \
    FAKE_SYSTEM_RUNNER_STATUS=23 \
    "$system_fixture/scripts/system-test.sh" \
    --arch rv --skip-build --workdir "$tmp/system-run-fail" >/dev/null
status=$?
set -e
[ "$status" -eq 23 ] || {
    printf 'test-ci-scripts: system gate returned %s, expected 23\n' "$status" >&2
    exit 1
}

cat >"$tmp/pass.log" <<'EOF'
CI_BOOT_GATE_START
CI_BOOT_GATE_ROOTFS_OK
CI_BOOT_GATE_TMPFS_OK
CI_BOOT_GATE_PROCFS_OK
CI_BOOT_GATE_BIND_OK
CI_BOOT_GATE_PASS
System is shutting down
EOF
"$CI_DIR/validate-boot-log.sh" rv "$tmp/pass.log" >/dev/null

cp "$tmp/pass.log" "$tmp/fail.log"
printf 'CI_BOOT_GATE_FAIL injected\n' >>"$tmp/fail.log"
if "$CI_DIR/validate-boot-log.sh" la "$tmp/fail.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: failure marker was accepted\n' >&2
    exit 1
fi

# Nightly guest validation must reject an explicit subsystem failure even if
# every expected success marker is also present later in the same log.
cat >"$tmp/nightly-wait-fail.log" <<'EOF'
CI_WAIT_BOUNDARY_FAIL injected errno=5 (Input/output error)
CI_WAIT_BOUNDARY_PASS
System is shutting down
EOF
if (
    # shellcheck source=../../scripts/ci/nightly/lib.sh
    source "$CI_DIR/nightly/lib.sh"
    nightly_validate_guest_log \
        "$tmp/nightly-wait-fail.log" clean CI_WAIT_BOUNDARY_PASS
) >/dev/null 2>&1; then
    printf 'test-ci-scripts: nightly validator accepted a wait-boundary failure marker\n' >&2
    exit 1
fi

cp "$tmp/pass.log" "$tmp/idle-timeout.log"
printf 'qemu-runner: QEMU idle timeout after 30s without console output\n' \
    >>"$tmp/idle-timeout.log"
if "$CI_DIR/validate-boot-log.sh" rv "$tmp/idle-timeout.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: QEMU idle timeout was accepted\n' >&2
    exit 1
fi

cp "$tmp/pass.log" "$tmp/missing.log"
sed -i '/CI_BOOT_GATE_BIND_OK/d' "$tmp/missing.log"
if "$CI_DIR/validate-boot-log.sh" rv "$tmp/missing.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: missing marker was accepted\n' >&2
    exit 1
fi

for category in pressure oom-failpoint fs-powercut nonloopback-network \
    smp-tlb-shootdown mm-performance; do
    "$CI_DIR/nightly-gate.sh" --list | grep -q "^${category}"
done

# The host-test linker must add -no-pie only to executables. Shared objects
# include proc macros and would fail to link if the C runtime expected main.
cat >"$tmp/fake-cc" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$FAKE_CC_LOG"
EOF
chmod +x "$tmp/fake-cc"
FAKE_CC_LOG="$tmp/shared-link.args" THEKERNEL_HOST_CC="$tmp/fake-cc" \
    "$CI_DIR/host-test-linker.sh" -shared input.o -o output.so
if grep -qx -- -no-pie "$tmp/shared-link.args"; then
    printf 'test-ci-scripts: shared link received -no-pie\n' >&2
    exit 1
fi
FAKE_CC_LOG="$tmp/executable-link.args" THEKERNEL_HOST_CC="$tmp/fake-cc" \
    "$CI_DIR/host-test-linker.sh" input.o -o output
grep -qx -- -no-pie "$tmp/executable-link.args"

env \
    THEKERNEL_NIGHTLY_PRESSURE_ENABLED=0 \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_ENABLED=0 \
    THEKERNEL_NIGHTLY_MM_PERFORMANCE_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly" >/dev/null

[ "$(awk -F '\t' '$2 == "skip" { count += 1 } END { print count + 0 }' "$tmp/nightly/nightly-status.tsv")" -eq 6 ]

# Configured adapters retain the same three-state contract as repository
# adapters. In particular, exit 78 must remain unsupported and make the whole
# gate return 78; it must never be rewritten to pass.
set +e
env \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND='exit 78' \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_ENABLED=0 \
    THEKERNEL_NIGHTLY_MM_PERFORMANCE_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly-unsupported" >/dev/null
status=$?
set -e
[ "$status" -eq 78 ] || {
    printf 'test-ci-scripts: unsupported adapter returned %s, expected 78\n' "$status" >&2
    exit 1
}
grep -q $'^pressure\tunsupported\t' "$tmp/nightly-unsupported/nightly-status.tsv"

set +e
env \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND='exit 9' \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_ENABLED=0 \
    THEKERNEL_NIGHTLY_MM_PERFORMANCE_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly-fail" >/dev/null
status=$?
set -e
[ "$status" -eq 1 ] || {
    printf 'test-ci-scripts: failed adapter returned %s, expected 1\n' "$status" >&2
    exit 1
}
grep -q $'^pressure\tfail\t' "$tmp/nightly-fail/nightly-status.tsv"

env \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND='exit 0' \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_ENABLED=0 \
    THEKERNEL_NIGHTLY_MM_PERFORMANCE_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly-pass" >/dev/null
grep -q $'^pressure\tpass\t' "$tmp/nightly-pass/nightly-status.tsv"

set +e
env \
    THEKERNEL_NIGHTLY_ARCHES=invalid \
    "$CI_DIR/nightly/pressure.sh" >"$tmp/invalid-arch.log" 2>&1
status=$?
set -e
[ "$status" -eq 1 ] || {
    printf 'test-ci-scripts: invalid adapter architecture returned %s, expected 1\n' "$status" >&2
    exit 1
}
grep -Fq 'THEKERNEL_NIGHTLY_ARCHES must be rv, la, or both' "$tmp/invalid-arch.log"
if grep -Fq 'UNSUPPORTED' "$tmp/invalid-arch.log"; then
    printf 'test-ci-scripts: invalid adapter architecture was misclassified as unsupported\n' >&2
    exit 1
fi

if THEKERNEL_SMP_TLB_CPUS=1 \
    "$CI_DIR/nightly/smp-tlb-shootdown.sh" \
    >"$tmp/invalid-smp-tlb-cpus.log" 2>&1; then
    printf 'test-ci-scripts: SMP TLB adapter accepted a one-CPU matrix\n' >&2
    exit 1
fi
grep -Fq 'THEKERNEL_SMP_TLB_CPUS must contain values from 2 to 64' \
    "$tmp/invalid-smp-tlb-cpus.log"

# Verify the shared runner records and enforces a real wall-clock timeout.
# shellcheck source=../../scripts/ci/lib.sh
source "$CI_DIR/lib.sh"
export CI_LOG_DIR="$tmp/runner"
ci_prepare_log_dir "$CI_LOG_DIR"
ci_run_step quick-pass 5 bash -c 'printf ok' >/dev/null
if ci_run_step must-timeout 1 bash -c 'sleep 5' >/dev/null 2>&1; then
    printf 'test-ci-scripts: timeout step unexpectedly passed\n' >&2
    exit 1
else
    status=$?
    [ "$status" -eq 124 ] || {
        printf 'test-ci-scripts: timeout returned %s, expected 124\n' "$status" >&2
        exit 1
    }
fi
grep -q $'^must-timeout\ttimeout\t124\t' "$CI_LOG_DIR/status.tsv"

# A guest can close the serial pipe before the throttled producer finishes.
# The runner must preserve the QEMU status and leave panic/missing-marker
# classification to validate-boot-log.sh instead of surfacing SIGPIPE 141.
mkdir -p "$tmp/fake-bin" "$tmp/fake-work"
cat >"$tmp/fake-bin/python3" <<'EOF'
#!/usr/bin/env bash
[ -z "${FAKE_QEMU_RUNNER_ARGS:-}" ] || printf '%s\n' "$@" >"$FAKE_QEMU_RUNNER_ARGS"
exit "${FAKE_QEMU_RUNNER_STATUS:-0}"
EOF
chmod +x "$tmp/fake-bin/python3"
for _ in $(seq 1 20000); do
    printf 'echo serial-input-padding-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n'
done >"$tmp/commands"
env PATH="$tmp/fake-bin:$PATH" FAKE_QEMU_RUNNER_STATUS=0 \
    "$CI_DIR/boot-shell-runner.sh" rv /dev/null /dev/null \
    "$tmp/fake-work" "$tmp/commands" 1 1 0
if env PATH="$tmp/fake-bin:$PATH" FAKE_QEMU_RUNNER_STATUS=23 \
    "$CI_DIR/boot-shell-runner.sh" rv /dev/null /dev/null \
    "$tmp/fake-work" "$tmp/commands" 1 1 0; then
    printf 'test-ci-scripts: QEMU runner failure was hidden by pipe handling\n' >&2
    exit 1
else
    status=$?
    [ "$status" -eq 23 ] || {
        printf 'test-ci-scripts: QEMU runner failure returned %s, expected 23\n' "$status" >&2
        exit 1
    }
fi

printf 'exit\n' >"$tmp/short-commands"
env PATH="$tmp/fake-bin:$PATH" FAKE_QEMU_RUNNER_STATUS=75 \
    FAKE_QEMU_RUNNER_ARGS="$tmp/qemu-runner.args" \
    THEKERNEL_QEMU_CPUS=8 \
    "$CI_DIR/boot-shell-runner.sh" rv kernel image "$tmp/fake-work" \
    "$tmp/short-commands" 1 1 0 extra.img STOP_MARKER || status=$?
[ "${status:-75}" -eq 75 ]
grep -Fxq -- '--input-after-marker' "$tmp/qemu-runner.args"
grep -Fxq -- 'THEKERNEL_SHELL_READY' "$tmp/qemu-runner.args"
awk '
    $0 == "--cpus" {
        getline
        found = ($0 == "8")
    }
    END { exit !found }
' "$tmp/qemu-runner.args"
awk '
    $0 == "--ready-timeout" {
        getline
        found = ($0 == "1")
    }
    END { exit !found }
' "$tmp/qemu-runner.args"
grep -Fxq -- '--extra-block' "$tmp/qemu-runner.args"
grep -Fxq -- 'extra.img' "$tmp/qemu-runner.args"
grep -Fxq -- '--stop-after-marker' "$tmp/qemu-runner.args"
grep -Fxq -- 'STOP_MARKER' "$tmp/qemu-runner.args"
if env PATH="$tmp/fake-bin:$PATH" THEKERNEL_QEMU_CPUS=0 \
    "$CI_DIR/boot-shell-runner.sh" rv kernel image "$tmp/fake-work" \
    "$tmp/short-commands" 1 1 0 >/dev/null 2>&1; then
    printf '%s\n' 'test-ci-scripts: runner accepted zero QEMU CPUs' >&2
    exit 1
fi

# The one-shot host peer rejects unauthenticated traffic and returns the nonce
# only after receiving the exact guest probe.
peer_port_file="$tmp/peer.port"
python3 "$CI_DIR/nightly/network-peer.py" \
    --nonce ci-unit --port-file "$peer_port_file" --timeout 10 \
    >"$tmp/peer.log" 2>&1 &
peer_pid=$!
for ((attempt = 0; attempt < 100; attempt += 1)); do
    [ -s "$peer_port_file" ] && break
    sleep 0.01
done
[ -s "$peer_port_file" ]
python3 - "$(tr -d '\r\n' <"$peer_port_file")" <<'PY'
import socket
import sys

with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5) as connection:
    connection.sendall(b"THEKERNEL_NETWORK_PROBE ci-unit\n")
    connection.shutdown(socket.SHUT_WR)
    assert connection.recv(4096) == b"THEKERNEL_NETWORK_REPLY ci-unit\n"
PY
wait "$peer_pid"
grep -Fq 'network-peer: validated guest request' "$tmp/peer.log"

# Compile the project guest helpers on the host. The guest OOM gate supplies the
# strict overcommit policy that makes its finite request fail deterministically.
cc -O2 -std=c11 -Wall -Wextra -Werror -pthread \
    "$REPO_ROOT/tests/guest/tools/mm-performance.c" \
    -o "$tmp/mm-performance"
cc -O2 -std=c11 -Wall -Wextra -Werror -pthread \
    "$REPO_ROOT/tests/guest/tools/smp-tlb-shootdown.c" \
    -o "$tmp/smp-tlb-shootdown"
cc -O2 -std=c11 -Wall -Wextra -Werror -pthread \
    "$REPO_ROOT/tests/guest/tools/wait-boundary.c" \
    -o "$tmp/wait-boundary"
cc -O2 -std=c11 "$REPO_ROOT/tests/guest/tools/oom-admission.c" \
    -o "$tmp/nightly-oom-admission"
"$tmp/nightly-oom-admission" --expect-success 4096 >/dev/null
"$tmp/nightly-oom-admission" --expect-failure 18446744073709551615 >/dev/null

# The SMP TLB parser requires the complete per-CPU, per-page-size case matrix
# and rejects a stale result even when a success marker remains in the log.
smp_tlb_log="$tmp/smp-tlb.log"
printf '%s\n' \
    'SMP_TLB_TOPOLOGY online_cpus=2 control_cpu=0 worker_count=1 worker_cpus=1' \
    >"$smp_tlb_log"
for window in 1 2 3; do
    printf 'SMP_TLB_LIVENESS window=%s window_ns=1000000000 cpus=2 tasks_per_cpu=2 status=ok min_delta=1\n' \
        "$window" >>"$smp_tlb_log"
done
for pages in 1 64; do
    for case_name in mprotect_revoke_write munmap_fixed_replace \
        mremap_fixed_old_alias fork_cow_snapshot; do
        printf 'SMP_TLB_CASE case=%s pages=%s worker_cpu=1 status=ok stale_count=0\n' \
            "$case_name" "$pages" >>"$smp_tlb_log"
    done
done
printf '%s\n' 'SMP_TLB_GATE status=ok stale_count=0' >>"$smp_tlb_log"
"$CI_DIR/validate-smp-tlb-log.sh" "$smp_tlb_log" 2 >/dev/null
sed \
    -e 's/control_cpu=0/control_cpu=2/' \
    -e 's/worker_cpus=1/worker_cpus=3/' \
    -e 's/worker_cpu=1/worker_cpu=3/g' \
    "$smp_tlb_log" >"$tmp/smp-tlb-out-of-range-topology.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-out-of-range-topology.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted out-of-range CPU IDs\n' >&2
    exit 1
fi
sed 's/worker_cpus=1/worker_cpus=1,/' \
    "$smp_tlb_log" >"$tmp/smp-tlb-trailing-worker.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-trailing-worker.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted a trailing worker token\n' >&2
    exit 1
fi
sed '0,/^SMP_TLB_CASE /{/^SMP_TLB_CASE /s/status=ok/status=stale/;}' \
    "$smp_tlb_log" >"$tmp/smp-tlb-stale.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-stale.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted a stale case\n' >&2
    exit 1
fi
sed '0,/^SMP_TLB_CASE /{/^SMP_TLB_CASE /d;}' \
    "$smp_tlb_log" >"$tmp/smp-tlb-incomplete.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-incomplete.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted an incomplete matrix\n' >&2
    exit 1
fi
sed '0,/^SMP_TLB_LIVENESS /{/^SMP_TLB_LIVENESS /d;}' \
    "$smp_tlb_log" >"$tmp/smp-tlb-incomplete-liveness.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-incomplete-liveness.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted incomplete liveness evidence\n' >&2
    exit 1
fi
sed '0,/min_delta=1/{s/min_delta=1/min_delta=0/;}' \
    "$smp_tlb_log" >"$tmp/smp-tlb-stalled-liveness.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-stalled-liveness.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted a stalled spin task\n' >&2
    exit 1
fi
sed \
    -e '0,/status=ok stale_count=0/{s/status=ok stale_count=0/status=stale stale_count=1/;}' \
    -e 's/^SMP_TLB_GATE status=ok stale_count=0$/SMP_TLB_GATE status=fail kind=stale stale_count=1/' \
    "$smp_tlb_log" >"$tmp/smp-tlb-mutation.log"
"$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-mutation.log" 2 stale >/dev/null
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$smp_tlb_log" 2 stale >/dev/null 2>&1; then
    printf 'test-ci-scripts: mutation parser accepted a clean positive log\n' >&2
    exit 1
fi
sed 's/kind=stale stale_count=1/kind=stale stale_count=2/' \
    "$tmp/smp-tlb-mutation.log" >"$tmp/smp-tlb-mutation-count-drift.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-mutation-count-drift.log" 2 stale >/dev/null 2>&1; then
    printf 'test-ci-scripts: mutation parser accepted stale-count drift\n' >&2
    exit 1
fi
sed 's/kind=stale stale_count=1/kind=operational stale_count=1/' \
    "$tmp/smp-tlb-mutation.log" >"$tmp/smp-tlb-mutation-operational.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-mutation-operational.log" 2 stale >/dev/null 2>&1; then
    printf 'test-ci-scripts: mutation parser accepted an operational failure\n' >&2
    exit 1
fi

printf 'test-ci-scripts: PASS\n'
