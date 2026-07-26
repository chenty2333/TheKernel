#!/usr/bin/env bash
set -euo pipefail

expect_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'missing required command: %s\n' "$1" >&2
        exit 1
    }
}

expect_target() {
    rustup target list --installed | grep -qx "$1" || {
        printf 'missing rust target: %s\n' "$1" >&2
        exit 1
    }
}

QEMU_EXPECTED_VERSION="${THEKERNEL_QEMU_VERSION:-9.2.1}"
RUST_TOOLCHAIN_EXPECTED="${RUSTUP_TOOLCHAIN:-nightly-2026-06-07}"

for cmd in \
    cargo rustc rustup python3 \
    cargo-axplat axconfig-gen rust-objcopy rust-objdump \
    qemu-system-riscv64 qemu-system-loongarch64 \
    mke2fs debugfs fakeroot truncate mkfs.vfat mkimage \
    riscv64-linux-gnu-gcc riscv64-linux-gnu-objcopy \
    riscv64-linux-musl-gcc riscv64-linux-musl-objcopy \
    loongarch64-linux-musl-gcc loongarch64-linux-musl-objcopy
do
    expect_cmd "$cmd"
done

cargo axplat --version >/dev/null
axconfig-gen --version >/dev/null

qemu-system-riscv64 --version | grep -q "version ${QEMU_EXPECTED_VERSION}"
qemu-system-loongarch64 --version | grep -q "version ${QEMU_EXPECTED_VERSION}"

rustup show active-toolchain | grep -q "^${RUST_TOOLCHAIN_EXPECTED}"

expect_target x86_64-unknown-none
expect_target riscv64gc-unknown-none-elf
expect_target aarch64-unknown-none-softfloat
expect_target loongarch64-unknown-none-softfloat

riscv64-linux-musl-gcc -print-sysroot >/dev/null
loongarch64-linux-musl-gcc -print-sysroot >/dev/null

printf 'TheKernel development image check: ok\n'
