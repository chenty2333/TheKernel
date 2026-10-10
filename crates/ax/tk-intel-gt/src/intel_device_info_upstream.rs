// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/intel_device_info.c, and
// intel_step_name() from drivers/gpu/drm/i915/intel_step.c (no other owner).
// The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::ffi::c_void;

use crate::{
    intel_device_info_types_upstream::*,
    i915_probe_provider_upstream as provider,
    i915_utils_upstream::i915_vtd_active,
    intel_pciids_upstream::*,
    linux::i915::{GRAPHICS_VER, IS_BROADWELL, IS_HASWELL, IntelIpVersion, IntelRuntimeInfo},
    linux::primitives::str_yes_no,
    linux_i915_private::DrmI915Private,
    linux_print::{CFormatArg, DrmLogLevel, drm_log_at, drm_printer, format_message},
};

/// `drm_info()` equivalent for the printk level used by this file.
macro_rules! drm_info {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let _ = $device;
        let __args: &[&dyn CFormatArg] = &[$(&($argument) as &dyn CFormatArg),*];
        let __message = format_message($format, __args);
        drm_log_at(DrmLogLevel::Info, "i915 DRM info", file!(), line!(), &__message);
    }};
}

/// `drm_printf(p, ...)` from drm_print.h; `p` is a printer object.
macro_rules! drm_printf_info {
    ($printer:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __args: &[&dyn CFormatArg] = &[$(&($argument) as &dyn CFormatArg),*];
        let __message = format_message($format, __args);
        crate::linux_print::drm_printer_write($printer as *mut drm_printer, &__message);
    }};
}

/// `platform_names[]` (intel_device_info.c). Entries left NULL in the C
/// designated initializer (INTEL_PLATFORM_UNINITIALIZED) are `None`.
static PLATFORM_NAMES: [Option<&str>; 38] = {
    let mut t: [Option<&str>; 38] = [None; 38];
    t[INTEL_I830 as usize] = Some("I830");
    t[INTEL_I845G as usize] = Some("I845G");
    t[INTEL_I85X as usize] = Some("I85X");
    t[INTEL_I865G as usize] = Some("I865G");
    t[INTEL_I915G as usize] = Some("I915G");
    t[INTEL_I915GM as usize] = Some("I915GM");
    t[INTEL_I945G as usize] = Some("I945G");
    t[INTEL_I945GM as usize] = Some("I945GM");
    t[INTEL_G33 as usize] = Some("G33");
    t[INTEL_PINEVIEW as usize] = Some("PINEVIEW");
    t[INTEL_I965G as usize] = Some("I965G");
    t[INTEL_I965GM as usize] = Some("I965GM");
    t[INTEL_G45 as usize] = Some("G45");
    t[INTEL_GM45 as usize] = Some("GM45");
    t[INTEL_IRONLAKE as usize] = Some("IRONLAKE");
    t[INTEL_SANDYBRIDGE as usize] = Some("SANDYBRIDGE");
    t[INTEL_IVYBRIDGE as usize] = Some("IVYBRIDGE");
    t[INTEL_VALLEYVIEW as usize] = Some("VALLEYVIEW");
    t[INTEL_HASWELL as usize] = Some("HASWELL");
    t[INTEL_BROADWELL as usize] = Some("BROADWELL");
    t[INTEL_CHERRYVIEW as usize] = Some("CHERRYVIEW");
    t[INTEL_SKYLAKE as usize] = Some("SKYLAKE");
    t[INTEL_BROXTON as usize] = Some("BROXTON");
    t[INTEL_KABYLAKE as usize] = Some("KABYLAKE");
    t[INTEL_GEMINILAKE as usize] = Some("GEMINILAKE");
    t[INTEL_COFFEELAKE as usize] = Some("COFFEELAKE");
    t[INTEL_COMETLAKE as usize] = Some("COMETLAKE");
    t[INTEL_ICELAKE as usize] = Some("ICELAKE");
    t[INTEL_ELKHARTLAKE as usize] = Some("ELKHARTLAKE");
    t[INTEL_JASPERLAKE as usize] = Some("JASPERLAKE");
    t[INTEL_TIGERLAKE as usize] = Some("TIGERLAKE");
    t[INTEL_ROCKETLAKE as usize] = Some("ROCKETLAKE");
    t[INTEL_DG1 as usize] = Some("DG1");
    t[INTEL_ALDERLAKE_S as usize] = Some("ALDERLAKE_S");
    t[INTEL_ALDERLAKE_P as usize] = Some("ALDERLAKE_P");
    t[INTEL_DG2 as usize] = Some("DG2");
    t[INTEL_METEORLAKE as usize] = Some("METEORLAKE");
    t
};

