// SPDX-License-Identifier: MIT
// Copyright © 2014-2019 Intel Corporation.
// Source: Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_ads.c.
// Rust source-order translation; GuC ABI records come from intel_guc_fwif.h.

#![allow(dead_code, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_char, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    intel_engine_cs_upstream::{I915_NUM_ENGINES, IntelEngineCs, intel_engine_context_size},
    intel_engine_regs_upstream::{RING_FORCE_TO_NONPRIV, RING_HWS_PGA, RING_IMR, RING_MODE_GEN7},
    intel_engine_types_upstream::I915_ENGINE_FIRST_RENDER_COMPUTE,
    intel_gt_api_upstream::{gt_to_guc, guc_to_gt},
    intel_gt_mcr_impl_upstream::intel_gt_mcr_get_nonterminated_steering,
    intel_gt_types_upstream::IntelGt,
    intel_guc_fwif_types_upstream::{
        GUC_BLITTER_CLASS, GUC_CAPTURE_LIST_CLASS_BLITTER, GUC_CAPTURE_LIST_CLASS_GSC_OTHER,
        GUC_CAPTURE_LIST_CLASS_RENDER_COMPUTE, GUC_CAPTURE_LIST_CLASS_VIDEO,
        GUC_CAPTURE_LIST_CLASS_VIDEOENHANCE, GUC_CAPTURE_LIST_INDEX_MAX, GUC_COMPUTE_CLASS,
        GUC_GENERIC_GT_SYSINFO_DOORBELL_COUNT_PER_SQIDI, GUC_GENERIC_GT_SYSINFO_MAX,
        GUC_GENERIC_GT_SYSINFO_SLICE_ENABLED, GUC_GENERIC_GT_SYSINFO_VDBOX_SFC_SUPPORT_MASK,
        GUC_GSC_OTHER_CLASS, GUC_MAX_ENGINE_CLASSES, GUC_MAX_INSTANCES_PER_CLASS,
        GUC_REGSET_MASKED, GUC_REGSET_NEEDS_STEERING, GUC_REGSET_STEERING_GROUP,
        GUC_REGSET_STEERING_INSTANCE, GUC_RENDER_CLASS, GUC_VIDEO_CLASS, GUC_VIDEOENHANCE_CLASS,
        engine_class_to_guc_class, guc_ads, guc_capture_type, guc_engine_usage, guc_gt_system_info,
        guc_mmio_reg, guc_mmio_reg_set, guc_policies,
    },
    intel_guc_types_upstream::IntelGuc,
    intel_uc_types_upstream::intel_uc_uses_guc_submission,
    intel_uncore_types_upstream::intel_uncore_read,
    intel_workarounds_types_upstream::{I915McrRegT, I915RegT, I915Wa},
    linux::{
        config::{ENOMEM, EOPNOTSUPP, GFP_KERNEL},
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, IP_VER, IS_DG2, IS_DGFX, IS_GFX_GT_IP_RANGE,
            IS_MEDIA_GT_IP_RANGE,
        },
        iosys_map::iosys_map_set_vaddr,
        memory::{kfree, kmalloc},
        registers::{GEN12_RCU_MODE, REG_FIELD_PREP},
    },
    linux_i915_private::DrmI915Private,
    linux_print::{DrmPrinter, drm_printer_write},
};

const PAGE_SIZE: u32 = 4096;
const GLOBAL_POLICY_DISABLE_ENGINE_RESET: u32 = 1;
const GLOBAL_POLICY_DEFAULT_DPC_PROMOTE_TIME_US: u32 = 500_000;
const GLOBAL_POLICY_MAX_NUM_WI: u32 = 15;
const ACTION_GLOBAL_SCHED_POLICY_CHANGE: u32 = 0x506;
const GUC_WORKAROUND_KLV_SERIALIZED_RA_MODE: u16 = 0x9001;
const GUC_WORKAROUND_KLV_BLOCK_INTERRUPTS_WHEN_MGSR_BLOCKED: u16 = 0x9002;
const GUC_WORKAROUND_KLV_AVOID_GFX_CLEAR_WHILE_ACTIVE: u16 = 0x9006;
const GUC_CAPTURE_LIST_INDEX_PF: u32 = 0;
const GUC_CAPTURE_LIST_INDEX_VF: u32 = 1;
const LNCFCMOCS_REG_COUNT: u32 = 32;
const GUC_CTB_SEND_FLAGS_NONE: u32 = 0;

#[repr(C, packed)]
struct GucAdsBlobFixed {
    ads: guc_ads,
    policies: guc_policies,
    system_info: guc_gt_system_info,
    engine_usage: guc_engine_usage,
    regset: [guc_mmio_reg; 0],
}

#[repr(C)]
struct TempRegset {
    registers: *mut guc_mmio_reg,
    storage: *mut guc_mmio_reg,
    storage_used: u32,
    storage_max: u32,
}

unsafe extern "C" {
    fn intel_guc_capture_getlist(
        guc: *mut IntelGuc,
        owner: u32,
        type_: u32,
        classid: u32,
        out: *mut *mut c_void,
    ) -> i32;
    fn intel_guc_capture_getlistsize(
        guc: *mut IntelGuc,
        owner: u32,
        type_: u32,
        classid: u32,
        size: *mut usize,
    ) -> i32;
    fn intel_guc_capture_getnullheader(
        guc: *mut IntelGuc,
        out: *mut *mut c_void,
        size: *mut usize,
    ) -> i32;
}

