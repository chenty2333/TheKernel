#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

LOG_DIR="$REPO_ROOT/.state/ci/per-commit"
STEP_TIMEOUT_SECS=${THEKERNEL_CI_STEP_TIMEOUT_SECS:-900}
AX_REPO=${THEKERNEL_AX_REPO:-$REPO_ROOT/../thekernel-ax}
LINUX_ABI_REPO=${THEKERNEL_LINUX_ABI_REPO:-$REPO_ROOT/../thekernel-linux-abi}

usage() {
    cat <<'EOF'
Usage: scripts/ci/per-commit.sh [--log-dir DIR] [--step-timeout SECS]

Runs the reusable per-commit gate: diff whitespace checks, rustfmt, vendored
source provenance validation, project tool tests, a host kernel check, and
focused tests for the maintained ax/Linux-ABI cores, local adapters, fallible
lifecycle, VFS, signal, and user-copy contracts that currently change most
often. Broader network, filesystem, and cross-architecture behavior belongs to
the PR and nightly gates.

CI_DIFF_BASE may name a merge base. When absent, the committed HEAD^..HEAD
diff is checked in addition to staged and unstaged changes.
EOF
}

while (($#)); do
    case "$1" in
        --log-dir)
            LOG_DIR=${2:-}
            shift 2
            ;;
        --step-timeout)
            STEP_TIMEOUT_SECS=${2:-}
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            ci_die "unknown per-commit argument: $1"
            ;;
    esac
done

ci_require_positive_int step_timeout "$STEP_TIMEOUT_SECS"
case "$LOG_DIR" in
    /*) ;;
    *) LOG_DIR="$REPO_ROOT/$LOG_DIR" ;;
esac

cd "$REPO_ROOT"
export CI_LOG_DIR="$LOG_DIR"
ci_prepare_log_dir "$CI_LOG_DIR"

canonical_workspace() {
    local label=$1
    local path=$2
    [ -f "$path/Cargo.toml" ] \
        || ci_die "$label workspace is missing: $path"
    (cd -- "$path" && pwd -P)
}

AX_REPO=$(canonical_workspace thekernel-ax "$AX_REPO")
LINUX_ABI_REPO=$(canonical_workspace thekernel-linux-abi "$LINUX_ABI_REPO")
SIBLING_TARGET_DIR="$REPO_ROOT/target/ci-maintained-siblings"

# lwext4's host build script probes generic tool names. Pin those names instead
# of inheriting cross-compiler variables from a developer shell or CI runner.
host_tool_env=(
    env
    CC=gcc
    CXX=g++
    AR=ar
    AS=as
    OBJCOPY=objcopy
    OBJDUMP=objdump
    SIZE=size
)

ci_run_step diff-check 60 bash -c '
    set -euo pipefail
    if [ -n "${CI_DIFF_BASE:-}" ] && git cat-file -e "${CI_DIFF_BASE}^{commit}" 2>/dev/null; then
        git diff --check "${CI_DIFF_BASE}" HEAD --
    elif git rev-parse --verify HEAD^ >/dev/null 2>&1; then
        git diff --check HEAD^ HEAD --
    fi
    git diff --check
    git diff --cached --check
'

ci_run_step rustfmt 180 cargo fmt --all -- --check
ci_run_step vendor-provenance 60 python3 \
    "$SCRIPT_DIR/validate_vendor_provenance.py" --archive-policy if-present
ci_run_step ci-script-tests 90 "$REPO_ROOT/tests/ci/test-ci-scripts.sh"
ci_run_step tool-tests "$STEP_TIMEOUT_SECS" make test-tools

ci_run_step kernel-host-check "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" cargo check --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu

ci_run_step kernel-raw-sigevent-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::usercopy::tests -- --test-threads=1

ci_run_step kernel-sigevent-signo-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::time::tests::sigevent_signo_does_not_wrap_before_validation \
    -- --test-threads=1

ci_run_step axsched-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$AX_REPO/Cargo.toml" \
    -p thekernel-axsched
ci_run_step axpoll-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$AX_REPO/Cargo.toml" \
    -p thekernel-axpoll
ci_run_step axtask-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$AX_REPO/Cargo.toml" \
    -p thekernel-axtask --no-default-features \
    --features multitask,sched-cfs,test
ci_run_step process-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$LINUX_ABI_REPO/Cargo.toml" \
    -p thekernel-linux-process
ci_run_step linux-vfs-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$LINUX_ABI_REPO/Cargo.toml" \
    -p thekernel-linux-vfs
ci_run_step linux-fd-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$LINUX_ABI_REPO/Cargo.toml" \
    -p thekernel-linux-fd
ci_run_step readiness-adapter-tests "$STEP_TIMEOUT_SECS" \
    cargo test --locked -p thekernel-readiness-adapter
ci_run_step process-adapter-tests "$STEP_TIMEOUT_SECS" \
    cargo test --locked -p thekernel-linux-process-adapter
ci_run_step scope-local-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/scope-local/Cargo.toml
ci_run_step axfs-vfs-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/axfs-ng-vfs/Cargo.toml \
    --features spin/spin_mutex,spin/once
ci_run_step axfs-pathwalk-policy-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/axfs-ng/Cargo.toml \
    pathwalk_policy_receives_real_components_and_final_position -- --test-threads=1
ci_run_step signal-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/starry-signal/Cargo.toml \
    -- --test-threads=1
ci_run_step usercopy-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/starry-vm/Cargo.toml

printf 'per-commit gate: PASS\n'
printf 'logs: %s\n' "$CI_LOG_DIR"