/// `intel_platform_name()`. An out-of-range or unnamed platform warns once
/// (WARN_ON_ONCE) and returns "<unknown>".
pub fn intel_platform_name(platform: u32) -> &'static str {
    let named = PLATFORM_NAMES.get(platform as usize).copied().flatten();
    match named {
        Some(name) => name,
        None => {
            axlog::warn!("intel_platform_name: unknown platform {}", platform);
            "<unknown>"
        }
    }
}

/// `intel_step_name()` from intel_step.c. STEP_NAME_LIST covers A0..J3
/// (enum values 1..=40); STEP_NONE and the FUTURE/FOREVER values print "**".
pub fn intel_step_name(step: u8) -> &'static str {
    const STEP_NAMES: [&str; 40] = [
        "A0", "A1", "A2", "A3", "B0", "B1", "B2", "B3", "C0", "C1", "C2", "C3", "D0", "D1",
        "D2", "D3", "E0", "E1", "E2", "E3", "F0", "F1", "F2", "F3", "G0", "G1", "G2", "G3",
        "H0", "H1", "H2", "H3", "I0", "I1", "I2", "I3", "J0", "J1", "J2", "J3",
    ];
    let step = step as usize;
    if step >= 1 && step <= STEP_NAMES.len() {
        STEP_NAMES[step - 1]
    } else {
        "**"
    }
}

/// `intel_device_info_print()`.
///
/// # Safety
/// `info` and `runtime` must be valid; `p` must be a live DRM printer.
pub unsafe fn intel_device_info_print(
    info: *const IntelDeviceInfo,
    runtime: *const IntelRuntimeInfo,
    p: *mut drm_printer,
) {
    let info = unsafe { &*info };
    let runtime = unsafe { &*runtime };
    let gfx = runtime.graphics.ip;
    let media = runtime.media.ip;

    if gfx.rel != 0 {
        drm_printf_info!(p, "graphics version: %u.%02u\n", gfx.ver as u32, gfx.rel as u32);
    } else {
        drm_printf_info!(p, "graphics version: %u\n", gfx.ver as u32);
    }

    if media.rel != 0 {
        drm_printf_info!(p, "media version: %u.%02u\n", media.ver as u32, media.rel as u32);
    } else {
        drm_printf_info!(p, "media version: %u\n", media.ver as u32);
    }

    drm_printf_info!(
        p,
        "graphics stepping: %s\n",
        intel_step_name(runtime.step.graphics_step)
    );
    drm_printf_info!(p, "media stepping: %s\n", intel_step_name(runtime.step.media_step));

    drm_printf_info!(p, "gt: %d\n", info.gt as i32);
    drm_printf_info!(p, "memory-regions: 0x%x\n", info.memory_regions);
    drm_printf_info!(p, "page-sizes: 0x%x\n", runtime.page_sizes);
    drm_printf_info!(p, "platform: %s\n", intel_platform_name(info.platform));
    drm_printf_info!(p, "ppgtt-size: %d\n", runtime.ppgtt_size as i32);
    drm_printf_info!(p, "ppgtt-type: %d\n", runtime.ppgtt_type);
    drm_printf_info!(p, "dma_mask_size: %u\n", info.dma_mask_size);

    // DEV_INFO_FOR_EACH_FLAG(PRINT_FLAG)
    for (name, bit) in DEV_INFO_FLAG_NAMES {
        drm_printf_info!(p, "%s: %s\n", name, str_yes_no(info.flag(bit)));
    }

    drm_printf_info!(p, "has_pooled_eu: %s\n", str_yes_no(runtime.has_pooled_eu));
}