#[inline]
fn page_align(size: u32) -> u32 {
    size.wrapping_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

#[inline]
unsafe fn map_is_null(map: *const crate::linux::iosys_map::IosysMap) -> bool {
    unsafe { (*map).addr.vaddr.is_null() }
}

#[inline]
unsafe fn map_address(map: *const crate::linux::iosys_map::IosysMap) -> *mut u8 {
    unsafe { (*map).addr.vaddr.cast::<u8>() }
}

unsafe fn map_read_u32(map: *const crate::linux::iosys_map::IosysMap, offset: u32) -> u32 {
    let address = unsafe { map_address(map).add(offset as usize).cast::<u32>() };
    unsafe { ptr::read_volatile(address) }
}

unsafe fn map_write_u32(map: *mut crate::linux::iosys_map::IosysMap, offset: u32, value: u32) {
    let address = unsafe { map_address(map).add(offset as usize).cast::<u32>() };
    unsafe { ptr::write_volatile(address, value) };
}

unsafe fn map_copy_to(
    map: *mut crate::linux::iosys_map::IosysMap,
    offset: u32,
    source: *const u8,
    size: usize,
) {
    let destination = unsafe { map_address(map).add(offset as usize) };
    if unsafe { (*map).is_iomem } {
        for index in 0..size {
            unsafe { ptr::write_volatile(destination.add(index), ptr::read(source.add(index))) };
        }
    } else {
        unsafe { ptr::copy_nonoverlapping(source, destination, size) };
    }
}

unsafe fn map_memset(
    map: *mut crate::linux::iosys_map::IosysMap,
    offset: u32,
    value: u8,
    size: usize,
) {
    let destination = unsafe { map_address(map).add(offset as usize) };
    if unsafe { (*map).is_iomem } {
        for index in 0..size {
            unsafe { ptr::write_volatile(destination.add(index), value) };
        }
    } else {
        unsafe { ptr::write_bytes(destination, value, size) };
    }
}

unsafe fn map_submap(
    source: *const crate::linux::iosys_map::IosysMap,
    offset: u32,
    target: *mut crate::linux::iosys_map::IosysMap,
) {
    unsafe { ptr::write(target, ptr::read(source)) };
    let pointer = unsafe { map_address(source).add(offset as usize) };
    if unsafe { (*target).is_iomem } {
        unsafe { (*target).addr.vaddr_iomem = pointer.cast() };
    } else {
        unsafe { (*target).addr.vaddr = pointer.cast() };
    }
}

unsafe fn info_offset(field_offset: usize) -> u32 {
    field_offset as u32
}

unsafe fn ads_offset(field_offset: usize) -> u32 {
    (offset_of!(GucAdsBlobFixed, ads) + field_offset) as u32
}

// upstream: intel_guc_ads.c guc_ads_regset_size()
unsafe fn guc_ads_regset_size(guc: *mut IntelGuc) -> u32 {
    GEM_BUG_ON!(unsafe { (*guc).ads_regset_size == 0 });
    unsafe { (*guc).ads_regset_size }
}

// upstream: intel_guc_ads.c guc_ads_golden_ctxt_size()
unsafe fn guc_ads_golden_ctxt_size(guc: *mut IntelGuc) -> u32 {
    page_align(unsafe { (*guc).ads_golden_ctxt_size })
}

// upstream: intel_guc_ads.c guc_ads_waklv_size()
unsafe fn guc_ads_waklv_size(guc: *mut IntelGuc) -> u32 {
    page_align(unsafe { (*guc).ads_waklv_size })
}

// upstream: intel_guc_ads.c guc_ads_capture_size()
unsafe fn guc_ads_capture_size(guc: *mut IntelGuc) -> u32 {
    page_align(unsafe { (*guc).ads_capture_size })
}

// upstream: intel_guc_ads.c guc_ads_private_data_size()
unsafe fn guc_ads_private_data_size(guc: *mut IntelGuc) -> u32 {
    page_align(unsafe { (*guc).fw.private_data_size })
}

// upstream: intel_guc_ads.c guc_ads_regset_offset()
unsafe fn guc_ads_regset_offset(_guc: *mut IntelGuc) -> u32 {
    offset_of!(GucAdsBlobFixed, regset) as u32
}

// upstream: intel_guc_ads.c guc_ads_golden_ctxt_offset()
unsafe fn guc_ads_golden_ctxt_offset(guc: *mut IntelGuc) -> u32 {
    page_align(unsafe { guc_ads_regset_offset(guc) + guc_ads_regset_size(guc) })
}

// upstream: intel_guc_ads.c guc_ads_waklv_offset()
unsafe fn guc_ads_waklv_offset(guc: *mut IntelGuc) -> u32 {
    page_align(unsafe { guc_ads_golden_ctxt_offset(guc) + guc_ads_golden_ctxt_size(guc) })
}

// upstream: intel_guc_ads.c guc_ads_capture_offset()
unsafe fn guc_ads_capture_offset(guc: *mut IntelGuc) -> u32 {
    page_align(unsafe { guc_ads_waklv_offset(guc) + guc_ads_waklv_size(guc) })
}

// upstream: intel_guc_ads.c guc_ads_private_data_offset()
unsafe fn guc_ads_private_data_offset(guc: *mut IntelGuc) -> u32 {
    page_align(unsafe { guc_ads_capture_offset(guc) + guc_ads_capture_size(guc) })
}

// upstream: intel_guc_ads.c guc_ads_blob_size()
unsafe fn guc_ads_blob_size(guc: *mut IntelGuc) -> u32 {
    unsafe { guc_ads_private_data_offset(guc) + guc_ads_private_data_size(guc) }
}

// upstream: intel_guc_ads.c guc_policies_init()
unsafe fn guc_policies_init(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let reset = unsafe { (*i915.cast::<DrmI915Private>()).params.reset };
    let flags = if reset < 2 {
        GLOBAL_POLICY_DISABLE_ENGINE_RESET
    } else {
        0
    };
    unsafe {
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            (offset_of!(GucAdsBlobFixed, policies) + offset_of!(guc_policies, dpc_promote_time))
                as u32,
            GLOBAL_POLICY_DEFAULT_DPC_PROMOTE_TIME_US,
        );
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            (offset_of!(GucAdsBlobFixed, policies) + offset_of!(guc_policies, max_num_work_items))
                as u32,
            GLOBAL_POLICY_MAX_NUM_WI,
        );
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            (offset_of!(GucAdsBlobFixed, policies) + offset_of!(guc_policies, global_flags)) as u32,
            flags,
        );
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            (offset_of!(GucAdsBlobFixed, policies) + offset_of!(guc_policies, is_valid)) as u32,
            1,
        );
    }
}

// upstream: intel_guc_ads.c intel_guc_ads_print_policy_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ads_print_policy_info(
    guc: *mut IntelGuc,
    printer: *mut DrmPrinter,
) {
    if unsafe { map_is_null(ptr::addr_of!((*guc).ads_map)) } {
        return;
    }
    let base = offset_of!(GucAdsBlobFixed, policies) as u32;
    let dpc = unsafe {
        map_read_u32(
            ptr::addr_of!((*guc).ads_map),
            base + offset_of!(guc_policies, dpc_promote_time) as u32,
        )
    };
    let max_items = unsafe {
        map_read_u32(
            ptr::addr_of!((*guc).ads_map),
            base + offset_of!(guc_policies, max_num_work_items) as u32,
        )
    };
    let flags = unsafe {
        map_read_u32(
            ptr::addr_of!((*guc).ads_map),
            base + offset_of!(guc_policies, global_flags) as u32,
        )
    };
    drm_printer_write(
        printer,
        &alloc::format!(
            "Global scheduling policies:\n  DPC promote time   = {dpc}\n  Max num work items = \
             {max_items}\n  Flags              = {flags}\n"
        ),
    );
}

// upstream: intel_guc_ads.c guc_action_policies_update()
unsafe fn guc_action_policies_update(guc: *mut IntelGuc, policy_offset: u32) -> i32 {
    let action = [ACTION_GLOBAL_SCHED_POLICY_CHANGE, policy_offset];
    unsafe {
        crate::intel_guc_ct_upstream::intel_guc_ct_send(
            ptr::addr_of_mut!((*guc).ct),
            action.as_ptr(),
            action.len() as u32,
            ptr::null_mut(),
            0,
            GUC_CTB_SEND_FLAGS_NONE,
        )
    }
}

