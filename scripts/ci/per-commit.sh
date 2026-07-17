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
export THEKERNEL_AX_REPO="$AX_REPO"
export THEKERNEL_LINUX_ABI_REPO="$LINUX_ABI_REPO"
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
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-T$REPO_ROOT/third_party/rust-patches/scope-local/percpu.x"
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
    "$SCRIPT_DIR/validate_vendor_provenance.py" --archive-policy if-present \
    --ax-repo "$AX_REPO" --linux-abi-repo "$LINUX_ABI_REPO"
ci_run_step ci-script-tests 90 \
    env THEKERNEL_AX_REPO="$AX_REPO" \
    THEKERNEL_LINUX_ABI_REPO="$LINUX_ABI_REPO" \
    "$REPO_ROOT/tests/ci/test-ci-scripts.sh"
ci_run_step tool-tests "$STEP_TIMEOUT_SECS" make test-tools

ci_run_step kernel-host-check "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" cargo check --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu

ci_run_step kernel-futex-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf,axtask/test --target x86_64-unknown-linux-gnu \
    task::futex::tests -- --test-threads=1

ci_run_step kernel-mm-transaction-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    mm:: -- --test-threads=1

ci_run_step kernel-io-test-control-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf,test-io-control --target x86_64-unknown-linux-gnu \
    pseudofs::io_test_control::tests -- --test-threads=1

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

ci_run_step kernel-thread-credential-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    task::thread_cred::tests -- --test-threads=1

ci_run_step kernel-composite-credential-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    task::creds::tests -- --test-threads=1

ci_run_step kernel-exec-credential-algebra-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    task::exec_cred::tests -- --test-threads=1

ci_run_step kernel-security-hook-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    task::security::tests -- --test-threads=1

ci_run_step kernel-clone-security-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::task::clone::tests -- --test-threads=1

ci_run_step kernel-named-create-transaction-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    file::namespace_mutation::tests -- --test-threads=1

ci_run_step kernel-vfs-permission-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    file::permission::tests -- --test-threads=1

ci_run_step kernel-file-open-security-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::fs::fd_ops::namespace_operation_tests -- --test-threads=1

ci_run_step kernel-opath-capability-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    opath -- --test-threads=1

ci_run_step kernel-open-description-status-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    status_snapshot_is_authoritative_without_an_axfs_append_mirror \
    -- --test-threads=1

ci_run_step kernel-file-lease-open-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    file::lease::tests -- --test-threads=1

ci_run_step kernel-pipe-status-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    file::pipe::tests -- --test-threads=1

ci_run_step kernel-mandatory-lock-range-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    file::flock::tests::mandatory_ -- --test-threads=1

ci_run_step kernel-write-transfer-status-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::fs::io::tests -- --test-threads=1

ci_run_step kernel-mmap-file-status-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::mm::mmap::tests -- --test-threads=1

ci_run_step kernel-socket-security-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::net:: -- --test-threads=1

ci_run_step kernel-signal-syscall-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::signal::tests -- --test-threads=1

ci_run_step kernel-pidfd-signal-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::fs::pidfd::tests -- --test-threads=1

ci_run_step kernel-group-leader-signal-identity-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    task::process::tests::group_leader_ -- --test-threads=1

ci_run_step kernel-sigchld-autoreap-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    task::ops::tests::sigchld_ -- --test-threads=1

ci_run_step kernel-group-exit-signal-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    task::ops::tests::group_exit_signals_only_peer_threads -- --test-threads=1

ci_run_step kernel-credential-caller-test-discovery "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    bash -c '
        set -euo pipefail
        output=$(cargo test --locked --manifest-path kernel/Cargo.toml \
            --tests --features bpf --target x86_64-unknown-linux-gnu \
            credential_caller_ -- --list)
        printf "%s\n" "$output"
        count=$(printf "%s\n" "$output" | awk "/: test$/ { count++ } END { print count + 0 }")
        if [ "$count" -lt 22 ]; then
            printf "credential-caller discovery: expected at least 22 tests, found %s\n" "$count" >&2
            exit 1
        fi
    '

ci_run_step kernel-credential-caller-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    credential_caller_ -- --test-threads=1

ci_run_step kernel-executable-lease-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    file::executable::tests -- --test-threads=1

ci_run_step kernel-exec-loader-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    mm::loader::tests -- --test-threads=1

ci_run_step kernel-exec-transition-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::task::execve::tests -- --test-threads=1

ci_run_step kernel-exec-orchestration-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    task::process::tests::leader_and_nonleader_exec_orchestration_ -- --test-threads=1

ci_run_step kernel-file-capability-xattr-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::fs::xattr::tests -- --test-threads=1

ci_run_step kernel-xattr-provider-policy-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    file::xattr_provider::tests -- --test-threads=1

ci_run_step kernel-credential-metadata-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::fs::ctl::tests -- --test-threads=1

ci_run_step kernel-file-write-killpriv-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    file::fs::tests -- --test-threads=1

ci_run_step kernel-memfd-seal-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    file::memfd::tests -- --test-threads=1

ci_run_step kernel-tmpfs-killpriv-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    pseudofs::tmp::tests -- --test-threads=1

ci_run_step kernel-file-mapping-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    mm::aspace::backend::file::tests -- --test-threads=1

ci_run_step kernel-mapping-lease-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    mm::aspace::mapping::tests -- --test-threads=1

ci_run_step kernel-mapping-backend-contract-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    mm::aspace::backend::tests -- --test-threads=1