/// Flag names in `DEV_INFO_FOR_EACH_FLAG` order, with their bit indexes.
const DEV_INFO_FLAG_NAMES: [(&str, u32); 37] = [
    ("is_mobile", DEV_INFO_FLAG_IS_MOBILE),
    ("require_force_probe", DEV_INFO_FLAG_REQUIRE_FORCE_PROBE),
    ("is_dgfx", DEV_INFO_FLAG_IS_DGFX),
    ("has_64bit_reloc", DEV_INFO_FLAG_HAS_64BIT_RELOC),
    ("has_64k_pages", DEV_INFO_FLAG_HAS_64K_PAGES),
    ("gpu_reset_clobbers_display", DEV_INFO_FLAG_GPU_RESET_CLOBBERS_DISPLAY),
    ("has_reset_engine", DEV_INFO_FLAG_HAS_RESET_ENGINE),
    ("has_3d_pipeline", DEV_INFO_FLAG_HAS_3D_PIPELINE),
    ("has_flat_ccs", DEV_INFO_FLAG_HAS_FLAT_CCS),
    ("has_global_mocs", DEV_INFO_FLAG_HAS_GLOBAL_MOCS),
    ("has_gmd_id", DEV_INFO_FLAG_HAS_GMD_ID),
    ("has_gt_uc", DEV_INFO_FLAG_HAS_GT_UC),
    ("has_heci_pxp", DEV_INFO_FLAG_HAS_HECI_PXP),
    ("has_heci_gscfi", DEV_INFO_FLAG_HAS_HECI_GSCFI),
    ("has_guc_deprivilege", DEV_INFO_FLAG_HAS_GUC_DEPRIVILEGE),
    ("has_guc_tlb_invalidation", DEV_INFO_FLAG_HAS_GUC_TLB_INVALIDATION),
    ("has_l3_ccs_read", DEV_INFO_FLAG_HAS_L3_CCS_READ),
    ("has_l3_dpf", DEV_INFO_FLAG_HAS_L3_DPF),
    ("has_llc", DEV_INFO_FLAG_HAS_LLC),
    ("has_logical_ring_contexts", DEV_INFO_FLAG_HAS_LOGICAL_RING_CONTEXTS),
    ("has_logical_ring_elsq", DEV_INFO_FLAG_HAS_LOGICAL_RING_ELSQ),
    ("has_media_ratio_mode", DEV_INFO_FLAG_HAS_MEDIA_RATIO_MODE),
    ("has_mslice_steering", DEV_INFO_FLAG_HAS_MSLICE_STEERING),
    ("has_oa_bpc_reporting", DEV_INFO_FLAG_HAS_OA_BPC_REPORTING),
    ("has_oa_slice_contrib_limits", DEV_INFO_FLAG_HAS_OA_SLICE_CONTRIB_LIMITS),
    ("has_oam", DEV_INFO_FLAG_HAS_OAM),
    ("has_one_eu_per_fuse_bit", DEV_INFO_FLAG_HAS_ONE_EU_PER_FUSE_BIT),
    ("has_pxp", DEV_INFO_FLAG_HAS_PXP),
    ("has_rc6", DEV_INFO_FLAG_HAS_RC6),
    ("has_rc6p", DEV_INFO_FLAG_HAS_RC6P),
    ("has_rps", DEV_INFO_FLAG_HAS_RPS),
    ("has_runtime_pm", DEV_INFO_FLAG_HAS_RUNTIME_PM),
    ("has_snoop", DEV_INFO_FLAG_HAS_SNOOP),
    ("has_coherent_ggtt", DEV_INFO_FLAG_HAS_COHERENT_GGTT),
    ("tuning_thread_rr_after_dep", DEV_INFO_FLAG_TUNING_THREAD_RR_AFTER_DEP),
    ("unfenced_needs_alignment", DEV_INFO_FLAG_UNFENCED_NEEDS_ALIGNMENT),
    ("hws_needs_physical", DEV_INFO_FLAG_HWS_NEEDS_PHYSICAL),
];

/// `find_devid()`.
fn find_devid(id: u16, p: &[u16]) -> bool {
    p.iter().any(|&entry| entry == id)
}