// upstream: intel_guc_ads.c intel_guc_global_policies_update()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_global_policies_update(guc: *mut IntelGuc) -> i32 {
    if unsafe { map_is_null(ptr::addr_of!((*guc).ads_map)) } {
        return -EOPNOTSUPP;
    }
    let offset = unsafe {
        map_read_u32(
            ptr::addr_of!((*guc).ads_map),
            ads_offset(offset_of!(guc_ads, scheduler_policies)),
        )
    };
    GEM_BUG_ON!(offset == 0);
    unsafe { guc_policies_init(guc) };
    if !unsafe {
        crate::intel_guc_types_upstream::intel_guc_is_fw_running(guc) && (*guc).ct.enabled
    } {
        return 0;
    }
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let mut ret = 0;
    with_intel_runtime_pm!(
        ptr::addr_of_mut!((*i915.cast::<DrmI915Private>()).runtime_pm),
        wakeref,
        {
            let _ = wakeref;
            ret = unsafe { guc_action_policies_update(guc, offset) };
        }
    );
    ret
}

// upstream: intel_guc_ads.c guc_mapping_table_init()
unsafe fn guc_mapping_table_init(
    gt: *mut IntelGt,
    info_map: *mut crate::linux::iosys_map::IosysMap,
) {
    for class in 0..GUC_MAX_ENGINE_CLASSES {
        for column in 0..GUC_MAX_INSTANCES_PER_CLASS {
            let offset = info_offset(
                offset_of!(guc_gt_system_info, mapping_table)
                    + class * GUC_MAX_INSTANCES_PER_CLASS
                    + column,
            );
            let address = unsafe { map_address(info_map).add(offset as usize) };
            unsafe { ptr::write_volatile(address, GUC_MAX_INSTANCES_PER_CLASS as u8) };
        }
    }
    for engine_id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[engine_id] };
        if engine.is_null() {
            continue;
        }
        let guc_class = engine_class_to_guc_class(unsafe { (*engine).class });
        let logical_index = unsafe { (*engine).logical_mask.trailing_zeros() as usize };
        if usize::from(guc_class) >= GUC_MAX_ENGINE_CLASSES
            || logical_index >= GUC_MAX_INSTANCES_PER_CLASS
        {
            continue;
        }
        let offset = info_offset(
            offset_of!(guc_gt_system_info, mapping_table)
                + usize::from(guc_class) * GUC_MAX_INSTANCES_PER_CLASS
                + logical_index,
        );
        let address = unsafe { map_address(info_map).add(offset as usize) };
        unsafe { ptr::write_volatile(address, (*engine).instance) };
    }
}

// upstream: intel_guc_ads.c guc_mmio_reg_cmp()
unsafe fn guc_mmio_reg_cmp(a: *const c_void, b: *const c_void) -> i32 {
    let left = unsafe { ptr::read_unaligned(ptr::addr_of!((*a.cast::<guc_mmio_reg>()).offset)) };
    let right = unsafe { ptr::read_unaligned(ptr::addr_of!((*b.cast::<guc_mmio_reg>()).offset)) };
    (left as i32).wrapping_sub(right as i32)
}

// Linux krealloc() behavior for the temporary ADS regset, including preserving
// the relative start of the current engine's sub-list when storage moves.
unsafe fn temp_regset_grow(regset: *mut TempRegset, new_max: u32) -> bool {
    let old = unsafe { (*regset).storage };
    let old_used = unsafe { (*regset).storage_used };
    let old_max = unsafe { (*regset).storage_max };
    let start = if old.is_null() || unsafe { (*regset).registers.is_null() } {
        0
    } else {
        unsafe { (*regset).registers.offset_from(old) as u32 }
    };
    let bytes = (new_max as usize).saturating_mul(size_of::<guc_mmio_reg>());
    let new_storage = crate::linux::memory::kmalloc(bytes, GFP_KERNEL).cast::<guc_mmio_reg>();
    if new_storage.is_null() {
        return false;
    }
    if !old.is_null() && old_used != 0 {
        unsafe { ptr::copy_nonoverlapping(old, new_storage, old_used as usize) };
    }
    unsafe {
        (*regset).storage = new_storage;
        (*regset).registers = new_storage.add(start as usize);
        (*regset).storage_max = new_max;
        if !old.is_null() {
            kfree(old);
        }
    }
    let _ = old_max;
    true
}

// upstream: intel_guc_ads.c __mmio_reg_add()
unsafe fn __mmio_reg_add(regset: *mut TempRegset, reg: *const guc_mmio_reg) -> *mut guc_mmio_reg {
    let pos = unsafe { (*regset).storage_used };
    if pos >= unsafe { (*regset).storage_max } {
        let bytes = page_align((pos + 1).saturating_mul(size_of::<guc_mmio_reg>() as u32));
        let max = bytes as usize / size_of::<guc_mmio_reg>();
        if !unsafe { temp_regset_grow(regset, max as u32) } {
            WARN_ONCE!(
                true,
                "Incomplete regset list: can't add register ({})\n",
                -ENOMEM
            );
            return ERR_PTR::<guc_mmio_reg>(-ENOMEM);
        }
    }
    let slot = unsafe { (*regset).storage.add(pos as usize) };
    unsafe {
        ptr::copy_nonoverlapping(reg, slot, 1);
        (*regset).storage_used += 1;
    }
    slot
}

