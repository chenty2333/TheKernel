// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/i915_pci.c: the Gen11+
// device-info chain (GEN9/GEN11/GEN12/XE_HP/DG2/MTL feature macros), the
// gen11-and-newer pciidlist, i915_pci_resource_valid(), i915_pci_probe(),
// i915_pci_remove(), i915_pci_shutdown() and driver registration. The
// pre-Gen11 device tables are outside the TheKernel Gen12+ scope and are not
// translated. The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code, non_upper_case_globals)]

use core::ffi::{c_char, c_int, c_void};

use crate::{
    i915_probe_provider_upstream as provider,
    intel_device_info_types_upstream::*,
    intel_gt_types_upstream::IntelGtTypeValue,
    intel_pciids_upstream::*,
    linux_i915_private::DrmI915Private,
};

/// `IORESOURCE_UNSET` from include/linux/ioport.h.
const IORESOURCE_UNSET: u64 = 0x2000_0000;

/// `TAINT_USER` (include/linux/panic.h) and `LOCKDEP_STILL_OK` (enum
/// lockdep_ok, first member = 0). The taint is recorded by the provider.
const LOCKDEP_STILL_OK: u32 = 0;
const TAINT_USER: u32 = 6;

unsafe extern "C" {
    fn pci_get_drvdata(pdev: *mut c_void) -> *mut c_void;
    fn pci_set_drvdata(pdev: *mut c_void, data: *mut c_void);
    fn add_taint(flag: u32, lockdep_ok: u32);
    /// `pci_resource_flags()`, owned by the LinuxKPI PCI layer (L1).
    fn pci_resource_flags(pdev: *mut c_void, bar: c_int) -> u64;
    /// `pci_resource_len()`, owned by the LinuxKPI PCI layer (L1).
    fn pci_resource_len(dev: *mut c_void, bar: u32) -> u64;
    /// `intel_mmio_bar()` from the i915 MMIO headers.
    fn intel_mmio_bar(graphics_ver: u32) -> u32;
}

/// Engine ids from gt/intel_engine_types.h, as engine-mask bits.
const fn eng(id: u32) -> u32 {
    1 << id
}
const RCS0: u32 = eng(0);
const BCS0: u32 = eng(1);
const VCS0: u32 = eng(10);
const VCS1: u32 = eng(11);
const VCS2: u32 = eng(12);
const VECS0: u32 = eng(18);
const VECS1: u32 = eng(19);
const CCS0: u32 = eng(22);
const CCS1: u32 = eng(23);
const CCS2: u32 = eng(24);
const CCS3: u32 = eng(25);

/// Memory region bits (`BIT(INTEL_REGION_*)`, intel_memory_region.h).
const REGION_SMEM: u32 = 1 << 0;
const REGION_LMEM_0: u32 = 1 << 1;
const REGION_STOLEN_SMEM: u32 = 1 << 5;
const REGION_STOLEN_LMEM: u32 = 1 << 6;

/// `I915_GTT_PAGE_SIZE_*` (gt/intel_gtt.h).
const PAGE_4K: u64 = 1 << 12;
const PAGE_64K: u64 = 1 << 16;
const PAGE_2M: u64 = 1 << 21;

/// `LEGACY_CACHELEVEL`, `TGL_CACHELEVEL`, `MTL_CACHELEVEL` (i915_pci.c).
const LEGACY_CACHELEVEL: [u32; 4] = [0, 1, 2, 3];
const TGL_CACHELEVEL: [u32; 4] = [3, 0, 0, 2];
const MTL_CACHELEVEL: [u32; 4] = [2, 3, 3, 1];

const GEN9_DEFAULT_PAGE_SIZES: u32 = (PAGE_4K | PAGE_64K) as u32;
const GEN11_DEFAULT_PAGE_SIZES: u32 = (PAGE_4K | PAGE_64K | PAGE_2M) as u32;
const XE_HP_PAGE_SIZES: u32 = (PAGE_4K | PAGE_64K | PAGE_2M) as u32;
const GEN_DEFAULT_REGIONS: u32 = REGION_SMEM | REGION_STOLEN_SMEM;
const DGFX_REGIONS: u32 = REGION_SMEM | REGION_LMEM_0 | REGION_STOLEN_LMEM;
const MTL_REGIONS: u32 = REGION_SMEM | REGION_STOLEN_LMEM;

/// `GEN(x)`: graphics and media IP version.
const fn set_gen(i: &mut IntelDeviceInfo, ver: u8) {
    i.runtime.graphics.ip.ver = ver;
    i.runtime.media.ip.ver = ver;
}

