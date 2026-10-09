// SPDX-License-Identifier: MIT
// Copyright © 2016-2019 Intel Corporation.
// Source: Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_uc_fw.c.
// This is a source-order Rust translation; Linux firmware/GEM services remain
// LinuxKPI boundaries, while uC policy and validation stay here.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{
    ffi::{CStr, c_char, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    i915_gem_lmem_upstream::i915_gem_object_is_lmem,
    i915_gem_object_api_upstream::{
        i915_gem_object_has_pinned_pages, i915_gem_object_put, i915_gem_object_unpin_map,
        i915_gem_object_unpin_pages,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, I915_BO_ALLOC_PM_EARLY},
    i915_gem_pages_upstream::{
        drm_clflush_sg, i915_gem_object_pin_map_unlocked, i915_gem_object_pin_pages_unlocked,
        sg_next, sg_page,
    },
    i915_gem_shmem_upstream::i915_gem_object_create_shmem_from_data,
    i915_vma_api_upstream::i915_vma_unpin_and_release,
    i915_vma_resource_types_upstream::I915VmaResource,
    i915_vma_types_upstream::I915Vma,
    intel_gsc_uc_types_upstream::IntelGscUc,
    intel_gt_types_upstream::{GT_MEDIA, IntelGt},
    intel_gtt_api_upstream::{I915Ggtt, PTE_LM},
    intel_guc_types_upstream::IntelGuc,
    intel_huc_types_upstream::IntelHuc,
    intel_uc_fw_types_upstream::{
        __intel_uc_fw_status, INTEL_UC_FIRMWARE_AVAILABLE, INTEL_UC_FIRMWARE_DISABLED,
        INTEL_UC_FIRMWARE_ERROR, INTEL_UC_FIRMWARE_INIT_FAIL, INTEL_UC_FIRMWARE_LOAD_FAIL,
        INTEL_UC_FIRMWARE_LOADABLE, INTEL_UC_FIRMWARE_MISSING, INTEL_UC_FIRMWARE_NOT_SUPPORTED,
        INTEL_UC_FIRMWARE_RUNNING, INTEL_UC_FIRMWARE_SELECTED, INTEL_UC_FIRMWARE_TRANSFERRED,
        INTEL_UC_FIRMWARE_UNINITIALIZED, INTEL_UC_FW_NUM_TYPES, INTEL_UC_FW_TYPE_GSC,
        INTEL_UC_FW_TYPE_GUC, INTEL_UC_FW_TYPE_HUC, INTEL_UC_RSVD_GGTT_PER_FW, IntelUcFw,
        IntelUcFwFile, IntelUcFwType, IntelUcFwVersion,
    },
    intel_uc_types_upstream::IntelUc,
    intel_uncore_types_upstream::{
        FORCEWAKE_ALL, IntelUncore, intel_uncore_forcewake_get, intel_uncore_forcewake_put,
        intel_uncore_read_fw, intel_uncore_write_fw, intel_wait_for_register_fw,
    },
    intel_wopcm_types_upstream::IntelWopcm,
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        config::{E2BIG, EINVAL, ENOENT, EPROTO, PAGE_SHIFT, PAGE_SIZE},
        gem::DrmGemObject,
        i915::{IS_PLATFORM, IntelDeviceInfoOverlay, i915_gem_get_pat_index},
        i915_private::DrmI915Private,
    },
};

const UC_FW_DEBUG_GUC: bool = false;
const INTEL_UC_FIRMWARE_URL: &str =
    "https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/tree/i915";
const ENABLE_GUC_MASK: i32 = 1 << 0;
const ENABLE_GUC_LOAD_HUC: i32 = 1 << 1;
const INTEL_UC_RSVD_GGTT_PER_FW_U32: u32 = 2 * 1024 * 1024;
const ENODATA: i32 = 61;
const ENOEXEC: i32 = 8;
const SZ_1K: usize = 1024;
const SZ_8M: u32 = 8 * 1024 * 1024;
const I915_CACHE_NONE: u32 = 0;
const INTEL_SKYLAKE: u32 = 22;
const INTEL_BROXTON: u32 = 23;
const INTEL_KABYLAKE: u32 = 24;
const INTEL_GEMINILAKE: u32 = 25;
const INTEL_COFFEELAKE: u32 = 26;
const INTEL_COMETLAKE: u32 = 27;
const INTEL_ICELAKE: u32 = 28;
const INTEL_ELKHARTLAKE: u32 = 29;
const INTEL_JASPERLAKE: u32 = 30;
const INTEL_TIGERLAKE: u32 = 31;
const INTEL_ROCKETLAKE: u32 = 32;
const INTEL_DG1: u32 = 33;
const INTEL_ALDERLAKE_S: u32 = 34;
const INTEL_ALDERLAKE_P: u32 = 35;
const INTEL_DG2: u32 = 36;
const INTEL_METEORLAKE: u32 = 37;

macro_rules! uc_gt_log {
    ($severity:expr, $gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let __gt = $gt;
        let __id = unsafe { (*__gt).info.id };
        let __args: &[&dyn crate::linux_print::CFormatArg] = &[
            $(&($argument) as &dyn crate::linux_print::CFormatArg),*
        ];
        let __message = crate::linux_print::format_message($format, __args);
        crate::linux_print::drm_log_at(
            $severity,
            &alloc::format!("GT{__id}"),
            file!(),
            line!(),
            &__message,
        );
    }};
}
macro_rules! uc_gt_warn {
    ($gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        uc_gt_log!(crate::linux_print::DrmLogLevel::Warn, $gt, $format $(, $argument)*);
    }};
}
macro_rules! uc_gt_info {
    ($gt:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        uc_gt_log!(crate::linux_print::DrmLogLevel::Info, $gt, $format $(, $argument)*);
    }};
}

#[repr(C, packed)]
struct UcCssHeader {
    module_type: u32,
    header_size_dw: u32,
    header_version: u32,
    module_id: u32,
    module_vendor: u32,
    date: u32,
    size_dw: u32,
    key_size_dw: u32,
    modulus_size_dw: u32,
    exponent_size_dw: u32,
    time: u32,
    username: [u8; 8],
    buildnumber: [u8; 12],
    sw_version: u32,
    vf_version: u32,
    reserved0: [u32; 12],
    private_data_size: u32,
    header_info: u32,
}
const _: [(); 128] = [(); size_of::<UcCssHeader>()];

#[repr(C, packed)]
struct IntelGscVersion {
    major: u16,
    minor: u16,
    hotfix: u16,
    build: u16,
}
#[repr(C, packed)]
struct IntelGscManifestHeader {
    header_type: u32,
    header_length: u32,
    header_version: u32,
    flags: u32,
    vendor: u32,
    date: u32,
    size: u32,
    header_id: u32,
    internal_data: u32,
    fw_version: IntelGscVersion,
    security_version: u32,
    meu_kit_version: IntelGscVersion,
    meu_manifest_version: u32,
    general_data: [u8; 4],
    reserved3: [u8; 56],
    modulus_size: u32,
    exponent_size: u32,
}

