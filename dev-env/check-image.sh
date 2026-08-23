#!/usr/bin/env bash
set -euo pipefail

expect_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'missing required command: %s\n' "$1" >&2
        exit 1
    }
}

QEMU_EXPECTED_VERSION="${THEKERNEL_QEMU_VERSION:-9.2.1}"

for cmd in \
    python3 \
    ip unshare \
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

qemu-system-x86_64 --version | grep -q "version ${QEMU_EXPECTED_VERSION}"

for ovmf in \
    /usr/share/edk2/ovmf/OVMF_CODE.fd \
    /usr/share/edk2/ovmf/OVMF_CODE_4M.fd \
    /usr/share/OVMF/OVMF_CODE.fd
do
    if [[ -r "$ovmf" ]]; then
        break
    fi
done
[[ -r "$ovmf" ]] || { printf 'missing OVMF code image\n' >&2; exit 1; }

x86_64-linux-gnu-gcc -print-sysroot >/dev/null

printf 'TheKernel development image check: ok\n'