/// `set_flag()` applies one `DEV_INFO_FOR_EACH_FLAG` designated-initializer
/// member (`.has_x = 1` or `.has_x = 0`).
const fn set_flag(i: &mut IntelDeviceInfo, bit: u32, on: bool) {
    let byte = (bit / 8) as usize;
    let mask = 1u8 << (bit % 8);
    if on {
        i.flags[byte] |= mask;
    } else {
        i.flags[byte] &= !mask;
    }
}

/// `GEN7_FEATURES` + `G75_FEATURES` + `GEN8_FEATURES` + `GEN9_FEATURES` +
/// `GEN11_FEATURES`, applied in C designated-initializer order.
const fn gen11_features(platform: u32, engine_mask: u32) -> IntelDeviceInfo {
    let mut i = IntelDeviceInfo::zeroed();
    // GEN7_FEATURES
    set_gen(&mut i, 7);
    i.platform_engine_mask = RCS0 | VCS0 | BCS0;
    set_flag(&mut i, DEV_INFO_FLAG_HAS_3D_PIPELINE, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_COHERENT_GGTT, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_LLC, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RC6, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RC6P, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RESET_ENGINE, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RPS, true);
    i.dma_mask_size = 40;
    i.max_pat_index = 3;
    i.runtime.ppgtt_type = INTEL_PPGTT_ALIASING;
    i.runtime.ppgtt_size = 31;
    i.runtime.page_sizes = GEN9_DEFAULT_PAGE_SIZES;
    i.memory_regions = GEN_DEFAULT_REGIONS;
    i.cachelevel_to_pat = LEGACY_CACHELEVEL;
    // G75_FEATURES
    i.platform_engine_mask = RCS0 | VCS0 | BCS0 | VECS0;
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RC6P, false);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RUNTIME_PM, true);
    // GEN8_FEATURES
    set_gen(&mut i, 8);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_LOGICAL_RING_CONTEXTS, true);
    i.dma_mask_size = 39;
    i.runtime.ppgtt_type = INTEL_PPGTT_FULL;
    i.runtime.ppgtt_size = 48;
    set_flag(&mut i, DEV_INFO_FLAG_HAS_64BIT_RELOC, true);
    // GEN9_FEATURES
    set_gen(&mut i, 9);
    i.runtime.page_sizes = GEN9_DEFAULT_PAGE_SIZES;
    set_flag(&mut i, DEV_INFO_FLAG_HAS_GT_UC, true);
    // GEN11_FEATURES
    i.runtime.page_sizes = GEN11_DEFAULT_PAGE_SIZES;
    set_gen(&mut i, 11);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_COHERENT_GGTT, false);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_LOGICAL_RING_ELSQ, true);
    // PLATFORM(x) and the per-platform engine mask
    i.platform = platform;
    i.platform_engine_mask = engine_mask;
    i
}

/// `GEN12_FEATURES` on top of `GEN11_FEATURES`.
const fn gen12_features(platform: u32, engine_mask: u32) -> IntelDeviceInfo {
    let mut i = gen11_features(platform, engine_mask);
    set_gen(&mut i, 12);
    i.cachelevel_to_pat = TGL_CACHELEVEL;
    set_flag(&mut i, DEV_INFO_FLAG_HAS_GLOBAL_MOCS, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_PXP, true);
    i.max_pat_index = 3;
    i
}

/// `DGFX_FEATURES` applied after the feature chain it is listed with.
const fn apply_dgfx(i: &mut IntelDeviceInfo) {
    i.memory_regions = DGFX_REGIONS;
    set_flag(i, DEV_INFO_FLAG_HAS_LLC, false);
    set_flag(i, DEV_INFO_FLAG_HAS_PXP, false);
    set_flag(i, DEV_INFO_FLAG_HAS_SNOOP, true);
    set_flag(i, DEV_INFO_FLAG_IS_DGFX, true);
    set_flag(i, DEV_INFO_FLAG_HAS_HECI_GSCFI, true);
}

