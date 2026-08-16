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
RUST_TOOLCHAIN_EXPECTED="${RUSTUP_TOOLCHAIN:-nightly}"

for cmd in \
    cargo rustc rustup python3 \
    cargo-axplat axconfig-gen rust-objcopy rust-objdump \
    qemu-system-x86_64 \
    mke2fs debugfs fakeroot truncate mkfs.vfat mkimage \
    parted mcopy mmd \
    x86_64-linux-gnu-gcc x86_64-linux-gnu-objcopy
do
    expect_cmd "$cmd"
done

# GRUB tools ship under different names depending on the distro.
command -v grub-file >/dev/null 2>&1 || command -v grub2-file >/dev/null 2>&1 \
    || { printf 'missing grub-file/grub2-file\n' >&2; exit 1; }
command -v grub-mkstandalone >/dev/null 2>&1 \
    || command -v grub2-mkstandalone >/dev/null 2>&1 \
    || { printf 'missing grub-mkstandalone/grub2-mkstandalone\n' >&2; exit 1; }

cargo axplat --version >/dev/null
axconfig-gen --version >/dev/null

qemu-system-x86_64 --version | grep -q "version ${QEMU_EXPECTED_VERSION}"

rustup show active-toolchain | grep -q "^${RUST_TOOLCHAIN_EXPECTED}"

expect_target x86_64-unknown-none

x86_64-linux-gnu-gcc -print-sysroot >/dev/null

printf 'TheKernel development image check: ok\n'
