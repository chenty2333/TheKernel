// SPDX-License-Identifier: MIT
// Copyright © 2021-2022 Intel Corporation.
//! Linux 7.2.3 intel_guc_capture.c native linked-node extraction dependency
//! path. The existing source-translated CaptureBuffer supplies the byte reader;
//! this owner preserves preallocated-node reuse and does not allocate on G2H.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
use core::{
    ffi::{CStr, c_char, c_void},
    mem::size_of,
    ptr,
};

use crate::{
    guc_capture::{
        CAPTURE_TYPE_ENGINE_CLASS, CAPTURE_TYPE_ENGINE_INSTANCE, CAPTURE_TYPE_GLOBAL,
        CaptureBuffer, CaptureRegister, CaptureRegisterList, gen12_register_name,
        gen12_static_registers, gen12_steered_capture_registers,
    },
    guc_log::{LogBufferState, LogStats, intel_guc_check_log_buf_overflow},
    intel_context_types_upstream::IntelContext,
    intel_engine_cs_upstream::ListHead,
    intel_engine_types_upstream::IntelEngineCs,
    intel_gt_mcr_impl_upstream::intel_gt_mcr_get_ss_steering,
    intel_gt_types_upstream::IntelGt,
    intel_guc_fwif_types_upstream::{guc_capture_type, guc_mmio_reg},
    intel_guc_types_upstream::IntelGuc,
    intel_sseu_types_upstream::intel_sseu_has_subslice,
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        i915::{GRAPHICS_VER_FULL, IP_VER},
        list::{INIT_LIST_HEAD, list_add_tail, list_del, list_empty},
        memory::{kfree, kmalloc, kzalloc, kzalloc_obj, kzalloc_objs},
    },
    linux_config::{EINVAL, EIO, ENODEV, ENOMEM, GFP_KERNEL},
    linux_print::{DrmPrinter, drm_printer_write},
};
const ENODATA: i32 = 61;
#[repr(C)]
pub struct CaptureAdsCache {
    pub is_valid: bool,
    pub ptr: *mut c_void,
    pub size: usize,
    pub status: i32,
}
#[repr(C)]
pub struct IntelGucStateCapture {
    pub reglists: *const c_void,
    pub extlists: *mut c_void,
    pub ads_cache: [[[CaptureAdsCache; 16]; 3]; 2],
    pub ads_null_cache: *mut c_void,
    pub cachelist: ListHead,
    pub max_mmio_per_node: i32,
    pub outlist: ListHead,
}
#[repr(C)]
pub struct RegInfo {
    pub vfid: u32,
    pub num_regs: u32,
    pub regs: *mut CaptureRegister,
}
#[repr(C)]
pub struct ParsedOutput {
    pub link: ListHead,
    pub is_partial: bool,
    pub eng_class: u32,
    pub eng_inst: u32,
    pub guc_id: u32,
    pub lrca: u32,
    pub reginfo: [RegInfo; 3],
}

#[repr(C)]
struct IntelEngineCoredumpView {
    engine: *mut IntelEngineCs,
    capture: *mut IntelGucStateCapture,
    guc_capture_node: *mut ParsedOutput,
    ipehr: u32,
    instdone: u32,
}

#[repr(C)]
struct CaptureRegDescriptor {
    reg: I915RegT,
    flags: u32,
    mask: u32,
    regname: *const c_char,
}

#[repr(C)]
struct CaptureRegGroup {
    list: *const CaptureRegDescriptor,
    num_regs: u32,
    owner: u32,
    type_: u32,
    engine: u32,
    extlist: *mut CaptureRegDescriptor,
}

#[repr(C)]
struct CaptureBufState {
    size: u32,
    data: *mut u8,
    rd: u32,
    wr: u32,
}

#[repr(C)]
#[derive(Default)]
struct CaptureGroupHeader {
    owner: u32,
    info: u32,
}

#[repr(C)]
#[derive(Default)]
struct CaptureDataHeader {
    owner: u32,
    info: u32,
    lrca: u32,
    guc_id: u32,
    num_mmios: u32,
}
const _: [(); 3136] = [(); core::mem::size_of::<IntelGucStateCapture>()];
const _: [(); 88] = [(); core::mem::size_of::<ParsedOutput>()];

// upstream: intel_guc_capture.c guc_capture_get_one_list()
unsafe fn guc_capture_get_one_list(
    reglists: *const CaptureRegGroup,
    owner: u32,
    type_: u32,
    id: u32,
) -> *const CaptureRegGroup {
    if reglists.is_null() {
        return ptr::null();
    }
    let mut index = 0usize;
    loop {
        let list = unsafe { reglists.add(index) };
        if unsafe { (*list).list.is_null() } {
            return ptr::null();
        }
        if unsafe {
            (*list).owner == owner
                && (*list).type_ == type_
                && ((*list).engine == id || (*list).type_ == CAPTURE_TYPE_GLOBAL as u32)
        } {
            return list;
        }
        index += 1;
    }
}

// upstream: intel_guc_capture.c guc_capture_get_one_ext_list()
unsafe fn guc_capture_get_one_ext_list(
    reglists: *mut CaptureRegGroup,
    owner: u32,
    type_: u32,
    id: u32,
) -> *mut CaptureRegGroup {
    if reglists.is_null() {
        return ptr::null_mut();
    }
    let mut index = 0usize;
    loop {
        let list = unsafe { reglists.add(index) };
        if unsafe { (*list).extlist.is_null() } {
            return ptr::null_mut();
        }
        if unsafe {
            (*list).owner == owner
                && (*list).type_ == type_
                && ((*list).engine == id || (*list).type_ == CAPTURE_TYPE_GLOBAL as u32)
        } {
            return list;
        }
        index += 1;
    }
}

// upstream: intel_guc_capture.c guc_capture_free_extlists()
unsafe fn guc_capture_free_extlists(reglists: *mut CaptureRegGroup) {
    if reglists.is_null() {
        return;
    }
    let mut index = 0usize;
    loop {
        let group = unsafe { reglists.add(index) };
        if unsafe { (*group).extlist.is_null() } {
            break;
        }
        unsafe { kfree((*group).extlist) };
        index += 1;
    }
}

#[repr(C)]
struct ExtSteerReg {
    name: *const c_char,
    reg: I915RegT,
}