/// `XE_HP_FEATURES` (does not include the Gen7-Gen11 chain).
const fn xe_hp_features(platform: u32, engine_mask: u32) -> IntelDeviceInfo {
    let mut i = IntelDeviceInfo::zeroed();
    i.runtime.page_sizes = XE_HP_PAGE_SIZES;
    i.cachelevel_to_pat = TGL_CACHELEVEL;
    i.dma_mask_size = 46;
    set_flag(&mut i, DEV_INFO_FLAG_HAS_3D_PIPELINE, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_64BIT_RELOC, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_FLAT_CCS, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_GLOBAL_MOCS, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_GT_UC, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_LLC, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_LOGICAL_RING_CONTEXTS, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_LOGICAL_RING_ELSQ, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_MSLICE_STEERING, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_OA_BPC_REPORTING, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_OA_SLICE_CONTRIB_LIMITS, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_OAM, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RC6, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RESET_ENGINE, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RPS, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_RUNTIME_PM, true);
    i.max_pat_index = 3;
    i.runtime.ppgtt_size = 48;
    i.runtime.ppgtt_type = INTEL_PPGTT_FULL;
    i.platform = platform;
    i.platform_engine_mask = engine_mask;
    i
}

/// `DG2_FEATURES`.
const fn dg2_features(platform: u32, engine_mask: u32) -> IntelDeviceInfo {
    let mut i = xe_hp_features(platform, engine_mask);
    apply_dgfx(&mut i);
    set_gen(&mut i, 12);
    i.runtime.graphics.ip.rel = 55;
    i.runtime.media.ip.rel = 55;
    set_flag(&mut i, DEV_INFO_FLAG_HAS_64K_PAGES, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_GUC_DEPRIVILEGE, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_HECI_PXP, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_MEDIA_RATIO_MODE, true);
    i
}

static ICL_INFO: IntelDeviceInfo = gen11_features(
    INTEL_ICELAKE,
    RCS0 | BCS0 | VECS0 | VCS0 | VCS2,
);

static EHL_INFO: IntelDeviceInfo = {
    let mut i = gen11_features(INTEL_ELKHARTLAKE, RCS0 | BCS0 | VCS0 | VECS0);
    i.runtime.ppgtt_size = 36;
    i
};

static JSL_INFO: IntelDeviceInfo = {
    let mut i = gen11_features(INTEL_JASPERLAKE, RCS0 | BCS0 | VCS0 | VECS0);
    i.runtime.ppgtt_size = 36;
    i
};

static TGL_INFO: IntelDeviceInfo = gen12_features(
    INTEL_TIGERLAKE,
    RCS0 | BCS0 | VECS0 | VCS0 | VCS2,
);

static RKL_INFO: IntelDeviceInfo = gen12_features(
    INTEL_ROCKETLAKE,
    RCS0 | BCS0 | VECS0 | VCS0,
);

static DG1_INFO: IntelDeviceInfo = {
    let mut i = gen12_features(
        INTEL_DG1,
        RCS0 | BCS0 | VECS0 | VCS0 | VCS2,
    );
    apply_dgfx(&mut i);
    // .__runtime.graphics.ip.rel = 10 (Wa_16011227922 sets ppgtt_size = 47)
    i.runtime.graphics.ip.rel = 10;
    i.runtime.ppgtt_size = 47;
    i
};

static ADL_S_INFO: IntelDeviceInfo = {
    let mut i = gen12_features(
        INTEL_ALDERLAKE_S,
        RCS0 | BCS0 | VECS0 | VCS0 | VCS2,
    );
    i.dma_mask_size = 39;
    i
};

static ADL_P_INFO: IntelDeviceInfo = {
    let mut i = gen12_features(
        INTEL_ALDERLAKE_P,
        RCS0 | BCS0 | VECS0 | VCS0 | VCS2,
    );
    i.runtime.ppgtt_size = 48;
    i.dma_mask_size = 39;
    i
};

static DG2_INFO: IntelDeviceInfo = dg2_features(
    INTEL_DG2,
    RCS0 | BCS0 | VECS0 | VECS1 | VCS0 | VCS2 | CCS0 | CCS1 | CCS2 | CCS3,
);

static ATS_M_INFO: IntelDeviceInfo = {
    let mut i = dg2_features(
        INTEL_DG2,
        RCS0 | BCS0 | VECS0 | VECS1 | VCS0 | VCS2 | CCS0 | CCS1 | CCS2 | CCS3,
    );
    set_flag(&mut i, DEV_INFO_FLAG_REQUIRE_FORCE_PROBE, true);
    set_flag(&mut i, DEV_INFO_FLAG_TUNING_THREAD_RR_AFTER_DEP, true);
    i
};