// upstream: intel_guc_ads.c guc_mmio_reg_add()
unsafe fn guc_mmio_reg_add(
    _gt: *mut IntelGt,
    regset: *mut TempRegset,
    offset: u32,
    flags: u32,
) -> isize {
    let start = unsafe { (*regset).registers };
    let start_index = if unsafe { (*regset).storage.is_null() } {
        0
    } else {
        unsafe { start.offset_from((*regset).storage) as u32 }
    };
    let count = unsafe { (*regset).storage_used - start_index };
    let mut low = 0usize;
    let mut high = count as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let found = unsafe { ptr::read_unaligned(ptr::addr_of!((*start.add(mid)).offset)) };
        if found == offset {
            return 0;
        } else if found < offset {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    let entry = guc_mmio_reg {
        offset,
        value: 0,
        flags,
        mask: 0,
    };
    let slot = unsafe { __mmio_reg_add(regset, &entry) };
    if IS_ERR!(slot) {
        return PTR_ERR!(slot) as isize;
    }
    let mut at = unsafe { slot.offset_from((*regset).registers) };
    while at > 0 {
        let left = unsafe { (*regset).registers.add(at as usize - 1) };
        let right = unsafe { (*regset).registers.add(at as usize) };
        let left_offset = unsafe { ptr::read_unaligned(ptr::addr_of!((*left).offset)) };
        let right_offset = unsafe { ptr::read_unaligned(ptr::addr_of!((*right).offset)) };
        GEM_BUG_ON!(left_offset == right_offset);
        if right_offset > left_offset {
            break;
        }
        unsafe { ptr::swap(left, right) };
        at -= 1;
    }
    0
}

// upstream: intel_guc_ads.c guc_mcr_reg_add()
unsafe fn guc_mcr_reg_add(
    gt: *mut IntelGt,
    regset: *mut TempRegset,
    reg: I915McrRegT,
    flags: u32,
) -> isize {
    let mut group = 0;
    let mut instance = 0;
    unsafe {
        intel_gt_mcr_get_nonterminated_steering(gt, reg, &mut group, &mut instance);
    }
    let steering = REG_FIELD_PREP(GUC_REGSET_STEERING_GROUP, group as u32)
        | REG_FIELD_PREP(GUC_REGSET_STEERING_INSTANCE, instance as u32)
        | GUC_REGSET_NEEDS_STEERING;
    unsafe { guc_mmio_reg_add(gt, regset, reg.reg, flags | steering) }
}

// upstream: intel_guc_ads.c guc_mmio_regset_init()
unsafe fn guc_mmio_regset_init(regset: *mut TempRegset, engine: *mut IntelEngineCs) -> i32 {
    let gt = unsafe { (*engine).gt };
    let base = unsafe { (*engine).mmio_base };
    let wal = unsafe { ptr::addr_of!((*engine).wa_list) };
    let start = unsafe { (*regset).storage_used };
    unsafe {
        (*regset).registers = if (*regset).storage.is_null() {
            ptr::null_mut()
        } else {
            (*regset).storage.add(start as usize)
        };
    }
    let mut ret = 0isize;
    ret |= unsafe { guc_mmio_reg_add(gt, regset, RING_MODE_GEN7(base).reg, GUC_REGSET_MASKED) };
    ret |= unsafe { guc_mmio_reg_add(gt, regset, RING_HWS_PGA(base).reg, 0) };
    ret |= unsafe { guc_mmio_reg_add(gt, regset, RING_IMR(base).reg, 0) };
    if unsafe { (*engine).flags & I915_ENGINE_FIRST_RENDER_COMPUTE != 0 }
        && unsafe { (*gt).ccs.cslices } != 0
    {
        ret |= unsafe { guc_mmio_reg_add(gt, regset, GEN12_RCU_MODE.reg, GUC_REGSET_MASKED) };
    }
    for index in 0..unsafe { (*wal).count as usize } {
        let wa = unsafe { (*wal).list.add(index) };
        let mcr = unsafe { ptr::read_unaligned(ptr::addr_of!((*wa).reg.mcr_reg)) };
        let flags = if unsafe { (*wa).masked_reg() } {
            GUC_REGSET_MASKED
        } else {
            0
        };
        ret |= unsafe { guc_mcr_reg_add(gt, regset, mcr, flags) };
    }
    for index in 0..crate::intel_engine_regs_upstream::RING_MAX_NONPRIV_SLOTS {
        ret |= unsafe { guc_mmio_reg_add(gt, regset, RING_FORCE_TO_NONPRIV(base, index).reg, 0) };
    }
    let i915 = unsafe { (*engine).i915 };
    for index in 0..LNCFCMOCS_REG_COUNT {
        let offset = 0xb020 + index * 4;
        if unsafe { GRAPHICS_VER_FULL(i915) } >= IP_VER(12, 55) {
            ret |= unsafe { guc_mcr_reg_add(gt, regset, I915McrRegT { reg: offset }, 0) };
        } else {
            ret |= unsafe { guc_mmio_reg_add(gt, regset, offset, 0) };
        }
    }
    if unsafe { GRAPHICS_VER(i915) } >= 12 {
        for offset in [0xe458, 0xe558, 0xe658, 0xe758, 0xe45c, 0xe55c, 0xe65c] {
            ret |= unsafe { guc_mcr_reg_add(gt, regset, I915McrRegT { reg: offset }, 0) };
        }
    }
    if ret != 0 { -1 } else { 0 }
}

// upstream: intel_guc_ads.c guc_mmio_reg_state_create()
unsafe fn guc_mmio_reg_state_create(guc: *mut IntelGuc) -> isize {
    let gt = unsafe { guc_to_gt(guc) };
    let mut temp = TempRegset {
        registers: ptr::null_mut(),
        storage: ptr::null_mut(),
        storage_used: 0,
        storage_max: 0,
    };
    let mut total = 0isize;
    for id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[id] };
        if engine.is_null() {
            continue;
        }
        let used = unsafe { temp.storage_used };
        let ret = unsafe { guc_mmio_regset_init(&mut temp, engine) };
        if ret < 0 {
            unsafe { kfree(temp.storage) };
            return ret as isize;
        }
        unsafe { (*guc).ads_regset_count[id] = temp.storage_used - used };
        total +=
            (unsafe { (*guc).ads_regset_count[id] } * size_of::<guc_mmio_reg>() as u32) as isize;
    }
    unsafe { (*guc).ads_regset = temp.storage.cast() };
    total
}

// upstream: intel_guc_ads.c guc_mmio_reg_state_init()
unsafe fn guc_mmio_reg_state_init(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    let offset = unsafe { guc_ads_regset_offset(guc) };
    let mut addr_ggtt =
        unsafe { crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, (*guc).ads_vma) + offset };
    let regset_size = unsafe { (*guc).ads_regset_size as usize };
    unsafe {
        map_copy_to(
            ptr::addr_of_mut!((*guc).ads_map),
            offset,
            (*guc).ads_regset.cast(),
            regset_size,
        )
    };
    for id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[id] };
        if engine.is_null() {
            continue;
        }
        let count = unsafe { (*guc).ads_regset_count[id] };
        GEM_BUG_ON!(unsafe { (*engine).instance as usize >= GUC_MAX_INSTANCES_PER_CLASS });
        let guc_class = engine_class_to_guc_class(unsafe { (*engine).class });
        let element = usize::from(guc_class) * GUC_MAX_INSTANCES_PER_CLASS
            + unsafe { (*engine).instance as usize };
        let reg_addr = ads_offset(
            offset_of!(guc_ads, reg_state_list)
                + element * size_of::<guc_mmio_reg_set>()
                + offset_of!(guc_mmio_reg_set, address),
        );
        let reg_count = ads_offset(
            offset_of!(guc_ads, reg_state_list)
                + element * size_of::<guc_mmio_reg_set>()
                + offset_of!(guc_mmio_reg_set, count),
        );
        if count == 0 {
            unsafe {
                map_write_u32(ptr::addr_of_mut!((*guc).ads_map), reg_addr, 0);
                let address = map_address(ptr::addr_of!((*guc).ads_map))
                    .add(reg_count as usize)
                    .cast::<u16>();
                ptr::write_volatile(address, 0);
            }
            continue;
        }
        unsafe {
            map_write_u32(ptr::addr_of_mut!((*guc).ads_map), reg_addr, addr_ggtt);
            let address = map_address(ptr::addr_of!((*guc).ads_map))
                .add(reg_count as usize)
                .cast::<u16>();
            ptr::write_volatile(address, count as u16);
        }
        addr_ggtt = addr_ggtt.wrapping_add(count * size_of::<guc_mmio_reg>() as u32);
    }
}

// upstream: intel_guc_ads.c fill_engine_enable_masks()
unsafe fn fill_engine_enable_masks(
    gt: *mut IntelGt,
    info_map: *mut crate::linux::iosys_map::IosysMap,
) {
    let mut masks = [0u32; GUC_MAX_ENGINE_CLASSES];
    for id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[id] };
        if engine.is_null() {
            continue;
        }
        let guc_class = engine_class_to_guc_class(unsafe { (*engine).class }) as usize;
        if guc_class < GUC_MAX_ENGINE_CLASSES && unsafe { (*engine).instance } < 32 {
            masks[guc_class] |= 1u32 << unsafe { (*engine).instance };
        }
    }
    for class in 0..GUC_MAX_ENGINE_CLASSES {
        let offset = info_offset(
            offset_of!(guc_gt_system_info, engine_enabled_masks) + class * size_of::<u32>(),
        );
        unsafe { map_write_u32(info_map, offset, masks[class]) };
    }
}

