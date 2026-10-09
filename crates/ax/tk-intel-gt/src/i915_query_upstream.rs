// SPDX-License-Identifier: MIT
// Copyright © 2018 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/i915_query.c.
// UAPI records are locally mirrored below; user access, engine traversal,
// memory-region accounting, and PERF ownership remain real kernel/i915 APIs.

#![allow(unsafe_code, non_snake_case, non_camel_case_types, dead_code)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::{MaybeUninit, offset_of, size_of},
    ptr,
};

use crate::{
    intel_context_upstream::{Kref, RadixTreeRoot},
    intel_engine_cs_upstream::{IntelEngineCs, Mutex, RbRoot},
    intel_engine_types_upstream::RENDER_CLASS,
    intel_engine_user_upstream::{engine_uabi_tree, intel_engine_lookup_user},
    intel_sseu_types_upstream::{GEN_SSEU_STRIDE, SseuDevInfo},
    intel_uc_types_upstream::intel_uc_uses_guc_submission,
    intel_workarounds_types_upstream::I915RegT,
    linux::{
        gem_memory::{INTEL_MEMORY_LOCAL, INTEL_REGION_UNKNOWN, IntelMemoryRegion},
        i915::{GRAPHICS_VER_FULL, IP_VER, i915_mmio_reg_offset, to_gt, to_i915},
        i915_private::{DrmI915Private, IntelRuntimePmPrefix},
        memory::{kfree, kmalloc, kref_get_unless_zero, kref_put},
        mm_native::{copy_from_user, copy_to_user},
        primitives::ilog2,
        rbtree::rb_next,
        rcu::{rcu_read_lock, rcu_read_unlock},
    },
    linux_config::{EFAULT, EINVAL, ENODEV, ENOENT, ENOMEM, GFP_KERNEL},
};