/// `xelpmp_extra_gt[]`: Standalone Media GT (MTL SA media definition).
static XELPMP_EXTRA_GT: [GtDefinitionC; 2] = [
    GtDefinitionC {
        type_: IntelGtTypeValue::Media as i32,
        name: c"Standalone Media GT".as_ptr(),
        mapping_base: 0,
        gsi_offset: 0x380000,
        engine_mask: eng(VECS0_ID) | eng(VCS0_ID) | eng(VCS2_ID) | eng(GSC0_ID),
    },
    GtDefinitionC {
        type_: 0,
        name: core::ptr::null_mut(),
        mapping_base: 0,
        gsi_offset: 0,
        engine_mask: 0,
    },
];
/// Layout-identical copy of `struct intel_gt_definition` (gt/intel_gt_types.h)
/// for the static MTL extra-GT table; `name` is a static C string.
#[repr(C)]
pub struct GtDefinitionC {
    pub type_: i32,
    pub name: *const c_char,
    pub mapping_base: u32,
    pub gsi_offset: u32,
    pub engine_mask: u32,
}
unsafe impl Sync for GtDefinitionC {}

const VECS0_ID: u32 = 18;
const VCS0_ID: u32 = 10;
const VCS2_ID: u32 = 12;
const GSC0_ID: u32 = 26;

static MTL_INFO: IntelDeviceInfo = {
    let mut i = xe_hp_features(INTEL_METEORLAKE, RCS0 | BCS0 | CCS0);
    // Real graphics IP version is read from GMD_ID; values here are sanity.
    i.runtime.graphics.ip.ver = 12;
    i.runtime.graphics.ip.rel = 70;
    i.runtime.media.ip.ver = 13;
    i.extra_gt_list = XELPMP_EXTRA_GT.as_ptr().cast();
    set_flag(&mut i, DEV_INFO_FLAG_HAS_FLAT_CCS, false);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_GMD_ID, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_GUC_DEPRIVILEGE, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_GUC_TLB_INVALIDATION, true);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_LLC, false);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_MSLICE_STEERING, false);
    set_flag(&mut i, DEV_INFO_FLAG_HAS_SNOOP, true);
    i.max_pat_index = 4;
    set_flag(&mut i, DEV_INFO_FLAG_HAS_PXP, true);
    i.memory_regions = MTL_REGIONS;
    i.cachelevel_to_pat = MTL_CACHELEVEL;
    i
};

/// One `pciidlist` row group: the IDs of a `INTEL_*_IDS` macro and the
/// `.driver_data` info that `INTEL_VGA_DEVICE` attaches to them.
pub struct PciIdGroup {
    pub ids: &'static [u16],
    pub info: &'static IntelDeviceInfo,
}

/// `pciidlist[]` (i915_pci.c), gen11 and newer, in table order. The first
/// group that contains a device id wins, as with `pci_match_id()`.
pub static PCIIDLIST: [PciIdGroup; 16] = [
    PciIdGroup { ids: PCI_ICL_IDS, info: &ICL_INFO },
    PciIdGroup { ids: PCI_EHL_IDS, info: &EHL_INFO },
    PciIdGroup { ids: PCI_JSL_IDS, info: &JSL_INFO },
    PciIdGroup { ids: PCI_TGL_IDS, info: &TGL_INFO },
    PciIdGroup { ids: PCI_RKL_IDS, info: &RKL_INFO },
    PciIdGroup { ids: PCI_ADLS_IDS, info: &ADL_S_INFO },
    PciIdGroup { ids: PCI_ADLP_IDS, info: &ADL_P_INFO },
    PciIdGroup { ids: PCI_ADLN_IDS, info: &ADL_P_INFO },
    PciIdGroup { ids: PCI_DG1_IDS, info: &DG1_INFO },
    PciIdGroup { ids: PCI_RPLS_IDS, info: &ADL_S_INFO },
    PciIdGroup { ids: PCI_RPLU_IDS, info: &ADL_P_INFO },
    PciIdGroup { ids: PCI_RPLP_IDS, info: &ADL_P_INFO },
    PciIdGroup { ids: PCI_DG2_IDS, info: &DG2_INFO },
    PciIdGroup { ids: PCI_ATS_M_IDS, info: &ATS_M_INFO },
    PciIdGroup { ids: PCI_ARL_IDS, info: &MTL_INFO },
    PciIdGroup { ids: PCI_MTL_IDS, info: &MTL_INFO },
];

/// `pci_match_id()` over `pciidlist`: returns the driver_data for the first
/// matching device id.
pub fn pci_match_id(device: u16) -> Option<&'static IntelDeviceInfo> {
    PCIIDLIST
        .iter()
        .find(|group| group.ids.contains(&device))
        .map(|group| group.info)
}