// upstream: intel_guc_ads.c guc_prep_golden_context()
unsafe fn guc_prep_golden_context(guc: *mut IntelGuc) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    let mapped = !unsafe { map_is_null(ptr::addr_of!((*guc).ads_map)) };
    let mut local_info: guc_gt_system_info = unsafe { core::mem::zeroed() };
    let mut local_map: crate::linux::iosys_map::IosysMap = unsafe { core::mem::zeroed() };
    let mut info_map = if mapped {
        let mut result = unsafe { ptr::read(ptr::addr_of!((*guc).ads_map)) };
        let base = offset_of!(GucAdsBlobFixed, system_info) as u32;
        let addr = unsafe { map_address(&result).add(base as usize) };
        if result.is_iomem {
            result.addr.vaddr_iomem = addr.cast();
        } else {
            result.addr.vaddr = addr.cast();
        }
        result
    } else {
        unsafe {
            iosys_map_set_vaddr(&mut local_map, ptr::addr_of_mut!(local_info).cast());
            fill_engine_enable_masks(gt, &mut local_map);
        }
        local_map
    };
    let mut total_size = 0u32;
    let mut offset = if mapped {
        unsafe { guc_ads_golden_ctxt_offset(guc) }
    } else {
        0
    };
    let mut addr_ggtt = if mapped {
        unsafe { crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, (*guc).ads_vma) + offset }
    } else {
        0
    };
    for engine_class in 0..=crate::intel_engine_types_upstream::MAX_ENGINE_CLASS as u8 {
        let guc_class = engine_class_to_guc_class(engine_class);
        let mask_offset = info_offset(
            offset_of!(guc_gt_system_info, engine_enabled_masks)
                + usize::from(guc_class) * size_of::<u32>(),
        );
        if unsafe { map_read_u32(&info_map, mask_offset) } == 0 {
            continue;
        }
        let real_size = unsafe { intel_engine_context_size(gt, engine_class) };
        let alloc_size = page_align(real_size);
        total_size = total_size.wrapping_add(alloc_size);
        if !mapped {
            continue;
        }
        let skip = PAGE_SIZE
            + if unsafe { GRAPHICS_VER_FULL((*gt).i915) } >= IP_VER(12, 55) {
                96 * 4
            } else {
                80 * 4
            };
        let engine_state_offset = ads_offset(
            offset_of!(guc_ads, eng_state_size) + usize::from(guc_class) * size_of::<u32>(),
        );
        let golden_offset = ads_offset(
            offset_of!(guc_ads, golden_context_lrca) + usize::from(guc_class) * size_of::<u32>(),
        );
        unsafe {
            map_write_u32(
                ptr::addr_of_mut!((*guc).ads_map),
                engine_state_offset,
                real_size.wrapping_sub(skip),
            );
            map_write_u32(ptr::addr_of_mut!((*guc).ads_map), golden_offset, addr_ggtt);
        }
        addr_ggtt = addr_ggtt.wrapping_add(alloc_size);
        offset = offset.wrapping_add(alloc_size);
    }
    if unsafe { (*guc).ads_golden_ctxt_size } != 0 {
        GEM_BUG_ON!(unsafe { (*guc).ads_golden_ctxt_size } != total_size);
    }
    total_size as i32
}

// upstream: intel_guc_ads.c find_engine_state()
unsafe fn find_engine_state(gt: *mut IntelGt, engine_class: u8) -> *mut IntelEngineCs {
    for id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[id] };
        if engine.is_null() {
            continue;
        }
        if unsafe { (*engine).class != engine_class || (*engine).default_state.is_null() } {
            continue;
        }
        return engine;
    }
    ptr::null_mut()
}

unsafe fn shmem_read_to_iosys_map(
    file: *mut crate::linux::shmem::File,
    source_offset: u32,
    map: *mut crate::linux::iosys_map::IosysMap,
    target_offset: u32,
    length: u32,
) -> i32 {
    let mut copied = 0u32;
    while copied < length {
        let position = source_offset + copied;
        let page = unsafe {
            crate::linux::shmem::shmem_read_mapping_page(
                (*file).f_mapping,
                (position / PAGE_SIZE) as core::ffi::c_ulong,
            )
        };
        if page.is_null() || IS_ERR!(page) {
            return PTR_ERR!(page);
        }
        let in_page = position % PAGE_SIZE;
        let chunk = (PAGE_SIZE - in_page).min(length - copied);
        let source = unsafe {
            crate::linux::shmem::page_address(page)
                .cast::<u8>()
                .add(in_page as usize)
        };
        unsafe { map_copy_to(map, target_offset + copied, source, chunk as usize) };
        unsafe { crate::linux::shmem::put_page(page) };
        copied += chunk;
    }
    0
}

// upstream: intel_guc_ads.c guc_init_golden_context()
unsafe fn guc_init_golden_context(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    if !unsafe { intel_uc_uses_guc_submission(ptr::addr_of!((*gt).uc)) } {
        return;
    }
    GEM_BUG_ON!(unsafe { map_is_null(ptr::addr_of!((*guc).ads_map)) });
    let mut offset = unsafe { guc_ads_golden_ctxt_offset(guc) };
    let mut addr_ggtt =
        unsafe { crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, (*guc).ads_vma) + offset };
    let mut total_size = 0u32;
    for engine_class in 0..=crate::intel_engine_types_upstream::MAX_ENGINE_CLASS as u8 {
        let guc_class = engine_class_to_guc_class(engine_class);
        let enabled_offset = offset_of!(GucAdsBlobFixed, system_info)
            + offset_of!(guc_gt_system_info, engine_enabled_masks)
            + usize::from(guc_class) * size_of::<u32>();
        if unsafe { map_read_u32(ptr::addr_of!((*guc).ads_map), enabled_offset as u32) } == 0 {
            continue;
        }
        let real_size = unsafe { intel_engine_context_size(gt, engine_class) };
        let alloc_size = page_align(real_size);
        total_size = total_size.wrapping_add(alloc_size);
        let engine = unsafe { find_engine_state(gt, engine_class) };
        if engine.is_null() {
            guc_err!(
                guc,
                "No engine state recorded for class %d!\n",
                engine_class
            );
            unsafe {
                map_write_u32(
                    ptr::addr_of_mut!((*guc).ads_map),
                    ads_offset(offset_of!(guc_ads, eng_state_size) + usize::from(guc_class) * 4),
                    0,
                );
                map_write_u32(
                    ptr::addr_of_mut!((*guc).ads_map),
                    ads_offset(
                        offset_of!(guc_ads, golden_context_lrca) + usize::from(guc_class) * 4,
                    ),
                    0,
                );
            }
            continue;
        }
        let skip = PAGE_SIZE
            + if unsafe { GRAPHICS_VER_FULL((*gt).i915) } >= IP_VER(12, 55) {
                96 * 4
            } else {
                80 * 4
            };
        let size_offset =
            ads_offset(offset_of!(guc_ads, eng_state_size) + usize::from(guc_class) * 4);
        let addr_offset =
            ads_offset(offset_of!(guc_ads, golden_context_lrca) + usize::from(guc_class) * 4);
        GEM_BUG_ON!(
            unsafe { map_read_u32(ptr::addr_of!((*guc).ads_map), size_offset) }
                != real_size.wrapping_sub(skip)
        );
        GEM_BUG_ON!(
            unsafe { map_read_u32(ptr::addr_of!((*guc).ads_map), addr_offset) } != addr_ggtt
        );
        addr_ggtt = addr_ggtt.wrapping_add(alloc_size);
        unsafe {
            let _ = shmem_read_to_iosys_map(
                (*engine).default_state.cast(),
                0,
                ptr::addr_of_mut!((*guc).ads_map),
                offset,
                real_size,
            );
        }
        offset = offset.wrapping_add(alloc_size);
    }
    GEM_BUG_ON!(unsafe { (*guc).ads_golden_ctxt_size } != total_size);
}