/// `intel_device_info_subplatform_init()`.
///
/// # Safety
/// `i915` must point to a live `DrmI915Private` whose `info` is set.
pub unsafe fn intel_device_info_subplatform_init(i915: *mut DrmI915Private) {
    let info = unsafe { &*((*i915).info as *const IntelDeviceInfo) };
    let platform = info.platform;
    let pi = (platform / PLATFORM_MASK_PBITS) as usize;
    let pb = platform % PLATFORM_MASK_PBITS + INTEL_SUBPLATFORM_BITS;
    let devid = unsafe { (*i915).runtime.device_id };
    let mut mask: u32 = 0;

    // Make sure IS_<platform> checks are working.
    unsafe { (*i915).runtime.platform_mask[pi] = 1 << pb };

    // Find and mark subplatform bits based on the PCI device id.
    if find_devid(devid, SUBPLATFORM_ULT_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_ULT;
    } else if find_devid(devid, SUBPLATFORM_ULX_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_ULX;
        if unsafe { IS_HASWELL(i915.cast::<c_void>()) || IS_BROADWELL(i915.cast::<c_void>()) } {
            // ULX machines are also considered ULT.
            mask |= 1 << INTEL_SUBPLATFORM_ULT;
        }
    } else if find_devid(devid, SUBPLATFORM_PORTF_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_PORTF;
    } else if find_devid(devid, SUBPLATFORM_UY_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_UY;
    } else if find_devid(devid, SUBPLATFORM_N_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_N;
    } else if find_devid(devid, SUBPLATFORM_RPL_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_RPL;
        if find_devid(devid, SUBPLATFORM_RPLU_IDS) {
            mask |= 1 << INTEL_SUBPLATFORM_RPLU;
        }
    } else if find_devid(devid, SUBPLATFORM_G10_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_G10;
    } else if find_devid(devid, SUBPLATFORM_G11_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_G11;
    } else if find_devid(devid, SUBPLATFORM_G12_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_G12;
    } else if find_devid(devid, SUBPLATFORM_ARL_H_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_ARL_H;
    } else if find_devid(devid, SUBPLATFORM_ARL_U_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_ARL_U;
    } else if find_devid(devid, SUBPLATFORM_ARL_S_IDS) {
        mask = 1 << INTEL_SUBPLATFORM_ARL_S;
    }

    // DG2_D ids span across multiple DG2 subplatforms.
    if find_devid(devid, SUBPLATFORM_DG2_D_IDS) {
        mask |= 1 << INTEL_SUBPLATFORM_D;
    }

    assert_eq!(mask & !INTEL_SUBPLATFORM_MASK, 0, "GEM_BUG_ON subplatform mask");
    unsafe { (*i915).runtime.platform_mask[pi] |= mask };
}

/// `ip_ver_read()`: peeks GMD_ID through BAR0 before MMIO is set up. The
/// register read is a PCI owner service; it is provided through the probe
/// provider table and fails closed when that table is not installed.
///
/// # Safety
/// `i915` must be a live `DrmI915Private`.
unsafe fn ip_ver_read(i915: *mut DrmI915Private, offset: u32, ip: *mut IntelIpVersion) {
    let ip = unsafe { &mut *ip };
    let expected_ver = ip.ver;
    let expected_rel = ip.rel;

    let mut val: u32 = 0;
    if !unsafe { provider::early_gmd_read(i915, offset, &mut val) } {
        // drm_WARN_ON(&i915->drm, !addr)
        axlog::warn!("i915 drm_WARN_ON: GMD_ID peek failed");
        return;
    }

    ip.ver = ((val & GMD_ID_ARCH_MASK) >> GMD_ID_ARCH_SHIFT) as u8;
    ip.rel = ((val & GMD_ID_RELEASE_MASK) >> GMD_ID_RELEASE_SHIFT) as u8;
    ip.step = (val & GMD_ID_STEP) as u8;

    // Sanity check against expected versions from device info.
    if IP_VER(ip.ver as u16, ip.rel as u16) < IP_VER(expected_ver as u16, expected_rel as u16) {
        drm_dbg!(
            core::ptr::null::<c_void>(),
            "Hardware reports GMD IP version %u.%u (REG[0x%x] = 0x%08x) but minimum expected is %u.%u\n",
            ip.ver as u32,
            ip.rel as u32,
            offset,
            val,
            expected_ver as u32,
            expected_rel as u32
        );
    }
}

/// `GMD_ID_ARCH_MASK` = REG_GENMASK(31, 22).
const GMD_ID_ARCH_MASK: u32 = 0xFFC0_0000;
const GMD_ID_ARCH_SHIFT: u32 = 22;
/// `GMD_ID_RELEASE_MASK` = REG_GENMASK(21, 14).
const GMD_ID_RELEASE_MASK: u32 = 0x003F_C000;
const GMD_ID_RELEASE_SHIFT: u32 = 14;
/// `GMD_ID_STEP` = REG_GENMASK(5, 0).
const GMD_ID_STEP: u32 = 0x0000_003F;
/// `GMD_ID_GRAPHICS` = `_MMIO(0xd8c)` (gt/intel_gt_regs.h).
pub const GMD_ID_GRAPHICS_OFFSET: u32 = 0xd8c;
/// `GMD_ID_MEDIA` = `_MMIO(MTL_MEDIA_GSI_BASE + 0xd8c)`.
pub const GMD_ID_MEDIA_OFFSET: u32 = 0x380000 + 0xd8c;

