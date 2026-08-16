#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    printf 'usage: %s KERNEL_ELF\n' "$0" >&2
    exit 2
fi

kernel=$1
[[ -f "$kernel" ]] || {
    printf 'x86 Multiboot gate: kernel does not exist: %s\n' "$kernel" >&2
    exit 1
}

if command -v grub2-file >/dev/null 2>&1; then
    grub_file=$(command -v grub2-file)
elif command -v grub-file >/dev/null 2>&1; then
    grub_file=$(command -v grub-file)
else
    printf 'x86 Multiboot gate: grub2-file or grub-file is required\n' >&2
    exit 1
fi

"$grub_file" --is-x86-multiboot "$kernel"
"$grub_file" --is-x86-multiboot2 "$kernel"
printf 'X86_MULTIBOOT_HEADER_GATE status=ok tool=%s kernel=%s\n' "$grub_file" "$kernel"
