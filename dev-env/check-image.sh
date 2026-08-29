#!/usr/bin/env bash
set -euo pipefail

expect_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'missing required command: %s\n' "$1" >&2
        exit 1
    }
}

QEMU_EXPECTED_VERSION="${THEKERNEL_QEMU_VERSION:-9.2.1}"
Q35_OVMF_CODE_SHA256=d9b568def24088c92f34b5479e0ed7e44d0a4d4cea8a0f5716719180bba48106
Q35_OVMF_VARS_SHA256=6ed987af3a3c155be71665f510eae3e007eda9b8b94afd59d45e91c4a11565cc

for cmd in \
    bc bison flex \
    python3 \
    ip unshare \
    qemu-system-x86_64 \
    mke2fs debugfs fakeroot truncate mkfs.vfat mkimage \
    parted mcopy mmd \
    x86_64-linux-gnu-gcc x86_64-linux-gnu-objcopy
do
    expect_cmd "$cmd"
done

# Building the pinned Linux ABI oracles also requires ELF development
# headers for objtool; command checks alone cannot detect that package.
[[ -r /usr/include/libelf.h && -r /usr/include/gelf.h ]] \
    || { printf 'missing libelf development headers\n' >&2; exit 1; }
[[ -r /usr/include/openssl/bio.h ]] \
    || { printf 'missing OpenSSL development headers\n' >&2; exit 1; }

# GRUB tools ship under different names depending on the distro.
command -v grub-file >/dev/null 2>&1 || command -v grub2-file >/dev/null 2>&1 \
    || { printf 'missing grub-file/grub2-file\n' >&2; exit 1; }
command -v grub-mkstandalone >/dev/null 2>&1 \
    || command -v grub2-mkstandalone >/dev/null 2>&1 \
    || { printf 'missing grub-mkstandalone/grub2-mkstandalone\n' >&2; exit 1; }

qemu-system-x86_64 --version | grep -q "version ${QEMU_EXPECTED_VERSION}"

ovmf_code=
for candidate in \
    /usr/share/edk2/ovmf/OVMF_CODE.fd \
    /usr/share/edk2/ovmf/OVMF_CODE_4M.fd \
    /usr/share/OVMF/OVMF_CODE.fd
do
    if [[ -r "$candidate" ]] &&
        [[ $(sha256sum "$candidate" | awk '{print $1}') == "$Q35_OVMF_CODE_SHA256" ]]; then
        ovmf_code=$candidate
        break
    fi
done
[[ -n "$ovmf_code" ]] || {
    printf 'missing pinned OVMF code image (%s)\n' "$Q35_OVMF_CODE_SHA256" >&2
    exit 1
}

ovmf_vars=
for candidate in \
    /usr/share/edk2/ovmf/OVMF_VARS.fd \
    /usr/share/edk2/ovmf/OVMF_VARS_4M.fd \
    /usr/share/OVMF/OVMF_VARS.fd
do
    if [[ -r "$candidate" ]] &&
        [[ $(sha256sum "$candidate" | awk '{print $1}') == "$Q35_OVMF_VARS_SHA256" ]]; then
        ovmf_vars=$candidate
        break
    fi
done
[[ -n "$ovmf_vars" ]] || {
    printf 'missing pinned OVMF vars image (%s)\n' "$Q35_OVMF_VARS_SHA256" >&2
    exit 1
}

x86_64-linux-gnu-gcc -print-sysroot >/dev/null

printf 'TheKernel development image check: ok\n'
