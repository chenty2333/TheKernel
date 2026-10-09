#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
[[ $# == 1 ]] || { printf 'usage: %s ROOTFS_STAGE\n' "$0" >&2; exit 2; }
STAGE=$(realpath -m "$1")
[[ -d "$STAGE" ]] || { printf 'rootfs stage does not exist: %s\n' "$STAGE" >&2; exit 1; }

# Firmware is optional rootfs data, never an ELF build input. Redistributors
# must carry the Realtek notice next to the bytecode.
if [ -n "${THEKERNEL_RTL8168_FIRMWARE_DIR:-}" ]; then
    firmware_dir=$THEKERNEL_RTL8168_FIRMWARE_DIR
    for name in rtl8168h-2.fw LICENSE.r8169; do
        [ -f "$firmware_dir/$name" ] || { printf 'missing firmware input: %s/%s\n' "$firmware_dir" "$name" >&2; exit 1; }
    done
    install -d "$STAGE/lib/firmware/rtl_nic"
    install -m 0644 "$firmware_dir/rtl8168h-2.fw" "$firmware_dir/LICENSE.r8169" "$STAGE/lib/firmware/rtl_nic/"
fi

# ADL-N uses the TGL GuC/HuC images selected by intel_uc_fw.c. Keep
# them explicit rootfs inputs rather than assuming DMC also supplies GT code.
if [ -n "${THEKERNEL_I915_UC_FIRMWARE_DIR:-}" ]; then
    uc_dir=$THEKERNEL_I915_UC_FIRMWARE_DIR
    for name in tgl_guc_70.bin tgl_huc.bin; do
        [ -s "$uc_dir/$name" ] || { printf 'missing uC firmware input: %s/%s\n' "$uc_dir" "$name" >&2; exit 1; }
        [ "$(wc -c < "$uc_dir/$name")" -le 2097152 ] || { printf 'uC firmware exceeds driver size cap: %s\n' "$name" >&2; exit 1; }
    done
    [ -s "$uc_dir/LICENSE.i915" ] || { printf 'missing uC firmware notice: %s/LICENSE.i915\n' "$uc_dir" >&2; exit 1; }
    install -d "$STAGE/lib/firmware/i915"
    install -m 0644 "$uc_dir/tgl_guc_70.bin" "$uc_dir/tgl_huc.bin" \
        "$uc_dir/LICENSE.i915" "$STAGE/lib/firmware/i915/"
fi

# DMC files are licensed binary firmware, not source inputs. The caller must
# provide the full Intel notice and the uncompressed names requested by i915;
# caps below come from intel_dmc.c for display versions 12/13.
if [ -n "${THEKERNEL_I915_DMC_FIRMWARE_DIR:-}" ]; then
    dmc_dir=$THEKERNEL_I915_DMC_FIRMWARE_DIR
    for name in adlp_dmc.bin adlp_dmc_ver2_16.bin; do
        [ -s "$dmc_dir/$name" ] || { printf 'missing DMC firmware input: %s/%s\n' "$dmc_dir" "$name" >&2; exit 1; }
        [ "$(wc -c < "$dmc_dir/$name")" -le 131072 ] || { printf 'DMC firmware exceeds display-13 limit: %s\n' "$name" >&2; exit 1; }
    done
    for name in adls_dmc_ver2_01.bin rkl_dmc_ver2_03.bin tgl_dmc_ver2_12.bin; do
        [ -s "$dmc_dir/$name" ] || { printf 'missing DMC firmware input: %s/%s\n' "$dmc_dir" "$name" >&2; exit 1; }
        [ "$(wc -c < "$dmc_dir/$name")" -le 24576 ] || { printf 'DMC firmware exceeds display-12 limit: %s\n' "$name" >&2; exit 1; }
    done
    [ -s "$dmc_dir/LICENSE.i915" ] || { printf 'missing DMC firmware notice: %s/LICENSE.i915\n' "$dmc_dir" >&2; exit 1; }
    install -d "$STAGE/lib/firmware/i915"
    install -m 0644 "$dmc_dir/adlp_dmc.bin" \
        "$dmc_dir/adlp_dmc_ver2_16.bin" \
        "$dmc_dir/adls_dmc_ver2_01.bin" \
        "$dmc_dir/rkl_dmc_ver2_03.bin" \
        "$dmc_dir/tgl_dmc_ver2_12.bin" \
        "$dmc_dir/LICENSE.i915" "$STAGE/lib/firmware/i915/"
fi

# The AX211 payload is opt-in and sourced from a caller-supplied linux-firmware
# tree. Validate its API, stage only the matching So/GF files, and carry Intel's
# firmware grant alongside them.
if [ -n "${THEKERNEL_IWX_FIRMWARE_DIR:-}" ]; then
    "$SCRIPT_DIR/build-iwx-firmware-payload.sh" \
        --source-dir "$THEKERNEL_IWX_FIRMWARE_DIR" --output "$STAGE"
fi

# Intel CNVi Bluetooth firmware is an explicit, redistributor-supplied rootfs
# input. Decompress the selected linux-firmware blobs offline and retain Intel's
# binary redistribution terms next to the staged files.
if [ -n "${THEKERNEL_INTEL_BT_FIRMWARE_DIR:-}" ]; then
    firmware_dir=$THEKERNEL_INTEL_BT_FIRMWARE_DIR
    license_file=${THEKERNEL_INTEL_BT_FIRMWARE_LICENSE:-/usr/share/licenses/linux-firmware/LICENSE.intel}
    "$SCRIPT_DIR/stage-intel-bt-firmware.sh" \
        "$firmware_dir" "$STAGE/lib/firmware/intel" "$license_file"
fi