const DRM_I915_QUERY_TOPOLOGY_INFO: u64 = 1;
const DRM_I915_QUERY_ENGINE_INFO: u64 = 2;
const DRM_I915_QUERY_PERF_CONFIG: u64 = 3;
const DRM_I915_QUERY_MEMORY_REGIONS: u64 = 4;
const DRM_I915_QUERY_HWCONFIG_BLOB: u64 = 5;
const DRM_I915_QUERY_GEOMETRY_SUBSLICES: u64 = 6;
const DRM_I915_QUERY_GUC_SUBMISSION_VERSION: u64 = 7;
const DRM_I915_QUERY_PERF_CONFIG_LIST: u32 = 1;
const DRM_I915_QUERY_PERF_CONFIG_DATA_FOR_UUID: u32 = 2;
const DRM_I915_QUERY_PERF_CONFIG_DATA_FOR_ID: u32 = 3;
const I915_ENGINE_INFO_HAS_LOGICAL_INSTANCE: u64 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmI915QueryItem {
    query_id: u64,
    length: i32,
    flags: u32,
    data_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmI915Query {
    num_items: u32,
    flags: u32,
    items_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmI915QueryTopologyInfo {
    flags: u16,
    max_slices: u16,
    max_subslices: u16,
    max_eus_per_subslice: u16,
    subslice_offset: u16,
    subslice_stride: u16,
    eu_offset: u16,
    eu_stride: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct I915EngineClassInstance {
    engine_class: u16,
    engine_instance: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmI915EngineInfo {
    engine: I915EngineClassInstance,
    rsvd0: u32,
    flags: u64,
    capabilities: u64,
    logical_instance: u16,
    rsvd1: [u16; 3],
    rsvd2: [u64; 3],
}

impl Default for DrmI915EngineInfo {
    fn default() -> Self {
        Self {
            engine: I915EngineClassInstance::default(),
            rsvd0: 0,
            flags: 0,
            capabilities: 0,
            logical_instance: 0,
            rsvd1: [0; 3],
            rsvd2: [0; 3],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmI915QueryEngineInfo {
    num_engines: u32,
    rsvd: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmI915GemMemoryClassInstance {
    memory_class: u16,
    memory_instance: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmI915MemoryRegionInfo {
    region: DrmI915GemMemoryClassInstance,
    rsvd0: u32,
    probed_size: u64,
    unallocated_size: u64,
    probed_cpu_visible_size: u64,
    unallocated_cpu_visible_size: u64,
    _rsvd1: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmI915QueryMemoryRegions {
    num_regions: u32,
    rsvd: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmI915QueryGucSubmissionVersion {
    branch: u32,
    major: u32,
    minor: u32,
    patch: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
union DrmI915QueryPerfConfigKey {
    n_configs: u64,
    config: u64,
    uuid: [c_char; 36],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmI915QueryPerfConfig {
    key: DrmI915QueryPerfConfigKey,
    flags: u32,
    _data: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmI915PerfOaConfig {
    uuid: [c_char; 36],
    n_mux_regs: u32,
    n_boolean_regs: u32,
    n_flex_regs: u32,
    mux_regs_ptr: u64,
    boolean_regs_ptr: u64,
    flex_regs_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct I915OaReg {
    addr: I915RegT,
    value: u32,
}

#[repr(C)]
struct I915Idr {
    radix_tree: RadixTreeRoot,
    base: u32,
    next: u32,
}

/// The i915_perf prefix through `metrics_idr`, with its source field offsets.
#[repr(C)]
struct I915PerfMetricsPrefix {
    i915: *mut DrmI915Private,
    metrics_kobj: *mut c_void,
    metrics_lock: Mutex,
    metrics_idr: I915Idr,
}

/// `i915_oa_config` fields consumed by query PERF, retaining the real kref and
/// upstream ref-release callback. The opaque sysfs members occupy source bytes.
#[repr(C)]
struct I915OaConfigView {
    perf: *mut c_void,
    uuid: [c_char; 37],
    id: c_int,
    mux_regs: *const I915OaReg,
    mux_regs_len: u32,
    b_counter_regs: *const I915OaReg,
    b_counter_regs_len: u32,
    flex_regs: *const I915OaReg,
    flex_regs_len: u32,
    _sysfs: [u8; 100],
    ref_: Kref,
    _rcu_align: u32,
    _rcu: [u64; 2],
}

const _: [(); 24] = [(); size_of::<DrmI915QueryItem>()];
const _: [(); 16] = [(); size_of::<DrmI915Query>()];
const _: [(); 16] = [(); size_of::<DrmI915QueryTopologyInfo>()];
const _: [(); 56] = [(); size_of::<DrmI915EngineInfo>()];
const _: [(); 16] = [(); size_of::<DrmI915QueryEngineInfo>()];
const _: [(); 88] = [(); size_of::<DrmI915MemoryRegionInfo>()];
const _: [(); 16] = [(); size_of::<DrmI915QueryMemoryRegions>()];
const _: [(); 16] = [(); size_of::<DrmI915QueryGucSubmissionVersion>()];
const _: [(); 48] = [(); size_of::<DrmI915QueryPerfConfig>()];
const _: [(); 72] = [(); size_of::<DrmI915PerfOaConfig>()];
const _: [(); 8] = [(); size_of::<I915OaReg>()];
const _: [(); 24] = [(); size_of::<I915Idr>()];
const _: [(); 40] = [(); offset_of!(I915PerfMetricsPrefix, metrics_idr)];
const I915_PERF_OFFSET: usize =
    offset_of!(DrmI915Private, runtime_pm) + size_of::<IntelRuntimePmPrefix>() + 48;
const _: [(); 3328] = [(); I915_PERF_OFFSET];
const _: [(); 8] = [(); offset_of!(I915OaConfigView, uuid)];
const _: [(); 48] = [(); offset_of!(I915OaConfigView, id)];
const _: [(); 56] = [(); offset_of!(I915OaConfigView, mux_regs)];
const _: [(); 64] = [(); offset_of!(I915OaConfigView, mux_regs_len)];
const _: [(); 72] = [(); offset_of!(I915OaConfigView, b_counter_regs)];
const _: [(); 80] = [(); offset_of!(I915OaConfigView, b_counter_regs_len)];
const _: [(); 88] = [(); offset_of!(I915OaConfigView, flex_regs)];
const _: [(); 96] = [(); offset_of!(I915OaConfigView, flex_regs_len)];
const _: [(); 200] = [(); offset_of!(I915OaConfigView, ref_)];
const _: [(); 224] = [(); size_of::<I915OaConfigView>()];

unsafe extern "C" {
    fn idr_get_next(idr: *mut I915Idr, next_id: *mut c_int) -> *mut c_void;
    fn i915_perf_get_oa_config(perf: *mut c_void, metrics_set: c_int) -> *mut c_void;
    fn i915_oa_config_release(ref_: *mut Kref);
    fn intel_memory_region_avail(
        mr: *mut IntelMemoryRegion,
        avail: *mut u64,
        visible_avail: *mut u64,
    );
    fn perfmon_capable() -> bool;
}

#[inline]
unsafe fn perf_prefix(i915: *mut DrmI915Private) -> *mut I915PerfMetricsPrefix {
    // C layout probe: drm_i915_private.perf is byte 3328 in this x86_64
    // Linux 7.2.3 target. The canonical private record keeps that region opaque.
    unsafe { i915.cast::<u8>().add(I915_PERF_OFFSET).cast() }
}

#[inline]
unsafe fn metrics_idr(perf: *mut I915PerfMetricsPrefix) -> *mut I915Idr {
    unsafe { ptr::addr_of_mut!((*perf).metrics_idr) }
}

#[inline]
unsafe fn oa_config_view(config: *mut c_void) -> *mut I915OaConfigView {
    config.cast()
}

#[inline]
unsafe fn read_user<T: Copy>(source: *const T) -> Result<T, i32> {
    let mut value = MaybeUninit::<T>::uninit();
    if unsafe { copy_from_user(value.as_mut_ptr().cast(), source.cast(), size_of::<T>()) } != 0 {
        return Err(-EFAULT);
    }
    Ok(unsafe { value.assume_init() })
}

#[inline]
unsafe fn write_user<T>(destination: *mut T, value: &T) -> bool {
    unsafe {
        copy_to_user(
            destination.cast(),
            (value as *const T).cast(),
            size_of::<T>(),
        ) == 0
    }
}

#[inline]
unsafe fn copy_user_bytes(destination: *mut c_void, source: *const c_void, size: usize) -> bool {
    unsafe { copy_to_user(destination, source, size) == 0 }
}

#[inline]
unsafe fn user_address(base: u64, offset: usize) -> *mut c_void {
    (base as usize).wrapping_add(offset) as *mut c_void
}

// upstream: i915_query.c copy_query_item()
unsafe fn copy_query_item(
    query_hdr: *mut c_void,
    query_sz: usize,
    total_length: u32,
    query_item: *const DrmI915QueryItem,
) -> i32 {
    let length = unsafe { (*query_item).length };
    if length == 0 {
        return total_length as i32;
    }
    if length < total_length as i32 {
        return -EINVAL;
    }
    let source = unsafe { user_address((*query_item).data_ptr, 0) };
    if unsafe { copy_from_user(query_hdr, source.cast_const(), query_sz) } != 0 {
        return -EFAULT;
    }
    0
}

// upstream: i915_query.c fill_topology_info()
unsafe fn fill_topology_info(
    sseu: *const SseuDevInfo,
    query_item: *const DrmI915QueryItem,
    _subslice_mask: crate::intel_sseu_types_upstream::IntelSseuSsMask,
) -> i32 {
    let max_slices = unsafe { (*sseu).max_slices as usize };
    if max_slices == 0 {
        return -ENODEV;
    }
    let max_subslices = unsafe { (*sseu).max_subslices as usize };
    let max_eus = unsafe { (*sseu).max_eus_per_subslice as usize };
    let slice_length = size_of::<u8>();
    let ss_stride = GEN_SSEU_STRIDE(max_subslices);
    let eu_stride = GEN_SSEU_STRIDE(max_eus);
    let subslice_length = max_slices * ss_stride;
    let eu_length = max_slices * max_subslices * eu_stride;
    let total_length =
        (size_of::<DrmI915QueryTopologyInfo>() + slice_length + subslice_length + eu_length) as u32;
    let mut topo = DrmI915QueryTopologyInfo::default();
    let ret = unsafe {
        copy_query_item(
            (&mut topo as *mut DrmI915QueryTopologyInfo).cast(),
            size_of::<DrmI915QueryTopologyInfo>(),
            total_length,
            query_item,
        )
    };
    if ret != 0 {
        return ret;
    }
    topo = DrmI915QueryTopologyInfo {
        flags: 0,
        max_slices: max_slices as u16,
        max_subslices: max_subslices as u16,
        max_eus_per_subslice: max_eus as u16,
        subslice_offset: slice_length as u16,
        subslice_stride: ss_stride as u16,
        eu_offset: (slice_length + subslice_length) as u16,
        eu_stride: eu_stride as u16,
    };

    let data_ptr = unsafe { (*query_item).data_ptr };
    if !unsafe {
        copy_user_bytes(
            user_address(data_ptr, 0),
            (&topo as *const DrmI915QueryTopologyInfo).cast(),
            size_of::<DrmI915QueryTopologyInfo>(),
        )
    } {
        return -EFAULT;
    }
    if !unsafe {
        copy_user_bytes(
            user_address(data_ptr, size_of::<DrmI915QueryTopologyInfo>()),
            ptr::addr_of!((*sseu).slice_mask).cast(),
            slice_length,
        )
    } {
        return -EFAULT;
    }
    let ss_dest = unsafe {
        user_address(
            data_ptr,
            size_of::<DrmI915QueryTopologyInfo>() + slice_length,
        )
    };
    if unsafe { crate::intel_sseu_upstream::intel_sseu_copy_ssmask_to_user(ss_dest, sseu) } != 0 {
        return -EFAULT;
    }
    let eu_dest = unsafe {
        user_address(
            data_ptr,
            size_of::<DrmI915QueryTopologyInfo>() + slice_length + subslice_length,
        )
    };
    if unsafe { crate::intel_sseu_upstream::intel_sseu_copy_eumask_to_user(eu_dest, sseu) } != 0 {
        return -EFAULT;
    }
    total_length as i32
}

// upstream: i915_query.c query_topology_info()
unsafe extern "C" fn query_topology_info(
    i915: *mut DrmI915Private,
    query_item: *mut DrmI915QueryItem,
) -> i32 {
    if unsafe { (*query_item).flags } != 0 {
        return -EINVAL;
    }
    let gt = unsafe { to_gt(i915) };
    let sseu = unsafe { ptr::addr_of!((*gt).info.sseu) };
    let mask = unsafe { (*sseu).subslice_mask };
    unsafe { fill_topology_info(sseu, query_item, mask) }
}

// upstream: i915_query.c query_geometry_subslices()
unsafe extern "C" fn query_geometry_subslices(
    i915: *mut DrmI915Private,
    query_item: *mut DrmI915QueryItem,
) -> i32 {
    if unsafe { GRAPHICS_VER_FULL(i915) } < IP_VER(12, 55) {
        return -ENODEV;
    }
    let classinstance = unsafe {
        ptr::read_unaligned(ptr::addr_of!((*query_item).flags).cast::<I915EngineClassInstance>())
    };
    let engine = unsafe {
        intel_engine_lookup_user(
            i915,
            classinstance.engine_class as u8,
            classinstance.engine_instance as u8,
        )
    };
    if engine.is_null() {
        return -EINVAL;
    }
    if unsafe { (*engine).class as i32 } != RENDER_CLASS {
        return -EINVAL;
    }
    let sseu = unsafe { ptr::addr_of!((*(*engine).gt).info.sseu) };
    let mask = unsafe { (*sseu).geometry_subslice_mask };
    unsafe { fill_topology_info(sseu, query_item, mask) }
}

#[inline]
unsafe fn uabi_first(root: *const RbRoot) -> *mut IntelEngineCs {
    let node = unsafe { crate::intel_engine_api_upstream::rb_first_uabi_engine(root) };
    if node.is_null() {
        ptr::null_mut()
    } else {
        unsafe { crate::intel_engine_api_upstream::rb_to_uabi_engine(node) }
    }
}

#[inline]
unsafe fn uabi_next(engine: *mut IntelEngineCs) -> *mut IntelEngineCs {
    let node = unsafe { ptr::addr_of_mut!((*engine).uabi.uabi_node) };
    let next = unsafe { rb_next(node) };
    if next.is_null() {
        ptr::null_mut()
    } else {
        unsafe { crate::intel_engine_api_upstream::rb_to_uabi_engine(next) }
    }
}

// upstream: i915_query.c query_engine_info()
unsafe extern "C" fn query_engine_info(
    i915: *mut DrmI915Private,
    query_item: *mut DrmI915QueryItem,
) -> i32 {
    if unsafe { (*query_item).flags } != 0 {
        return -EINVAL;
    }
    let root = unsafe { engine_uabi_tree(i915) };
    let mut num_uabi_engines = 0usize;
    let mut engine = unsafe { uabi_first(root) };
    while !engine.is_null() {
        num_uabi_engines += 1;
        engine = unsafe { uabi_next(engine) };
    }
    let total_length = size_of::<DrmI915QueryEngineInfo>()
        .saturating_add(num_uabi_engines.saturating_mul(size_of::<DrmI915EngineInfo>()));
    if total_length > i32::MAX as usize {
        return -EINVAL;
    }
    let mut query = DrmI915QueryEngineInfo::default();
    let ret = unsafe {
        copy_query_item(
            (&mut query as *mut DrmI915QueryEngineInfo).cast(),
            size_of::<DrmI915QueryEngineInfo>(),
            total_length as u32,
            query_item,
        )
    };
    if ret != 0 {
        return ret;
    }
    if query.num_engines != 0 || query.rsvd.iter().any(|value| *value != 0) {
        return -EINVAL;
    }

    let base = unsafe { (*query_item).data_ptr };
    let mut index = 0usize;
    engine = unsafe { uabi_first(root) };
    while !engine.is_null() {
        let logical_mask = unsafe { (*engine).logical_mask };
        let mut info = DrmI915EngineInfo::default();
        info.engine.engine_class = unsafe { (*engine).uabi_class };
        info.engine.engine_instance = unsafe { (*engine).uabi_instance };
        info.flags = I915_ENGINE_INFO_HAS_LOGICAL_INSTANCE;
        info.capabilities = unsafe { (*engine).uabi_capabilities as u64 };
        info.logical_instance = if logical_mask == 0 {
            0
        } else {
            ilog2(logical_mask) as u16
        };
        let destination = unsafe {
            user_address(
                base,
                size_of::<DrmI915QueryEngineInfo>() + index * size_of::<DrmI915EngineInfo>(),
            )
        };
        if !unsafe { write_user(destination.cast(), &info) } {
            return -EFAULT;
        }
        query.num_engines += 1;
        index += 1;
        engine = unsafe { uabi_next(engine) };
    }
    let destination = unsafe { user_address(base, 0) };
    if !unsafe { write_user(destination.cast(), &query) } {
        return -EFAULT;
    }
    total_length as i32
}

// upstream: i915_query.c can_copy_perf_config_registers_or_number()
unsafe fn can_copy_perf_config_registers_or_number(
    user_n_regs: u32,
    _user_regs_ptr: u64,
    kernel_n_regs: u32,
) -> i32 {
    if user_n_regs == 0 {
        return 0;
    }
    if user_n_regs < kernel_n_regs {
        return -EINVAL;
    }
    0
}

// upstream: i915_query.c copy_perf_config_registers_or_number()
unsafe fn copy_perf_config_registers_or_number(
    kernel_regs: *const I915OaReg,
    kernel_n_regs: u32,
    user_regs_ptr: u64,
    user_n_regs: &mut u32,
) -> i32 {
    if *user_n_regs == 0 {
        *user_n_regs = kernel_n_regs;
        return 0;
    }
    *user_n_regs = kernel_n_regs;
    for index in 0..kernel_n_regs as usize {
        let register = unsafe { *kernel_regs.add(index) };
        let pair = [i915_mmio_reg_offset(register.addr), register.value];
        let destination = unsafe { user_address(user_regs_ptr, index * 2 * size_of::<u32>()) };
        if !unsafe { copy_user_bytes(destination, pair.as_ptr().cast(), size_of::<[u32; 2]>()) } {
            return -EFAULT;
        }
    }
    0
}

#[inline]
unsafe fn oa_config_get(config: *mut c_void) -> *mut c_void {
    if config.is_null() {
        return config;
    }
    let view = unsafe { oa_config_view(config) };
    if unsafe { kref_get_unless_zero(&mut (*view).ref_) } {
        config
    } else {
        ptr::null_mut()
    }
}

#[inline]
unsafe fn oa_config_put(config: *mut c_void) {
    if config.is_null() {
        return;
    }
    let view = unsafe { oa_config_view(config) };
    unsafe { kref_put(ptr::addr_of_mut!((*view).ref_), i915_oa_config_release) };
}

#[inline]
fn uuid_cstr_eq(left: &[c_char; 37], right: &[c_char; 37]) -> bool {
    for index in 0..37 {
        if left[index] != right[index] {
            return false;
        }
        if left[index] == 0 {
            return true;
        }
    }
    true
}

unsafe fn oa_config_by_uuid(perf: *mut I915PerfMetricsPrefix, uuid: &[c_char; 37]) -> *mut c_void {
    let idr = unsafe { metrics_idr(perf) };
    let mut id = 0;
    let mut found = ptr::null_mut();
    rcu_read_lock();
    loop {
        let config = unsafe { idr_get_next(idr, &mut id) };
        if config.is_null() {
            break;
        }
        let view = unsafe { oa_config_view(config) };
        if uuid_cstr_eq(unsafe { &(*view).uuid }, uuid) {
            found = unsafe { oa_config_get(config) };
            break;
        }
        id = id.wrapping_add(1);
    }
    rcu_read_unlock();
    found
}

// upstream: i915_query.c query_perf_config_data()
unsafe fn query_perf_config_data(
    i915: *mut DrmI915Private,
    query_item: *const DrmI915QueryItem,
    use_uuid: bool,
) -> i32 {
    let perf = unsafe { perf_prefix(i915) };
    if unsafe { (*perf).i915.is_null() } {
        return -ENODEV;
    }
    let total_size = size_of::<DrmI915QueryPerfConfig>() + size_of::<DrmI915PerfOaConfig>();
    let length = unsafe { (*query_item).length };
    if length == 0 {
        return total_size as i32;
    }
    if length < total_size as i32 {
        return -EINVAL;
    }

    let base = unsafe { (*query_item).data_ptr };
    let query_config_ptr = unsafe { user_address(base, 0) }.cast::<DrmI915QueryPerfConfig>();
    let flags = match unsafe { read_user(ptr::addr_of!((*query_config_ptr).flags)) } {
        Ok(value) => value,
        Err(error) => return error,
    };
    if flags != 0 {
        return -EINVAL;
    }

    let config = if use_uuid {
        let mut uuid = [0 as c_char; 37];
        if !unsafe {
            copy_from_user(
                uuid.as_mut_ptr().cast(),
                ptr::addr_of!((*query_config_ptr).key.uuid).cast(),
                36,
            ) == 0
        } {
            return -EFAULT;
        }
        unsafe { oa_config_by_uuid(perf, &uuid) }
    } else {
        let config_id = match unsafe { read_user(ptr::addr_of!((*query_config_ptr).key.config)) } {
            Ok(value) => value,
            Err(error) => return error,
        };
        unsafe { i915_perf_get_oa_config(perf.cast(), config_id as c_int) }
    };
    if config.is_null() {
        return -ENOENT;
    }

    let user_config_ptr = unsafe { user_address(base, size_of::<DrmI915QueryPerfConfig>()) }
        .cast::<DrmI915PerfOaConfig>();
    let mut user_config = match unsafe { read_user(user_config_ptr) } {
        Ok(value) => value,
        Err(error) => {
            unsafe { oa_config_put(config) };
            return error;
        }
    };
    let oa = unsafe { oa_config_view(config) };
    let mut ret = unsafe {
        can_copy_perf_config_registers_or_number(
            user_config.n_boolean_regs,
            user_config.boolean_regs_ptr,
            (*oa).b_counter_regs_len,
        )
    };
    if ret == 0 {
        ret = unsafe {
            can_copy_perf_config_registers_or_number(
                user_config.n_flex_regs,
                user_config.flex_regs_ptr,
                (*oa).flex_regs_len,
            )
        };
    }
    if ret == 0 {
        ret = unsafe {
            can_copy_perf_config_registers_or_number(
                user_config.n_mux_regs,
                user_config.mux_regs_ptr,
                (*oa).mux_regs_len,
            )
        };
    }
    if ret == 0 {
        ret = unsafe {
            copy_perf_config_registers_or_number(
                (*oa).b_counter_regs,
                (*oa).b_counter_regs_len,
                user_config.boolean_regs_ptr,
                &mut user_config.n_boolean_regs,
            )
        };
    }
    if ret == 0 {
        ret = unsafe {
            copy_perf_config_registers_or_number(
                (*oa).flex_regs,
                (*oa).flex_regs_len,
                user_config.flex_regs_ptr,
                &mut user_config.n_flex_regs,
            )
        };
    }
    if ret == 0 {
        ret = unsafe {
            copy_perf_config_registers_or_number(
                (*oa).mux_regs,
                (*oa).mux_regs_len,
                user_config.mux_regs_ptr,
                &mut user_config.n_mux_regs,
            )
        };
    }
    if ret == 0 {
        user_config
            .uuid
            .copy_from_slice(unsafe { &(&(*oa).uuid)[..36] });
        if !unsafe { write_user(user_config_ptr, &user_config) } {
            ret = -EFAULT;
        } else {
            ret = total_size as i32;
        }
    }
    unsafe { oa_config_put(config) };
    ret
}

// upstream: i915_query.c sizeof_perf_config_list()
unsafe fn sizeof_perf_config_list(count: usize) -> usize {
    size_of::<DrmI915QueryPerfConfig>() + size_of::<u64>() * count
}

// upstream: i915_query.c sizeof_perf_metrics()
unsafe fn sizeof_perf_metrics(perf: *mut I915PerfMetricsPrefix) -> usize {
    let idr = unsafe { metrics_idr(perf) };
    let mut count = 1usize;
    let mut id = 0;
    rcu_read_lock();
    loop {
        let entry = unsafe { idr_get_next(idr, &mut id) };
        if entry.is_null() {
            break;
        }
        count += 1;
        id = id.wrapping_add(1);
    }
    rcu_read_unlock();
    unsafe { sizeof_perf_config_list(count) }
}

#[inline]
unsafe fn query_read_flags(item: *const DrmI915QueryItem, ptr_offset: usize) -> Result<u32, i32> {
    let source = unsafe { user_address((*item).data_ptr, ptr_offset) }.cast::<u32>();
    unsafe { read_user(source) }
}

#[inline]
unsafe fn query_write_u64(item: *const DrmI915QueryItem, ptr_offset: usize, value: u64) -> bool {
    let destination = unsafe { user_address((*item).data_ptr, ptr_offset) }.cast::<u64>();
    unsafe { write_user(destination, &value) }
}

// upstream: i915_query.c query_perf_config_list()
unsafe fn query_perf_config_list(
    i915: *mut DrmI915Private,
    query_item: *const DrmI915QueryItem,
) -> i32 {
    let perf = unsafe { perf_prefix(i915) };
    if unsafe { (*perf).i915.is_null() } {
        return -ENODEV;
    }
    if unsafe { (*query_item).length } == 0 {
        return unsafe { sizeof_perf_metrics(perf) } as i32;
    }
    let flags =
        match unsafe { query_read_flags(query_item, offset_of!(DrmI915QueryPerfConfig, flags)) } {
            Ok(value) => value,
            Err(error) => return error,
        };
    if flags != 0 {
        return -EINVAL;
    }

    let mut ids = ptr::null_mut::<u64>();
    let mut capacity = 0usize;
    let mut n_configs = 1usize;
    loop {
        let wanted = n_configs;
        let new_capacity = wanted;
        if new_capacity != capacity {
            let new_ids =
                kmalloc(new_capacity.saturating_mul(size_of::<u64>()), GFP_KERNEL).cast::<u64>();
            if new_ids.is_null() {
                unsafe { kfree(ids) };
                return -ENOMEM;
            }
            if !ids.is_null() {
                unsafe {
                    ptr::copy_nonoverlapping(ids, new_ids, core::cmp::min(capacity, new_capacity));
                    kfree(ids);
                }
            }
            ids = new_ids;
            capacity = new_capacity;
        }
        let alloc = n_configs;
        n_configs = 0;
        unsafe { *ids.add(n_configs) = 1 };
        n_configs += 1;

        let idr = unsafe { metrics_idr(perf) };
        let mut id = 0;
        rcu_read_lock();
        loop {
            let entry = unsafe { idr_get_next(idr, &mut id) };
            if entry.is_null() {
                break;
            }
            if n_configs < alloc {
                unsafe { *ids.add(n_configs) = id as u64 };
            }
            n_configs += 1;
            id = id.wrapping_add(1);
        }
        rcu_read_unlock();
        if n_configs <= alloc {
            break;
        }
    }

    let total_size = unsafe { sizeof_perf_config_list(n_configs) };
    if unsafe { (*query_item).length } < total_size as i32 {
        unsafe { kfree(ids) };
        return -EINVAL;
    }
    if !unsafe {
        query_write_u64(
            query_item,
            offset_of!(DrmI915QueryPerfConfig, key),
            n_configs as u64,
        )
    } {
        unsafe { kfree(ids) };
        return -EFAULT;
    }
    let array_destination =
        unsafe { user_address((*query_item).data_ptr, size_of::<DrmI915QueryPerfConfig>()) };
    let copy_ok = unsafe {
        copy_user_bytes(
            array_destination,
            ids.cast(),
            n_configs.saturating_mul(size_of::<u64>()),
        )
    };
    unsafe { kfree(ids) };
    if !copy_ok {
        return -EFAULT;
    }
    total_size as i32
}

// upstream: i915_query.c query_perf_config()
unsafe extern "C" fn query_perf_config(
    i915: *mut DrmI915Private,
    query_item: *mut DrmI915QueryItem,
) -> i32 {
    match unsafe { (*query_item).flags } {
        DRM_I915_QUERY_PERF_CONFIG_LIST => unsafe { query_perf_config_list(i915, query_item) },
        DRM_I915_QUERY_PERF_CONFIG_DATA_FOR_UUID => unsafe {
            query_perf_config_data(i915, query_item, true)
        },
        DRM_I915_QUERY_PERF_CONFIG_DATA_FOR_ID => unsafe {
            query_perf_config_data(i915, query_item, false)
        },
        _ => -EINVAL,
    }
}

// upstream: i915_query.c query_memregion_info()
unsafe extern "C" fn query_memregion_info(
    i915: *mut DrmI915Private,
    query_item: *mut DrmI915QueryItem,
) -> i32 {
    if unsafe { (*query_item).flags } != 0 {
        return -EINVAL;
    }
    let mut total_length = size_of::<DrmI915QueryMemoryRegions>();
    for index in 0..INTEL_REGION_UNKNOWN as usize {
        let mr = unsafe { (*i915).mm.regions[index] };
        if !mr.is_null() && !unsafe { (*mr).private } {
            total_length += size_of::<DrmI915MemoryRegionInfo>();
        }
    }
    if total_length > i32::MAX as usize {
        return -EINVAL;
    }
    let mut query = DrmI915QueryMemoryRegions::default();
    let ret = unsafe {
        copy_query_item(
            (&mut query as *mut DrmI915QueryMemoryRegions).cast(),
            size_of::<DrmI915QueryMemoryRegions>(),
            total_length as u32,
            query_item,
        )
    };
    if ret != 0 {
        return ret;
    }
    if query.num_regions != 0 || query.rsvd.iter().any(|value| *value != 0) {
        return -EINVAL;
    }

    let base = unsafe { (*query_item).data_ptr };
    let mut index = 0usize;
    for region_id in 0..INTEL_REGION_UNKNOWN as usize {
        let mr = unsafe { (*i915).mm.regions[region_id] };
        if mr.is_null() || unsafe { (*mr).private } {
            continue;
        }
        let memory_type = unsafe { (*mr).r#type };
        let probed_size = unsafe { (*mr).total };
        let probed_cpu_visible_size = if memory_type == INTEL_MEMORY_LOCAL {
            let resource = unsafe { &(*mr).io };
            resource.end.wrapping_sub(resource.start).wrapping_add(1)
        } else {
            probed_size
        };
        let (unallocated_size, unallocated_cpu_visible_size) = if unsafe { perfmon_capable() } {
            let mut available = 0u64;
            let mut visible_available = 0u64;
            unsafe { intel_memory_region_avail(mr, &mut available, &mut visible_available) };
            (available, visible_available)
        } else {
            (probed_size, probed_cpu_visible_size)
        };
        let info = DrmI915MemoryRegionInfo {
            region: DrmI915GemMemoryClassInstance {
                memory_class: memory_type,
                memory_instance: unsafe { (*mr).instance },
            },
            rsvd0: 0,
            probed_size,
            unallocated_size,
            probed_cpu_visible_size,
            unallocated_cpu_visible_size,
            _rsvd1: [0; 6],
        };
        let destination = unsafe {
            user_address(
                base,
                size_of::<DrmI915QueryMemoryRegions>()
                    + index * size_of::<DrmI915MemoryRegionInfo>(),
            )
        };
        if !unsafe { write_user(destination.cast(), &info) } {
            return -EFAULT;
        }
        query.num_regions += 1;
        index += 1;
    }
    if !unsafe { write_user(user_address(base, 0).cast(), &query) } {
        return -EFAULT;
    }
    total_length as i32
}

// upstream: i915_query.c query_hwconfig_blob()
unsafe extern "C" fn query_hwconfig_blob(
    i915: *mut DrmI915Private,
    query_item: *mut DrmI915QueryItem,
) -> i32 {
    let gt = unsafe { to_gt(i915) };
    let hwconfig = unsafe { ptr::addr_of!((*gt).info.hwconfig) };
    let size = unsafe { (*hwconfig).size };
    let data = unsafe { (*hwconfig).ptr };
    if size == 0 || data.is_null() {
        return -ENODEV;
    }
    if unsafe { (*query_item).length } == 0 {
        return size as i32;
    }
    if unsafe { (*query_item).length } < size as i32 {
        return -EINVAL;
    }
    if !unsafe {
        copy_user_bytes(
            user_address(unsafe { (*query_item).data_ptr }, 0),
            data.cast_const(),
            size as usize,
        )
    } {
        return -EFAULT;
    }
    size as i32
}

// upstream: i915_query.c query_guc_submission_version()
unsafe extern "C" fn query_guc_submission_version(
    i915: *mut DrmI915Private,
    query_item: *mut DrmI915QueryItem,
) -> i32 {
    let gt = unsafe { to_gt(i915) };
    let uc = unsafe { ptr::addr_of_mut!((*gt).uc) };
    if !unsafe { intel_uc_uses_guc_submission(uc) } {
        return -ENODEV;
    }
    let mut version = DrmI915QueryGucSubmissionVersion::default();
    let ret = unsafe {
        copy_query_item(
            (&mut version as *mut DrmI915QueryGucSubmissionVersion).cast(),
            size_of::<DrmI915QueryGucSubmissionVersion>(),
            size_of::<DrmI915QueryGucSubmissionVersion>() as u32,
            query_item,
        )
    };
    if ret != 0 {
        return ret;
    }
    if version.branch != 0 || version.major != 0 || version.minor != 0 || version.patch != 0 {
        return -EINVAL;
    }
    let guc = unsafe { ptr::addr_of!((*gt).uc.guc) };
    version.branch = 0;
    version.major = unsafe { (*guc).submission_version.major };
    version.minor = unsafe { (*guc).submission_version.minor };
    version.patch = unsafe { (*guc).submission_version.patch };
    if !unsafe { write_user(user_address((*query_item).data_ptr, 0).cast(), &version) } {
        return -EFAULT;
    }
    0
}

static I915_QUERY_FUNCS: [unsafe extern "C" fn(*mut DrmI915Private, *mut DrmI915QueryItem) -> i32;
    7] = [
    query_topology_info,
    query_engine_info,
    query_perf_config,
    query_memregion_info,
    query_hwconfig_blob,
    query_geometry_subslices,
    query_guc_submission_version,
];

// Linux `array_index_nospec()` followed by an x86 speculation barrier. The
// checked index is masked before it is used to address the function table.
#[inline]
fn array_index_nospec(index: usize, size: usize) -> usize {
    let mask = 0usize.wrapping_sub((index < size) as usize);
    let checked = core::hint::black_box(index & mask);
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("lfence", options(nostack, preserves_flags));
    }
    checked
}

// upstream: i915_query.c i915_query_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_query_ioctl(
    dev: *mut c_void,
    data: *mut c_void,
    _file: *mut c_void,
) -> c_int {
    let i915 = unsafe { to_i915(dev) };
    let args = data.cast::<DrmI915Query>();
    if unsafe { (*args).flags } != 0 {
        return -EINVAL;
    }
    let items_ptr = unsafe { (*args).items_ptr };
    let num_items = unsafe { (*args).num_items };
    for item_index in 0..num_items as usize {
        let item_address = unsafe {
            user_address(
                items_ptr,
                item_index.saturating_mul(size_of::<DrmI915QueryItem>()),
            )
        };
        let item = match unsafe { read_user(item_address.cast_const().cast::<DrmI915QueryItem>()) }
        {
            Ok(value) => value,
            Err(error) => return error,
        };
        if item.query_id == 0 {
            return -EINVAL;
        }
        // x86_64 u64 and unsigned long have the same width; this explicit
        // check retains the source overflow guard if the ABI mirror changes.
        if item.query_id - 1 > c_ulong::MAX as u64 {
            return -EINVAL;
        }
        let mut ret = -EINVAL;
        let index = (item.query_id - 1) as usize;
        if index < I915_QUERY_FUNCS.len() {
            let safe_index = array_index_nospec(index, I915_QUERY_FUNCS.len());
            ret = unsafe { I915_QUERY_FUNCS[safe_index](i915, &item as *const _ as *mut _) };
        }
        if ret != item.length {
            let length_ptr = unsafe {
                user_address(
                    items_ptr,
                    item_index
                        .saturating_mul(size_of::<DrmI915QueryItem>())
                        .saturating_add(offset_of!(DrmI915QueryItem, length)),
                )
            };
            if !unsafe { write_user(length_ptr.cast(), &ret) } {
                return -EFAULT;
            }
        }
    }
    0
}