// upstream: intel_guc_capture.c __fill_ext_reg()
unsafe fn __fill_ext_reg(
    ext: *mut CaptureRegDescriptor,
    source: *const ExtSteerReg,
    slice_id: u32,
    subslice_id: u32,
) {
    unsafe {
        (*ext).reg = (*source).reg;
        (*ext).flags = crate::linux::registers::REG_FIELD_PREP(
            crate::intel_guc_fwif_types_upstream::GUC_REGSET_STEERING_GROUP,
            slice_id,
        ) | crate::linux::registers::REG_FIELD_PREP(
            crate::intel_guc_fwif_types_upstream::GUC_REGSET_STEERING_INSTANCE,
            subslice_id,
        );
        (*ext).mask = 0;
        (*ext).regname = (*source).name;
    }
}

// upstream: intel_guc_capture.c __alloc_ext_regs()
unsafe fn __alloc_ext_regs(
    newlist: *mut CaptureRegGroup,
    rootlist: *const CaptureRegGroup,
    num_regs: i32,
) -> i32 {
    if num_regs <= 0 {
        return -EINVAL;
    }
    let list = kzalloc_objs::<CaptureRegDescriptor, _>(num_regs as usize);
    if list.is_null() {
        return -ENOMEM;
    }
    unsafe {
        (*newlist).extlist = list;
        (*newlist).num_regs = num_regs as u32;
        (*newlist).owner = (*rootlist).owner;
        (*newlist).engine = (*rootlist).engine;
        (*newlist).type_ = (*rootlist).type_;
    }
    0
}

// upstream: intel_guc_capture.c guc_capture_alloc_steered_lists()
unsafe fn guc_capture_alloc_steered_lists(guc: *mut IntelGuc, lists: *const CaptureRegGroup) {
    let gt = crate::intel_gt_api_upstream::guc_to_gt(guc);
    let root = unsafe { guc_capture_get_one_list(lists, 0, CAPTURE_TYPE_ENGINE_CLASS as u32, 0) };
    if root.is_null()
        || unsafe { !(*guc).capture.is_null() && !(*(*guc).capture).extlists.is_null() }
    {
        return;
    }
    let mut steering = alloc::vec::Vec::new();
    let sseu = unsafe { ptr::addr_of!((*gt).info.sseu) };
    let xehp = unsafe { GRAPHICS_VER_FULL((*gt).i915) >= IP_VER(12, 55) };
    for dss in 0..crate::intel_sseu_types_upstream::I915_MAX_SS_FUSE_BITS as u32 {
        let mut group = 0u32;
        let mut instance = 0u32;
        unsafe { intel_gt_mcr_get_ss_steering(gt, dss, &mut group, &mut instance) };
        let present = unsafe {
            if xehp {
                intel_sseu_has_subslice(&*sseu, 0, dss as i32)
            } else {
                intel_sseu_has_subslice(&*sseu, group as i32, instance as i32)
            }
        };
        if present {
            steering.push((group as u8, instance as u8));
        }
    }
    if steering.is_empty() {
        return;
    }
    let ip = unsafe { GRAPHICS_VER_FULL((*gt).i915) };
    let regs = match gen12_steered_capture_registers(&steering, ((ip >> 8) as u8, ip as u8)) {
        Ok(regs) => regs,
        Err(_) => return,
    };
    let groups = kzalloc_objs::<CaptureRegGroup, _>(2);
    if groups.is_null() {
        return;
    }
    if unsafe { __alloc_ext_regs(groups, root, regs.len() as i32) } != 0 {
        unsafe { kfree(groups) };
        return;
    }
    for (index, reg) in regs.iter().enumerate() {
        let entry = unsafe { (*groups).extlist.add(index) };
        unsafe {
            (*entry).reg.reg = reg.offset;
            (*entry).flags = reg.flags;
            (*entry).mask = reg.mask;
            (*entry).regname = ptr::null();
        }
    }
    unsafe { (*(*guc).capture).extlists = groups.cast() };
}

// upstream: intel_guc_capture.c guc_capture_get_device_reglist()
unsafe fn guc_capture_get_device_reglist(guc: *mut IntelGuc) -> *const CaptureRegGroup {
    let gc = unsafe { (*guc).capture };
    let groups = kzalloc_objs::<CaptureRegGroup, _>(12);
    if groups.is_null() {
        return ptr::null();
    }
    let mut definitions = alloc::vec::Vec::new();
    definitions.push((CAPTURE_TYPE_GLOBAL, 0));
    for class in 0..5 {
        definitions.push((CAPTURE_TYPE_ENGINE_CLASS, class));
    }
    for class in 0..5 {
        definitions.push((CAPTURE_TYPE_ENGINE_INSTANCE, class));
    }
    for (index, (type_, classid)) in definitions.into_iter().enumerate() {
        let Some(registers) = gen12_static_registers(0, type_ as u32, classid, 0) else {
            unsafe { free_reglist_groups(groups) };
            return ptr::null();
        };
        let descriptors = kzalloc_objs::<CaptureRegDescriptor, _>(registers.len().max(1));
        if descriptors.is_null() {
            unsafe { free_reglist_groups(groups) };
            return ptr::null();
        }
        for (slot, register) in registers.iter().enumerate() {
            unsafe {
                (*descriptors.add(slot)).reg = I915RegT {
                    reg: register.offset,
                };
                (*descriptors.add(slot)).flags = register.flags;
                (*descriptors.add(slot)).mask = register.mask;
            }
        }
        let group = unsafe { groups.add(index) };
        unsafe {
            (*group).list = descriptors;
            (*group).num_regs = registers.len() as u32;
            (*group).owner = 0;
            (*group).type_ = type_ as u32;
            (*group).engine = classid;
        }
    }
    unsafe {
        guc_capture_alloc_steered_lists(guc, groups);
        (*gc).reglists = groups.cast();
    }
    groups.cast()
}

unsafe fn free_reglist_groups(groups: *mut CaptureRegGroup) {
    if groups.is_null() {
        return;
    }
    for index in 0..11 {
        let group = unsafe { groups.add(index) };
        if !unsafe { (*group).list.is_null() } {
            unsafe { kfree((*group).list.cast_mut()) };
        }
    }
    unsafe { kfree(groups) };
}

// upstream: intel_guc_capture.c __stringify_type()
unsafe fn __stringify_type(type_: u32) -> *const c_char {
    match type_ {
        x if x == CAPTURE_TYPE_GLOBAL as u32 => c"Global".as_ptr(),
        x if x == CAPTURE_TYPE_ENGINE_CLASS as u32 => c"Class".as_ptr(),
        x if x == CAPTURE_TYPE_ENGINE_INSTANCE as u32 => c"Instance".as_ptr(),
        _ => c"unknown".as_ptr(),
    }
}