// upstream: intel_guc_ads.c guc_get_capture_engine_mask()
unsafe fn guc_get_capture_engine_mask(
    info_map: *mut crate::linux::iosys_map::IosysMap,
    capture_class: u32,
) -> u32 {
    let read_mask = |class: usize| unsafe {
        map_read_u32(
            info_map,
            info_offset(offset_of!(guc_gt_system_info, engine_enabled_masks) + class * 4),
        )
    };
    match capture_class {
        GUC_CAPTURE_LIST_CLASS_RENDER_COMPUTE => {
            read_mask(GUC_RENDER_CLASS as usize) | read_mask(GUC_COMPUTE_CLASS as usize)
        }
        GUC_CAPTURE_LIST_CLASS_VIDEO => read_mask(GUC_VIDEO_CLASS as usize),
        GUC_CAPTURE_LIST_CLASS_VIDEOENHANCE => read_mask(GUC_VIDEOENHANCE_CLASS as usize),
        GUC_CAPTURE_LIST_CLASS_BLITTER => read_mask(GUC_BLITTER_CLASS as usize),
        GUC_CAPTURE_LIST_CLASS_GSC_OTHER => read_mask(GUC_GSC_OTHER_CLASS as usize),
        _ => 0,
    }
}

// upstream: intel_guc_ads.c guc_capture_prep_lists()
unsafe fn guc_capture_prep_lists(guc: *mut IntelGuc) -> i32 {
    let gt = unsafe { guc_to_gt(guc) };
    let mapped = !unsafe { map_is_null(ptr::addr_of!((*guc).ads_map)) };
    let mut local_info: guc_gt_system_info = unsafe { core::mem::zeroed() };
    let mut local_map: crate::linux::iosys_map::IosysMap = unsafe { core::mem::zeroed() };
    let mut info_map = if mapped {
        let mut result = unsafe { ptr::read(ptr::addr_of!((*guc).ads_map)) };
        let addr = unsafe { map_address(&result).add(offset_of!(GucAdsBlobFixed, system_info)) };
        if result.is_iomem {
            result.addr.vaddr_iomem = addr.cast();
        } else {
            result.addr.vaddr = addr.cast();
        }
        result
    } else {
        unsafe {
            iosys_map_set_vaddr(&mut local_map, ptr::addr_of_mut!(local_info).cast());
            fill_engine_enable_masks(gt, &mut local_map);
        }
        local_map
    };
    let mut capture_offset = if mapped {
        unsafe { guc_ads_capture_offset(guc) }
    } else {
        0
    };
    let ads_ggtt = if mapped {
        unsafe { crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, (*guc).ads_vma) }
    } else {
        0
    };
    let mut total_size = PAGE_SIZE;
    let mut null_ggtt = 0u32;
    if mapped {
        let mut ptr_null = ptr::null_mut();
        let mut size = 0usize;
        if unsafe { intel_guc_capture_getnullheader(guc, &mut ptr_null, &mut size) } == 0 {
            unsafe {
                map_copy_to(
                    ptr::addr_of_mut!((*guc).ads_map),
                    capture_offset,
                    ptr_null.cast(),
                    size,
                )
            };
        }
        null_ggtt = ads_ggtt + capture_offset;
        capture_offset += PAGE_SIZE;
    }
    for index in 0..GUC_CAPTURE_LIST_INDEX_MAX {
        for class in 0..GUC_MAX_ENGINE_CLASSES as u32 {
            let engine_mask = unsafe { guc_get_capture_engine_mask(&mut info_map, class) };
            let class_field = ads_offset(
                offset_of!(guc_ads, capture_class)
                    + (index * GUC_MAX_ENGINE_CLASSES as u32 + class) as usize * 4,
            );
            let instance_field = ads_offset(
                offset_of!(guc_ads, capture_instance)
                    + (index * GUC_MAX_ENGINE_CLASSES as u32 + class) as usize * 4,
            );
            if engine_mask == 0 {
                if mapped {
                    unsafe {
                        map_write_u32(ptr::addr_of_mut!((*guc).ads_map), class_field, null_ggtt);
                        map_write_u32(ptr::addr_of_mut!((*guc).ads_map), instance_field, null_ggtt);
                    }
                }
                continue;
            }
            let mut size = 0usize;
            if unsafe {
                intel_guc_capture_getlistsize(
                    guc,
                    index as u32,
                    guc_capture_type::GUC_CAPTURE_LIST_TYPE_ENGINE_CLASS as u32,
                    class,
                    &mut size,
                )
            } == 0
            {
                total_size = total_size.wrapping_add(size as u32);
                if mapped {
                    let mut list = ptr::null_mut();
                    if total_size > unsafe { (*guc).ads_capture_size }
                        || unsafe {
                            intel_guc_capture_getlist(
                                guc,
                                index as u32,
                                guc_capture_type::GUC_CAPTURE_LIST_TYPE_ENGINE_CLASS as u32,
                                class,
                                &mut list,
                            )
                        } != 0
                    {
                        unsafe {
                            map_write_u32(ptr::addr_of_mut!((*guc).ads_map), class_field, null_ggtt)
                        };
                    } else {
                        unsafe {
                            map_write_u32(
                                ptr::addr_of_mut!((*guc).ads_map),
                                class_field,
                                ads_ggtt + capture_offset,
                            );
                            map_copy_to(
                                ptr::addr_of_mut!((*guc).ads_map),
                                capture_offset,
                                list.cast(),
                                size,
                            );
                        }
                        capture_offset += size as u32;
                    }
                }
            } else if mapped {
                unsafe { map_write_u32(ptr::addr_of_mut!((*guc).ads_map), class_field, null_ggtt) };
            }
            let mut size = 0usize;
            if unsafe {
                intel_guc_capture_getlistsize(
                    guc,
                    index as u32,
                    guc_capture_type::GUC_CAPTURE_LIST_TYPE_ENGINE_INSTANCE as u32,
                    class,
                    &mut size,
                )
            } == 0
            {
                total_size = total_size.wrapping_add(size as u32);
                if mapped {
                    let mut list = ptr::null_mut();
                    if total_size > unsafe { (*guc).ads_capture_size }
                        || unsafe {
                            intel_guc_capture_getlist(
                                guc,
                                index as u32,
                                guc_capture_type::GUC_CAPTURE_LIST_TYPE_ENGINE_INSTANCE as u32,
                                class,
                                &mut list,
                            )
                        } != 0
                    {
                        unsafe {
                            map_write_u32(
                                ptr::addr_of_mut!((*guc).ads_map),
                                instance_field,
                                null_ggtt,
                            )
                        };
                    } else {
                        unsafe {
                            map_write_u32(
                                ptr::addr_of_mut!((*guc).ads_map),
                                instance_field,
                                ads_ggtt + capture_offset,
                            );
                            map_copy_to(
                                ptr::addr_of_mut!((*guc).ads_map),
                                capture_offset,
                                list.cast(),
                                size,
                            );
                        }
                        capture_offset += size as u32;
                    }
                }
            } else if mapped {
                unsafe {
                    map_write_u32(ptr::addr_of_mut!((*guc).ads_map), instance_field, null_ggtt)
                };
            }
        }
        let global_field = ads_offset(offset_of!(guc_ads, capture_global) + index as usize * 4);
        let mut size = 0usize;
        if unsafe {
            intel_guc_capture_getlistsize(
                guc,
                index as u32,
                guc_capture_type::GUC_CAPTURE_LIST_TYPE_GLOBAL as u32,
                0,
                &mut size,
            )
        } == 0
        {
            total_size = total_size.wrapping_add(size as u32);
            if mapped {
                let mut list = ptr::null_mut();
                if total_size > unsafe { (*guc).ads_capture_size }
                    || unsafe {
                        intel_guc_capture_getlist(
                            guc,
                            index as u32,
                            guc_capture_type::GUC_CAPTURE_LIST_TYPE_GLOBAL as u32,
                            0,
                            &mut list,
                        )
                    } != 0
                {
                    unsafe {
                        map_write_u32(ptr::addr_of_mut!((*guc).ads_map), global_field, null_ggtt)
                    };
                } else {
                    unsafe {
                        map_write_u32(
                            ptr::addr_of_mut!((*guc).ads_map),
                            global_field,
                            ads_ggtt + capture_offset,
                        );
                        map_copy_to(
                            ptr::addr_of_mut!((*guc).ads_map),
                            capture_offset,
                            list.cast(),
                            size,
                        );
                    }
                    capture_offset += size as u32;
                }
            }
        } else if mapped {
            unsafe { map_write_u32(ptr::addr_of_mut!((*guc).ads_map), global_field, null_ggtt) };
        }
    }
    let aligned = page_align(total_size);
    if unsafe { (*guc).ads_capture_size != 0 && (*guc).ads_capture_size != aligned } {
        guc_warn!(
            guc,
            "ADS capture alloc size changed from %d to %d\n",
            unsafe { (*guc).ads_capture_size },
            aligned
        );
    }
    aligned as i32
}

