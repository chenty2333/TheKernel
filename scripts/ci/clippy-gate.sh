#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

usage() {
    cat <<'EOF'
Usage: scripts/ci/clippy-gate.sh --profile host|x86_64 [--target-dir DIR]

Runs clippy over TheKernel-owned packages and fails on any remaining owned
diagnostic. Vendored sources under third_party/rust-patches and the maintained
sibling workspaces are built as ordinary dependencies: they keep their upstream
lint posture and their own gates. Their diagnostics are still counted and
printed so that a clean owned report never implies a clean tree.

Profiles:
  host  x86_64 host test build, including `--tests`. Answers lints about test
        code and about generic paths the architecture builds share.
  x86_64 x86_64-unknown-none kernel build.

Configuration-sensitive lints report different facts per profile: a symbol
unreachable in the host test build is often the live architecture path, and
`GlobalGrace` only carries drop glue when `smp-tlb-shootdown` is enabled. The
profiles are therefore complementary; neither substitutes for the other.

The lint policy itself lives in `[workspace.lints]` in the root Cargo.toml so
editors and local `cargo clippy` see exactly what this gate enforces. This
script adds no lint levels of its own.
EOF
}

PROFILE=
TARGET_DIR=

while (($#)); do
    case "$1" in
        --profile)
            PROFILE=${2:-}
            shift 2
            ;;
        --target-dir)
            TARGET_DIR=${2:-}
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            ci_die "unknown clippy-gate argument: $1"
            ;;
    esac
done

case "$PROFILE" in
    host|x86_64) ;;
    '') ci_die 'clippy-gate requires --profile host|x86_64' ;;
    *) ci_die "unknown clippy profile: $PROFILE" ;;
esac

ci_require_command cargo
ci_require_command python3
cargo clippy --version >/dev/null 2>&1 \
    || ci_die 'cargo clippy is not available in this toolchain'

cd "$REPO_ROOT"

# TheKernel-owned packages. Everything else in the graph is either vendored
# upstream source or a maintained sibling with its own gate, and is built as an
# ordinary dependency without the clippy driver.
OWNED_PACKAGES=(
    thekernel
    thekernel-kernel
    axnet-ng
    axtask
    thekernel-linux-process-adapter
    thekernel-readiness-adapter
)

package_args=()
for package in "${OWNED_PACKAGES[@]}"; do
    package_args+=(-p "$package")
done

report() {
    python3 "$SCRIPT_DIR/clippy-report.py" --profile "$PROFILE"
}

if [ "$PROFILE" = host ]; then
    # The host profile mirrors the `kernel-host-check` step: same manifest,
    # same features, same percpu link script, plus test targets. Package
    # selection is implicit here because the kernel manifest is the entry
    # point rather than the workspace root.
    TARGET_DIR=${TARGET_DIR:-$REPO_ROOT/target/ci-clippy-host}
    mkdir -p -- "$TARGET_DIR" || ci_die "cannot create clippy target dir: $TARGET_DIR"
    set +e
    env \
        CC=gcc CXX=g++ AR=ar AS=as OBJCOPY=objcopy OBJDUMP=objdump SIZE=size \
        CARGO_TARGET_DIR="$TARGET_DIR" \
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-T$REPO_ROOT/third_party/rust-patches/scope-local/percpu.x" \
        cargo clippy --locked --manifest-path kernel/Cargo.toml \
        --tests --features bpf --target x86_64-unknown-linux-gnu \
        --message-format=json | report
    # `pipefail` keeps a resolver/tool invocation failure from disappearing
    # behind an otherwise clean (possibly empty) diagnostic report.
    status=$?
    set -e
    exit "$status"
fi

# Architecture profiles reuse the real kernel build machinery so platform
# config, feature resolution, and RUSTFLAGS match the shipped image exactly.
# A dedicated target directory keeps clippy from invalidating the build cache:
# cargo fingerprints clippy-driver and rustc outputs into the same slots.
arch=x86_64
extra_make_args=(PLAT_CONFIG=platforms/axplat-x86-q35-uefi.toml)

TARGET_DIR=${TARGET_DIR:-$REPO_ROOT/.state/$arch/clippy-target}
mkdir -p -- "$TARGET_DIR" || ci_die "cannot create clippy target dir: $TARGET_DIR"

# Keep these in sync with KERNEL_ENV in tools/build.py. The kernel the gate
# lints must be the kernel the gate builds.
kernel_env=(
    DEBUGINFO=y
    DWARF=n
    LOG=off
    BANNER=n
    BACKTRACE=n
    NO_AXSTD=y
    AX_LIB=axfeat
    BLK=y
    NET=y
    VSOCK=n
    MEM=1G
    LTO=
    MODE=release
)

make_args=(
    -C "$REPO_ROOT/make"
    "A=$REPO_ROOT"
    "ARCH=$arch"
    "${extra_make_args[@]}"
    "${kernel_env[@]}"
    APP_FEATURES=qemu
    "TARGET_DIR=$TARGET_DIR"
)

export PYTHONDONTWRITEBYTECODE=1

# `defconfig` materializes the platform configuration `clippy-elf` reads.
make "${make_args[@]}" defconfig >/dev/null

set +e
make "${make_args[@]}" \
    "CLIPPY_PACKAGES=${package_args[*]}" \
    CLIPPY_ARGS="--message-format=json" \
    clippy-elf | report
# See the host pipeline above: both the producer and the report are gates.
status=$?
set -e
exit "$status"