// upstream: intel_guc_capture.c __stringify_engclass()
unsafe fn __stringify_engclass(class: u32) -> *const c_char {
    match class {
        0 => c"Render/Compute".as_ptr(),
        1 => c"Video".as_ptr(),
        2 => c"VideoEnhance".as_ptr(),
        3 => c"Blitter".as_ptr(),
        4 => c"GSC-Other".as_ptr(),
        _ => c"unknown".as_ptr(),
    }
}

// upstream: intel_guc_capture.c guc_capture_list_init()
unsafe fn guc_capture_list_init(
    guc: *mut IntelGuc,
    owner: u32,
    type_: u32,
    classid: u32,
    output: *mut guc_mmio_reg,
    num_entries: u16,
) -> i32 {
    let gc = unsafe { (*guc).capture };
    if gc.is_null() || unsafe { (*gc).reglists.is_null() } {
        return -ENODEV;
    }
    let regs = unsafe { (*gc).reglists.cast::<CaptureRegGroup>() };
    let base = unsafe { guc_capture_get_one_list(regs, owner, type_, classid) };
    if base.is_null() {
        return -ENODATA;
    }
    let mut i = 0usize;
    let mut j = 0usize;
    while i < num_entries as usize && i < unsafe { (*base).num_regs as usize } {
        let source = unsafe { (*base).list.add(i) };
        unsafe {
            ptr::write(
                output.add(i),
                guc_mmio_reg {
                    offset: (*source).reg.reg,
                    value: 0xDEAD_F00D,
                    flags: (*source).flags,
                    mask: (*source).mask,
                },
            );
        }
        i += 1;
    }
    let ext = unsafe { guc_capture_get_one_ext_list((*gc).extlists.cast(), owner, type_, classid) };
    if !ext.is_null() {
        while i < num_entries as usize
            && i < (unsafe { (*base).num_regs + (*ext).num_regs } as usize)
            && j < unsafe { (*ext).num_regs as usize }
        {
            let source = unsafe { (*ext).extlist.add(j) };
            unsafe {
                ptr::write(
                    output.add(i),
                    guc_mmio_reg {
                        offset: (*source).reg.reg,
                        value: 0xDEAD_F00D,
                        flags: (*source).flags,
                        mask: (*source).mask,
                    },
                );
            }
            i += 1;
            j += 1;
        }
    }
    0
}

// upstream: intel_guc_capture.c guc_cap_list_num_regs()
unsafe fn guc_cap_list_num_regs(
    gc: *mut IntelGucStateCapture,
    owner: u32,
    type_: u32,
    classid: u32,
) -> i32 {
    if gc.is_null() || unsafe { (*gc).reglists.is_null() } {
        return 0;
    }
    let base = unsafe { guc_capture_get_one_list((*gc).reglists.cast(), owner, type_, classid) };
    if base.is_null() {
        return 0;
    }
    let mut count = unsafe { (*base).num_regs as i32 };
    let ext = unsafe { guc_capture_get_one_ext_list((*gc).extlists.cast(), owner, type_, classid) };
    if !ext.is_null() {
        count += unsafe { (*ext).num_regs as i32 };
    }
    count
}

// upstream: intel_guc_capture.c guc_capture_getlistsize()
unsafe fn guc_capture_getlistsize(
    guc: *mut IntelGuc,
    owner: u32,
    type_: u32,
    classid: u32,
    size: *mut usize,
    is_purpose_est: bool,
) -> i32 {
    let gc = unsafe { (*guc).capture };
    if gc.is_null() || unsafe { (*gc).reglists.is_null() } {
        guc_warn!(guc, "No capture reglist for this device\n");
        return -ENODEV;
    }
    if owner >= 2 || type_ >= 3 || classid >= 16 {
        return -EINVAL;
    }
    let cache = unsafe { &mut (*gc).ads_cache[owner as usize][type_ as usize][classid as usize] };
    if cache.is_valid {
        if !size.is_null() {
            unsafe { *size = cache.size };
        }
        return cache.status;
    }
    if !is_purpose_est && owner == 0 {
        let base =
            unsafe { guc_capture_get_one_list((*gc).reglists.cast(), owner, type_, classid) };
        if base.is_null() {
            return -ENODATA;
        }
    }
    let count = unsafe { guc_cap_list_num_regs(gc, owner, type_, classid) };
    if count <= 0 {
        return -ENODATA;
    }
    if !size.is_null() {
        unsafe { *size = (4 + count as usize * size_of::<guc_mmio_reg>()).next_multiple_of(4096) };
    }
    0
}

// upstream: intel_guc_capture.c intel_guc_capture_getlistsize()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_capture_getlistsize(
    guc: *mut IntelGuc,
    owner: u32,
    type_: u32,
    classid: u32,
    size: *mut usize,
) -> i32 {
    unsafe { guc_capture_getlistsize(guc, owner, type_, classid, size, false) }
}

// upstream: intel_guc_capture.c intel_guc_capture_getlist()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_capture_getlist(
    guc: *mut IntelGuc,
    owner: u32,
    type_: u32,
    classid: u32,
    out: *mut *mut c_void,
) -> i32 {
    let gc = unsafe { (*guc).capture };
    if gc.is_null() || unsafe { (*gc).reglists.is_null() } {
        return -ENODEV;
    }
    if owner >= 2 || type_ >= 3 || classid >= 16 {
        return -EINVAL;
    }
    let cache = unsafe { &mut (*gc).ads_cache[owner as usize][type_ as usize][classid as usize] };
    if cache.is_valid {
        unsafe { *out = cache.ptr };
        return cache.status;
    }
    unsafe { guc_capture_create_prealloc_nodes(guc) };
    let mut size = 0usize;
    let ret = unsafe { intel_guc_capture_getlistsize(guc, owner, type_, classid, &mut size) };
    if ret != 0 {
        unsafe {
            cache.is_valid = true;
            cache.ptr = ptr::null_mut();
            cache.size = 0;
            cache.status = ret;
            *out = ptr::null_mut();
        }
        return ret;
    }
    let caplist = kzalloc(size, GFP_KERNEL).cast::<u8>();
    if caplist.is_null() {
        return -ENOMEM;
    }
    let count = unsafe { guc_cap_list_num_regs(gc, owner, type_, classid) };
    unsafe {
        ptr::write(caplist.cast::<u32>(), count as u32);
        let _ = guc_capture_list_init(
            guc,
            owner,
            type_,
            classid,
            caplist.add(4).cast::<guc_mmio_reg>(),
            count as u16,
        );
        cache.is_valid = true;
        cache.ptr = caplist.cast();
        cache.size = size;
        cache.status = 0;
        *out = caplist.cast();
    }
    0
}

