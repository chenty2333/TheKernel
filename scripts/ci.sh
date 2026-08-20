#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
CI_DIR="$ROOT/scripts/ci"
AX_REPO=${THEKERNEL_AX_REPO:-$ROOT/../thekernel-ax}
LINUX_ABI_REPO=${THEKERNEL_LINUX_ABI_REPO:-$ROOT/../thekernel-linux-abi}
VISA_REPO=${VISA_REPO:-$ROOT/../vISA}
CARGO_TARGET_DIR=${THEKERNEL_CI_TARGET_DIR:-$ROOT/.state/ci/cargo-target}
export CARGO_TARGET_DIR
export PYTHONDONTWRITEBYTECODE=1

usage() {
    cat <<'USAGE'
Usage: scripts/ci.sh COMMAND [ARGS...]

Commands:
  layout                 validate the required sibling checkout layout
  quick                  PR gate: format, provenance, tools, host check/test/lint
  kernel                 build/lint x86_64 and check non-default kernel profiles
  patches                test maintained patched/local mechanism crates
  all                    run quick, patches, and kernel
  system                 run the x86_64 QEMU semantic system test
  smoke NAME [ARGS...]   run one named semantic smoke
  differential CASE      run one host differential script

The default command is quick.
USAGE
}

step() {
    local name=$1
    shift
    printf '\n==> %s\n' "$name"
    "$@"
}

canonical_repo() {
    local label=$1
    local path=$2
    [ -f "$path/Cargo.toml" ] || {
        printf 'missing %s sibling workspace: %s\n' "$label" "$path" >&2
        exit 2
    }
    (cd -- "$path" && pwd -P)
}

check_layout() {
    [ -f "$ROOT/Cargo.toml" ] || {
        printf 'TheKernel Cargo.toml is missing from %s\n' "$ROOT" >&2
        exit 2
    }
    AX_REPO=$(canonical_repo thekernel-ax "$AX_REPO")
    LINUX_ABI_REPO=$(canonical_repo thekernel-linux-abi "$LINUX_ABI_REPO")
    VISA_REPO=$(canonical_repo vISA "$VISA_REPO")
    [ -f "$VISA_REPO/crates/visa-core/Cargo.toml" ] || {
        printf 'vISA checkout has no visa-core crate: %s\n' "$VISA_REPO" >&2
        exit 2
    }
    [ -f "$VISA_REPO/crates/visa-coordinator/Cargo.toml" ] || {
        printf 'vISA checkout has no visa-coordinator crate: %s\n' "$VISA_REPO" >&2
        exit 2
    }
    printf 'layout: PASS\n  TheKernel: %s\n  vISA: %s\n  thekernel-ax: %s\n  thekernel-linux-abi: %s\n' \
        "$ROOT" "$VISA_REPO" "$AX_REPO" "$LINUX_ABI_REPO"
}

diff_check() {
    cd "$ROOT"
    if [ -n "${CI_DIFF_BASE:-}" ] \
        && git cat-file -e "${CI_DIFF_BASE}^{commit}" 2>/dev/null; then
        git diff --check "$CI_DIFF_BASE" HEAD --
    elif git rev-parse --verify HEAD^ >/dev/null 2>&1; then
        git diff --check HEAD^ HEAD --
    fi
    git diff --check
    git diff --cached --check
}

host_env=(
    env
    CC=gcc
    CXX=g++
    AR=ar
    AS=as
    OBJCOPY=objcopy
    OBJDUMP=objdump
    SIZE=size
    "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
    "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS=-C link-arg=-T$ROOT/third_party/rust-patches/scope-local/percpu.x"
)

host_test_env=(
    "${host_env[@]}"
    "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=$CI_DIR/host-test-linker.sh"
)

quick() {
    check_layout
    cd "$ROOT"
    step 'diff whitespace' diff_check
    step 'rustfmt' cargo fmt --all -- --check
    step 'vendor provenance' python3 \
        "$CI_DIR/validate_vendor_provenance.py" \
        --archive-policy if-present \
        --ax-repo "$AX_REPO" \
        --linux-abi-repo "$LINUX_ABI_REPO"
    step 'build-tool tests' make test-tools
    step 'differential framework tests' \
        bash "$ROOT/tests/ci/test-differential-framework.sh"
    step 'syz differential prototype tests' \
        bash "$ROOT/tools/syz-differential/test_prototype.sh"
    step 'readiness adapter tests' \
        cargo test --locked -p thekernel-readiness-adapter
    step 'process adapter tests' \
        cargo test --locked -p thekernel-linux-process-adapter
    step 'kernel host check' \
        "${host_env[@]}" cargo check --locked \
        --manifest-path kernel/Cargo.toml \
        --tests --features bpf \
        --target x86_64-unknown-linux-gnu
    step 'kernel host tests' \
        "${host_test_env[@]}" cargo test --locked \
        --manifest-path kernel/Cargo.toml \
        --tests --features bpf,axtask/test \
        --target x86_64-unknown-linux-gnu
    step 'host clippy' "$CI_DIR/clippy-gate.sh" --profile host
}