/// `i915_pci_resource_valid()` from i915_pci.c.
///
/// # Safety
/// `pdev` must be a live PCI device object owned by the PCI provider.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_pci_resource_valid(pdev: *mut c_void, bar: c_int) -> bool {
    if unsafe { pci_resource_flags(pdev, bar) } == 0 {
        return false;
    }

    if unsafe { pci_resource_flags(pdev, bar) } & IORESOURCE_UNSET != 0 {
        return false;
    }

    if unsafe { pci_resource_len(pdev, bar as u32) } == 0 {
        return false;
    }

    true
}

/// `pdev_to_i915(pdev)` = `to_i915(pci_get_drvdata(pdev))`.
///
/// # Safety
/// `pdev` must be a live PCI device.
unsafe fn pdev_to_i915(pdev: *mut c_void) -> *mut DrmI915Private {
    unsafe { crate::linux::i915::to_i915(pci_get_drvdata(pdev)) }
}

/// `i915_pci_remove()`.
///
/// # Safety
/// `pdev` must be a PCI device bound to this driver.
pub unsafe fn i915_pci_remove(pdev: *mut c_void) {
    let i915 = unsafe { pdev_to_i915(pdev) };
    if i915.is_null() {
        // driver load aborted, nothing to cleanup
        return;
    }
    unsafe { crate::i915_driver_upstream::i915_driver_remove(i915) };
    unsafe { pci_set_drvdata(pdev, core::ptr::null_mut()) };
}

/// `device_id_in_list()`: `devices` is a comma-separated list of hex device
/// ids, with `!` prefixes for the negative form and `*` / `!*` as wildcards.
pub fn device_id_in_list(device_id: u16, devices: *const c_char, negative: bool) -> bool {
    if devices.is_null() {
        return false;
    }
    let devices = unsafe { core::ffi::CStr::from_ptr(devices) }.to_bytes();
    if devices.is_empty() {
        return false;
    }

    // match everything
    if negative && devices == b"!*" {
        return true;
    }
    if !negative && devices == b"*" {
        return true;
    }

    for mut tok in devices.split(|&b| b == b',') {
        if negative && tok.first() == Some(&b'!') {
            tok = &tok[1..];
        } else if (negative && tok.first() != Some(&b'!'))
            || (!negative && tok.first() == Some(&b'!'))
        {
            continue;
        }
        if let Some(val) = parse_kstrtou16_hex(tok) {
            if val == device_id {
                return true;
            }
        }
    }
    false
}

/// `kstrtou16(tok, 16, &val) == 0` (no whitespace skipping; a single
/// trailing newline is accepted, as kstrtoull does).
fn parse_kstrtou16_hex(tok: &[u8]) -> Option<u16> {
    let mut s = tok;
    if let Some(stripped) = s.strip_suffix(b"\n") {
        s = stripped;
    }
    if s.len() >= 2 && s[0] == b'0' && (s[1] == b'x' || s[1] == b'X') {
        s = &s[2..];
    }
    if s.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &b in s {
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as u32,
            b'a'..=b'f' => (b - b'a' + 10) as u32,
            b'A'..=b'F' => (b - b'A' + 10) as u32,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit)?;
        if value > u16::MAX as u32 {
            return None;
        }
    }
    Some(value as u16)
}

/// `id_forced()`.
pub fn id_forced(device_id: u16) -> bool {
    device_id_in_list(device_id, provider::modparam_force_probe(), false)
}

/// `id_blocked()`.
pub fn id_blocked(device_id: u16) -> bool {
    device_id_in_list(device_id, provider::modparam_force_probe(), true)
}

/// `intel_mmio_bar_valid()`.
///
/// # Safety
/// `pdev` must be a live PCI device object.
unsafe fn intel_mmio_bar_valid(pdev: *mut c_void, info: &IntelDeviceInfo) -> bool {
    let bar = unsafe { intel_mmio_bar(info.runtime.graphics.ip.ver as u32) };
    unsafe { i915_pci_resource_valid(pdev, bar as c_int) }
}