// upstream: intel_guc_capture.c intel_guc_capture_getnullheader()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_capture_getnullheader(
    guc: *mut IntelGuc,
    out: *mut *mut c_void,
    size: *mut usize,
) -> i32 {
    let gc = unsafe { (*guc).capture };
    if gc.is_null() {
        return -ENODEV;
    }
    let size_bytes = 4 * size_of::<u32>();
    if unsafe { (*gc).ads_null_cache.is_null() } {
        let header = kzalloc(size_bytes, GFP_KERNEL);
        if header.is_null() {
            return -ENOMEM;
        }
        unsafe { (*gc).ads_null_cache = header };
    }
    unsafe {
        *out = (*gc).ads_null_cache;
        *size = size_bytes;
    }
    0
}

// upstream: intel_guc_capture.c guc_capture_output_min_size_est()
unsafe fn guc_capture_output_min_size_est(guc: *mut IntelGuc) -> i32 {
    let gt = crate::intel_gt_api_upstream::guc_to_gt(guc);
    if unsafe { (*guc).capture.is_null() } {
        return -ENODEV;
    }
    let mut minimum = 0i32;
    for engine in unsafe { &(*gt).engine } {
        if engine.is_null() {
            continue;
        }
        minimum += 2 * 4 + 3 * 5 * 4;
        for type_ in [
            CAPTURE_TYPE_GLOBAL as u32,
            CAPTURE_TYPE_ENGINE_CLASS as u32,
            CAPTURE_TYPE_ENGINE_INSTANCE as u32,
        ] {
            let mut size = 0usize;
            if unsafe {
                guc_capture_getlistsize(guc, 0, type_, (*(*engine)).class as u32, &mut size, true)
            } == 0
            {
                minimum = minimum.saturating_add(size as i32);
            }
        }
    }
    minimum
}

// upstream: intel_guc_capture.c check_guc_capture_size()
unsafe fn check_guc_capture_size(guc: *mut IntelGuc) {
    let min_size = unsafe { guc_capture_output_min_size_est(guc) };
    let spare_size = min_size.saturating_mul(3);
    let buffer_size = unsafe { (*guc).log.sizes[2].bytes };
    if min_size < 0 {
        guc_warn!(
            guc,
            "Failed to calculate error state capture buffer minimum size: %d!\n",
            min_size
        );
    } else if min_size > buffer_size {
        guc_warn!(
            guc,
            "Error state capture buffer maybe small: %d < %d\n",
            buffer_size,
            min_size
        );
    } else if spare_size > buffer_size {
        guc_dbg!(
            guc,
            "Error state capture buffer lacks spare size: %d < %d (min = %d)\n",
            buffer_size,
            spare_size,
            min_size
        );
    }
}

// upstream: intel_guc_capture.c guc_capture_buf_cnt()
unsafe fn guc_capture_buf_cnt(buf: *const CaptureBufState) -> i32 {
    let read = unsafe { (*buf).rd };
    let write = unsafe { (*buf).wr };
    if write >= read {
        (write - read) as i32
    } else {
        ((*buf).size - read + write) as i32
    }
}

// upstream: intel_guc_capture.c guc_capture_buf_cnt_to_end()
unsafe fn guc_capture_buf_cnt_to_end(buf: *const CaptureBufState) -> i32 {
    let read = unsafe { (*buf).rd };
    let write = unsafe { (*buf).wr };
    if read > write {
        ((*buf).size - read) as i32
    } else {
        (write - read) as i32
    }
}

// upstream: intel_guc_capture.c guc_capture_log_remove_dw()
unsafe fn guc_capture_log_remove_dw(
    _guc: *mut IntelGuc,
    buf: *mut CaptureBufState,
    dw: *mut u32,
) -> i32 {
    if unsafe { guc_capture_buf_cnt(buf) } == 0 {
        return 0;
    }
    for _ in 0..2 {
        let available = unsafe { guc_capture_buf_cnt_to_end(buf) };
        if available >= 4 {
            let source = unsafe { (*buf).data.add((*buf).rd as usize).cast::<u32>() };
            unsafe {
                ptr::write_unaligned(dw, ptr::read_unaligned(source));
                (*buf).rd += 4;
            }
            return 4;
        }
        unsafe { (*buf).rd = 0 };
    }
    0
}

// upstream: intel_guc_capture.c guc_capture_data_extracted()
unsafe fn guc_capture_data_extracted(
    buf: *mut CaptureBufState,
    size: i32,
    destination: *mut c_void,
) -> bool {
    if unsafe { guc_capture_buf_cnt_to_end(buf) } >= size {
        unsafe {
            ptr::copy_nonoverlapping(
                (*buf).data.add((*buf).rd as usize),
                destination.cast(),
                size as usize,
            );
            (*buf).rd += size as u32;
        }
        true
    } else {
        false
    }
}

// upstream: intel_guc_capture.c guc_capture_log_get_group_hdr()
unsafe fn guc_capture_log_get_group_hdr(
    guc: *mut IntelGuc,
    buf: *mut CaptureBufState,
    header: *mut CaptureGroupHeader,
) -> i32 {
    let bytes = size_of::<CaptureGroupHeader>() as i32;
    if bytes > unsafe { guc_capture_buf_cnt(buf) } {
        return -1;
    }
    if unsafe { guc_capture_data_extracted(buf, bytes, header.cast()) } {
        return 0;
    }
    let mut read = 0;
    read += unsafe { guc_capture_log_remove_dw(guc, buf, ptr::addr_of_mut!((*header).owner)) };
    read += unsafe { guc_capture_log_remove_dw(guc, buf, ptr::addr_of_mut!((*header).info)) };
    if read != bytes { -1 } else { 0 }
}

// upstream: intel_guc_capture.c guc_capture_log_get_data_hdr()
unsafe fn guc_capture_log_get_data_hdr(
    guc: *mut IntelGuc,
    buf: *mut CaptureBufState,
    header: *mut CaptureDataHeader,
) -> i32 {
    let bytes = size_of::<CaptureDataHeader>() as i32;
    if bytes > unsafe { guc_capture_buf_cnt(buf) } {
        return -1;
    }
    if unsafe { guc_capture_data_extracted(buf, bytes, header.cast()) } {
        return 0;
    }
    let mut read = 0;
    for field in [
        ptr::addr_of_mut!((*header).owner),
        ptr::addr_of_mut!((*header).info),
        ptr::addr_of_mut!((*header).lrca),
        ptr::addr_of_mut!((*header).guc_id),
        ptr::addr_of_mut!((*header).num_mmios),
    ] {
        read += unsafe { guc_capture_log_remove_dw(guc, buf, field) };
    }
    if read != bytes { -1 } else { 0 }
}