// upstream: intel_guc_ads.c guc_waklv_enable_simple()
unsafe fn guc_waklv_enable_simple(
    guc: *mut IntelGuc,
    offset: *mut u32,
    remain: *mut u32,
    klv_id: u16,
) {
    let size = size_of::<u32>() as u32;
    GEM_BUG_ON!(unsafe { *remain < size });
    let header = u32::from(klv_id);
    unsafe {
        map_write_u32(ptr::addr_of_mut!((*guc).ads_map), *offset, header);
        *offset += size;
        *remain -= size;
    }
}

// upstream: intel_guc_ads.c guc_waklv_init()
unsafe fn guc_waklv_init(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    if !unsafe { intel_uc_uses_guc_submission(ptr::addr_of!((*gt).uc)) } {
        return;
    }
    let version = unsafe { (*guc).fw.version };
    if (version.major, version.minor, version.patch) < (70, 10, 0) {
        return;
    }
    GEM_BUG_ON!(unsafe { map_is_null(ptr::addr_of!((*guc).ads_map)) });
    let mut offset = unsafe { guc_ads_waklv_offset(guc) };
    let mut remain = unsafe { guc_ads_waklv_size(guc) };
    if unsafe { IS_GFX_GT_IP_RANGE(gt, IP_VER(12, 70), IP_VER(12, 74)) } {
        unsafe {
            guc_waklv_enable_simple(
                guc,
                &mut offset,
                &mut remain,
                GUC_WORKAROUND_KLV_SERIALIZED_RA_MODE,
            );
            guc_waklv_enable_simple(
                guc,
                &mut offset,
                &mut remain,
                GUC_WORKAROUND_KLV_AVOID_GFX_CLEAR_WHILE_ACTIVE,
            );
        }
    }
    let i915 = unsafe { (*gt).i915 };
    let needs_wa = (version.major, version.minor, version.patch) >= (70, 21, 1)
        && (unsafe { IS_GFX_GT_IP_RANGE(gt, IP_VER(12, 70), IP_VER(12, 74)) }
            || unsafe { IS_MEDIA_GT_IP_RANGE(gt, IP_VER(13, 0), IP_VER(13, 0)) }
            || unsafe { IS_DG2(i915) });
    if needs_wa {
        unsafe {
            guc_waklv_enable_simple(
                guc,
                &mut offset,
                &mut remain,
                GUC_WORKAROUND_KLV_BLOCK_INTERRUPTS_WHEN_MGSR_BLOCKED,
            )
        };
    }
    let size = unsafe { guc_ads_waklv_size(guc) - remain };
    if size == 0 {
        return;
    }
    let offset = unsafe { guc_ads_waklv_offset(guc) };
    let addr =
        unsafe { crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, (*guc).ads_vma) + offset };
    unsafe {
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            ads_offset(offset_of!(guc_ads, wa_klv_addr_lo)),
            addr,
        );
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            ads_offset(offset_of!(guc_ads, wa_klv_addr_hi)),
            0,
        );
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            ads_offset(offset_of!(guc_ads, wa_klv_size)),
            size,
        );
    }
}

// upstream: intel_guc_ads.c guc_prep_waklv()
unsafe fn guc_prep_waklv(_guc: *mut IntelGuc) -> i32 {
    // Fudge something chunky for now, as in the Linux 7.2.3 source.
    PAGE_SIZE as i32
}