/// `i915_pci_probe()`. `info` is the matched `driver_data` (the device-info
/// the table row points at); `device` is `pdev->device`.
///
/// # Safety
/// `pdev` must be a live PCI device object and `info` must come from
/// `pci_match_id()`.
pub unsafe fn i915_pci_probe(pdev: *mut c_void, device: u16, info: &IntelDeviceInfo) -> c_int {
    const ENODEV: c_int = 19;
    const ENXIO: c_int = 6;
    const EPROBE_DEFER: c_int = 517;

    if info.flag(DEV_INFO_FLAG_REQUIRE_FORCE_PROBE) && !id_forced(device) {
        axlog::info!(
            "Your graphics device {:04x} is not properly supported by i915 in this kernel version. To force driver probe anyway, use i915.force_probe={:04x} module parameter or CONFIG_DRM_I915_FORCE_PROBE={:04x} configuration option, or (recommended) check for kernel updates.",
            device,
            device,
            device
        );
        return -ENODEV;
    }

    if id_blocked(device) {
        axlog::info!("I915 probe blocked for Device ID {:04x}.", device);
        return -ENODEV;
    }

    if info.flag(DEV_INFO_FLAG_REQUIRE_FORCE_PROBE) {
        axlog::info!(
            "Force probing unsupported Device ID {:04x}, tainting kernel",
            device
        );
        unsafe { add_taint(TAINT_USER, LOCKDEP_STILL_OK) };
    }

    // Only bind to function 0 of the device. Early generations used function
    // 1 as a placeholder for multi-head.
    if unsafe { provider::pci_func(pdev) } != 0 {
        return -ENODEV;
    }

    if !unsafe { intel_mmio_bar_valid(pdev, info) } {
        return -ENXIO;
    }

    // Detect if we need to wait for other drivers early on.
    if unsafe { provider::display_probe_defer(pdev) } {
        return -EPROBE_DEFER;
    }

    let err = unsafe { crate::i915_driver_upstream::i915_driver_probe(pdev, device, info) };
    if err != 0 {
        return err;
    }

    // CONFIG_DRM_I915_SELFTEST is off: i915_live_selftests() and
    // i915_perf_selftests() are the static inline `return 0` stubs.
    0
}

/// `i915_pci_shutdown()`.
///
/// # Safety
/// `pdev` must be a PCI device bound to this driver.
pub unsafe fn i915_pci_shutdown(pdev: *mut c_void) {
    let i915 = unsafe { pdev_to_i915(pdev) };
    unsafe { crate::i915_driver_upstream::i915_driver_shutdown(i915) };
}

/// `i915_pci_register_driver()`.
pub fn i915_pci_register_driver() -> c_int {
    unsafe { provider::pci_register_driver((&I915_PCI_DRIVER as *const I915PciDriver).cast()) }
}

/// `i915_pci_unregister_driver()`.
pub fn i915_pci_unregister_driver() {
    unsafe { provider::pci_unregister_driver((&I915_PCI_DRIVER as *const I915PciDriver).cast()) }
}

/// `struct pci_driver i915_pci_driver` (i915_pci.c). The entry points take
/// the matched `driver_data` pointer (`*const IntelDeviceInfo`) in place of
/// `const struct pci_device_id *ent`, because the device id is passed beside it.
#[repr(C)]
pub struct I915PciDriver {
    pub name: *const c_char,
    pub probe: unsafe extern "C" fn(pdev: *mut c_void, device: u16, info: *const IntelDeviceInfo) -> c_int,
    pub remove: unsafe extern "C" fn(pdev: *mut c_void),
    pub shutdown: unsafe extern "C" fn(pdev: *mut c_void),
}
unsafe impl Sync for I915PciDriver {}

unsafe extern "C" fn i915_pci_probe_entry(
    pdev: *mut c_void,
    device: u16,
    info: *const IntelDeviceInfo,
) -> c_int {
    unsafe { i915_pci_probe(pdev, device, &*info) }
}

unsafe extern "C" fn i915_pci_remove_entry(pdev: *mut c_void) {
    unsafe { i915_pci_remove(pdev) }
}

unsafe extern "C" fn i915_pci_shutdown_entry(pdev: *mut c_void) {
    unsafe { i915_pci_shutdown(pdev) }
}

/// `i915_pci_driver` (`.name = DRIVER_NAME`, `.id_table = pciidlist`,
/// `.probe`, `.remove`, `.shutdown`). `.driver.pm` is installed by the
/// kernel PM owner with the i915 PM ops.
pub static I915_PCI_DRIVER: I915PciDriver = I915PciDriver {
    name: c"i915".as_ptr(),
    probe: i915_pci_probe_entry,
    remove: i915_pci_remove_entry,
    shutdown: i915_pci_shutdown_entry,
};