// upstream: intel_guc_capture.c guc_capture_log_get_register()
unsafe fn guc_capture_log_get_register(
    guc: *mut IntelGuc,
    buf: *mut CaptureBufState,
    register: *mut guc_mmio_reg,
) -> i32 {
    let bytes = size_of::<guc_mmio_reg>() as i32;
    if bytes > unsafe { guc_capture_buf_cnt(buf) } {
        return -1;
    }
    if unsafe { guc_capture_data_extracted(buf, bytes, register.cast()) } {
        return 0;
    }
    let mut read = 0;
    for field in [
        ptr::addr_of_mut!((*register).offset),
        ptr::addr_of_mut!((*register).value),
        ptr::addr_of_mut!((*register).flags),
        ptr::addr_of_mut!((*register).mask),
    ] {
        read += unsafe { guc_capture_log_remove_dw(guc, buf, field) };
    }
    if read != bytes { -1 } else { 0 }
}

// upstream: intel_guc_capture.c guc_capture_delete_one_node()
unsafe fn guc_capture_delete_one_node(_guc: *mut IntelGuc, node: *mut ParsedOutput) {
    for list in &(*node).reginfo {
        kfree(list.regs);
    }
    list_del(&mut (*node).link);
    kfree(node);
}
// upstream: intel_guc_capture.c guc_capture_delete_prealloc_nodes()
unsafe fn guc_capture_delete_prealloc_nodes(guc: *mut IntelGuc) {
    for head in [
        &mut (*(*guc).capture).outlist as *mut ListHead,
        &mut (*(*guc).capture).cachelist as *mut ListHead,
    ] {
        while !list_empty(&*head) {
            guc_capture_delete_one_node(guc, (*head).next.cast());
        }
    }
}
// upstream: intel_guc_capture.c guc_capture_add_node_to_list()
unsafe fn guc_capture_add_node_to_list(node: *mut ParsedOutput, list: *mut ListHead) {
    list_add_tail(&mut (*node).link, list);
}
// upstream: intel_guc_capture.c guc_capture_add_node_to_outlist()
unsafe fn guc_capture_add_node_to_outlist(gc: *mut IntelGucStateCapture, node: *mut ParsedOutput) {
    guc_capture_add_node_to_list(node, &mut (*gc).outlist);
}
// upstream: intel_guc_capture.c guc_capture_add_node_to_cachelist()
pub unsafe fn guc_capture_add_node_to_cachelist(
    gc: *mut IntelGucStateCapture,
    node: *mut ParsedOutput,
) {
    guc_capture_add_node_to_list(node, &mut (*gc).cachelist);
}
// upstream: intel_guc_capture.c guc_capture_init_node()
unsafe fn guc_capture_init_node(guc: *mut IntelGuc, node: *mut ParsedOutput) {
    let mut tmp = [ptr::null_mut(); 3];
    for i in 0..3 {
        tmp[i] = (*node).reginfo[i].regs;
        ptr::write_bytes(tmp[i], 0, (*(*guc).capture).max_mmio_per_node as usize);
    }
    ptr::write_bytes(node, 0, 1);
    for i in 0..3 {
        (*node).reginfo[i].regs = tmp[i];
    }
    INIT_LIST_HEAD(&mut (*node).link);
}
// upstream: intel_guc_capture.c guc_capture_get_prealloc_node()
unsafe fn guc_capture_get_prealloc_node(guc: *mut IntelGuc) -> *mut ParsedOutput {
    let gc = (*guc).capture;
    let mut found: *mut ParsedOutput = ptr::null_mut();
    if !list_empty(&(*gc).cachelist) {
        found = (*gc).cachelist.next.cast();
        list_del(&mut (*found).link);
    } else {
        let head = &mut (*gc).outlist as *mut ListHead;
        let mut n = (*head).next;
        while n != head {
            found = n.cast();
            n = (*n).next;
        }
        if !found.is_null() {
            list_del(&mut (*found).link);
        }
    }
    if !found.is_null() {
        guc_capture_init_node(guc, found);
    }
    found
}
// upstream: intel_guc_capture.c guc_capture_alloc_one_node()
unsafe fn guc_capture_alloc_one_node(guc: *mut IntelGuc) -> *mut ParsedOutput {
    let new = kzalloc_obj::<ParsedOutput>();
    if new.is_null() {
        return new;
    }
    for i in 0..3 {
        (*new).reginfo[i].regs =
            kzalloc_objs::<CaptureRegister, _>((*(*guc).capture).max_mmio_per_node as usize);
        if (*new).reginfo[i].regs.is_null() {
            for j in (0..i).rev() {
                kfree((*new).reginfo[j].regs);
            }
            kfree(new);
            return ptr::null_mut();
        }
    }
    guc_capture_init_node(guc, new);
    new
}
// upstream: intel_guc_capture.c guc_capture_clone_node()
unsafe fn guc_capture_clone_node(
    guc: *mut IntelGuc,
    original: *mut ParsedOutput,
    keep: u32,
) -> *mut ParsedOutput {
    let new = guc_capture_get_prealloc_node(guc);
    if new.is_null() {
        return new;
    }
    if original.is_null() {
        return new;
    }
    (*new).is_partial = (*original).is_partial;
    for i in 0..3 {
        if keep & (1 << i) != 0 {
            GEM_BUG_ON!(
                (*original).reginfo[i].num_regs > (*(*guc).capture).max_mmio_per_node as u32
            );
            ptr::copy_nonoverlapping(
                (*original).reginfo[i].regs,
                (*new).reginfo[i].regs,
                (*original).reginfo[i].num_regs as usize,
            );
            (*new).reginfo[i].num_regs = (*original).reginfo[i].num_regs;
            (*new).reginfo[i].vfid = (*original).reginfo[i].vfid;
            if i == 1 {
                (*new).eng_class = (*original).eng_class;
            } else if i == 2 {
                (*new).eng_inst = (*original).eng_inst;
                (*new).guc_id = (*original).guc_id;
                (*new).lrca = (*original).lrca;
            }
        }
    }
    new
}
// upstream: intel_guc_capture.c __guc_capture_create_prealloc_nodes()
unsafe fn __guc_capture_create_prealloc_nodes(guc: *mut IntelGuc) {
    for _ in 0..3 * 16 * 32 {
        let node = guc_capture_alloc_one_node(guc);
        if node.is_null() {
            axlog::warn!("Register capture pre-alloc-cache failure");
            return;
        }
        guc_capture_add_node_to_cachelist((*guc).capture, node);
    }
}
/// Explicit mechanism initialization; runtime ADS/IRQ wiring remains off. The
/// owner supplies the max register count measured while building its ADS lists.
pub unsafe fn init_native_capture(guc: *mut IntelGuc, max_registers: i32) -> i32 {
    if max_registers <= 0 || !(*guc).capture.is_null() {
        return -crate::linux_config::EINVAL;
    }
    let gc = kzalloc_obj::<IntelGucStateCapture>();
    if gc.is_null() {
        return -ENOMEM;
    }
    INIT_LIST_HEAD(&mut (*gc).cachelist);
    INIT_LIST_HEAD(&mut (*gc).outlist);
    (*gc).max_mmio_per_node = max_registers;
    (*guc).capture = gc;
    __guc_capture_create_prealloc_nodes(guc);
    0
}

