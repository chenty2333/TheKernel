#!/usr/bin/env bash
# Build a small GPT/FAT32 EFI system partition for either supported x86_64
# boot protocol.
#
# The image deliberately contains only the GRUB standalone EFI binary and the
# kernel and, in module mode, the generated root filesystem as a Multiboot2
# module.  The drive modes leave the rootfs on QEMU's sole virtio-blk device.

set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

usage() {
    cat >&2 <<'EOF'
usage: scripts/build-x86-uefi-esp.sh --mode {multiboot|multiboot-drive|linux} --kernel PATH --output PATH [options]

Create a GPT/FAT32 ESP containing GRUB's x86_64 EFI fallback loader.
`multiboot` stages TheKernel.elf and rootfs-x86.img; `multiboot-drive` stages
only TheKernel.elf and boots its external /dev/vda root filesystem; `linux`
stages only /vmlinuz and also boots its external rootfs. The default GRUB
configuration follows the selected mode.

options:
  --mode MODE            multiboot, multiboot-drive, or linux (default: multiboot)
  --kernel PATH          x86_64 kernel image to place on the ESP
  --rootfs PATH          required only for multiboot; staged as a module
  --output PATH          ESP disk image to create
  --grub-config PATH     GRUB config (default: config/x86_64/grub.cfg)
  --size-mib N           image size, at least 128 MiB (default: auto-sized)
  --grub-mkstandalone P  grub2-mkstandalone executable to use
EOF
    exit 2
}

kernel=
rootfs=
output=
mode=multiboot
grub_config=
size_mib=
grub_mkstandalone=${GRUB_MKSTANDALONE:-}

while (($# > 0)); do
    case "$1" in
        --mode)
            (($# >= 2)) || usage
            mode=$2
            shift 2
            ;;
        --kernel)
            (($# >= 2)) || usage
            kernel=$2
            shift 2
            ;;
        --rootfs)
            (($# >= 2)) || usage
            rootfs=$2
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

[[ "$mode" == multiboot || "$mode" == multiboot-drive || "$mode" == linux ]] || { printf 'unsupported ESP mode: %s\n' "$mode" >&2; usage; }
[[ -n "$kernel" && -n "$output" ]] || usage
if [[ "$mode" == multiboot ]]; then
    [[ -n "$rootfs" ]] || usage
elif [[ -n "$rootfs" ]]; then
    printf '%s\n' '--rootfs is only valid with --mode multiboot; drive modes boot /dev/vda directly' >&2
    exit 2
fi
if [[ -z "$grub_config" ]]; then
    case "$mode" in
        multiboot) grub_config="$REPO_ROOT/config/x86_64/grub.cfg" ;;
        multiboot-drive) grub_config="$REPO_ROOT/config/x86_64/grub-drive.cfg" ;;
        linux) grub_config="$REPO_ROOT/config/x86_64/grub-linux.cfg" ;;
    esac
fi
[[ -f "$kernel" ]] || { printf 'kernel does not exist: %s\n' "$kernel" >&2; exit 1; }
if [[ "$mode" == multiboot ]]; then
    [[ -f "$rootfs" ]] || { printf 'rootfs does not exist: %s\n' "$rootfs" >&2; exit 1; }
fi
[[ -f "$grub_config" ]] || {
    printf 'GRUB config does not exist: %s\n' "$grub_config" >&2
    exit 1
}
if [[ -n "$size_mib" ]]; then
    [[ "$size_mib" =~ ^[0-9]+$ && "$size_mib" -ge 128 ]] || {
        printf 'ESP size must be an integer of at least 128 MiB: %s\n' "$size_mib" >&2
        exit 1
    }
fi

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
if [[ "$mode" == multiboot ]]; then
    rootfs=$(CDPATH= cd -- "$(dirname -- "$rootfs")" && pwd)/$(basename -- "$rootfs")
fi
grub_config=$(CDPATH= cd -- "$(dirname -- "$grub_config")" && pwd)/$(basename -- "$grub_config")
output_parent=$(dirname -- "$output")
mkdir -p "$output_parent"
output=$(CDPATH= cd -- "$output_parent" && pwd)/$(basename -- "$output")
[[ "$kernel" != "$output" ]] || { printf 'output must differ from kernel\n' >&2; exit 1; }
if [[ "$mode" == multiboot ]]; then
    [[ "$rootfs" != "$output" ]] || { printf 'output must differ from rootfs\n' >&2; exit 1; }
fi

# FAT stores the full logical rootfs image in multiboot mode even when ext4 is
# sparse. Linux mode stages only its kernel.
# Reserve 64 MiB for the kernel, standalone GRUB image, FAT metadata, and
# growth, then round the default ESP size up to a 64 MiB boundary.
payload_bytes=$(stat -c %s "$kernel")
if [[ "$mode" == multiboot ]]; then
    payload_bytes=$(( payload_bytes + $(stat -c %s "$rootfs") ))
fi
minimum_mib=$(( (payload_bytes + 1048575) / 1048576 + 64 ))
minimum_mib=$(( (minimum_mib + 63) / 64 * 64 ))
(( minimum_mib < 128 )) && minimum_mib=128
if [[ -z "$size_mib" ]]; then
    size_mib=$minimum_mib
elif (( size_mib < minimum_mib )); then
    printf 'ESP size %s MiB is too small for the supplied payloads; need at least %s MiB\n' \
        "$size_mib" "$minimum_mib" >&2
    exit 1
fi

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

# The standalone image needs filesystem discovery plus the selected boot
# protocol before GRUB's normal module path is available.
# EFI cannot provide the legacy EGA text console that Multiboot defaults to.
# Keep GRUB itself on serial, but make the firmware framebuffer available to
# the Multiboot loader so it can hand the kernel a usable graphics console.
grub_modules='part_gpt fat search search_fs_file serial terminal all_video'
if [[ "$mode" == multiboot || "$mode" == multiboot-drive ]]; then
    grub_modules="$grub_modules multiboot2"
else
    grub_modules="$grub_modules linux"
fi
"$grub_mkstandalone" \
    -O x86_64-efi \
    --modules="$grub_modules" \
    -o "$grub_efi" \
    "boot/grub/grub.cfg=$grub_config"

esp="$image@@1M"
mmd -i "$esp" ::/EFI
mmd -i "$esp" ::/EFI/BOOT
mcopy -i "$esp" "$grub_efi" ::/EFI/BOOT/BOOTX64.EFI
if [[ "$mode" == multiboot || "$mode" == multiboot-drive ]]; then
    mcopy -i "$esp" "$kernel" ::/TheKernel.elf
    if [[ "$mode" == multiboot ]]; then
        mcopy -i "$esp" "$rootfs" ::/rootfs-x86.img
    fi
else
    mcopy -i "$esp" "$kernel" ::/vmlinuz
fi

mv "$image" "$output"
printf '%s\n' "$output"