#[repr(C)]
struct Firmware {
    size: usize,
    data: *const u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UcFwBlob {
    path: &'static CStr,
    legacy: bool,
    major: u8,
    minor: u8,
    patch: u8,
    has_gsc_headers: bool,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct UcFwRequirement {
    platform: u32,
    rev: u8,
    blob: UcFwBlob,
}
const fn blob(
    path: &'static CStr,
    major: u8,
    minor: u8,
    patch: u8,
    legacy: bool,
    gsc: bool,
) -> UcFwBlob {
    UcFwBlob {
        path,
        legacy,
        major,
        minor,
        patch,
        has_gsc_headers: gsc,
    }
}
const fn requirement(platform: u32, rev: u8, blob: UcFwBlob) -> UcFwRequirement {
    UcFwRequirement {
        platform,
        rev,
        blob,
    }
}

// This is the source table expansion of INTEL_GUC_FIRMWARE_DEFS and
// INTEL_HUC_FIRMWARE_DEFS, kept in the upstream newer-to-older ordering.
const GUC_BLOBS: &[UcFwRequirement] = &[
    requirement(
        INTEL_METEORLAKE,
        0,
        blob(c"i915/mtl_guc_70.bin", 70, 53, 0, false, false),
    ),
    requirement(
        INTEL_DG2,
        0,
        blob(c"i915/dg2_guc_70.bin", 70, 53, 0, false, false),
    ),
    requirement(
        INTEL_ALDERLAKE_P,
        0,
        blob(c"i915/adlp_guc_70.bin", 70, 12, 1, false, false),
    ),
    requirement(
        INTEL_ALDERLAKE_P,
        0,
        blob(c"i915/adlp_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        INTEL_ALDERLAKE_P,
        0,
        blob(c"i915/adlp_guc_69.0.3.bin", 69, 0, 3, true, false),
    ),
    requirement(
        INTEL_ALDERLAKE_S,
        0,
        blob(c"i915/tgl_guc_70.bin", 70, 12, 1, false, false),
    ),
    requirement(
        INTEL_ALDERLAKE_S,
        0,
        blob(c"i915/tgl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        INTEL_ALDERLAKE_S,
        0,
        blob(c"i915/tgl_guc_69.0.3.bin", 69, 0, 3, true, false),
    ),
    requirement(
        INTEL_DG1,
        0,
        blob(c"i915/dg1_guc_70.5.1.bin", 70, 5, 1, true, false),
    ),
    requirement(
        INTEL_ROCKETLAKE,
        0,
        blob(c"i915/tgl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        INTEL_TIGERLAKE,
        0,
        blob(c"i915/tgl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        INTEL_JASPERLAKE,
        0,
        blob(c"i915/ehl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        INTEL_ELKHARTLAKE,
        0,
        blob(c"i915/ehl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        INTEL_ICELAKE,
        0,
        blob(c"i915/icl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        27,
        5,
        blob(c"i915/cml_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        27,
        0,
        blob(c"i915/kbl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        26,
        0,
        blob(c"i915/kbl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        25,
        0,
        blob(c"i915/glk_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        24,
        0,
        blob(c"i915/kbl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        23,
        0,
        blob(c"i915/bxt_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
    requirement(
        22,
        0,
        blob(c"i915/skl_guc_70.1.1.bin", 70, 1, 1, true, false),
    ),
];
const HUC_BLOBS: &[UcFwRequirement] = &[
    requirement(
        INTEL_METEORLAKE,
        0,
        blob(c"i915/mtl_huc_gsc.bin", 1, 0, 0, false, true),
    ),
    requirement(
        INTEL_DG2,
        0,
        blob(c"i915/dg2_huc_gsc.bin", 1, 0, 0, false, true),
    ),
    requirement(
        INTEL_ALDERLAKE_P,
        0,
        blob(c"i915/tgl_huc.bin", 0, 0, 0, false, false),
    ),
    requirement(
        INTEL_ALDERLAKE_P,
        0,
        blob(c"i915/tgl_huc_7.9.3.bin", 7, 9, 3, true, false),
    ),
    requirement(
        INTEL_ALDERLAKE_S,
        0,
        blob(c"i915/tgl_huc.bin", 0, 0, 0, false, false),
    ),
    requirement(
        INTEL_ALDERLAKE_S,
        0,
        blob(c"i915/tgl_huc_7.9.3.bin", 7, 9, 3, true, false),
    ),
    requirement(
        INTEL_DG1,
        0,
        blob(c"i915/dg1_huc.bin", 0, 0, 0, false, false),
    ),
    requirement(
        INTEL_ROCKETLAKE,
        0,
        blob(c"i915/tgl_huc_7.9.3.bin", 7, 9, 3, true, false),
    ),
    requirement(
        INTEL_TIGERLAKE,
        0,
        blob(c"i915/tgl_huc_7.9.3.bin", 7, 9, 3, true, false),
    ),
    requirement(
        INTEL_JASPERLAKE,
        0,
        blob(c"i915/ehl_huc_9.0.0.bin", 9, 0, 0, true, false),
    ),
    requirement(
        INTEL_ELKHARTLAKE,
        0,
        blob(c"i915/ehl_huc_9.0.0.bin", 9, 0, 0, true, false),
    ),
    requirement(
        INTEL_ICELAKE,
        0,
        blob(c"i915/icl_huc_9.0.0.bin", 9, 0, 0, true, false),
    ),
    requirement(27, 5, blob(c"i915/cml_huc_4.0.0.bin", 4, 0, 0, true, false)),
    requirement(27, 0, blob(c"i915/kbl_huc_4.0.0.bin", 4, 0, 0, true, false)),
    requirement(26, 0, blob(c"i915/kbl_huc_4.0.0.bin", 4, 0, 0, true, false)),
    requirement(25, 0, blob(c"i915/glk_huc_4.0.0.bin", 4, 0, 0, true, false)),
    requirement(24, 0, blob(c"i915/kbl_huc_4.0.0.bin", 4, 0, 0, true, false)),
    requirement(23, 0, blob(c"i915/bxt_huc_2.0.0.bin", 2, 0, 0, true, false)),
    requirement(22, 0, blob(c"i915/skl_huc_2.0.0.bin", 2, 0, 0, true, false)),
];
const GSC_BLOBS: &[UcFwRequirement] = &[requirement(
    INTEL_METEORLAKE,
    0,
    blob(c"i915/mtl_gsc_1.bin", 1, 0, 0, false, true),
)];

#[inline]
fn type_repr(ty: IntelUcFwType) -> &'static str {
    match ty {
        INTEL_UC_FW_TYPE_GUC => "GuC",
        INTEL_UC_FW_TYPE_HUC => "HuC",
        INTEL_UC_FW_TYPE_GSC => "GSC",
        _ => "uC",
    }
}
#[inline]
fn status_repr(status: i32) -> &'static str {
    match status {
        INTEL_UC_FIRMWARE_NOT_SUPPORTED => "N/A",
        INTEL_UC_FIRMWARE_UNINITIALIZED => "UNINITIALIZED",
        INTEL_UC_FIRMWARE_DISABLED => "DISABLED",
        INTEL_UC_FIRMWARE_SELECTED => "SELECTED",
        INTEL_UC_FIRMWARE_MISSING => "MISSING",
        INTEL_UC_FIRMWARE_ERROR => "ERROR",
        INTEL_UC_FIRMWARE_AVAILABLE => "AVAILABLE",
        INTEL_UC_FIRMWARE_INIT_FAIL => "INIT FAIL",
        INTEL_UC_FIRMWARE_LOADABLE => "LOADABLE",
        INTEL_UC_FIRMWARE_LOAD_FAIL => "LOAD FAIL",
        INTEL_UC_FIRMWARE_TRANSFERRED => "TRANSFERRED",
        INTEL_UC_FIRMWARE_RUNNING => "RUNNING",
        _ => "<invalid>",
    }
}
#[inline]
unsafe fn set_status(uc_fw: *mut IntelUcFw, status: i32) {
    unsafe { (*uc_fw).status.__status = status };
}
#[inline]
unsafe fn selected_path(uc_fw: *const IntelUcFw) -> *const c_char {
    unsafe { (*uc_fw).file_selected.path }
}
#[inline]
unsafe fn path_str(path: *const c_char) -> alloc::string::String {
    if path.is_null() {
        alloc::string::String::from("<none>")
    } else {
        unsafe { CStr::from_ptr(path) }
            .to_string_lossy()
            .into_owned()
    }
}
#[inline]
unsafe fn same_firmware_path(left: *const c_char, right: *const c_char) -> bool {
    if left.is_null() || right.is_null() {
        left == right
    } else {
        unsafe { CStr::from_ptr(left).to_bytes() == CStr::from_ptr(right).to_bytes() }
    }
}
#[inline]
fn table_for_type(ty: IntelUcFwType) -> Option<&'static [UcFwRequirement]> {
    match ty {
        INTEL_UC_FW_TYPE_GUC => Some(GUC_BLOBS),
        INTEL_UC_FW_TYPE_HUC => Some(HUC_BLOBS),
        INTEL_UC_FW_TYPE_GSC => Some(GSC_BLOBS),
        _ => None,
    }
}
#[inline]
unsafe fn is_adlp_n(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { (*i915).info.cast::<IntelDeviceInfoOverlay>() };
    if info.is_null() {
        return false;
    }
    let id = unsafe { (*info).runtime.device_id };
    matches!(id, 0x46d0..=0x46d4)
}

unsafe fn has_gt_uc(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { (*i915).info.cast::<IntelDeviceInfoOverlay>() };
    !info.is_null() && unsafe { (*info).flags[1] & (1 << 3) != 0 }
}

unsafe fn platform_id(i915: *mut DrmI915Private) -> u32 {
    for platform in [
        INTEL_METEORLAKE,
        INTEL_DG2,
        INTEL_ALDERLAKE_P,
        INTEL_ALDERLAKE_S,
        INTEL_DG1,
        INTEL_ROCKETLAKE,
        INTEL_TIGERLAKE,
        INTEL_JASPERLAKE,
        INTEL_ELKHARTLAKE,
        INTEL_ICELAKE,
        INTEL_COMETLAKE,
        INTEL_COFFEELAKE,
        INTEL_GEMINILAKE,
        INTEL_KABYLAKE,
        INTEL_BROXTON,
        INTEL_SKYLAKE,
    ] {
        if unsafe { IS_PLATFORM(i915, platform) } {
            return platform;
        }
    }
    0
}

// Linux's request_firmware_nowarn/release_firmware are supplied by the
// firmware LinuxKPI adapter. The request is fail-closed when no provider is
// installed; callers preserve upstream retry/error state transitions.
unsafe extern "C" {
    fn tk_linux_firmware_request_nowarn(
        firmware: *mut *const Firmware,
        name: *const c_char,
        device: *mut c_void,
    ) -> i32;
    fn tk_linux_firmware_release(firmware: *const Firmware);
    fn intel_huc_fw_get_binary_info(uc_fw: *mut IntelUcFw, data: *const u8, size: usize) -> i32;
    fn intel_gsc_fw_get_binary_info(uc_fw: *mut IntelUcFw, data: *const u8, size: usize) -> i32;
    fn intel_guc_allocate_vma(guc: *mut IntelGuc, size: u32) -> *mut I915Vma;
}

// upstream: intel_uc_fw.c ____uc_fw_to_gt()
#[inline]
pub unsafe fn ____uc_fw_to_gt(uc_fw: *mut IntelUcFw, ty: IntelUcFwType) -> *mut IntelGt {
    GEM_BUG_ON!(ty >= INTEL_UC_FW_NUM_TYPES);
    let fw = uc_fw.cast::<u8>();
    let (owner, field) = match ty {
        INTEL_UC_FW_TYPE_GUC => (
            offset_of!(IntelGt, uc) + offset_of!(IntelUc, guc),
            offset_of!(IntelGuc, fw),
        ),
        INTEL_UC_FW_TYPE_HUC => (
            offset_of!(IntelGt, uc) + offset_of!(IntelUc, huc),
            offset_of!(IntelHuc, fw),
        ),
        INTEL_UC_FW_TYPE_GSC => (
            offset_of!(IntelGt, uc) + offset_of!(IntelUc, gsc),
            offset_of!(IntelGscUc, fw),
        ),
        _ => return ptr::null_mut(),
    };
    unsafe { fw.sub(owner + field).cast::<IntelGt>() }
}

// upstream: intel_uc_fw.c __uc_fw_to_gt()
#[inline]
pub unsafe fn __uc_fw_to_gt(uc_fw: *mut IntelUcFw) -> *mut IntelGt {
    GEM_BUG_ON!(unsafe { __intel_uc_fw_status(uc_fw) } == INTEL_UC_FIRMWARE_UNINITIALIZED);
    unsafe { ____uc_fw_to_gt(uc_fw, (*uc_fw).r#type) }
}

// upstream: intel_uc_fw.c intel_uc_fw_change_status()
pub unsafe fn intel_uc_fw_change_status(uc_fw: *mut IntelUcFw, status: i32) {
    unsafe { set_status(uc_fw, status) };
    if UC_FW_DEBUG_GUC {
        let gt = unsafe { __uc_fw_to_gt(uc_fw) };
        let state = if status == INTEL_UC_FIRMWARE_SELECTED {
            unsafe { path_str((*uc_fw).file_selected.path) }
        } else {
            alloc::string::String::from(status_repr(status))
        };
        gt_dbg!(
            gt,
            "%s firmware -> %s\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            state
        );
    }
}

// upstream: intel_uc_fw.c __uc_fw_auto_select()
unsafe fn __uc_fw_auto_select(i915: *mut DrmI915Private, uc_fw: *mut IntelUcFw) {
    let ty = unsafe { (*uc_fw).r#type };
    let Some(table) = table_for_type(ty) else {
        GEM_BUG_ON!(true);
        return;
    };
    let mut platform = unsafe { platform_id(i915) };
    if unsafe { is_adlp_n(i915) } {
        platform = INTEL_ALDERLAKE_S;
    }
    let revid = unsafe {
        (*(*i915).info.cast::<IntelDeviceInfoOverlay>())
            .runtime
            .step
            .graphics_step
    };
    GEM_BUG_ON!(table.len() > u32::MAX as usize);
    let mut found = false;
    for entry in table {
        if platform > entry.platform {
            break;
        }
        if platform != entry.platform || revid < entry.rev {
            continue;
        }
        let path = entry.blob.path.as_ptr();
        if !unsafe { (*uc_fw).file_selected.path }.is_null() {
            if unsafe { same_firmware_path((*uc_fw).file_selected.path, path) } {
                unsafe { (*uc_fw).file_selected.path = ptr::null() };
            }
            continue;
        }
        unsafe {
            (*uc_fw).file_selected.path = path;
            (*uc_fw).file_wanted.path = path;
            (*uc_fw).file_wanted.ver.major = entry.blob.major as u32;
            (*uc_fw).file_wanted.ver.minor = entry.blob.minor as u32;
            (*uc_fw).file_wanted.ver.patch = entry.blob.patch as u32;
            (*uc_fw).has_gsc_headers = entry.blob.has_gsc_headers;
        }
        found = true;
        break;
    }
    if !found && !unsafe { (*uc_fw).file_selected.path }.is_null() {
        unsafe { (*uc_fw).file_selected.path = ptr::null() };
    }
}

// upstream: intel_uc_fw.c validate_fw_table_type()
unsafe fn validate_fw_table_type(i915: *mut DrmI915Private, ty: IntelUcFwType) -> bool {
    let Some(table) = table_for_type(ty) else {
        drm_err!(
            unsafe { &mut (*i915).drm },
            "No blob array for %s\\n",
            type_repr(ty)
        );
        return false;
    };
    if table.is_empty() {
        return true;
    }
    for i in 1..table.len() {
        for j in i + 1..table.len() {
            if table[i].platform == table[j].platform
                && unsafe {
                    same_firmware_path(table[i].blob.path.as_ptr(), table[j].blob.path.as_ptr())
                }
            {
                drm_err!(
                    unsafe { &mut (*i915).drm },
                    "Duplicate %s blobs: %u r%u [%s] matches [%s]\\n",
                    type_repr(ty),
                    table[j].platform,
                    table[j].rev,
                    table[j].blob.path.to_str().unwrap_or(""),
                    table[i].blob.path.to_str().unwrap_or("")
                );
                return false;
            }
        }
        let previous = table[i - 1];
        let current = table[i];
        if current.platform < previous.platform
            || (current.platform == previous.platform && current.rev < previous.rev)
        {
            continue;
        }
        if current.platform != previous.platform || current.rev != previous.rev {
            return false;
        }
        if current.blob.major < previous.blob.major {
            continue;
        }
        if !current.blob.legacy && previous.blob.legacy {
            return false;
        }
        if current.blob.legacy && !previous.blob.legacy {
            if previous.blob.major == 0 || current.blob.major == previous.blob.major {
                continue;
            }
        }
        if current.blob.major != previous.blob.major {
            return false;
        }
        if current.blob.minor < previous.blob.minor {
            continue;
        }
        if current.blob.minor != previous.blob.minor {
            return false;
        }
        if current.blob.patch < previous.blob.patch {
            continue;
        }
        drm_err!(
            unsafe { &mut (*i915).drm },
            "Invalid %s blob order on platform %u rev %u\\n",
            type_repr(ty),
            current.platform,
            current.rev
        );
        return false;
    }
    true
}

// upstream: intel_uc_fw.c __override_guc_firmware_path()
unsafe fn __override_guc_firmware_path(i915: *mut DrmI915Private) -> *const c_char {
    if unsafe { (*i915).params.enable_guc } & ENABLE_GUC_MASK != 0 {
        unsafe { (*i915).params.guc_firmware_path }
    } else {
        c"".as_ptr()
    }
}
// upstream: intel_uc_fw.c __override_huc_firmware_path()
unsafe fn __override_huc_firmware_path(i915: *mut DrmI915Private) -> *const c_char {
    if unsafe { (*i915).params.enable_guc } & ENABLE_GUC_LOAD_HUC != 0 {
        unsafe { (*i915).params.huc_firmware_path }
    } else {
        c"".as_ptr()
    }
}
// upstream: intel_uc_fw.c __override_gsc_firmware_path()
unsafe fn __override_gsc_firmware_path(i915: *mut DrmI915Private) -> *const c_char {
    unsafe { (*i915).params.gsc_firmware_path }
}
// upstream: intel_uc_fw.c __uc_fw_user_override()
unsafe fn __uc_fw_user_override(i915: *mut DrmI915Private, uc_fw: *mut IntelUcFw) {
    let path = match unsafe { (*uc_fw).r#type } {
        INTEL_UC_FW_TYPE_GUC => unsafe { __override_guc_firmware_path(i915) },
        INTEL_UC_FW_TYPE_HUC => unsafe { __override_huc_firmware_path(i915) },
        INTEL_UC_FW_TYPE_GSC => unsafe { __override_gsc_firmware_path(i915) },
        _ => ptr::null(),
    };
    if !path.is_null() {
        unsafe {
            (*uc_fw).file_selected.path = path;
            (*uc_fw).user_overridden = true;
        }
    }
}

// upstream: intel_uc_fw.c intel_uc_fw_version_from_gsc_manifest()
pub unsafe fn intel_uc_fw_version_from_gsc_manifest(
    ver: *mut IntelUcFwVersion,
    data: *const c_void,
) {
    let manifest = data.cast::<IntelGscManifestHeader>();
    unsafe {
        (*ver).major = ptr::addr_of!((*manifest).fw_version.major).read_unaligned() as u32;
        (*ver).minor = ptr::addr_of!((*manifest).fw_version.minor).read_unaligned() as u32;
        (*ver).patch = ptr::addr_of!((*manifest).fw_version.hotfix).read_unaligned() as u32;
        (*ver).build = ptr::addr_of!((*manifest).fw_version.build).read_unaligned() as u32;
    }
}

// upstream: intel_uc_fw.c intel_uc_fw_init_early()
pub unsafe fn intel_uc_fw_init_early(
    uc_fw: *mut IntelUcFw,
    ty: IntelUcFwType,
    needs_ggtt_mapping: bool,
) {
    let gt = unsafe { ____uc_fw_to_gt(uc_fw, ty) };
    let i915 = unsafe { (*gt).i915 };
    GEM_BUG_ON!(ty >= INTEL_UC_FW_NUM_TYPES);
    GEM_BUG_ON!(unsafe { (*uc_fw).status.status } != 0);
    GEM_BUG_ON!(!unsafe { (*uc_fw).file_selected.path }.is_null());
    unsafe {
        (*uc_fw).r#type = ty;
        (*uc_fw).needs_ggtt_mapping = needs_ggtt_mapping;
    }
    if unsafe { has_gt_uc(i915) } {
        if !unsafe { validate_fw_table_type(i915, ty) } {
            unsafe {
                (*gt).uc.fw_table_invalid = true;
            }
            unsafe { intel_uc_fw_change_status(uc_fw, INTEL_UC_FIRMWARE_NOT_SUPPORTED) };
            return;
        }
        unsafe {
            __uc_fw_auto_select(i915, uc_fw);
            __uc_fw_user_override(i915, uc_fw);
        }
    }
    let path = unsafe { (*uc_fw).file_selected.path };
    let status = if path.is_null() {
        INTEL_UC_FIRMWARE_NOT_SUPPORTED
    } else if unsafe { *path == 0 } {
        INTEL_UC_FIRMWARE_DISABLED
    } else {
        INTEL_UC_FIRMWARE_SELECTED
    };
    unsafe { intel_uc_fw_change_status(uc_fw, status) };
}

// upstream: intel_uc_fw.c uc_unpack_css_version()
fn uc_unpack_css_version(ver: &mut IntelUcFwVersion, css_value: u32) {
    ver.major = (css_value >> 16) & 0xff;
    ver.minor = (css_value >> 8) & 0xff;
    ver.patch = css_value & 0xff;
}
// upstream: intel_uc_fw.c guc_read_css_info()
unsafe fn guc_read_css_info(uc_fw: *mut IntelUcFw, css: *const UcCssHeader) {
    let guc = unsafe {
        uc_fw
            .cast::<u8>()
            .sub(offset_of!(IntelGuc, fw))
            .cast::<IntelGuc>()
    };
    let mut submission = IntelUcFwVersion::default();
    let major = unsafe { (*uc_fw).file_selected.ver.major };
    let minor = unsafe { (*uc_fw).file_selected.ver.minor };
    if major >= 70 {
        if minor >= 6 {
            uc_unpack_css_version(&mut submission, unsafe {
                ptr::addr_of!((*css).vf_version).read_unaligned()
            });
        } else if minor >= 3 {
            submission.major = 1;
            submission.minor = 1;
        } else {
            submission.major = 1;
        }
    } else if major >= 69 {
        submission.minor = 10;
    } else {
        submission.minor = 1;
    }
    unsafe {
        (*guc).submission_version = submission;
    }
    unsafe {
        (*uc_fw).private_data_size = ptr::addr_of!((*css).private_data_size).read_unaligned();
    }
}

// upstream: intel_uc_fw.c __check_ccs_header()
unsafe fn __check_ccs_header(
    gt: *mut IntelGt,
    fw_data: *const u8,
    fw_size: usize,
    uc_fw: *mut IntelUcFw,
) -> i32 {
    let header_size = size_of::<UcCssHeader>();
    if fw_size < header_size {
        uc_gt_warn!(
            gt,
            "%s firmware %s: invalid size: %zu < %zu\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            unsafe { path_str((*uc_fw).file_selected.path) },
            fw_size,
            header_size
        );
        return -ENODATA;
    }
    let css = fw_data.cast::<UcCssHeader>();
    let header_dw = unsafe { ptr::addr_of!((*css).header_size_dw).read_unaligned() };
    let key_dw = unsafe { ptr::addr_of!((*css).key_size_dw).read_unaligned() };
    let mod_dw = unsafe { ptr::addr_of!((*css).modulus_size_dw).read_unaligned() };
    let exp_dw = unsafe { ptr::addr_of!((*css).exponent_size_dw).read_unaligned() };
    let calculated = header_dw
        .wrapping_sub(key_dw)
        .wrapping_sub(mod_dw)
        .wrapping_sub(exp_dw) as usize
        * 4;
    if calculated != header_size {
        uc_gt_warn!(
            gt,
            "%s firmware %s: unexpected header size: %zu != %zu\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            unsafe { path_str((*uc_fw).file_selected.path) },
            calculated,
            header_size
        );
        return -EPROTO;
    }
    let size_dw = unsafe { ptr::addr_of!((*css).size_dw).read_unaligned() };
    let ucode_size = size_dw.wrapping_sub(header_dw).wrapping_mul(4);
    let rsa_size = key_dw.wrapping_mul(4);
    unsafe {
        (*uc_fw).ucode_size = ucode_size;
        (*uc_fw).rsa_size = rsa_size;
    }
    let required = header_size + ucode_size as usize + rsa_size as usize;
    if fw_size < required {
        uc_gt_warn!(
            gt,
            "%s firmware %s: invalid size: %zu < %zu\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            unsafe { path_str((*uc_fw).file_selected.path) },
            fw_size,
            required
        );
        return -ENOEXEC;
    }
    let upload_size = header_size + ucode_size as usize;
    if upload_size >= unsafe { (*gt).wopcm.size as usize } {
        uc_gt_warn!(
            gt,
            "%s firmware %s: invalid size: %zu > %zu\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            unsafe { path_str((*uc_fw).file_selected.path) },
            upload_size,
            unsafe { (*gt).wopcm.size as usize }
        );
        return -E2BIG;
    }
    unsafe {
        uc_unpack_css_version(
            &mut (*uc_fw).file_selected.ver,
            ptr::addr_of!((*css).sw_version).read_unaligned(),
        );
    }
    if unsafe { (*uc_fw).r#type } == INTEL_UC_FW_TYPE_GUC {
        unsafe { guc_read_css_info(uc_fw, css) };
    }
    0
}

// upstream: intel_uc_fw.c check_gsc_manifest()
unsafe fn check_gsc_manifest(gt: *mut IntelGt, fw: *const Firmware, uc_fw: *mut IntelUcFw) -> i32 {
    let ret = match unsafe { (*uc_fw).r#type } {
        INTEL_UC_FW_TYPE_HUC => unsafe {
            intel_huc_fw_get_binary_info(uc_fw, (*fw).data, (*fw).size)
        },
        INTEL_UC_FW_TYPE_GSC => unsafe {
            intel_gsc_fw_get_binary_info(uc_fw, (*fw).data, (*fw).size)
        },
        _ => {
            gt_WARN_ONCE!(gt, true, "Unexpected uC firmware type %d\\n", unsafe {
                (*uc_fw).r#type
            });
            return -EINVAL;
        }
    };
    if ret != 0 {
        return ret;
    }
    let delta = unsafe { (*uc_fw).dma_start_offset } as usize;
    if delta != 0 {
        let data = unsafe { (*fw).data.add(delta) };
        let len = unsafe { (*fw).size }.wrapping_sub(delta);
        let _ = unsafe { __check_ccs_header(gt, data, len, uc_fw) };
    }
    0
}

// upstream: intel_uc_fw.c check_ccs_header()
unsafe fn check_ccs_header(gt: *mut IntelGt, fw: *const Firmware, uc_fw: *mut IntelUcFw) -> i32 {
    unsafe { __check_ccs_header(gt, (*fw).data, (*fw).size, uc_fw) }
}

// upstream: intel_uc_fw.c is_ver_8bit()
fn is_ver_8bit(ver: &IntelUcFwVersion) -> bool {
    ver.major < 0xff && ver.minor < 0xff && ver.patch < 0xff
}

// upstream: intel_uc_fw.c guc_check_version_range()
unsafe fn guc_check_version_range(uc_fw: *mut IntelUcFw) -> i32 {
    let guc = unsafe {
        uc_fw
            .cast::<u8>()
            .sub(offset_of!(IntelGuc, fw))
            .cast::<IntelGuc>()
    };
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    if !is_ver_8bit(unsafe { &(*uc_fw).file_selected.ver }) {
        uc_gt_warn!(
            gt,
            "%s firmware: invalid file version: 0x%02X:%02X:%02X\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            unsafe { (*uc_fw).file_selected.ver.major },
            unsafe { (*uc_fw).file_selected.ver.minor },
            unsafe { (*uc_fw).file_selected.ver.patch }
        );
        return -EINVAL;
    }
    if !is_ver_8bit(unsafe { &(*guc).submission_version }) {
        uc_gt_warn!(
            gt,
            "%s firmware: invalid submit version: 0x%02X:%02X:%02X\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            unsafe { (*guc).submission_version.major },
            unsafe { (*guc).submission_version.minor },
            unsafe { (*guc).submission_version.patch }
        );
        return -EINVAL;
    }
    0
}

// upstream: intel_uc_fw.c check_fw_header()
unsafe fn check_fw_header(gt: *mut IntelGt, fw: *const Firmware, uc_fw: *mut IntelUcFw) -> i32 {
    let err = if unsafe { (*uc_fw).has_gsc_headers } {
        unsafe { check_gsc_manifest(gt, fw, uc_fw) }
    } else {
        unsafe { check_ccs_header(gt, fw, uc_fw) }
    };
    if err != 0 {
        return err;
    }
    0
}

// upstream: intel_uc_fw.c try_firmware_load()
unsafe fn try_firmware_load(uc_fw: *mut IntelUcFw, fw: *mut *const Firmware) -> i32 {
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    let dev = unsafe { (*(*gt).i915).drm.dev };
    let path = unsafe { (*uc_fw).file_selected.path };
    let err = unsafe { tk_linux_firmware_request_nowarn(fw, path, dev) };
    if err != 0 {
        return err;
    }
    if unsafe { (*uc_fw).needs_ggtt_mapping && (**fw).size > INTEL_UC_RSVD_GGTT_PER_FW as usize } {
        gt_err!(
            gt,
            "%s firmware %s: size (%zuKB) exceeds max supported size (%uKB)\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            unsafe { path_str(path) },
            unsafe { (**fw).size / SZ_1K },
            INTEL_UC_RSVD_GGTT_PER_FW as usize / SZ_1K
        );
        unsafe {
            tk_linux_firmware_release(*fw);
            *fw = ptr::null();
        }
        return -ENOENT;
    }
    0
}

// upstream: intel_uc_fw.c check_mtl_huc_guc_compatibility()
unsafe fn check_mtl_huc_guc_compatibility(
    gt: *mut IntelGt,
    huc_selected: *mut IntelUcFwFile,
) -> i32 {
    let guc = unsafe { crate::intel_gt_api_upstream::gt_to_guc(gt) };
    let guc_selected = unsafe { ptr::addr_of!((*guc).fw.file_selected) };
    let huc_ver = unsafe { &(*huc_selected).ver };
    let guc_ver = unsafe { &(*guc_selected).ver };
    GEM_BUG_ON!(unsafe { (*huc_selected).path.is_null() || (*guc_selected).path.is_null() });
    let new_huc = huc_ver.major > 8
        || (huc_ver.major == 8 && huc_ver.minor > 5)
        || (huc_ver.major == 8 && huc_ver.minor == 5 && huc_ver.patch >= 1);
    let new_guc = guc_ver.major > 70 || (guc_ver.major == 70 && guc_ver.minor >= 7);
    if new_huc != new_guc {
        gt_notice!(
            gt,
            "HuC %u.%u.%u is incompatible with GuC %u.%u.%u\\n",
            huc_ver.major,
            huc_ver.minor,
            huc_ver.patch,
            guc_ver.major,
            guc_ver.minor,
            guc_ver.patch
        );
        uc_gt_info!(
            gt,
            "MTL GuC 70.7.0+ and HuC 8.5.1+ don't work with older releases\\n"
        );
        return -ENOEXEC;
    }
    0
}

// upstream: intel_uc_fw.c intel_uc_check_file_version()
pub unsafe fn intel_uc_check_file_version(uc_fw: *mut IntelUcFw, old_ver: *mut bool) -> i32 {
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    let i915 = unsafe { (*gt).i915 };
    let wanted = unsafe { &(*uc_fw).file_wanted };
    let selected = unsafe { &(*uc_fw).file_selected };
    if unsafe { IS_PLATFORM(i915, INTEL_METEORLAKE) && (*uc_fw).r#type == INTEL_UC_FW_TYPE_HUC } {
        let ret = unsafe {
            check_mtl_huc_guc_compatibility(gt, ptr::addr_of_mut!((*uc_fw).file_selected))
        };
        if ret != 0 {
            return ret;
        }
    }
    if wanted.ver.major == 0 || selected.ver.major == 0 {
        return 0;
    }
    if selected.ver.major != wanted.ver.major {
        gt_notice!(
            gt,
            "%s firmware %s: unexpected version: %u.%u != %u.%u\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            unsafe { path_str(selected.path) },
            selected.ver.major,
            selected.ver.minor,
            wanted.ver.major,
            wanted.ver.minor
        );
        if !unsafe { (*uc_fw).user_overridden } {
            return -ENOEXEC;
        }
    } else if !old_ver.is_null() {
        if selected.ver.minor < wanted.ver.minor {
            unsafe {
                *old_ver = true;
            }
        } else if selected.ver.minor == wanted.ver.minor && selected.ver.patch < wanted.ver.patch {
            unsafe {
                *old_ver = true;
            }
        }
    }
    0
}

// upstream: intel_uc_fw.c intel_uc_fw_fetch()
pub unsafe fn intel_uc_fw_fetch(uc_fw: *mut IntelUcFw) -> i32 {
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    let i915 = unsafe { (*gt).i915 };
    let mut file_ideal = unsafe { (*uc_fw).file_wanted };
    let mut obj: *mut DrmI915GemObject;
    let mut fw: *const Firmware = ptr::null();
    let mut old_ver = false;
    GEM_BUG_ON!(unsafe { (*gt).wopcm.size } == 0);
    GEM_BUG_ON!(!unsafe { crate::intel_uc_fw_types_upstream::intel_uc_fw_is_enabled(uc_fw) });
    let mut err = unsafe { try_firmware_load(uc_fw, &mut fw) };
    file_ideal = unsafe { (*uc_fw).file_wanted };
    if err != 0 && unsafe { (*uc_fw).user_overridden } {
        return unsafe { fetch_fail(uc_fw, fw, err) };
    }
    while err == -ENOENT {
        old_ver = true;
        unsafe {
            __uc_fw_auto_select(i915, uc_fw);
        }
        if unsafe { (*uc_fw).file_selected.path.is_null() } {
            unsafe {
                (*uc_fw).file_selected.path = file_ideal.path;
                (*uc_fw).file_wanted = file_ideal;
            }
            break;
        }
        err = unsafe { try_firmware_load(uc_fw, &mut fw) };
    }
    if err != 0 {
        return unsafe { fetch_fail(uc_fw, fw, err) };
    }
    err = unsafe { check_fw_header(gt, fw, uc_fw) };
    if err != 0 {
        return unsafe { fetch_fail(uc_fw, fw, err) };
    }
    if unsafe { (*uc_fw).r#type == INTEL_UC_FW_TYPE_GUC } {
        err = unsafe { guc_check_version_range(uc_fw) };
        if err != 0 {
            return unsafe { fetch_fail(uc_fw, fw, err) };
        }
    }
    err = unsafe { intel_uc_check_file_version(uc_fw, &mut old_ver) };
    if err != 0 {
        return unsafe { fetch_fail(uc_fw, fw, err) };
    }
    if old_ver && unsafe { (*uc_fw).file_selected.ver.major != 0 } {
        unsafe {
            (*uc_fw).file_wanted = file_ideal;
        }
        gt_notice!(
            gt,
            "%s firmware %s (%u.%u.%u) is recommended, but only %s (%u.%u.%u) was found\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            unsafe { path_str((*uc_fw).file_wanted.path) },
            unsafe { (*uc_fw).file_wanted.ver.major },
            unsafe { (*uc_fw).file_wanted.ver.minor },
            unsafe { (*uc_fw).file_wanted.ver.patch },
            unsafe { path_str((*uc_fw).file_selected.path) },
            unsafe { (*uc_fw).file_selected.ver.major },
            unsafe { (*uc_fw).file_selected.ver.minor },
            unsafe { (*uc_fw).file_selected.ver.patch }
        );
        uc_gt_info!(
            gt,
            "Consider updating your linux-firmware pkg or downloading from %s\\n",
            INTEL_UC_FIRMWARE_URL
        );
    }
    if unsafe { crate::linux::i915::HAS_LMEM(i915) } {
        obj = unsafe {
            crate::i915_gem_lmem_upstream::i915_gem_object_create_lmem_from_data(
                i915,
                (*fw).data.cast(),
                (*fw).size,
            )
        };
        if !crate::linux_config::IS_ERR(obj) {
            unsafe {
                (*obj).flags |= I915_BO_ALLOC_PM_EARLY;
            }
        }
    } else {
        obj = unsafe {
            i915_gem_object_create_shmem_from_data(i915, (*fw).data.cast(), (*fw).size as u64)
        };
    }
    if crate::linux_config::IS_ERR(obj) {
        err = crate::linux_config::PTR_ERR(obj);
        return unsafe { fetch_fail(uc_fw, fw, err) };
    }
    unsafe {
        (*uc_fw).obj = obj;
        (*uc_fw).size = (*fw).size;
        intel_uc_fw_change_status(uc_fw, INTEL_UC_FIRMWARE_AVAILABLE);
        tk_linux_firmware_release(fw);
    }
    0
}

unsafe fn fetch_fail(uc_fw: *mut IntelUcFw, fw: *const Firmware, err: i32) -> i32 {
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    let state = if err == -ENOENT {
        INTEL_UC_FIRMWARE_MISSING
    } else {
        INTEL_UC_FIRMWARE_ERROR
    };
    unsafe {
        intel_uc_fw_change_status(uc_fw, state);
    }
    gt_notice!(
        gt,
        "%s firmware %s: fetch failed %d\\n",
        type_repr(unsafe { (*uc_fw).r#type }),
        unsafe { path_str((*uc_fw).file_selected.path) },
        err
    );
    uc_gt_info!(
        gt,
        "%s firmware(s) can be downloaded from %s\\n",
        type_repr(unsafe { (*uc_fw).r#type }),
        INTEL_UC_FIRMWARE_URL
    );
    if !fw.is_null() {
        unsafe {
            tk_linux_firmware_release(fw);
        }
    }
    err
}

// upstream: intel_uc_fw.c uc_fw_ggtt_offset()
unsafe fn uc_fw_ggtt_offset(uc_fw: *mut IntelUcFw) -> u32 {
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    let ggtt = unsafe { (*gt).ggtt };
    let node = unsafe { ptr::addr_of_mut!((*ggtt).uc_fw) };
    let offset = unsafe { (*uc_fw).r#type as u32 }.wrapping_mul(INTEL_UC_RSVD_GGTT_PER_FW_U32);
    GEM_BUG_ON!(unsafe { (*gt).type_ == GT_MEDIA && (*gt).info.id > 1 });
    let offset = if unsafe { (*gt).type_ == GT_MEDIA } {
        offset + SZ_8M
    } else {
        offset
    };
    GEM_BUG_ON!(!unsafe { crate::linux::gem_memory::drm_mm_node_allocated(node) });
    let start = unsafe { (*node).start };
    let node_size = unsafe { (*node).size };
    GEM_BUG_ON!(start >> 32 != 0);
    GEM_BUG_ON!(start.wrapping_add(node_size).wrapping_sub(1) >> 32 != 0);
    let obj_size = unsafe { object_size((*uc_fw).obj) };
    GEM_BUG_ON!(offset as u64 + obj_size > node_size);
    GEM_BUG_ON!(obj_size > INTEL_UC_RSVD_GGTT_PER_FW as u64);
    (start + offset as u64) as u32
}

unsafe fn object_size(obj: *mut DrmI915GemObject) -> u64 {
    let offset = offset_of!(DrmI915GemObject, base) + offset_of!(DrmGemObject, size);
    unsafe { ptr::read_unaligned(obj.cast::<u8>().add(offset).cast::<u64>()) }
}

// upstream: intel_uc_fw.c uc_fw_bind_ggtt()
unsafe fn uc_fw_bind_ggtt(uc_fw: *mut IntelUcFw) {
    let obj = unsafe { (*uc_fw).obj };
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    let ggtt = unsafe { (*gt).ggtt };
    let resource = unsafe { ptr::addr_of_mut!((*uc_fw).vma_res) };
    if !unsafe { (*uc_fw).needs_ggtt_mapping } {
        return;
    }
    unsafe {
        (*resource).start = uc_fw_ggtt_offset(uc_fw) as u64;
        (*resource).node_size = object_size(obj);
        (*resource).bi.pages = (*obj).mm.pages;
    }
    GEM_BUG_ON!(!unsafe { i915_gem_object_has_pinned_pages(obj) });
    if unsafe { crate::i915_gem_object_upstream::i915_gem_object_has_struct_page(obj) } {
        unsafe {
            drm_clflush_sg((*resource).bi.pages);
        }
    }
    let mut pte_flags = 0;
    if unsafe { i915_gem_object_is_lmem(obj) } {
        pte_flags |= PTE_LM;
    }
    let vm = unsafe { ptr::addr_of_mut!((*ggtt).vm) };
    let pat = unsafe { i915_gem_get_pat_index((*vm).i915, I915_CACHE_NONE) };
    unsafe {
        if let Some(insert) = (*vm).raw_insert_entries {
            insert(vm, resource, pat, pte_flags);
        } else {
            (*vm).insert_entries.expect("GGTT insert_entries callback")(
                vm, resource, pat, pte_flags,
            );
        }
    }
}

// upstream: intel_uc_fw.c uc_fw_unbind_ggtt()
unsafe fn uc_fw_unbind_ggtt(uc_fw: *mut IntelUcFw) {
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    let ggtt = unsafe { (*gt).ggtt };
    let resource = unsafe { ptr::addr_of_mut!((*uc_fw).vma_res) };
    if unsafe { (*resource).node_size == 0 } {
        return;
    }
    let vm = unsafe { ptr::addr_of_mut!((*ggtt).vm) };
    unsafe {
        (*vm).clear_range.expect("GGTT clear_range callback")(
            vm,
            (*resource).start,
            (*resource).node_size,
        );
    }
}

const DMA_ADDR_0_LOW: u32 = 0xc300;
const DMA_ADDR_0_HIGH: u32 = 0xc304;
const DMA_ADDR_1_LOW: u32 = 0xc308;
const DMA_ADDR_1_HIGH: u32 = 0xc30c;
const DMA_COPY_SIZE: u32 = 0xc310;
const DMA_CTRL: u32 = 0xc314;
const START_DMA: u32 = 1;
const DMA_ADDRESS_SPACE_WOPCM: u32 = 7 << 16;

// upstream: intel_uc_fw.c uc_fw_xfer()
pub unsafe fn uc_fw_xfer(uc_fw: *mut IntelUcFw, dst_offset: u32, dma_flags: u32) -> i32 {
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    let uncore = unsafe { (*gt).uncore };
    intel_uncore_forcewake_get(uncore, FORCEWAKE_ALL);
    let offset = unsafe { (*uc_fw).vma_res.start + (*uc_fw).dma_start_offset as u64 };
    GEM_BUG_ON!(((offset >> 32) as u32) & 0xffff_0000 != 0);
    unsafe {
        intel_uncore_write_fw(
            uncore,
            I915RegT {
                reg: DMA_ADDR_0_LOW,
            },
            offset as u32,
        );
        intel_uncore_write_fw(
            uncore,
            I915RegT {
                reg: DMA_ADDR_0_HIGH,
            },
            (offset >> 32) as u32,
        );
        intel_uncore_write_fw(
            uncore,
            I915RegT {
                reg: DMA_ADDR_1_LOW,
            },
            dst_offset,
        );
        intel_uncore_write_fw(
            uncore,
            I915RegT {
                reg: DMA_ADDR_1_HIGH,
            },
            DMA_ADDRESS_SPACE_WOPCM,
        );
        intel_uncore_write_fw(
            uncore,
            I915RegT { reg: DMA_COPY_SIZE },
            size_of::<UcCssHeader>() as u32 + (*uc_fw).ucode_size,
        );
        intel_uncore_write_fw(
            uncore,
            I915RegT { reg: DMA_CTRL },
            REG_MASKED_FIELD_ENABLE!(dma_flags | START_DMA),
        );
    }
    let ret = unsafe {
        intel_wait_for_register_fw(
            uncore,
            I915RegT { reg: DMA_CTRL },
            START_DMA,
            0,
            100,
            ptr::null_mut(),
        )
    };
    if ret != 0 {
        let status = unsafe { intel_uncore_read_fw(uncore, I915RegT { reg: DMA_CTRL }) };
        gt_err!(
            gt,
            "DMA for %s fw failed, DMA_CTRL=%u\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            status
        );
    }
    unsafe {
        intel_uncore_write_fw(
            uncore,
            I915RegT { reg: DMA_CTRL },
            REG_MASKED_FIELD_DISABLE!(dma_flags),
        );
        intel_uncore_forcewake_put(uncore, FORCEWAKE_ALL);
    }
    ret
}

// upstream: intel_uc_fw.c intel_uc_fw_mark_load_failed()
pub unsafe fn intel_uc_fw_mark_load_failed(uc_fw: *mut IntelUcFw, err: i32) -> i32 {
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    GEM_BUG_ON!(!unsafe { intel_uc_fw_is_loadable(uc_fw) });
    gt_notice!(
        gt,
        "Failed to load %s firmware %s %d\\n",
        type_repr(unsafe { (*uc_fw).r#type }),
        unsafe { path_str((*uc_fw).file_selected.path) },
        err
    );
    unsafe {
        intel_uc_fw_change_status(uc_fw, INTEL_UC_FIRMWARE_LOAD_FAIL);
    }
    err
}

// upstream: intel_uc_fw.c intel_uc_fw_upload()
pub unsafe fn intel_uc_fw_upload(uc_fw: *mut IntelUcFw, dst_offset: u32, dma_flags: u32) -> i32 {
    GEM_BUG_ON!(unsafe { intel_uc_fw_is_loaded(uc_fw) });
    if !unsafe { intel_uc_fw_is_loadable(uc_fw) } {
        return -ENOEXEC;
    }
    let err = unsafe { uc_fw_xfer(uc_fw, dst_offset, dma_flags) };
    if err != 0 {
        return unsafe { intel_uc_fw_mark_load_failed(uc_fw, err) };
    }
    unsafe {
        intel_uc_fw_change_status(uc_fw, INTEL_UC_FIRMWARE_TRANSFERRED);
    }
    0
}

// upstream: intel_uc_fw.c uc_fw_need_rsa_in_memory()
#[inline]
fn uc_fw_need_rsa_in_memory(uc_fw: *const IntelUcFw) -> bool {
    unsafe { (*uc_fw).r#type == INTEL_UC_FW_TYPE_HUC || (*uc_fw).rsa_size > 256 }
}

// upstream: intel_uc_fw.c uc_fw_rsa_data_create()
unsafe fn uc_fw_rsa_data_create(uc_fw: *mut IntelUcFw) -> i32 {
    if !uc_fw_need_rsa_in_memory(uc_fw) {
        return 0;
    }
    let gt = unsafe { __uc_fw_to_gt(uc_fw) };
    let guc = unsafe { crate::intel_gt_api_upstream::gt_to_guc(gt) };
    GEM_BUG_ON!(unsafe { (*uc_fw).rsa_size as usize > PAGE_SIZE });
    let mut vma = unsafe { intel_guc_allocate_vma(guc, PAGE_SIZE as u32) };
    if crate::linux_config::IS_ERR(vma) {
        return crate::linux_config::PTR_ERR(vma);
    }
    let obj = unsafe { (*vma).obj };
    let map_type =
        unsafe { crate::intel_gt_api_upstream::intel_gt_coherent_map_type(gt, obj, true) };
    let vaddr = unsafe { i915_gem_object_pin_map_unlocked(obj, map_type) };
    if crate::linux_config::IS_ERR(vaddr) {
        unsafe {
            i915_vma_unpin_and_release(&mut vma, 0);
        }
        return crate::linux_config::PTR_ERR(vaddr);
    }
    let copied = unsafe { intel_uc_fw_copy_rsa(uc_fw, vaddr, (*vma).size as u32) };
    unsafe {
        i915_gem_object_unpin_map(obj);
    }
    if copied < unsafe { (*uc_fw).rsa_size as usize } {
        unsafe {
            i915_vma_unpin_and_release(&mut vma, 0);
        }
        return -crate::linux_config::ENOMEM;
    }
    unsafe {
        (*uc_fw).rsa_data = vma;
    }
    0
}

// upstream: intel_uc_fw.c uc_fw_rsa_data_destroy()
unsafe fn uc_fw_rsa_data_destroy(uc_fw: *mut IntelUcFw) {
    unsafe {
        i915_vma_unpin_and_release(ptr::addr_of_mut!((*uc_fw).rsa_data), 0);
    }
}

// upstream: intel_uc_fw.c intel_uc_fw_init()
pub unsafe fn intel_uc_fw_init(uc_fw: *mut IntelUcFw) -> i32 {
    GEM_BUG_ON!(unsafe { intel_uc_fw_is_loaded(uc_fw) });
    if !unsafe { crate::intel_uc_fw_types_upstream::intel_uc_fw_is_available(uc_fw) } {
        return -ENOEXEC;
    }
    let obj = unsafe { (*uc_fw).obj };
    let mut err = unsafe { i915_gem_object_pin_pages_unlocked(obj) };
    if err != 0 {
        let gt = unsafe { __uc_fw_to_gt(uc_fw) };
        gt_dbg!(
            gt,
            "%s fw pin-pages failed %d\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            err
        );
        return err;
    }
    err = unsafe { uc_fw_rsa_data_create(uc_fw) };
    if err != 0 {
        let gt = unsafe { __uc_fw_to_gt(uc_fw) };
        gt_dbg!(
            gt,
            "%s fw rsa data creation failed %d\\n",
            type_repr(unsafe { (*uc_fw).r#type }),
            err
        );
        unsafe {
            i915_gem_object_unpin_pages(obj);
        }
        return err;
    }
    unsafe {
        uc_fw_bind_ggtt(uc_fw);
    }
    0
}

// upstream: intel_uc_fw.c intel_uc_fw_fini()
pub unsafe fn intel_uc_fw_fini(uc_fw: *mut IntelUcFw) {
    unsafe {
        uc_fw_unbind_ggtt(uc_fw);
        uc_fw_rsa_data_destroy(uc_fw);
    }
    let obj = unsafe { (*uc_fw).obj };
    if unsafe { i915_gem_object_has_pinned_pages(obj) } {
        unsafe {
            i915_gem_object_unpin_pages(obj);
        }
    }
    unsafe {
        intel_uc_fw_change_status(uc_fw, INTEL_UC_FIRMWARE_AVAILABLE);
    }
}

// upstream: intel_uc_fw.c intel_uc_fw_resume_mapping()
pub unsafe fn intel_uc_fw_resume_mapping(uc_fw: *mut IntelUcFw) {
    if !unsafe { crate::intel_uc_fw_types_upstream::intel_uc_fw_is_available(uc_fw) } {
        return;
    }
    if !unsafe { i915_gem_object_has_pinned_pages((*uc_fw).obj) } {
        return;
    }
    unsafe {
        uc_fw_bind_ggtt(uc_fw);
    }
}

// upstream: intel_uc_fw.c intel_uc_fw_cleanup_fetch()
pub unsafe fn intel_uc_fw_cleanup_fetch(uc_fw: *mut IntelUcFw) {
    if !unsafe { crate::intel_uc_fw_types_upstream::intel_uc_fw_is_available(uc_fw) } {
        return;
    }
    let obj = unsafe { ptr::replace(ptr::addr_of_mut!((*uc_fw).obj), ptr::null_mut()) };
    unsafe {
        i915_gem_object_put(obj);
        intel_uc_fw_change_status(uc_fw, INTEL_UC_FIRMWARE_SELECTED);
    }
}

unsafe extern "C" {
    fn io_mapping_map_atomic_wc(mapping: *mut c_void, offset: isize) -> *mut c_void;
    fn io_mapping_unmap_atomic(address: *mut c_void);
    fn memcpy_fromio(destination: *mut c_void, source: *const c_void, length: usize);
}

// upstream: intel_uc_fw.c intel_uc_fw_copy_rsa()
pub unsafe fn intel_uc_fw_copy_rsa(uc_fw: *mut IntelUcFw, dst: *mut c_void, max_len: u32) -> usize {
    let obj = unsafe { (*uc_fw).obj };
    let mr = unsafe { (*obj).mm.region };
    let mut size = core::cmp::min(unsafe { (*uc_fw).rsa_size }, max_len) as usize;
    let mut offset = unsafe {
        (*uc_fw).dma_start_offset + size_of::<UcCssHeader>() as u32 + (*uc_fw).ucode_size
    };
    let mut count = 0usize;
    GEM_BUG_ON!(!unsafe { crate::intel_uc_fw_types_upstream::intel_uc_fw_is_available(uc_fw) });
    let mut index = offset >> PAGE_SHIFT;
    offset &= PAGE_SIZE as u32 - 1;
    if unsafe { crate::i915_gem_object_upstream::i915_gem_object_has_struct_page(obj) } {
        let pages = unsafe { (*obj).mm.pages };
        let mut sg = unsafe { (*pages).sgl };
        while !sg.is_null() && size != 0 {
            let first = unsafe { sg_page(sg) };
            let first_pfn = unsafe { crate::linux::page::page_to_pfn(first) };
            let entry_offset = unsafe { (*sg).offset } as usize;
            let entry_len = unsafe { (*sg).length } as usize;
            let page_count = (entry_offset + entry_len + PAGE_SIZE - 1) / PAGE_SIZE;
            for page_index in 0..page_count {
                if index != 0 {
                    index -= 1;
                    continue;
                }
                let page =
                    crate::linux::shmem::pfn_to_page(first_pfn + page_index as core::ffi::c_ulong);
                assert!(!page.is_null(), "LinuxKPI PFN-to-page missed an SG page");
                let address = unsafe { crate::linux::highmem::page_address(page) }.cast::<u8>();
                let len = core::cmp::min(size, PAGE_SIZE - offset as usize);
                unsafe {
                    ptr::copy_nonoverlapping(
                        address.add(offset as usize),
                        dst.cast::<u8>().add(count),
                        len,
                    );
                }
                offset = 0;
                size -= len;
                count += len;
                if size == 0 {
                    break;
                }
            }
            sg = unsafe { sg_next(sg) };
        }
    } else {
        let pages = unsafe { (*obj).mm.pages };
        let mut iter = unsafe { crate::intel_gtt_api_upstream::for_each_sgt_daddr(pages) };
        while let Some(addr) = iter.next() {
            if index != 0 {
                index -= 1;
                continue;
            }
            let len = core::cmp::min(size, PAGE_SIZE - offset as usize);
            let io_offset = addr.wrapping_sub(unsafe { (*mr).region.start }) as isize;
            let vaddr = unsafe {
                io_mapping_map_atomic_wc(ptr::addr_of_mut!((*mr).iomap).cast(), io_offset)
            };
            unsafe {
                memcpy_fromio(
                    dst.cast::<u8>().add(count).cast(),
                    vaddr.cast::<u8>().add(offset as usize).cast(),
                    len,
                );
            }
            unsafe {
                io_mapping_unmap_atomic(vaddr);
            }
            offset = 0;
            size -= len;
            count += len;
            if size == 0 {
                break;
            }
        }
    }
    count
}

// upstream: intel_uc_fw.c intel_uc_fw_dump()
pub unsafe fn intel_uc_fw_dump(
    uc_fw: *const IntelUcFw,
    printer: *mut crate::linux_print::DrmPrinter,
) {
    let selected = unsafe { &(*uc_fw).file_selected };
    let wanted = unsafe { &(*uc_fw).file_wanted };
    let ty = unsafe { (*uc_fw).r#type };
    let status = unsafe { __intel_uc_fw_status(uc_fw) };
    drm_printf!(printer, "%s firmware: %s\\n", type_repr(ty), unsafe {
        path_str(selected.path)
    });
    if selected.path != wanted.path {
        drm_printf!(
            printer,
            "%s firmware wanted: %s\\n",
            type_repr(ty),
            unsafe { path_str(wanted.path) }
        );
    }
    drm_printf!(printer, "\\tstatus: %s\\n", status_repr(status));
    let got_wanted = if selected.ver.major < wanted.ver.major {
        false
    } else if selected.ver.major == wanted.ver.major && selected.ver.minor < wanted.ver.minor {
        false
    } else if selected.ver.major == wanted.ver.major
        && selected.ver.minor == wanted.ver.minor
        && selected.ver.patch < wanted.ver.patch
    {
        false
    } else {
        true
    };
    if !got_wanted {
        drm_printf!(
            printer,
            "\\tversion: wanted %u.%u.%u, found %u.%u.%u\\n",
            wanted.ver.major,
            wanted.ver.minor,
            wanted.ver.patch,
            selected.ver.major,
            selected.ver.minor,
            selected.ver.patch
        );
    } else {
        drm_printf!(
            printer,
            "\\tversion: found %u.%u.%u\\n",
            selected.ver.major,
            selected.ver.minor,
            selected.ver.patch
        );
    }
    drm_printf!(printer, "\\tuCode: %u bytes\\n", unsafe {
        (*uc_fw).ucode_size
    });
    drm_printf!(printer, "\\tRSA: %u bytes\\n", unsafe { (*uc_fw).rsa_size });
}

#[inline]
pub unsafe fn intel_uc_fw_is_loadable(uc_fw: *const IntelUcFw) -> bool {
    unsafe { __intel_uc_fw_status(uc_fw) >= INTEL_UC_FIRMWARE_LOADABLE }
}
#[inline]
pub unsafe fn intel_uc_fw_is_loaded(uc_fw: *const IntelUcFw) -> bool {
    unsafe { __intel_uc_fw_status(uc_fw) >= INTEL_UC_FIRMWARE_TRANSFERRED }
}

/// `intel_uc_fw_get_upload_size()` and its internal helper from intel_uc_fw.h.
pub unsafe fn intel_uc_fw_get_upload_size(uc_fw: *const IntelUcFw) -> u32 {
    if !unsafe { crate::intel_uc_fw_types_upstream::intel_uc_fw_is_available(uc_fw) } {
        return 0;
    }
    size_of::<UcCssHeader>() as u32 + unsafe { (*uc_fw).ucode_size }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_tables_retain_adl_and_gsc_order() {
        assert_eq!(GUC_BLOBS[2].blob.path.to_bytes(), b"i915/adlp_guc_70.bin");
        assert_eq!(GUC_BLOBS[5].blob.path.to_bytes(), b"i915/tgl_guc_70.bin");
        assert_eq!(HUC_BLOBS[0].blob.path.to_bytes(), b"i915/mtl_huc_gsc.bin");
        assert!(HUC_BLOBS[0].blob.has_gsc_headers);
        assert_eq!(GSC_BLOBS[0].platform, INTEL_METEORLAKE);
    }

    #[test]
    fn version_range_checks_preserve_source_8_bit_bound() {
        assert!(is_ver_8bit(&IntelUcFwVersion {
            major: 254,
            minor: 0,
            patch: 1,
            build: 0
        }));
        assert!(!is_ver_8bit(&IntelUcFwVersion {
            major: 255,
            minor: 0,
            patch: 1,
            build: 0
        }));
    }
}