// upstream: intel_guc_capture.c guc_get_max_reglist_count()
unsafe fn guc_get_max_reglist_count(guc: *mut IntelGuc) -> i32 {
    let gc = unsafe { (*guc).capture };
    let mut maximum = 0;
    for owner in 0..2 {
        for type_ in 0..3 {
            for classid in 0..16 {
                if type_ == CAPTURE_TYPE_GLOBAL as u32 && classid > 0 {
                    continue;
                }
                maximum = maximum.max(unsafe { guc_cap_list_num_regs(gc, owner, type_, classid) });
            }
        }
    }
    if maximum == 0 { 64 } else { maximum }
}

// upstream: intel_guc_capture.c guc_capture_create_prealloc_nodes()
unsafe fn guc_capture_create_prealloc_nodes(guc: *mut IntelGuc) {
    let gc = unsafe { (*guc).capture };
    if gc.is_null() || unsafe { (*gc).max_mmio_per_node != 0 } {
        return;
    }
    unsafe { (*gc).max_mmio_per_node = guc_get_max_reglist_count(guc) };
    unsafe { __guc_capture_create_prealloc_nodes(guc) };
}

// upstream: intel_guc_capture.c guc_capture_extract_reglists()
unsafe fn guc_capture_extract_reglists(guc: *mut IntelGuc, buf: &mut CaptureBuffer<'_>) -> i32 {
    let mut node: *mut ParsedOutput = ptr::null_mut();
    let mut ret = 0;
    let i = buf.count();
    if i == 0 {
        return -ENODATA;
    }
    if i % 4 != 0 {
        axlog::warn!("Got mis-aligned register capture entries");
        return -EIO;
    }
    let ghdr = match buf.read_words::<2>() {
        Ok(v) => v,
        Err(_) => return -EIO,
    };
    let partial = (ghdr[1] >> 8) & 0xff != 0;
    let mut lists = (ghdr[1] & 0xff) as i32;
    while lists > 0 {
        lists -= 1;
        let hdr = match buf.read_words::<5>() {
            Ok(v) => v,
            Err(_) => {
                ret = -EIO;
                break;
            }
        };
        let datatype = (hdr[1] & 0xf) as usize;
        if datatype > 2 {
            let mut regs = (hdr[4] & 0x3ff) as i32;
            while regs > 0 {
                regs -= 1;
                if buf.read_words::<4>().is_err() {
                    ret = -EIO;
                    break;
                }
            }
            continue;
        } else if !node.is_null() {
            if datatype == 0 {
                guc_capture_add_node_to_outlist((*guc).capture, node);
                node = ptr::null_mut();
            } else if datatype == 1 && (*node).reginfo[1].num_regs != 0 {
                guc_capture_add_node_to_outlist((*guc).capture, node);
                node = guc_capture_clone_node(guc, node, 1);
            } else if datatype == 2 && (*node).reginfo[2].num_regs != 0 {
                guc_capture_add_node_to_outlist((*guc).capture, node);
                node = guc_capture_clone_node(guc, node, 3);
            }
        }
        if node.is_null() {
            node = guc_capture_get_prealloc_node(guc);
            if node.is_null() {
                ret = -ENOMEM;
                break;
            }
            if datatype != 0 {
                axlog::debug!("Register capture missing global dump: {datatype:08x}");
            }
        }
        (*node).is_partial = partial;
        (*node).reginfo[datatype].vfid = hdr[0] & 0xff;
        match datatype {
            2 => {
                (*node).eng_class = (hdr[1] >> 4) & 0xf;
                (*node).eng_inst = (hdr[1] >> 8) & 0xf;
                (*node).lrca = hdr[2];
                (*node).guc_id = hdr[3];
            }
            1 => (*node).eng_class = (hdr[1] >> 4) & 0xf,
            _ => {}
        }
        let mut numregs = hdr[4] & 0x3ff;
        if numregs > (*(*guc).capture).max_mmio_per_node as u32 {
            axlog::debug!("Register capture list extraction clipped by prealloc");
            numregs = (*(*guc).capture).max_mmio_per_node as u32;
        }
        (*node).reginfo[datatype].num_regs = numregs;
        for i in 0..numregs {
            let v = match buf.read_words::<4>() {
                Ok(v) => v,
                Err(_) => {
                    ret = -EIO;
                    break;
                }
            };
            (*node).reginfo[datatype]
                .regs
                .add(i as usize)
                .write(CaptureRegister {
                    offset: v[0],
                    value: v[1],
                    flags: v[2],
                    mask: v[3],
                });
        }
    }
    if !node.is_null() {
        for i in 0..3 {
            if !(*node).reginfo[i].regs.is_null() {
                guc_capture_add_node_to_outlist((*guc).capture, node);
                node = ptr::null_mut();
                break;
            }
        }
        if !node.is_null() {
            guc_capture_add_node_to_cachelist((*guc).capture, node);
        }
    }
    ret
}
// upstream: intel_guc_capture.c __guc_capture_flushlog_complete()
unsafe fn __guc_capture_flushlog_complete(guc: *mut IntelGuc) -> i32 {
    crate::guc_submission_upstream::intel_guc_send_nb(&mut *guc, &[0x30, 2], 0)
}
// upstream: intel_guc_capture.c __guc_capture_process_output()
unsafe fn __guc_capture_process_output(guc: *mut IntelGuc) {
    let gt = crate::intel_gt_api_upstream::guc_to_gt(guc);
    let state = (*guc).log.buf_addr.cast::<LogBufferState>().add(2);
    assert!(
        !(*guc).log.buf_addr.is_null(),
        "capture log has no mapped buffer owner"
    );
    let local = state.read_unaligned();
    let size = (*guc).log.sizes[2].bytes as usize;
    let source = (*guc)
        .log
        .buf_addr
        .cast::<u8>()
        .add(4096 + (*guc).log.sizes[0].bytes as usize + (*guc).log.sizes[1].bytes as usize);
    let mut rd = local.read_ptr as usize;
    let mut wr = local.sampled_write_ptr as usize;
    let full = (local.flags >> 1) & 0xf;
    (*guc).log.stats[2].flush = (*guc).log.stats[2].flush.wrapping_add(local.flags & 1);
    let s = &mut (*guc).log.stats[2];
    let mut stats = LogStats {
        sampled_overflow: s.sampled_overflow,
        overflow: s.overflow,
        flush: s.flush,
    };
    let overflow = intel_guc_check_log_buf_overflow(&mut stats, full);
    s.sampled_overflow = stats.sampled_overflow;
    s.overflow = stats.overflow;
    if overflow {
        rd = 0;
        wr = size;
    } else if rd > size || wr > size {
        axlog::error!("Register capture buffer in invalid state: read={rd:x}, size={size:x}");
        rd = 0;
        wr = size;
    }
    if !(*gt).uc.reset_in_progress {
        let mut buf = CaptureBuffer::new(core::slice::from_raw_parts(source, size), rd, wr)
            .expect("invalid configured capture buffer");
        loop {
            if guc_capture_extract_reglists(guc, &mut buf) < 0 {
                break;
            }
        }
    }
    ptr::addr_of_mut!((*state).read_ptr).write_unaligned(wr as u32);
    let flags = ptr::addr_of!((*state).flags).read_unaligned();
    ptr::addr_of_mut!((*state).flags).write_unaligned(flags & !1);
    __guc_capture_flushlog_complete(guc);
}