// upstream: intel_guc_ads.c __guc_ads_init()
unsafe fn __guc_ads_init(guc: *mut IntelGuc) {
    let gt = unsafe { guc_to_gt(guc) };
    let i915 = unsafe { (*gt).i915 };
    let mut info_map: crate::linux::iosys_map::IosysMap = unsafe { core::mem::zeroed() };
    unsafe {
        guc_policies_init(guc);
        map_submap(
            ptr::addr_of!((*guc).ads_map),
            offset_of!(GucAdsBlobFixed, system_info) as u32,
            &mut info_map,
        );
        fill_engine_enable_masks(gt, &mut info_map);
        let slice_mask = (*gt).info.sseu.slice_mask;
        map_write_u32(
            &mut info_map,
            offset_info(
                offset_of!(guc_gt_system_info, generic_gt_sysinfo)
                    + GUC_GENERIC_GT_SYSINFO_SLICE_ENABLED as usize * 4,
            ),
            slice_mask.count_ones(),
        );
        map_write_u32(
            &mut info_map,
            offset_info(
                offset_of!(guc_gt_system_info, generic_gt_sysinfo)
                    + GUC_GENERIC_GT_SYSINFO_VDBOX_SFC_SUPPORT_MASK as usize * 4,
            ),
            (*gt).info.vdbox_sfc_access as u32,
        );
        if GRAPHICS_VER(i915) >= 12 && !IS_DGFX(i915) {
            let value = intel_uncore_read((*gt).uncore, I915RegT { reg: 0xd08 });
            let doorbells = ((value >> 16) & 0xff) + 1;
            map_write_u32(
                &mut info_map,
                offset_info(
                    offset_of!(guc_gt_system_info, generic_gt_sysinfo)
                        + GUC_GENERIC_GT_SYSINFO_DOORBELL_COUNT_PER_SQIDI as usize * 4,
                ),
                doorbells,
            );
        }
        let _ = guc_prep_golden_context(guc);
        guc_mapping_table_init(gt, &mut info_map);
    }
    let base = unsafe { crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, (*guc).ads_vma) };
    unsafe {
        let _ = guc_capture_prep_lists(guc);
    }
    unsafe {
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            ads_offset(offset_of!(guc_ads, scheduler_policies)),
            base + offset_of!(GucAdsBlobFixed, policies) as u32,
        );
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            ads_offset(offset_of!(guc_ads, gt_system_info)),
            base + offset_of!(GucAdsBlobFixed, system_info) as u32,
        );
        guc_mmio_reg_state_init(guc);
        guc_waklv_init(guc);
        map_write_u32(
            ptr::addr_of_mut!((*guc).ads_map),
            ads_offset(offset_of!(guc_ads, private_data)),
            base + guc_ads_private_data_offset(guc),
        );
        crate::i915_gem_object_header_upstream::i915_gem_object_flush_map((*(*guc).ads_vma).obj);
    }
}

unsafe fn offset_info(field_offset: usize) -> u32 {
    field_offset as u32
}

// upstream: intel_guc_ads.c intel_guc_ads_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ads_create(guc: *mut IntelGuc) -> i32 {
    GEM_BUG_ON!(unsafe { !(*guc).ads_vma.is_null() });
    let regset_size = unsafe { guc_mmio_reg_state_create(guc) };
    if regset_size < 0 {
        return regset_size as i32;
    }
    unsafe { (*guc).ads_regset_size = regset_size as u32 };
    let golden_size = unsafe { guc_prep_golden_context(guc) };
    if golden_size < 0 {
        return golden_size;
    }
    unsafe { (*guc).ads_golden_ctxt_size = golden_size as u32 };
    let capture_size = unsafe { guc_capture_prep_lists(guc) };
    if capture_size < 0 {
        return capture_size;
    }
    unsafe { (*guc).ads_capture_size = capture_size as u32 };
    let waklv_size = unsafe { guc_prep_waklv(guc) };
    if waklv_size < 0 {
        return waklv_size;
    }
    unsafe { (*guc).ads_waklv_size = waklv_size as u32 };
    let size = unsafe { guc_ads_blob_size(guc) };
    let mut blob = ptr::null_mut();
    let ret = unsafe {
        crate::intel_guc_upstream::intel_guc_allocate_and_map_vma(
            guc,
            size,
            ptr::addr_of_mut!((*guc).ads_vma),
            &mut blob,
        )
    };
    if ret != 0 {
        return ret;
    }
    let obj = unsafe { (*(*guc).ads_vma).obj };
    if unsafe { crate::i915_gem_lmem_upstream::i915_gem_object_is_lmem(obj) } {
        unsafe {
            (*guc).ads_map.addr.vaddr_iomem = blob;
            (*guc).ads_map.is_iomem = true;
        }
    } else {
        unsafe { iosys_map_set_vaddr(ptr::addr_of_mut!((*guc).ads_map), blob) };
    }
    unsafe { __guc_ads_init(guc) };
    0
}

// upstream: intel_guc_ads.c intel_guc_ads_init_late()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ads_init_late(guc: *mut IntelGuc) {
    unsafe { guc_init_golden_context(guc) };
}

// upstream: intel_guc_ads.c intel_guc_ads_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ads_destroy(guc: *mut IntelGuc) {
    unsafe {
        crate::i915_vma_api_upstream::i915_vma_unpin_and_release(
            ptr::addr_of_mut!((*guc).ads_vma),
            crate::i915_vma_api_upstream::I915_VMA_RELEASE_MAP,
        );
        (*guc).ads_map.addr.vaddr = ptr::null_mut();
        (*guc).ads_map.is_iomem = false;
        kfree((*guc).ads_regset);
        (*guc).ads_regset = ptr::null_mut();
    }
}

// upstream: intel_guc_ads.c guc_ads_private_data_reset()
unsafe fn guc_ads_private_data_reset(guc: *mut IntelGuc) {
    let size = unsafe { guc_ads_private_data_size(guc) };
    if size == 0 {
        return;
    }
    unsafe {
        map_memset(
            ptr::addr_of_mut!((*guc).ads_map),
            guc_ads_private_data_offset(guc),
            0,
            size as usize,
        )
    };
}

// upstream: intel_guc_ads.c intel_guc_ads_reset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ads_reset(guc: *mut IntelGuc) {
    if unsafe { (*guc).ads_vma.is_null() } {
        return;
    }
    unsafe {
        __guc_ads_init(guc);
        guc_ads_private_data_reset(guc);
    }
}

// upstream: intel_guc_ads.c intel_guc_engine_usage_offset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_engine_usage_offset(guc: *mut IntelGuc) -> u32 {
    unsafe {
        crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, (*guc).ads_vma)
            + offset_of!(GucAdsBlobFixed, engine_usage) as u32
    }
}

// upstream: intel_guc_ads.c intel_guc_engine_usage_record_map()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_engine_usage_record_map(
    engine: *mut IntelEngineCs,
) -> crate::linux::iosys_map::IosysMap {
    let gt = unsafe { (*engine).gt };
    let guc = unsafe { gt_to_guc(gt) };
    let class = engine_class_to_guc_class(unsafe { (*engine).class }) as usize;
    let logical_index = unsafe { (*engine).logical_mask.trailing_zeros() as usize };
    let offset = offset_of!(GucAdsBlobFixed, engine_usage)
        + offset_of!(guc_engine_usage, engines)
        + (class * GUC_MAX_INSTANCES_PER_CLASS + logical_index)
            * (size_of::<guc_engine_usage>()
                / (GUC_MAX_ENGINE_CLASSES * GUC_MAX_INSTANCES_PER_CLASS));
    let mut map = unsafe { ptr::read(ptr::addr_of!((*guc).ads_map)) };
    let address = unsafe { map_address(&map).add(offset) };
    if map.is_iomem {
        map.addr.vaddr_iomem = address.cast();
    } else {
        map.addr.vaddr = address.cast();
    }
    map
}