kernel() {
    check_layout
    cd "$ROOT"
    step 'diagnostic feature profile' \
        "${host_env[@]}" cargo check --locked \
        --manifest-path kernel/Cargo.toml \
        --tests --features bpf,asid-switch-diagnostics,pmu-diagnostics \
        --target x86_64-unknown-linux-gnu
    step 'I/O test-control profile' \
        "${host_test_env[@]}" cargo test --locked \
        --manifest-path kernel/Cargo.toml \
        --tests --features bpf,test-io-control \
        --target x86_64-unknown-linux-gnu \
        pseudofs::io_test_control::tests -- --test-threads=1
    step 'x86_64 kernel build' make kernel-x86_64
    step 'x86_64 clippy' "$CI_DIR/clippy-gate.sh" --profile x86_64
}

patches() {
    check_layout
    cd "$ROOT"

    step 'vendored smoltcp UDP contracts' \
        env RUSTUP_TOOLCHAIN=1.85.0 \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/starry-smoltcp/Cargo.toml \
        socket::udp::test -- --test-threads=1
    step 'axhal IPI broker' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axhal/Cargo.toml \
        --features ipi -- --test-threads=1
    step 'axruntime IPI profile' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axruntime/Cargo.toml \
        --no-run --features ipi,multitask,smp
    step 'axfeat IPI profile' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axfeat/Cargo.toml \
        --no-run --features ipi,smp
    step 'axsync multitask contracts' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axsync/Cargo.toml \
        --features multitask -- --test-threads=1
    step 'memory-set contracts' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/memory_set/Cargo.toml
    step 'scope-local contracts' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/scope-local/Cargo.toml
    step 'axfs VFS contracts' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axfs-ng-vfs/Cargo.toml \
        --features spin/spin_mutex,spin/once
    step 'axfs ext4 xattr provider' \
        "${host_env[@]}" "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axfs-ng/Cargo.toml \
        --features ext4,std,test-ramdisk \
        fs::ext4::inode::tests::ext4_xattr_ -- --test-threads=1
    step 'axfs pathwalk policy' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axfs-ng/Cargo.toml \
        highlevel::fs::tests::pathwalk_ -- --test-threads=1
    step 'axfs final-component resolution' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axfs-ng/Cargo.toml \
        preserving_final_parent_resolution -- --test-threads=1
    step 'axfs file-open contracts' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axfs-ng/Cargo.toml \
        highlevel::file::tests -- --test-threads=1
    step 'axfs named-create admission' \
        "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/axfs-ng/Cargo.toml \
        highlevel::fs::tests::create_open -- --test-threads=1
    step 'lwext4 namespace/timestamp contracts' \
        "${host_env[@]}" "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/lwext4_rust/Cargo.toml \
        --no-default-features --features std \
        fs::tests -- --test-threads=1
    step 'lwext4 xattr persistence' \
        "${host_env[@]}" "$CI_DIR/focused-cargo-test.sh" \
        third_party/rust-patches/lwext4_rust/Cargo.toml \
        --no-default-features --features std \
        inode::xattr::tests -- --test-threads=1
    step 'axnet core contracts' \
        "${host_test_env[@]}" "$CI_DIR/focused-cargo-test.sh" \
        crates/axnet-ng/Cargo.toml \
        --features axtask/test -- --test-threads=1
    step 'axnet vsock contracts' \
        "${host_test_env[@]}" "$CI_DIR/focused-cargo-test.sh" \
        crates/axnet-ng/Cargo.toml \
        --features axtask/test,vsock -- --test-threads=1
}

command=${1:-quick}
if [ "$#" -gt 0 ]; then
    shift
fi

case "$command" in
    layout)
        [ "$#" -eq 0 ] || { usage >&2; exit 2; }
        check_layout
        ;;
    quick)
        [ "$#" -eq 0 ] || { usage >&2; exit 2; }
        quick
        ;;
    kernel)
        [ "$#" -eq 0 ] || { usage >&2; exit 2; }
        kernel
        ;;
    patches)
        [ "$#" -eq 0 ] || { usage >&2; exit 2; }
        patches
        ;;
    all)
        [ "$#" -eq 0 ] || { usage >&2; exit 2; }
        quick
        patches
        kernel
        ;;
    system)
        [ "$#" -eq 0 ] || { usage >&2; exit 2; }
        check_layout
        cd "$ROOT"
        step 'x86_64 semantic system test' make system-test
        ;;
    smoke)
        [ "$#" -ge 1 ] || { usage >&2; exit 2; }
        check_layout
        cd "$ROOT"
        exec "$ROOT/scripts/smoke.sh" "$@"
        ;;
    differential)
        [ "$#" -eq 1 ] || { usage >&2; exit 2; }
        case_name=$1
        script="$CI_DIR/${case_name}-host-differential.sh"
        [ -x "$script" ] || {
            printf 'unknown or non-executable differential case: %s\n' "$case_name" >&2
            exit 2
        }
        cd "$ROOT"
        exec "$script"
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
