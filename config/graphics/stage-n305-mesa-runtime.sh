#!/usr/bin/env bash
# Stage only the target-built Mesa runtime needed by the N305 guest paths.
# Keep this shared between the full Buildroot and lean existing-rootfs builders.
set -euo pipefail

check_only=0
if [[ $# == 2 && $1 == --check ]]; then
    check_only=1
    source=$(realpath -e "$2")
    target=
elif [[ $# == 2 ]]; then
    source=$(realpath -e "$1")
    target=$(realpath -e "$2")
else
    echo "usage: $0 [--check] MESA_STAGE [ROOTFS_STAGE]" >&2
    exit 2
fi
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
command -v readelf >/dev/null || { echo 'readelf is required to validate the Mesa runtime stage' >&2; exit 1; }

gallium=$source/usr/lib/libgallium-26.1.2.so
dril=$source/usr/lib/dri/libdril_dri.so
iris_alias=$source/usr/lib/dri/iris_dri.so

validate_x86_64() {
    local binary=$1
    readelf -h "$binary" | grep -q 'Class:.*ELF64'
    readelf -h "$binary" | grep -q 'Machine:.*Advanced Micro Devices X86-64'
}

[[ -r $gallium ]] || { echo "Mesa iris Gallium DSO missing: $gallium" >&2; exit 1; }
[[ -r $dril ]] || { echo "Mesa DRIL compatibility DSO missing: $dril" >&2; exit 1; }
[[ -L $iris_alias && $(readlink "$iris_alias") == libdril_dri.so ]] || {
    echo "Mesa Iris DRI alias must point to libdril_dri.so: $iris_alias" >&2
    exit 1
}
validate_x86_64 "$gallium"
validate_x86_64 "$dril"
readelf -d "$gallium" | grep -q 'SONAME.*libgallium-26\.1\.2\.so'
grep -aFq iris_driver_descriptor "$gallium" || {
    echo "staged libgallium does not contain the Iris driver" >&2
    exit 1
}
readelf --dyn-syms -W "$dril" | awk \
    '$4 == "FUNC" && $5 == "GLOBAL" && $8 == "__driDriverGetExtensions_iris" { found=1 }
     END { exit !found }' || {
    echo "staged DRIL module has no Iris extension metadata getter" >&2
    exit 1
}

if [[ $check_only == 0 ]]; then
    install -D -m 0644 "$gallium" "$target/usr/lib/libgallium-26.1.2.so"
    install -D -m 0644 "$dril" "$target/usr/lib/dri/libdril_dri.so"
    ln -sfn libdril_dri.so "$target/usr/lib/dri/iris_dri.so"
    install -D -m 0755 \
        "$repo/config/graphics/overlay/n305-iris-smoke/etc/init.d/S90n305-iris-smoke" \
        "$target/etc/thekernel/n305-iris-loader-smoke"
fi

# ANV is optional for a Mesa stage. If either generated payload file exists,
# require the canonical x86_64-generated pair; never rename guessed metadata.
anv=$source/usr/lib/libvulkan_intel.so
icd=$source/usr/share/vulkan/icd.d/intel_icd.x86_64.json
if [[ -e $anv || -e $icd ]]; then
    [[ -r $anv && -r $icd ]] || {
        echo "incomplete Mesa ANV stage: expected libvulkan_intel.so and intel_icd.x86_64.json" >&2
        exit 1
    }
    validate_x86_64 "$anv"
    readelf -d "$anv" | grep -q 'SONAME.*libvulkan_intel\.so'
    python3 - "$icd" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    metadata = json.load(stream)
icd = metadata.get("ICD", {})
if (icd.get("library_path") != "/usr/lib/libvulkan_intel.so"
        or icd.get("library_arch") != "64"
        or not icd.get("api_version")):
    raise SystemExit("Mesa ANV ICD metadata does not match the target DSO")
PY
    if [[ $check_only == 0 ]]; then
        install -D -m 0644 "$anv" "$target/usr/lib/libvulkan_intel.so"
        install -D -m 0644 "$icd" "$target/usr/share/vulkan/icd.d/intel_icd.x86_64.json"
        install -D -m 0755 \
            "$repo/config/graphics/overlay/n305-iris-smoke/etc/init.d/S91n305-vulkan-smoke" \
            "$target/etc/thekernel/n305-anv-loader-smoke"
    fi
fi