// upstream: intel_guc_capture.c guc_capture_reg_to_str()
unsafe fn guc_capture_reg_to_str(
    guc: *mut IntelGuc,
    owner: u32,
    type_: u32,
    classid: u32,
    id: u32,
    offset: u32,
    is_ext: *mut u32,
) -> Option<&'static str> {
    unsafe { ptr::write(is_ext, 0) };
    let gc = unsafe { (*guc).capture };
    if gc.is_null() || unsafe { (*gc).reglists.is_null() } {
        return None;
    }
    let base = unsafe { guc_capture_get_one_list((*gc).reglists.cast(), owner, type_, id) };
    if base.is_null() {
        return None;
    }
    for index in 0..unsafe { (*base).num_regs as usize } {
        if unsafe { (*(*base).list.add(index)).reg.reg } == offset {
            return gen12_register_name(type_, classid, 0, offset);
        }
    }
    let ext = unsafe { guc_capture_get_one_ext_list((*gc).extlists.cast(), owner, type_, id) };
    if ext.is_null() {
        return None;
    }
    for index in 0..unsafe { (*ext).num_regs as usize } {
        let reg = unsafe { (*(*ext).extlist.add(index)).reg.reg };
        if reg == offset {
            unsafe { ptr::write(is_ext, 1) };
            return crate::guc_capture::gen12_steered_register_name(offset);
        }
    }
    None
}

// upstream: intel_guc_capture.c intel_guc_capture_print_engine_node()
unsafe fn intel_guc_capture_print_engine_node(
    printer: *mut DrmPrinter,
    engine: *mut IntelEngineCs,
    node: *mut ParsedOutput,
) -> i32 {
    if printer.is_null() || engine.is_null() {
        return -EINVAL;
    }
    drm_printer_write(
        printer,
        &alloc::format!(
            "global --- GuC Error Capture on {} command stream:\n",
            unsafe { core::ffi::CStr::from_ptr((*engine).name.as_ptr()) }.to_string_lossy()
        ),
    );
    if node.is_null() {
        drm_printer_write(printer, "  No matching ee-node\n");
        return 0;
    }
    drm_printer_write(
        printer,
        if unsafe { (*node).is_partial } {
            "Coverage: partial-capture\n"
        } else {
            "Coverage: full-capture\n"
        },
    );
    let guc = crate::intel_gt_api_upstream::gt_to_guc(unsafe { (*engine).gt });
    for type_ in 0..3usize {
        let info = unsafe { ptr::addr_of!((*node).reginfo[type_]) };
        drm_printer_write(
            printer,
            &alloc::format!(
                "  RegListType: {}\n    Owner-Id: {}\n",
                ["Global", "Engine-Class", "Engine-Instance"][type_],
                unsafe { (*info).vfid },
            ),
        );
        if type_ == CAPTURE_TYPE_ENGINE_CLASS as usize {
            drm_printer_write(
                printer,
                &alloc::format!("    GuC-Eng-Class: {}\n", unsafe { (*node).eng_class }),
            );
        } else if type_ == CAPTURE_TYPE_ENGINE_INSTANCE as usize {
            drm_printer_write(
                printer,
                &alloc::format!(
                    "    i915-Eng-Class: {}\n    i915-Eng-Inst-Id: {}\n    GuC-Engine-Inst-Id: \
                     0x{:08x}\n    GuC-Context-Id: 0x{:08x}\n    LRCA: 0x{:08x}\n",
                    unsafe { (*engine).class },
                    unsafe { (*engine).instance },
                    unsafe { (*node).eng_inst },
                    unsafe { (*node).guc_id },
                    unsafe { (*node).lrca },
                ),
            );
        }
        let count = unsafe { (*info).num_regs };
        drm_printer_write(printer, &alloc::format!("    NumRegs: {count}\n"));
        for index in 0..count as usize {
            let reg = unsafe { ptr::read_unaligned((*info).regs.add(index)) };
            let mut is_ext = 0;
            let name = unsafe {
                guc_capture_reg_to_str(
                    guc,
                    0,
                    type_ as u32,
                    (*node).eng_class,
                    0,
                    reg.offset,
                    &mut is_ext,
                )
            };
            if let Some(name) = name {
                drm_printer_write(printer, &alloc::format!("      {name}"));
            } else {
                drm_printer_write(printer, &alloc::format!("      REG-0x{:08x}", reg.offset));
            }
            if is_ext != 0 {
                let group = (reg.flags
                    & crate::intel_guc_fwif_types_upstream::GUC_REGSET_STEERING_GROUP)
                    >> 12;
                let instance = (reg.flags
                    & crate::intel_guc_fwif_types_upstream::GUC_REGSET_STEERING_INSTANCE)
                    >> 20;
                drm_printer_write(printer, &alloc::format!("[{group}][{instance}]"));
            }
            drm_printer_write(printer, &alloc::format!(":  0x{:08x}\n", reg.value));
        }
    }
    0
}