/// `PLATFORM_MASK_BITS` for `__platform_mask_index/bit`: BITS_PER_TYPE(u32) -
/// INTEL_SUBPLATFORM_BITS.
const PLATFORM_MASK_PBITS: u32 = 32 - INTEL_SUBPLATFORM_BITS;

/// `IP_VER(ver, rel)` from i915_drv.h.
#[allow(non_snake_case)]
const fn IP_VER(ver: u16, rel: u16) -> u16 {
    (ver << 8) | rel
}

/// `intel_ipver_early_init()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private`.
unsafe fn intel_ipver_early_init(i915: *mut DrmI915Private) {
    let has_gmd_id = {
        let info = unsafe { &*((*i915).info as *const IntelDeviceInfo) };
        info.flag(DEV_INFO_FLAG_HAS_GMD_ID)
    };
    if !has_gmd_id {
        let ver = unsafe { (*i915).runtime.graphics.ip.ver };
        if ver > 12 {
            axlog::warn!("i915 drm_WARN_ON: graphics ip version {} > 12", ver);
        }
        // On older platforms, graphics and media share the same ip version
        // and release.
        unsafe { (*i915).runtime.media.ip = (*i915).runtime.graphics.ip };
        return;
    }

    unsafe {
        ip_ver_read(
            i915,
            GMD_ID_GRAPHICS_OFFSET,
            core::ptr::addr_of_mut!((*i915).runtime.graphics.ip),
        )
    };

    // Wa_22012778468
    let platform = {
        let info = unsafe { &*((*i915).info as *const IntelDeviceInfo) };
        info.platform
    };
    let runtime = unsafe { &mut (*i915).runtime };
    if runtime.graphics.ip.ver == 0x0 && platform == INTEL_METEORLAKE {
        runtime.graphics.ip.ver = 12;
        runtime.graphics.ip.rel = 70;
    }

    unsafe {
        ip_ver_read(
            i915,
            GMD_ID_MEDIA_OFFSET,
            core::ptr::addr_of_mut!((*i915).runtime.media.ip),
        )
    };
}

/// `intel_device_info_runtime_init_early()`.
///
/// # Safety
/// `i915` must be a live `DrmI915Private` with `info` set.
pub unsafe fn intel_device_info_runtime_init_early(i915: *mut DrmI915Private) {
    unsafe {
        intel_ipver_early_init(i915);
        intel_device_info_subplatform_init(i915);
    }
}

/// `intel_device_info_runtime_init()`.
///
/// # Safety
/// `dev_priv` must be a live `DrmI915Private`; MMIO and PCH must be set up.
pub unsafe fn intel_device_info_runtime_init(dev_priv: *mut DrmI915Private) {
    if unsafe { GRAPHICS_VER(dev_priv.cast::<c_void>()) } == 6
        && unsafe { i915_vtd_active(dev_priv) }
    {
        drm_info!(core::ptr::null::<c_void>(), "Disabling ppGTT for VT-d support\n");
        unsafe { (*dev_priv).runtime.ppgtt_type = INTEL_PPGTT_NONE };
    }
}

/// `intel_device_info_driver_create()`: sets up INTEL_INFO() and the initial
/// runtime info from static data and the PCI device id.
///
/// # Safety
/// `i915` must point to writable `DrmI915Private` storage.
pub unsafe fn intel_device_info_driver_create(
    i915: *mut DrmI915Private,
    device_id: u16,
    match_info: *const IntelDeviceInfo,
) {
    // Setup INTEL_INFO().
    unsafe { (*i915).info = match_info.cast::<c_void>() };

    // Initialize initial runtime info from static const data and pdev.
    let info = unsafe { &*match_info };
    unsafe {
        (*i915).runtime = info.runtime;
        (*i915).runtime.device_id = device_id;
    }
}

/// `intel_driver_caps_print()`.
///
/// # Safety
/// `p` must be a live DRM printer.
pub unsafe fn intel_driver_caps_print(caps: *const IntelDriverCaps, p: *mut drm_printer) {
    let caps = unsafe { &*caps };
    drm_printf_info!(
        p,
        "Has logical contexts? %s\n",
        str_yes_no(caps.has_logical_contexts)
    );
    drm_printf_info!(p, "scheduler: 0x%x\n", caps.scheduler);
}