ci_run_step kernel-task-parent-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    task::thread::task_parent_tests -- --test-threads=1

ci_run_step kernel-ptrace-action-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::task::ptrace::tests -- --test-threads=1

ci_run_step kernel-ptrace-wait-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::task::wait::tests -- --test-threads=1

ci_run_step kernel-setfsid-abi-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    syscall::sys::tests::invalid_or_unmapped_setfsid_returns_old_without_calling_writer \
    -- --exact --test-threads=1

ci_run_step kernel-namespace-owner-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    namespace_owner_ -- --test-threads=1

ci_run_step kernel-process-access-test-discovery "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    bash -c '
        set -euo pipefail
        output=$(cargo test --locked --manifest-path kernel/Cargo.toml \
            --tests --features bpf --target x86_64-unknown-linux-gnu \
            process_access_ -- --list)
        printf "%s\n" "$output"
        count=$(printf "%s\n" "$output" | awk "/: test$/ { count++ } END { print count + 0 }")
        if [ "$count" -lt 25 ]; then
            printf "process-access discovery: expected at least 25 tests, found %s\n" "$count" >&2
            exit 1
        fi
    '

ci_run_step kernel-process-access-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    cargo test --locked --manifest-path kernel/Cargo.toml \
    --tests --features bpf --target x86_64-unknown-linux-gnu \
    process_access_ -- --test-threads=1

ci_run_step axsched-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$AX_REPO/Cargo.toml" \
    -p thekernel-axsched
ci_run_step axpoll-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$AX_REPO/Cargo.toml" \
    -p thekernel-axpoll
ci_run_step axtlb-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo +1.85.0 test --locked --manifest-path "$AX_REPO/Cargo.toml" \
    -p thekernel-axtlb --all-targets
ci_run_step axtask-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$AX_REPO/Cargo.toml" \
    -p thekernel-axtask --no-default-features \
    --features multitask,sched-cfs,test
ci_run_step credential-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$LINUX_ABI_REPO/Cargo.toml" \
    -p thekernel-linux-cred
ci_run_step credential-core-check "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo check --locked --manifest-path "$LINUX_ABI_REPO/Cargo.toml" \
    -p thekernel-linux-cred --no-default-features
ci_run_step mm-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$LINUX_ABI_REPO/Cargo.toml" \
    -p thekernel-linux-mm
ci_run_step mm-core-check "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo check --locked --manifest-path "$LINUX_ABI_REPO/Cargo.toml" \
    -p thekernel-linux-mm --no-default-features
ci_run_step io-uring-core-tests "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo test --locked --manifest-path "$LINUX_ABI_REPO/Cargo.toml" \
    -p thekernel-linux-io-uring
ci_run_step io-uring-core-check "$STEP_TIMEOUT_SECS" \
    env CARGO_TARGET_DIR="$SIBLING_TARGET_DIR" \
    cargo check --locked --manifest-path "$LINUX_ABI_REPO/Cargo.toml" \
    -p thekernel-linux-io-uring --no-default-features
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
ci_run_step axsync-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/axsync/Cargo.toml \
    --features multitask -- --test-threads=1
ci_run_step memory-set-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/memory_set/Cargo.toml
ci_run_step scope-local-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/scope-local/Cargo.toml
ci_run_step axfs-vfs-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/axfs-ng-vfs/Cargo.toml \
    --features spin/spin_mutex,spin/once
ci_run_step axfs-ext4-xattr-provider-tests "$STEP_TIMEOUT_SECS" \
	"${host_tool_env[@]}" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/axfs-ng/Cargo.toml \
	--features ext4,std,test-ramdisk \
    fs::ext4::inode::tests::ext4_xattr_ -- --test-threads=1
ci_run_step axfs-pathwalk-policy-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/axfs-ng/Cargo.toml \
    highlevel::fs::tests::pathwalk_ -- --test-threads=1
ci_run_step axfs-final-component-resolution-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/axfs-ng/Cargo.toml \
    preserving_final_parent_resolution -- --test-threads=1
ci_run_step axfs-file-open-contract-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/axfs-ng/Cargo.toml \
    highlevel::file::tests -- --test-threads=1
ci_run_step axfs-named-create-admission-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/axfs-ng/Cargo.toml \
    highlevel::fs::tests::create_open -- --test-threads=1
ci_run_step lwext4-namespace-timestamp-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/lwext4_rust/Cargo.toml \
    --no-default-features --features std fs::tests -- --test-threads=1
ci_run_step lwext4-xattr-persistence-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/lwext4_rust/Cargo.toml \
    --no-default-features --features std inode::xattr::tests -- --test-threads=1
ci_run_step axnet-core-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    "$SCRIPT_DIR/focused-cargo-test.sh" crates/axnet-ng/Cargo.toml \
    --features axtask/test -- --test-threads=1
ci_run_step axnet-vsock-tests "$STEP_TIMEOUT_SECS" \
    "${host_tool_env[@]}" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$SCRIPT_DIR/host-test-linker.sh" \
    "$SCRIPT_DIR/focused-cargo-test.sh" crates/axnet-ng/Cargo.toml \
    --features axtask/test,vsock -- --test-threads=1
ci_run_step signal-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/starry-signal/Cargo.toml \
    -- --test-threads=1
ci_run_step usercopy-tests "$STEP_TIMEOUT_SECS" \
    "$SCRIPT_DIR/focused-cargo-test.sh" third_party/rust-patches/starry-vm/Cargo.toml

printf 'per-commit gate: PASS\n'
printf 'logs: %s\n' "$CI_LOG_DIR"
