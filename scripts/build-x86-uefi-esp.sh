#!/usr/bin/env bash
# Build a small GPT/FAT32 EFI system partition for the x86_64 Multiboot path.
#
# The image deliberately contains only the GRUB standalone EFI binary and the
# kernel.  The Linux root filesystem remains a separate virtio block device;
# this keeps the firmware boot contract independent from the kernel's storage
# driver selection.

set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

usage() {
    cat >&2 <<'EOF'
usage: scripts/build-x86-uefi-esp.sh --kernel PATH --output PATH [options]

Create a GPT/FAT32 ESP containing GRUB's x86_64 EFI fallback loader and a
Multiboot kernel named TheKernel.elf.  The default GRUB configuration is
config/x86_64/grub.cfg.

options:
  --kernel PATH          x86_64 Multiboot ELF to place at ESP root
  --output PATH          ESP disk image to create
  --grub-config PATH     GRUB config (default: config/x86_64/grub.cfg)
  --size-mib N           image size, at least 128 MiB (default: 128)
  --grub-mkstandalone P  grub2-mkstandalone executable to use
EOF
    exit 2
}

kernel=
output=
grub_config="$REPO_ROOT/config/x86_64/grub.cfg"
size_mib=128
grub_mkstandalone=${GRUB_MKSTANDALONE:-}

while (($# > 0)); do
    case "$1" in
        --kernel)
            (($# >= 2)) || usage
            kernel=$2
            shift 2
            ;;
        --output)
            (($# >= 2)) || usage
            output=$2
            shift 2
            ;;
        --grub-config)
            (($# >= 2)) || usage
            grub_config=$2
            shift 2
            ;;
        --size-mib)
            (($# >= 2)) || usage
            size_mib=$2
            shift 2
            ;;
        --grub-mkstandalone)
            (($# >= 2)) || usage
            grub_mkstandalone=$2
            shift 2
            ;;
        -h|--help)
            usage
            ;;
        *)
            printf 'unknown option: %s\n' "$1" >&2
            usage
            ;;
    esac
done

[[ -n "$kernel" && -n "$output" ]] || usage
[[ -f "$kernel" ]] || { printf 'kernel does not exist: %s\n' "$kernel" >&2; exit 1; }
[[ -f "$grub_config" ]] || {
    printf 'GRUB config does not exist: %s\n' "$grub_config" >&2
    exit 1
}
[[ "$size_mib" =~ ^[0-9]+$ && "$size_mib" -ge 128 ]] || {
    printf 'ESP size must be an integer of at least 128 MiB: %s\n' "$size_mib" >&2
    exit 1
}

if [[ -z "$grub_mkstandalone" ]]; then
    if command -v grub2-mkstandalone >/dev/null 2>&1; then
        grub_mkstandalone=$(command -v grub2-mkstandalone)
    elif command -v grub-mkstandalone >/dev/null 2>&1; then
        grub_mkstandalone=$(command -v grub-mkstandalone)
    else
        printf 'grub2-mkstandalone (or grub-mkstandalone) is required\n' >&2
        exit 1
    fi
fi

for tool in parted mkfs.fat mcopy mmd; do
    command -v "$tool" >/dev/null 2>&1 || {
        printf '%s is required to build an ESP\n' "$tool" >&2
        exit 1
    }
done

kernel=$(CDPATH= cd -- "$(dirname -- "$kernel")" && pwd)/$(basename -- "$kernel")
grub_config=$(CDPATH= cd -- "$(dirname -- "$grub_config")" && pwd)/$(basename -- "$grub_config")
output_parent=$(dirname -- "$output")
mkdir -p "$output_parent"
output=$(CDPATH= cd -- "$output_parent" && pwd)/$(basename -- "$output")
[[ "$kernel" != "$output" ]] || { printf 'output must differ from kernel\n' >&2; exit 1; }

tmp_dir=$(mktemp -d "${output}.tmp.XXXXXX")
cleanup() {
    rm -rf "$tmp_dir"
}
trap cleanup EXIT

image="$tmp_dir/esp.img"
grub_efi="$tmp_dir/BOOTX64.EFI"

# Keep the first partition at the conventional 1 MiB boundary.  mtools can
# address the partition in-place through its @@ byte/size offset syntax, so no
# root-only loop-device setup is needed.
truncate -s "${size_mib}M" "$image"
parted -s -a optimal "$image" mklabel gpt
parted -s -a optimal "$image" mkpart ESP fat32 1MiB 100%
parted -s "$image" set 1 esp on
mkfs.fat -F 32 --invariant -n THEKERNEL --offset=2048 "$image" >/dev/null

# This exact preload set is required because the standalone image must locate
# /TheKernel.elf on a GPT/FAT ESP before the normal GRUB module path exists.
"$grub_mkstandalone" \
    -O x86_64-efi \
    --modules='part_gpt fat search search_fs_file multiboot multiboot2 serial terminal' \
    -o "$grub_efi" \
    "boot/grub/grub.cfg=$grub_config"

esp="$image@@1M"
mmd -i "$esp" ::/EFI
mmd -i "$esp" ::/EFI/BOOT
mcopy -i "$esp" "$grub_efi" ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "$esp" "$kernel" ::/TheKernel.elf

mv "$image" "$output"
printf '%s\n' "$output"