// upstream: intel_guc_capture.c guc_capture_find_ecode()
unsafe fn guc_capture_find_ecode(engine_dump: *mut IntelEngineCoredumpView) {
    if unsafe { (*engine_dump).guc_capture_node.is_null() } {
        return;
    }
    let info = unsafe {
        ptr::addr_of!(
            (*(*engine_dump).guc_capture_node).reginfo[CAPTURE_TYPE_ENGINE_INSTANCE as usize]
        )
    };
    for index in 0..unsafe { (*info).num_regs as usize } {
        let reg = unsafe { ptr::read_unaligned((*info).regs.add(index)) };
        if reg.offset == crate::intel_engine_regs_upstream::RING_IPEHR(0).reg {
            unsafe { (*engine_dump).ipehr = reg.value };
        } else if reg.offset == crate::intel_engine_regs_upstream::RING_INSTDONE(0).reg {
            unsafe { (*engine_dump).instdone = reg.value };
        }
    }
}

// upstream: intel_guc_capture.c intel_guc_capture_free_node()
unsafe fn intel_guc_capture_free_node(engine_dump: *mut IntelEngineCoredumpView) {
    if engine_dump.is_null() || unsafe { (*engine_dump).guc_capture_node.is_null() } {
        return;
    }
    unsafe {
        guc_capture_add_node_to_cachelist((*engine_dump).capture, (*engine_dump).guc_capture_node);
        (*engine_dump).capture = ptr::null_mut();
        (*engine_dump).guc_capture_node = ptr::null_mut();
    }
}

// upstream: intel_guc_capture.c intel_guc_capture_is_matching_engine()
pub unsafe fn intel_guc_capture_is_matching_engine(
    gt: *mut IntelGt,
    ce: *mut IntelContext,
    engine: *mut IntelEngineCs,
) -> bool {
    if gt.is_null() || ce.is_null() || engine.is_null() {
        return false;
    }
    let guc = crate::intel_gt_api_upstream::gt_to_guc(gt);
    if (*guc).capture.is_null() {
        return false;
    }
    let head = &(*(*guc).capture).outlist as *const ListHead as *mut ListHead;
    let mut node = (*head).next;
    while node != head {
        let n = node.cast::<ParsedOutput>();
        if (*n).eng_inst == ((*engine).guc_id as u32 >> 3) & 0xf
            && (*n).eng_class == (*engine).guc_id as u32 & 7
            && (*n).guc_id == (*ce).guc_id.id as u32
            && ((*n).lrca & 0xfffff000) == ((&(*ce).lrc).lrca & 0xfffff000)
        {
            return true;
        }
        node = (*node).next;
    }
    false
}

// upstream: intel_guc_capture.c intel_guc_capture_get_matching_node()
unsafe fn intel_guc_capture_get_matching_node(
    gt: *mut IntelGt,
    engine_dump: *mut IntelEngineCoredumpView,
    ce: *mut IntelContext,
) {
    if gt.is_null() || engine_dump.is_null() || ce.is_null() {
        return;
    }
    let engine = unsafe { (*engine_dump).engine };
    if engine.is_null() {
        return;
    }
    let guc = crate::intel_gt_api_upstream::gt_to_guc(gt);
    if unsafe { (*guc).capture.is_null() } {
        return;
    }
    GEM_BUG_ON!(unsafe { !(*engine_dump).guc_capture_node.is_null() });
    let gc = unsafe { (*guc).capture };
    let head = ptr::addr_of_mut!((*gc).outlist);
    let mut node = unsafe { (*head).next };
    while node != head {
        let output = node.cast::<ParsedOutput>();
        if unsafe {
            (*output).eng_inst == ((*engine).guc_id >> 3) & 0xf
                && (*output).eng_class == (*engine).guc_id & 7
                && (*output).guc_id == (*ce).guc_id.id as u32
                && ((*output).lrca & 0xffff_f000) == ((&(*ce).lrc).lrca & 0xffff_f000)
        } {
            unsafe {
                list_del(&mut (*output).link);
                (*engine_dump).guc_capture_node = output;
                (*engine_dump).capture = gc;
                guc_capture_find_ecode(engine_dump);
            }
            return;
        }
        node = unsafe { (*node).next };
    }
    guc_warn!(
        guc,
        "No register capture node found for 0x%04X / 0x%08X\n",
        unsafe { (*ce).guc_id.id },
        unsafe { (*ce).lrc.lrca }
    );
}

// upstream: intel_guc_capture.c intel_guc_capture_process()
pub unsafe fn intel_guc_capture_process(guc: *mut IntelGuc) {
    if !(*guc).capture.is_null() {
        __guc_capture_process_output(guc);
    }
}

// upstream: intel_guc_capture.c guc_capture_free_ads_cache()
unsafe fn guc_capture_free_ads_cache(gc: *mut IntelGucStateCapture) {
    if gc.is_null() {
        return;
    }
    for owner in 0..2 {
        for type_ in 0..3 {
            for classid in 0..16 {
                let cache = unsafe { &mut (*gc).ads_cache[owner][type_][classid] };
                if cache.is_valid {
                    unsafe { kfree(cache.ptr) };
                    cache.ptr = ptr::null_mut();
                }
            }
        }
    }
    unsafe {
        kfree((*gc).ads_null_cache);
        (*gc).ads_null_cache = ptr::null_mut();
    }
}

// upstream: intel_guc_capture.c intel_guc_capture_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_capture_destroy(guc: *mut IntelGuc) {
    let gc = unsafe { (*guc).capture };
    if gc.is_null() {
        return;
    }
    unsafe {
        guc_capture_free_ads_cache(gc);
        guc_capture_delete_prealloc_nodes(guc);
        guc_capture_free_extlists((*gc).extlists.cast());
        kfree((*gc).extlists);
        free_reglist_groups((*gc).reglists.cast_mut());
        kfree(gc);
        (*guc).capture = ptr::null_mut();
    }
}

// upstream: intel_guc_capture.c intel_guc_capture_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_capture_init(guc: *mut IntelGuc) -> i32 {
    if unsafe { !(*guc).capture.is_null() } {
        return -EINVAL;
    }
    let gc = kzalloc_obj::<IntelGucStateCapture>();
    if gc.is_null() {
        return -ENOMEM;
    }
    unsafe {
        INIT_LIST_HEAD(&mut (*gc).outlist);
        INIT_LIST_HEAD(&mut (*gc).cachelist);
        (*guc).capture = gc;
        let lists = guc_capture_get_device_reglist(guc);
        if lists.is_null() {
            intel_guc_capture_destroy(guc);
            return -ENOMEM;
        }
        (*gc).reglists = lists.cast();
        check_guc_capture_size(guc);
    }
    0
}
