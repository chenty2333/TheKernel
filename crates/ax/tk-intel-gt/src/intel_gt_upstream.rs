// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_gt.c.
// The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use core::{
    ffi::{c_char, c_long, c_void},
    mem::size_of,
    ptr,
};

use crate::{
    i915_gem_lmem_upstream::i915_gem_object_is_lmem,
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_request_types_upstream::{DrmPrinter, I915Request},
    intel_context_types_upstream::IntelContext,
    intel_engine_cs_upstream::{BCS0, I915_NUM_ENGINES, Spinlock, intel_engines_init_mmio},
    intel_engine_types_upstream::{IntelEngineCs, IntelEngineMask},
    intel_gt_api_upstream::gt_is_root,
    intel_gt_buffer_pool_types_upstream::I915MapType,
    intel_gt_types_upstream::{
        GT_MEDIA, GT_PRIMARY, GT_TILE, IntelGt, IntelGtDefinition, IntelGtInfo, PhysAddrT,
    },
    intel_gtt_api_upstream::{I915AddressSpace, I915Ggtt},
    intel_runtime_pm_upstream::{
        intel_runtime_pm_get, intel_runtime_pm_get_if_in_use, intel_runtime_pm_put_raw,
    },
    intel_uncore_types_upstream::{
        FORCEWAKE_ALL, IntelUncore, intel_uncore_forcewake_get, intel_uncore_forcewake_put,
        intel_uncore_posting_read, intel_uncore_posting_read_fw, intel_uncore_read,
        intel_uncore_rmw, intel_uncore_write,
    },
    intel_workarounds_types_upstream::{I915McrRegT, I915RegT},
    intel_workarounds_upstream::{intel_engine_emit_ctx_wa, intel_engine_verify_workarounds},
    linux::i915::{GRAPHICS_VER, GRAPHICS_VER_FULL, HAS_LLC, IS_HASWELL},
    linux_i915_private::DrmI915Private,
};

#[repr(C)]
struct IntelRenderstate {
    ww: crate::i915_gem_ww_upstream::I915GemWwCtx,
    rodata: *const c_void,
    vma: *mut crate::i915_vma_types_upstream::I915Vma,
    batch_offset: u32,
    batch_size: u32,
    aux_offset: u32,
    aux_size: u32,
}

const I915_GEM_IDLE_TIMEOUT: c_long = (crate::linux_config::HZ / 5) as c_long;
const GEN2_IIR: I915RegT = reg(0x20a4);
const PGTBL_ER: I915RegT = reg(0x2024);
const EIR: I915RegT = reg(0x20b0);
const EMR: I915RegT = reg(0x20b4);
const IPEIR_I965: I915RegT = reg(0x2064);
const HSW_IDICR: I915RegT = reg(0x9008);
const HSW_MI_PREDICATE_RESULT_2: I915RegT = reg(0x2214);
const GT0_PERF_LIMIT_REASONS: I915RegT = reg(0x1381a8);
const MTL_MEDIA_PERF_LIMIT_REASONS: I915RegT = reg(0x138030);
const GEN8_RING_FAULT_REG: I915RegT = reg(0x4094);
const GEN12_RING_FAULT_REG: I915RegT = reg(0xcec4);
const XELPMP_RING_FAULT_REG: I915RegT = reg(0xcec4);
const GEN8_FAULT_TLB_DATA0: I915RegT = reg(0x4b10);
const GEN8_FAULT_TLB_DATA1: I915RegT = reg(0x4b14);
const GEN12_FAULT_TLB_DATA0: I915RegT = reg(0xceb8);
const GEN12_FAULT_TLB_DATA1: I915RegT = reg(0xcebc);
const RING_FAULT_VALID: u32 = 1;
const RING_FAULT_VADDR_MASK: u32 = 0xffff_f000;
const RING_FAULT_GTTSEL_MASK: u32 = 1 << 11;
const RING_FAULT_SRCID_MASK: u32 = 0x7f8;
const RING_FAULT_FAULT_TYPE_MASK: u32 = 0x6;
const RING_FAULT_ENGINE_ID_MASK: u32 = 0x1f000;
const FAULT_VA_HIGH_BITS: u32 = 0xf;
const FAULT_GTT_SEL: u32 = 1 << 4;
const PRB1_BASE: u32 = 0x2010;
const PRB2_BASE: u32 = 0x2020;
const SRB0_BASE: u32 = 0x20d0;
const SRB1_BASE: u32 = 0x20e0;
const SRB2_BASE: u32 = 0x20f0;
const SRB3_BASE: u32 = 0x2100;
const I915_MASTER_ERROR_INTERRUPT: u32 = 1 << 3;
const I915_MAP_WB: I915MapType = 0;
const I915_MAP_WC: I915MapType = 1;
const INTEL_PPGTT_ALIASING: u32 = 1;
const INTEL_REGION_LMEM_0: u32 = 1;
const INTEL_REGION_UNKNOWN: u32 = 7;

const fn reg(offset: u32) -> I915RegT {
    I915RegT { reg: offset }
}
const fn mcr_reg(offset: u32) -> I915McrRegT {
    I915McrRegT { reg: offset }
}
const fn ring_ctl(base: u32) -> I915RegT {
    reg(base + 0x3c)
}
const fn ring_head(base: u32) -> I915RegT {
    reg(base + 0x34)
}
const fn ring_tail(base: u32) -> I915RegT {
    reg(base + 0x30)
}
const fn ring_start(base: u32) -> I915RegT {
    reg(base + 0x38)
}
const fn ring_fault_reg(engine: *const IntelEngineCs) -> I915RegT {
    // GEN6_RING_FAULT_REG() selects the class-specific instance register.
    let base = unsafe { (*engine).class as i32 };
    reg(match base {
        0 => 0x4094,
        1 => 0x4194,
        2 => 0x4294,
        3 => 0x4394,
        _ => 0x4094,
    })
}

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn drmm_kzalloc(drm: *mut c_void, size: usize, flags: u32) -> *mut c_void;
    fn intel_gt_init_buffer_pool(gt: *mut IntelGt);
    fn intel_gt_init_reset(gt: *mut IntelGt);
    fn intel_gt_init_requests(gt: *mut IntelGt);
    fn intel_gt_init_timelines(gt: *mut IntelGt);
    fn intel_gt_init_tlb(gt: *mut IntelGt);
    fn intel_gt_pm_init_early(gt: *mut IntelGt);
    fn intel_wopcm_init_early(wopcm: *mut c_void);
    fn intel_uc_init_early(uc: *mut c_void);
    fn intel_rps_init_early(rps: *mut c_void);
    fn intel_gt_setup_lmem(gt: *mut IntelGt) -> *mut crate::linux::gem_memory::IntelMemoryRegion;
    fn i915_ggtt_create(i915: *mut DrmI915Private) -> *mut I915Ggtt;
    fn intel_gt_init_clock_frequency(gt: *mut IntelGt);
    fn intel_sseu_info_init(gt: *mut IntelGt);
    fn intel_gt_apply_workarounds(gt: *mut IntelGt);
    fn intel_gt_verify_workarounds(gt: *mut IntelGt, where_: *const c_char);
    fn intel_gt_init_swizzling(gt: *mut IntelGt);
    fn i915_ppgtt_init_hw(gt: *mut IntelGt) -> i32;
    fn intel_uc_init_hw(uc: *mut c_void) -> i32;
    fn intel_mocs_init(gt: *mut IntelGt);
    fn intel_gt_init_workarounds(gt: *mut IntelGt);
    fn intel_gt_pm_init(gt: *mut IntelGt);
    fn intel_gt_pm_fini(gt: *mut IntelGt);
    fn intel_set_mocs_index(gt: *mut IntelGt);
    fn intel_engines_init(gt: *mut IntelGt) -> i32;
    fn intel_uc_init(uc: *mut c_void) -> i32;
    fn intel_gt_resume(gt: *mut IntelGt) -> i32;
    fn intel_gt_init_hwconfig(gt: *mut IntelGt) -> i32;
    fn intel_uc_init_late(uc: *mut c_void);
    fn intel_migrate_init(migrate: *mut c_void, gt: *mut IntelGt);
    fn intel_gt_set_wedged(gt: *mut IntelGt);
    fn intel_gt_set_wedged_on_init(gt: *mut IntelGt);
    fn intel_gt_set_wedged_on_fini(gt: *mut IntelGt);
    fn intel_gt_suspend_prepare(gt: *mut IntelGt);
    fn intel_gt_suspend_late(gt: *mut IntelGt);
    fn intel_gt_retire_requests_timeout(
        gt: *mut IntelGt,
        timeout: c_long,
        remaining: *mut c_long,
    ) -> c_long;
    fn intel_uc_wait_for_idle(uc: *mut c_void, timeout: c_long) -> i32;
    fn intel_engines_release(gt: *mut IntelGt);
    fn intel_engines_free(gt: *mut IntelGt);
    fn intel_gt_flush_buffer_pool(gt: *mut IntelGt);
    fn intel_gt_fini_buffer_pool(gt: *mut IntelGt);
    fn intel_gt_fini_hwconfig(gt: *mut IntelGt);
    fn intel_migrate_fini(migrate: *mut c_void);
    fn intel_uc_driver_remove(uc: *mut c_void);
    fn intel_gt_sysfs_unregister(gt: *mut IntelGt);
    fn intel_rps_driver_unregister(rps: *mut c_void);
    fn intel_gsc_fini(gsc: *mut c_void);
    fn intel_gsc_uc_flush_work(gsc: *mut c_void);
    fn intel_gt_reset_all_engines(gt: *mut IntelGt);
    fn intel_wa_list_free(list: *mut c_void);
    fn intel_uc_driver_late_release(uc: *mut c_void);
    fn intel_gt_fini_requests(gt: *mut IntelGt);
    fn intel_gt_fini_reset(gt: *mut IntelGt);
    fn intel_gt_fini_timelines(gt: *mut IntelGt);
    fn intel_gt_fini_tlb(gt: *mut IntelGt);
    fn intel_gt_debugfs_register(gt: *mut IntelGt);
    fn intel_rps_driver_register(rps: *mut c_void);
    fn intel_gsc_init(gsc: *mut c_void, i915: *mut DrmI915Private);
    fn i915_gem_object_create_lmem(
        i915: *mut DrmI915Private,
        size: u64,
        flags: u32,
    ) -> *mut DrmI915GemObject;
    fn i915_gem_object_create_stolen(i915: *mut DrmI915Private, size: u64)
    -> *mut DrmI915GemObject;
    fn i915_gem_object_create_internal(
        i915: *mut DrmI915Private,
        size: u64,
    ) -> *mut DrmI915GemObject;
    fn i915_gem_object_put(obj: *mut DrmI915GemObject);
    fn i915_vma_instance(
        obj: *mut DrmI915GemObject,
        vm: *mut I915AddressSpace,
        ww: *mut c_void,
    ) -> *mut crate::i915_vma_types_upstream::I915Vma;
    fn i915_ggtt_pin(
        vma: *mut crate::i915_vma_types_upstream::I915Vma,
        ww: *mut c_void,
        size: u64,
        flags: u32,
    ) -> i32;
    fn i915_vma_make_unshrinkable(
        vma: *mut crate::i915_vma_types_upstream::I915Vma,
    ) -> *mut crate::i915_vma_types_upstream::I915Vma;
    fn i915_vma_unpin_and_release(
        vma: *mut *mut crate::i915_vma_types_upstream::I915Vma,
        flags: u32,
    );
    fn i915_vm_get(vm: *mut I915AddressSpace) -> *mut I915AddressSpace;
    fn i915_vm_put(vm: *mut I915AddressSpace);
    fn i915_ppgtt_create(
        gt: *mut IntelGt,
        flags: u64,
    ) -> *mut crate::intel_gtt_api_upstream::I915Ppgtt;
    fn intel_context_create(engine: *mut IntelEngineCs) -> *mut IntelContext;
    fn intel_context_put(ce: *mut IntelContext);
    fn intel_renderstate_init(state: *mut IntelRenderstate, ce: *mut IntelContext) -> i32;
    fn intel_renderstate_emit(state: *mut IntelRenderstate, rq: *mut I915Request) -> i32;
    fn intel_renderstate_fini(state: *mut IntelRenderstate, ce: *mut IntelContext);
    fn i915_request_create(ce: *mut IntelContext) -> *mut I915Request;
    fn i915_request_get(rq: *mut I915Request) -> *mut I915Request;
    fn i915_request_add(rq: *mut I915Request);
    fn i915_request_put(rq: *mut I915Request);
    fn shmem_create_from_object(obj: *mut DrmI915GemObject) -> *mut c_void;
    fn intel_uncore_init_early(uncore: *mut IntelUncore, gt: *mut IntelGt);
    fn intel_uncore_setup_mmio(uncore: *mut IntelUncore, phys: PhysAddrT) -> i32;
    fn intel_sa_mediagt_setup(gt: *mut IntelGt, phys: PhysAddrT, gsi: u32) -> i32;
    fn intel_mmio_bar(graphics_ver: u32) -> u32;
    fn pci_resource_start(dev: *mut c_void, bar: u32) -> PhysAddrT;
    fn pci_resource_len(dev: *mut c_void, bar: u32) -> u64;
    fn to_pci_dev(dev: *mut c_void) -> *mut c_void;
    fn intel_gt_sysfs_register(gt: *mut IntelGt);
    fn intel_gt_probe_error(
        i915: *mut DrmI915Private,
        fmt: *const c_char,
        name: *const c_char,
        ret: i32,
    );
    fn i915_probe_error(i915: *mut DrmI915Private, fmt: *const c_char, ...);
    fn intel_ggtt_gmch_flush();
    fn signal_pending_state(state: i32, task: *mut c_void) -> bool;
}

fn current_task_ptr() -> *mut c_void {
    axhal::percpu::current_task_ptr::<()>().cast_mut().cast()
}

fn signal_pending_current() -> bool {
    // Linux TASK_INTERRUPTIBLE: `signal_pending_state` then reports any
    // pending user signal, matching the source's `signal_pending(current)`.
    const TASK_INTERRUPTIBLE: i32 = 1;
    unsafe { signal_pending_state(TASK_INTERRUPTIBLE, current_task_ptr()) }
}

/// `intel_gt_common_init_early()` — initialize the shared per-GT software state.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_common_init_early()
pub unsafe extern "C" fn intel_gt_common_init_early(gt: *mut IntelGt) {
    assert!(!gt.is_null());
    unsafe {
        crate::linux_locks::spin_lock_init(&mut *(*gt).irq_lock);
        crate::linux_list::INIT_LIST_HEAD(core::ptr::addr_of_mut!((*gt).closed_vma));
        crate::linux_locks::spin_lock_init(&mut (*gt).closed_lock);
        crate::linux_list::init_llist_head(core::ptr::addr_of_mut!((*gt).watchdog.list));
        crate::linux_workqueue::INIT_WORK_C(
            &mut (*gt).watchdog.work,
            crate::intel_gt_api_upstream::intel_gt_watchdog_work,
        );
        intel_gt_init_buffer_pool(gt);
        intel_gt_init_reset(gt);
        intel_gt_init_requests(gt);
        intel_gt_init_timelines(gt);
        intel_gt_init_tlb(gt);
        intel_gt_pm_init_early(gt);
        intel_wopcm_init_early(core::ptr::addr_of_mut!((*gt).wopcm).cast());
        intel_uc_init_early(core::ptr::addr_of_mut!((*gt).uc).cast());
        intel_rps_init_early(core::ptr::addr_of_mut!((*gt).rps).cast());
    }
}

/// `intel_root_gt_init_early()` — allocate and attach the primary GT.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_root_gt_init_early()
pub unsafe extern "C" fn intel_root_gt_init_early(i915: *mut DrmI915Private) -> i32 {
    assert!(!i915.is_null());
    let drm = unsafe { core::ptr::addr_of_mut!((*i915).drm).cast::<c_void>() };
    let gt = unsafe { drmm_kzalloc(drm, size_of::<IntelGt>(), crate::linux_config::GFP_KERNEL) }
        .cast::<IntelGt>();
    if gt.is_null() {
        return -crate::linux_config::ENOMEM;
    }
    unsafe {
        (*i915).gt[0] = gt;
        (*gt).i915 = i915;
        (*gt).uncore = crate::intel_uncore_types_upstream::to_intel_uncore(
            core::ptr::addr_of_mut!((*i915).drm)
                .cast::<crate::intel_uncore_types_upstream::DrmDevice>(),
        );
        (*gt).irq_lock =
            drmm_kzalloc(drm, size_of::<Spinlock>(), crate::linux_config::GFP_KERNEL).cast();
        if (*gt).irq_lock.is_null() {
            return -crate::linux_config::ENOMEM;
        }
    }
    unsafe { intel_gt_common_init_early(gt) };
    0
}

/// `intel_gt_probe_lmem()` — install an available GT-local-memory region.
// upstream: intel_gt.c intel_gt_probe_lmem()
unsafe fn intel_gt_probe_lmem(gt: *mut IntelGt) -> i32 {
    let i915 = unsafe { (*gt).i915 };
    let instance = unsafe { (*gt).info.id };
    let id = INTEL_REGION_LMEM_0 + instance;
    let mem = unsafe { intel_gt_setup_lmem(gt) };
    if crate::linux_config::IS_ERR(mem) {
        let err = crate::linux_config::PTR_ERR(mem) as i32;
        if err == -crate::linux_config::ENODEV {
            return 0;
        }
        gt_err!(gt, "Failed to setup region(%d) type=%d\n", err, 1i32);
        return err;
    }
    unsafe {
        (*mem).id = id as i32;
        (*mem).instance = instance as u16;
        crate::intel_memory_region_upstream::intel_memory_region_set_name(
            mem,
            c"local%u".as_ptr(),
            &[&instance],
        );
        let info = (*i915)
            .info
            .cast::<crate::linux::i915::IntelDeviceInfoOverlay>();
        assert!(
            (*info).memory_regions & (1 << id) != 0,
            "GEM_BUG_ON: missing GT local-memory region bit"
        );
        assert!(
            (*i915).mm.regions[id as usize].is_null(),
            "GEM_BUG_ON: duplicate GT local-memory region"
        );
        (*i915).mm.regions[id as usize] = mem;
    }
    0
}

/// `intel_gt_assign_ggtt()` — share the root GGTT for a media GT or create a tile GGTT.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_assign_ggtt()
pub unsafe extern "C" fn intel_gt_assign_ggtt(gt: *mut IntelGt) -> i32 {
    assert!(!gt.is_null());
    unsafe {
        if (*gt).type_ == GT_MEDIA {
            let root = crate::linux::i915::to_gt((*gt).i915);
            assert!(
                !root.is_null(),
                "media GT cannot be initialized before root GT"
            );
            (*gt).ggtt = (*root).ggtt;
        } else {
            (*gt).ggtt = i915_ggtt_create((*gt).i915);
            if crate::linux_config::IS_ERR((*gt).ggtt) {
                return crate::linux_config::PTR_ERR((*gt).ggtt) as i32;
            }
        }
        assert!(
            !(*gt).ggtt.is_null(),
            "media GT requires an initialized root GGTT"
        );
        crate::linux_list::list_add_tail(
            core::ptr::addr_of_mut!((*gt).ggtt_link),
            core::ptr::addr_of_mut!((*(*gt).ggtt).gt_list),
        );
    }
    0
}

/// `intel_gt_init_mmio()` — initialize clocks, firmware MMIO, SSEU and MCR state.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_init_mmio()
pub unsafe extern "C" fn intel_gt_init_mmio(gt: *mut IntelGt) -> i32 {
    unsafe {
        intel_gt_init_clock_frequency(gt);
        intel_uc_init_mmio(core::ptr::addr_of_mut!((*gt).uc).cast());
        intel_sseu_info_init(gt);
        crate::intel_gt_mcr_upstream::intel_gt_mcr_init(gt);
        intel_engines_init_mmio(gt)
    }
}

unsafe extern "C" {
    fn intel_uc_init_mmio(uc: *mut c_void);
}

/// `init_unused_ring()` — clear ring state for an unused legacy ring.
// upstream: intel_gt.c init_unused_ring()
unsafe fn init_unused_ring(gt: *mut IntelGt, base: u32) {
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        intel_uncore_write(uncore, ring_ctl(base), 0);
        intel_uncore_write(uncore, ring_head(base), 0);
        intel_uncore_write(uncore, ring_tail(base), 0);
        intel_uncore_write(uncore, ring_start(base), 0);
    }
}

/// `init_unused_rings()` — idle legacy rings that are not represented by an engine.
// upstream: intel_gt.c init_unused_rings()
unsafe fn init_unused_rings(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    if unsafe { crate::linux::i915::IS_PLATFORM(i915, 1) } {
        for base in [PRB1_BASE, SRB0_BASE, SRB1_BASE, SRB2_BASE, SRB3_BASE] {
            unsafe { init_unused_ring(gt, base) };
        }
    } else {
        match unsafe { GRAPHICS_VER(i915) } {
            2 => {
                for base in [SRB0_BASE, SRB1_BASE] {
                    unsafe { init_unused_ring(gt, base) };
                }
            }
            3 => {
                for base in [PRB1_BASE, PRB2_BASE] {
                    unsafe { init_unused_ring(gt, base) };
                }
            }
            _ => {}
        }
    }
}

/// `intel_gt_init_hw()` — initialize hardware state under forcewake.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_init_hw()
pub unsafe extern "C" fn intel_gt_init_hw(gt: *mut IntelGt) -> i32 {
    let i915 = unsafe { (*gt).i915 };
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        (*gt).last_init_time = crate::linux::primitives::ktime_get();
        intel_uncore_forcewake_get(uncore, FORCEWAKE_ALL);
    }
    let mut ret = 0;
    if unsafe { (*i915).edram_size_mb != 0 && GRAPHICS_VER(i915) < 9 } {
        unsafe {
            intel_uncore_rmw(uncore, HSW_IDICR, 0, 0xf << 16);
        }
    }
    if unsafe { IS_HASWELL(i915) } {
        let value = if unsafe {
            (*i915)
                .info
                .cast::<crate::linux::i915::IntelDeviceInfoOverlay>()
                .as_ref()
                .unwrap()
                .gt
        } == 3
        {
            1
        } else {
            0
        };
        unsafe {
            intel_uncore_write(uncore, HSW_MI_PREDICATE_RESULT_2, value);
        }
    }
    unsafe {
        intel_gt_apply_workarounds(gt);
        intel_gt_verify_workarounds(gt, c"init".as_ptr());
        intel_gt_init_swizzling(gt);
        init_unused_rings(gt);
        ret = i915_ppgtt_init_hw(gt);
        if ret == 0 {
            ret = intel_uc_init_hw(core::ptr::addr_of_mut!((*gt).uc).cast());
        }
        if ret == 0 {
            intel_mocs_init(gt);
        }
        intel_uncore_forcewake_put(uncore, FORCEWAKE_ALL);
    }
    ret
}

// upstream: intel_gt.c gen6_clear_engine_error_register()
unsafe fn gen6_clear_engine_error_register(engine: *mut IntelEngineCs) {
    let uncore = unsafe { (*engine).uncore };
    let reg = unsafe { ring_fault_reg(engine) };
    let value = unsafe { intel_uncore_read(uncore, reg) } & !RING_FAULT_VALID;
    unsafe {
        intel_uncore_write(uncore, reg, value);
        intel_uncore_posting_read(uncore, reg);
    }
}

/// `intel_gt_perf_limit_reasons_reg()` — select a GT/media perf-limit MMIO register.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_perf_limit_reasons_reg()
pub unsafe extern "C" fn intel_gt_perf_limit_reasons_reg(gt: *mut IntelGt) -> I915RegT {
    if unsafe { GRAPHICS_VER((*gt).i915) } < 11 {
        return crate::linux::registers::INVALID_MMIO_REG;
    }
    if unsafe { (*gt).type_ == GT_MEDIA } {
        MTL_MEDIA_PERF_LIMIT_REASONS
    } else {
        GT0_PERF_LIMIT_REASONS
    }
}

/// `intel_gt_clear_error_registers()` — clear sticky global and engine faults.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_clear_error_registers()
pub unsafe extern "C" fn intel_gt_clear_error_registers(
    gt: *mut IntelGt,
    engine_mask: IntelEngineMask,
) {
    let i915 = unsafe { (*gt).i915 };
    let uncore = unsafe { (*gt).uncore };
    if unsafe { GRAPHICS_VER(i915) } != 2 {
        unsafe {
            intel_uncore_write(uncore, PGTBL_ER, 0);
        }
    }
    let ipeir = if unsafe { GRAPHICS_VER(i915) } < 4 {
        reg(0x2088)
    } else {
        IPEIR_I965
    };
    unsafe {
        intel_uncore_write(uncore, ipeir, 0);
        intel_uncore_write(uncore, EIR, 0);
    }
    let eir = unsafe { intel_uncore_read(uncore, EIR) };
    if eir != 0 {
        gt_dbg!(gt, "EIR stuck: 0x%08x, masking\n", eir);
        unsafe {
            intel_uncore_rmw(uncore, EMR, 0, eir);
            intel_uncore_write(uncore, GEN2_IIR, I915_MASTER_ERROR_INTERRUPT);
        }
    }
    if unsafe { crate::linux::i915::MEDIA_VER(i915) >= 13 && (*gt).type_ == GT_MEDIA } {
        unsafe {
            intel_uncore_rmw(uncore, XELPMP_RING_FAULT_REG, RING_FAULT_VALID, 0);
            intel_uncore_posting_read(uncore, XELPMP_RING_FAULT_REG);
        }
    } else if unsafe { GRAPHICS_VER_FULL(i915) >= crate::linux::i915::IP_VER(12, 55) } {
        unsafe {
            crate::intel_gt_mcr_upstream::intel_gt_mcr_multicast_rmw(
                gt,
                mcr_reg(0xcec4),
                RING_FAULT_VALID,
                0,
            );
            crate::intel_gt_mcr_upstream::intel_gt_mcr_read_any(gt, mcr_reg(0xcec4));
        }
    } else if unsafe { GRAPHICS_VER(i915) >= 12 } {
        unsafe {
            intel_uncore_rmw(uncore, GEN12_RING_FAULT_REG, RING_FAULT_VALID, 0);
            intel_uncore_posting_read(uncore, GEN12_RING_FAULT_REG);
        }
    } else if unsafe { GRAPHICS_VER(i915) >= 8 } {
        unsafe {
            intel_uncore_rmw(uncore, GEN8_RING_FAULT_REG, RING_FAULT_VALID, 0);
            intel_uncore_posting_read(uncore, GEN8_RING_FAULT_REG);
        }
    } else if unsafe { GRAPHICS_VER(i915) >= 6 } {
        for id in 0..I915_NUM_ENGINES as usize {
            let engine = unsafe { (*gt).engine[id] };
            if engine.is_null() || (engine_mask & (1u32 << id)) == 0 {
                continue;
            }
            unsafe {
                gen6_clear_engine_error_register(engine);
            }
        }
    }
}

// upstream: intel_gt.c gen6_check_faults()
unsafe fn gen6_check_faults(gt: *mut IntelGt) {
    for id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[id] };
        if engine.is_null() {
            continue;
        }
        let fault = unsafe { intel_uncore_read((*gt).uncore, ring_fault_reg(engine)) };
        if fault & RING_FAULT_VALID != 0 {
            gt_dbg!(
                gt,
                "Unexpected fault\n\tAddr: 0x%08x\n\tAddress space: %s\n\tSource ID: %d\n\tType: \
                 %d\n",
                fault & RING_FAULT_VADDR_MASK,
                if fault & RING_FAULT_GTTSEL_MASK != 0 {
                    c"GGTT".as_ptr()
                } else {
                    c"PPGTT".as_ptr()
                },
                crate::linux::registers::REG_FIELD_GET(RING_FAULT_SRCID_MASK, fault),
                crate::linux::registers::REG_FIELD_GET(RING_FAULT_FAULT_TYPE_MASK, fault)
            );
        }
    }
}

// upstream: intel_gt.c gen8_report_fault()
unsafe fn gen8_report_fault(gt: *mut IntelGt, fault: u32, data0: u32, data1: u32) {
    let address = (((data1 & FAULT_VA_HIGH_BITS) as u64) << 44) | ((data0 as u64) << 12);
    gt_dbg!(
        gt,
        "Unexpected fault\n\tAddr: 0x%08x_%08x\n\tAddress space: %s\n\tEngine ID: %d\n\tSource \
         ID: %d\n\tType: %d\n",
        (address >> 32) as u32,
        address as u32,
        if data1 & FAULT_GTT_SEL != 0 {
            c"GGTT".as_ptr()
        } else {
            c"PPGTT".as_ptr()
        },
        crate::linux::registers::REG_FIELD_GET(RING_FAULT_ENGINE_ID_MASK, fault),
        crate::linux::registers::REG_FIELD_GET(RING_FAULT_SRCID_MASK, fault),
        crate::linux::registers::REG_FIELD_GET(RING_FAULT_FAULT_TYPE_MASK, fault)
    );
}

// upstream: intel_gt.c xehp_check_faults()
unsafe fn xehp_check_faults(gt: *mut IntelGt) {
    let fault = unsafe { crate::intel_gt_mcr_upstream::intel_gt_mcr_read_any(gt, mcr_reg(0xcec4)) };
    if fault & RING_FAULT_VALID != 0 {
        let data0 =
            unsafe { crate::intel_gt_mcr_upstream::intel_gt_mcr_read_any(gt, mcr_reg(0xceb8)) };
        let data1 =
            unsafe { crate::intel_gt_mcr_upstream::intel_gt_mcr_read_any(gt, mcr_reg(0xcebc)) };
        unsafe {
            gen8_report_fault(gt, fault, data0, data1);
        }
    }
}

// upstream: intel_gt.c gen8_check_faults()
unsafe fn gen8_check_faults(gt: *mut IntelGt) {
    let (fault_reg, data0_reg, data1_reg) = if unsafe { GRAPHICS_VER((*gt).i915) } >= 12 {
        (
            GEN12_RING_FAULT_REG,
            GEN12_FAULT_TLB_DATA0,
            GEN12_FAULT_TLB_DATA1,
        )
    } else {
        (
            GEN8_RING_FAULT_REG,
            GEN8_FAULT_TLB_DATA0,
            GEN8_FAULT_TLB_DATA1,
        )
    };
    let uncore = unsafe { (*gt).uncore };
    let fault = unsafe { intel_uncore_read(uncore, fault_reg) };
    if fault & RING_FAULT_VALID != 0 {
        unsafe {
            gen8_report_fault(
                gt,
                fault,
                intel_uncore_read(uncore, data0_reg),
                intel_uncore_read(uncore, data1_reg),
            );
        }
    }
}

/// `intel_gt_check_and_clear_faults()` — report and acknowledge faults for the active generation.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_check_and_clear_faults()
pub unsafe extern "C" fn intel_gt_check_and_clear_faults(gt: *mut IntelGt) {
    let i915 = unsafe { (*gt).i915 };
    if unsafe { GRAPHICS_VER_FULL(i915) >= crate::linux::i915::IP_VER(12, 55) } {
        unsafe {
            xehp_check_faults(gt);
        }
    } else if unsafe { GRAPHICS_VER(i915) >= 8 } {
        unsafe {
            gen8_check_faults(gt);
        }
    } else if unsafe { GRAPHICS_VER(i915) >= 6 } {
        unsafe {
            gen6_check_faults(gt);
        }
    } else {
        return;
    }
    unsafe {
        intel_gt_clear_error_registers(gt, crate::intel_engine_types_upstream::ALL_ENGINES);
    }
}

/// `intel_gt_flush_ggtt_writes()` — enforce ordering between GGTT and MMIO writes.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_flush_ggtt_writes()
pub unsafe extern "C" fn intel_gt_flush_ggtt_writes(gt: *mut IntelGt) {
    let uncore = unsafe { (*gt).uncore };
    unsafe {
        crate::linux::primitives::wmb();
    }
    let info = unsafe {
        (*(*gt).i915)
            .info
            .cast::<crate::linux::i915::IntelDeviceInfoOverlay>()
    };
    if unsafe { !info.is_null() && (*info).flags[4] & 2 != 0 } {
        return;
    }
    unsafe {
        intel_gt_chipset_flush(gt);
    }
    let rpm = unsafe { (*uncore).rpm };
    let wakeref = unsafe { intel_runtime_pm_get_if_in_use(rpm) };
    if !wakeref.is_null() {
        let mut flags = 0 as core::ffi::c_ulong;
        unsafe {
            crate::linux_locks::spin_lock_irqsave_raw(
                core::ptr::addr_of_mut!((*uncore).lock),
                &mut flags,
            );
            intel_uncore_posting_read_fw(uncore, ring_tail(0x2000));
            crate::linux_locks::spin_unlock_irqrestore_raw(
                core::ptr::addr_of_mut!((*uncore).lock),
                flags,
            );
            intel_runtime_pm_put_raw(rpm, wakeref);
        }
    }
}

/// `intel_gt_chipset_flush()` — flush CPU ordering and pre-gen6 GMCH writes.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_chipset_flush()
pub unsafe extern "C" fn intel_gt_chipset_flush(gt: *mut IntelGt) {
    unsafe {
        crate::linux::primitives::wmb();
    }
    if unsafe { GRAPHICS_VER((*gt).i915) } < 6 {
        unsafe {
            intel_ggtt_gmch_flush();
        }
    }
}

/// `intel_gt_driver_register()` — register debugfs/sysfs/GSC/RPS for this GT.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_driver_register()
pub unsafe extern "C" fn intel_gt_driver_register(gt: *mut IntelGt) {
    unsafe {
        intel_gsc_init(core::ptr::addr_of_mut!((*gt).gsc).cast(), (*gt).i915);
        intel_rps_driver_register(core::ptr::addr_of_mut!((*gt).rps).cast());
        intel_gt_debugfs_register(gt);
        intel_gt_sysfs_register(gt);
    }
}

/// `intel_gt_init_scratch()` — allocate and pin the GT's zeroed scratch page.
// upstream: intel_gt.c intel_gt_init_scratch()
unsafe fn intel_gt_init_scratch(gt: *mut IntelGt, size: u64) -> i32 {
    let i915 = unsafe { (*gt).i915 };
    let mut obj = unsafe {
        i915_gem_object_create_lmem(
            i915,
            size,
            crate::linux_config::I915_BO_ALLOC_VOLATILE
                | crate::linux_config::I915_BO_ALLOC_GPU_ONLY,
        )
    };
    if crate::linux_config::IS_ERR(obj) && !unsafe { crate::linux::i915::IS_METEORLAKE(i915) } {
        obj = unsafe { i915_gem_object_create_stolen(i915, size) };
    }
    if crate::linux_config::IS_ERR(obj) {
        obj = unsafe { i915_gem_object_create_internal(i915, size) };
    }
    if crate::linux_config::IS_ERR(obj) {
        gt_err!(gt, "Failed to allocate scratch page\n");
        return crate::linux_config::PTR_ERR(obj) as i32;
    }
    let vma = unsafe {
        i915_vma_instance(
            obj,
            core::ptr::addr_of_mut!((*(*gt).ggtt).vm),
            ptr::null_mut(),
        )
    };
    if crate::linux_config::IS_ERR(vma) {
        let err = crate::linux_config::PTR_ERR(vma) as i32;
        unsafe {
            i915_gem_object_put(obj);
        }
        return err;
    }
    let ret = unsafe {
        i915_ggtt_pin(
            vma,
            ptr::null_mut(),
            0,
            crate::linux_config::PIN_HIGH as u32,
        )
    };
    if ret != 0 {
        unsafe {
            i915_gem_object_put(obj);
        }
        return ret;
    }
    unsafe {
        (*gt).scratch = i915_vma_make_unshrinkable(vma);
    }
    0
}

/// `intel_gt_fini_scratch()` — unpin and release the scratch mapping.
// upstream: intel_gt.c intel_gt_fini_scratch()
unsafe fn intel_gt_fini_scratch(gt: *mut IntelGt) {
    unsafe {
        i915_vma_unpin_and_release(core::ptr::addr_of_mut!((*gt).scratch), 0);
    }
}

/// `kernel_vm()` — choose full PPGTT or retain the GGTT for aliasing mode.
// upstream: intel_gt.c kernel_vm()
unsafe fn kernel_vm(gt: *mut IntelGt) -> *mut I915AddressSpace {
    let info = unsafe {
        (*(*gt).i915)
            .info
            .cast::<crate::linux::i915::IntelDeviceInfoOverlay>()
    };
    if unsafe { (*info).runtime.ppgtt_type as u32 > INTEL_PPGTT_ALIASING } {
        unsafe {
            i915_ppgtt_create(gt, crate::linux_config::I915_BO_ALLOC_PM_EARLY as u64)
                .cast::<I915AddressSpace>()
        }
    } else {
        unsafe { i915_vm_get(core::ptr::addr_of_mut!((*(*gt).ggtt).vm)) }
    }
}

/// `__engines_record_defaults()` — save the sanitized initial register state per engine.
// upstream: intel_gt.c __engines_record_defaults()
unsafe fn __engines_record_defaults(gt: *mut IntelGt) -> i32 {
    let mut requests: [*mut I915Request; I915_NUM_ENGINES as usize] =
        [ptr::null_mut(); I915_NUM_ENGINES as usize];
    let mut err = 0;
    for id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[id] };
        if engine.is_null() {
            continue;
        }
        assert!(
            !unsafe { (*engine).kernel_context }.is_null(),
            "GEM_BUG_ON: missing kernel context"
        );
        let ce = unsafe { intel_context_create(engine) };
        if crate::linux_config::IS_ERR(ce) {
            err = crate::linux_config::PTR_ERR(ce) as i32;
            break;
        }
        let mut state: IntelRenderstate = unsafe { core::mem::zeroed() };
        err = unsafe { intel_renderstate_init(&mut state, ce) };
        if err != 0 {
            unsafe {
                intel_context_put(ce);
            }
            break;
        }
        let rq = unsafe { i915_request_create(ce) };
        if crate::linux_config::IS_ERR(rq) {
            err = crate::linux_config::PTR_ERR(rq) as i32;
            unsafe {
                intel_renderstate_fini(&mut state, ce);
                intel_context_put(ce);
            }
            break;
        }
        err = unsafe { intel_engine_emit_ctx_wa(rq) };
        if err == 0 {
            err = unsafe { intel_renderstate_emit(&mut state, rq) };
        }
        requests[id] = unsafe { i915_request_get(rq) };
        unsafe {
            i915_request_add(rq);
            intel_renderstate_fini(&mut state, ce);
        }
        if err != 0 {
            unsafe {
                intel_context_put(ce);
            }
            break;
        }
    }
    if err == 0
        && unsafe { intel_gt_wait_for_idle(gt, I915_GEM_IDLE_TIMEOUT) }
            == -crate::linux_config::ETIME
    {
        err = -crate::linux_config::EIO;
    }
    if err == 0 {
        for rq in requests.iter().copied().filter(|rq| !rq.is_null()) {
            if unsafe { (*rq).fence.error } != 0 {
                err = -crate::linux_config::EIO;
                break;
            }
            let ce = unsafe { (*rq).context };
            assert_ne!(
                unsafe { (*ce).flags & (1 << 1) },
                0,
                "GEM_BUG_ON: context allocation bit is clear"
            );
            let vma = unsafe { (*ce).state };
            if vma.is_null() {
                continue;
            }
            let file = unsafe { shmem_create_from_object((*vma).obj) };
            if crate::linux_config::IS_ERR(file) {
                err = crate::linux_config::PTR_ERR(file) as i32;
                break;
            }
            unsafe {
                (*(*rq).engine).default_state = file.cast();
            }
        }
    }
    if err != 0 {
        unsafe {
            intel_gt_set_wedged(gt);
        }
    }
    for rq in requests.iter().copied().filter(|rq| !rq.is_null()) {
        let ce = unsafe { (*rq).context };
        unsafe {
            i915_request_put(rq);
            intel_context_put(ce);
        }
    }
    err
}

/// `__engines_verify_workarounds()` — check that context workarounds remain installed.
// upstream: intel_gt.c __engines_verify_workarounds()
unsafe fn __engines_verify_workarounds(gt: *mut IntelGt) -> i32 {
    if !crate::linux_config::CONFIG_DRM_I915_DEBUG_GEM {
        return 0;
    }
    let mut err = 0;
    for id in 0..I915_NUM_ENGINES as usize {
        let engine = unsafe { (*gt).engine[id] };
        if !engine.is_null()
            && unsafe { intel_engine_verify_workarounds(engine, c"load".as_ptr()) } != 0
        {
            err = -crate::linux_config::EIO;
        }
    }
    if unsafe { intel_gt_wait_for_idle(gt, I915_GEM_IDLE_TIMEOUT) } == -crate::linux_config::ETIME {
        err = -crate::linux_config::EIO;
    }
    err
}

/// `__intel_gt_disable()` — stop submissions and suspend the GT before teardown.
// upstream: intel_gt.c __intel_gt_disable()
unsafe fn __intel_gt_disable(gt: *mut IntelGt) {
    unsafe {
        intel_gt_set_wedged_on_fini(gt);
        intel_gt_suspend_prepare(gt);
        intel_gt_suspend_late(gt);
        assert!(
            !crate::linux_pm::intel_gt_pm_is_awake(gt),
            "GEM_BUG_ON: GT remains awake after suspend"
        );
    }
}

/// `intel_gt_wait_for_idle()` — wait for request retirement and uC quiescence.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_wait_for_idle()
pub unsafe extern "C" fn intel_gt_wait_for_idle(gt: *mut IntelGt, mut timeout: c_long) -> i32 {
    if !unsafe { crate::linux_pm::intel_gt_pm_is_awake(gt) } {
        return 0;
    }
    let mut remaining = 0;
    loop {
        timeout = unsafe { intel_gt_retire_requests_timeout(gt, timeout, &mut remaining) };
        if timeout <= 0 {
            break;
        }
        crate::linux_wait::cond_resched();
        if unsafe { signal_pending_current() } {
            return -crate::linux_config::EINTR;
        }
    }
    if timeout != 0 {
        return timeout as i32;
    }
    if remaining < 0 {
        remaining = 0;
    }
    unsafe { intel_uc_wait_for_idle(core::ptr::addr_of_mut!((*gt).uc).cast(), remaining) }
}

/// `intel_gt_init()` — allocate GT scratch/kernel VM, initialize engines and firmware.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_init()
pub unsafe extern "C" fn intel_gt_init(gt: *mut IntelGt) -> i32 {
    unsafe {
        intel_gt_init_workarounds(gt);
        intel_uncore_forcewake_get((*gt).uncore, FORCEWAKE_ALL);
    }
    let size = if unsafe { GRAPHICS_VER((*gt).i915) } == 2 {
        256 * 1024
    } else {
        4096
    };
    let mut err = unsafe { intel_gt_init_scratch(gt, size) };
    let mut cleanup_stage = 0;
    if err == 0 {
        unsafe {
            intel_gt_pm_init(gt);
        }
        cleanup_stage = 1;
        unsafe {
            (*gt).vm = kernel_vm(gt);
        }
        if unsafe { (*gt).vm.is_null() } {
            err = -crate::linux_config::ENOMEM;
        } else {
            unsafe {
                intel_set_mocs_index(gt);
            }
            err = unsafe { intel_engines_init(gt) };
        }
    }
    if err == 0 {
        err = unsafe { intel_uc_init(core::ptr::addr_of_mut!((*gt).uc).cast()) };
    }
    if err == 0 {
        cleanup_stage = 2;
        err = unsafe { intel_gt_resume(gt) };
    }
    if err == 0 {
        cleanup_stage = 3;
        let hwconfig_err = unsafe { intel_gt_init_hwconfig(gt) };
        if hwconfig_err != 0 {
            gt_err!(gt, "Failed to retrieve hwconfig table: %d\n", hwconfig_err);
        }
        err = unsafe { __engines_record_defaults(gt) };
        if err == 0 {
            err = unsafe { __engines_verify_workarounds(gt) };
        }
    }
    if err == 0 {
        unsafe {
            intel_uc_init_late(core::ptr::addr_of_mut!((*gt).uc).cast());
            intel_migrate_init(core::ptr::addr_of_mut!((*gt).migrate).cast(), gt);
        }
    } else {
        unsafe {
            if cleanup_stage >= 3 {
                __intel_gt_disable(gt);
                intel_uc_fini_hw(core::ptr::addr_of_mut!((*gt).uc).cast());
            }
            if cleanup_stage >= 2 {
                intel_uc_fini(core::ptr::addr_of_mut!((*gt).uc).cast());
            }
            if cleanup_stage >= 1 {
                intel_engines_release(gt);
                let vm = core::mem::replace(&mut (*gt).vm, ptr::null_mut());
                if !vm.is_null() {
                    i915_vm_put(vm);
                }
                intel_gt_pm_fini(gt);
                intel_gt_fini_scratch(gt);
            }
            intel_gt_set_wedged_on_init(gt);
        }
    }
    unsafe {
        intel_uncore_forcewake_put((*gt).uncore, FORCEWAKE_ALL);
    }
    err
}

unsafe extern "C" {
    fn intel_uc_fini(uc: *mut c_void);
    fn intel_uc_fini_hw(uc: *mut c_void);
}

/// `intel_gt_driver_remove()` — stop GT runtime services and release engine resources.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_driver_remove()
pub unsafe extern "C" fn intel_gt_driver_remove(gt: *mut IntelGt) {
    unsafe {
        __intel_gt_disable(gt);
        intel_migrate_fini(core::ptr::addr_of_mut!((*gt).migrate).cast());
        intel_uc_driver_remove(core::ptr::addr_of_mut!((*gt).uc).cast());
        intel_engines_release(gt);
        intel_gt_flush_buffer_pool(gt);
    }
}

/// `intel_gt_driver_unregister()` — unregister user-visible hooks and scrub hardware.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_driver_unregister()
pub unsafe extern "C" fn intel_gt_driver_unregister(gt: *mut IntelGt) {
    unsafe {
        let rpm = (*(*gt).uncore).rpm;
        let wakeref = intel_runtime_pm_get(rpm);
        intel_gt_sysfs_unregister(gt);
        intel_rps_driver_unregister(core::ptr::addr_of_mut!((*gt).rps).cast());
        intel_gsc_fini(core::ptr::addr_of_mut!((*gt).gsc).cast());
        intel_gsc_uc_flush_work(core::ptr::addr_of_mut!((*gt).uc.gsc).cast());
        intel_gt_set_wedged_on_fini(gt);
        if !wakeref.is_null() {
            intel_gt_reset_all_engines(gt);
            intel_runtime_pm_put_raw(rpm, wakeref);
        }
    }
}

/// `intel_gt_driver_release()` — release VM, workaround, PM and scratch resources.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_driver_release()
pub unsafe extern "C" fn intel_gt_driver_release(gt: *mut IntelGt) {
    let vm = unsafe { core::mem::replace(&mut (*gt).vm, ptr::null_mut()) };
    if !vm.is_null() {
        unsafe {
            i915_vm_put(vm);
        }
    }
    unsafe {
        intel_wa_list_free(core::ptr::addr_of_mut!((*gt).wa_list).cast());
        intel_gt_pm_fini(gt);
        intel_gt_fini_scratch(gt);
        intel_gt_fini_buffer_pool(gt);
        intel_gt_fini_hwconfig(gt);
    }
}

/// `intel_gt_driver_late_release_all()` — drain RCU and finalize all initialized tiles.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_driver_late_release_all()
pub unsafe extern "C" fn intel_gt_driver_late_release_all(i915: *mut DrmI915Private) {
    crate::linux::rcu::rcu_barrier();
    for id in 0..crate::intel_gt_types_upstream::I915_MAX_GT {
        let gt = unsafe { (*i915).gt[id] };
        if gt.is_null() {
            continue;
        }
        unsafe {
            intel_uc_driver_late_release(core::ptr::addr_of_mut!((*gt).uc).cast());
            intel_gt_fini_requests(gt);
            intel_gt_fini_reset(gt);
            intel_gt_fini_timelines(gt);
            intel_gt_fini_tlb(gt);
            intel_engines_free(gt);
        }
    }
}

/// `intel_gt_tile_setup()` — allocate subordinate tile state and map its MMIO aperture.
// upstream: intel_gt.c intel_gt_tile_setup()
unsafe fn intel_gt_tile_setup(gt: *mut IntelGt, phys_addr: PhysAddrT) -> i32 {
    if !unsafe { gt_is_root(gt) } {
        let drm = unsafe { core::ptr::addr_of_mut!((*(*gt).i915).drm).cast::<c_void>() };
        let uncore = unsafe {
            drmm_kzalloc(
                drm,
                size_of::<IntelUncore>(),
                crate::linux_config::GFP_KERNEL,
            )
        }
        .cast::<IntelUncore>();
        if uncore.is_null() {
            return -crate::linux_config::ENOMEM;
        }
        let irq_lock =
            unsafe { drmm_kzalloc(drm, size_of::<Spinlock>(), crate::linux_config::GFP_KERNEL) }
                .cast::<Spinlock>();
        if irq_lock.is_null() {
            return -crate::linux_config::ENOMEM;
        }
        unsafe {
            (*gt).uncore = uncore;
            (*gt).irq_lock = irq_lock;
            intel_gt_common_init_early(gt);
        }
    }
    unsafe {
        intel_uncore_init_early((*gt).uncore, gt);
    }
    let ret = unsafe { intel_uncore_setup_mmio((*gt).uncore, phys_addr) };
    if ret != 0 {
        return ret;
    }
    unsafe {
        (*gt).phys_addr = phys_addr;
    }
    0
}

/// `intel_gt_probe_all()` — initialize the root GT then any additional GT tiles.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_probe_all()
pub unsafe extern "C" fn intel_gt_probe_all(i915: *mut DrmI915Private) -> i32 {
    let pdev = unsafe { to_pci_dev((*i915).drm.dev) };
    let mut gt = unsafe { (*i915).gt[0] };
    let info = unsafe {
        (*i915)
            .info
            .cast::<crate::linux::i915::IntelDeviceInfoOverlay>()
    };
    assert!(
        !gt.is_null() && !info.is_null(),
        "primary GT / device info is not initialized"
    );
    unsafe {
        (*gt).i915 = i915;
        (*gt).name = c"Primary GT".as_ptr();
        (*gt).info.engine_mask = (*info).platform_engine_mask;
    }
    let bar = unsafe { intel_mmio_bar(GRAPHICS_VER(i915) as u32) };
    let phys = unsafe { pci_resource_start(pdev, bar) };
    gt_dbg!(gt, "Setting up %s\n", (*gt).name);
    let mut ret = unsafe { intel_gt_tile_setup(gt, phys) };
    if ret != 0 {
        return ret;
    }
    let defs = unsafe {
        (*i915)
            .info
            .cast::<u8>()
            .add(8)
            .cast::<*const IntelGtDefinition>()
            .read()
    };
    if defs.is_null() {
        return 0;
    }
    for (idx, def) in unsafe {
        core::slice::from_raw_parts(defs, crate::intel_gt_types_upstream::I915_MAX_GT - 1)
    }
    .iter()
    .enumerate()
    {
        if def.name.is_null() {
            break;
        }
        let drm = unsafe { core::ptr::addr_of_mut!((*i915).drm).cast::<c_void>() };
        gt = unsafe { drmm_kzalloc(drm, size_of::<IntelGt>(), crate::linux_config::GFP_KERNEL) }
            .cast();
        if gt.is_null() {
            ret = -crate::linux_config::ENOMEM;
            break;
        }
        unsafe {
            (*gt).i915 = i915;
            (*gt).name = def.name;
            (*gt).type_ = def.type_;
            (*gt).info.engine_mask = def.engine_mask;
            (*gt).info.id = (idx + 1) as u32;
        }
        let len = unsafe { pci_resource_len(pdev, bar) };
        if def.mapping_base as u64 + 16 * 1024 * 1024 > len {
            ret = -crate::linux_config::ENODEV;
            break;
        }
        let tile_phys = phys + def.mapping_base as u64;
        ret = match def.type_ {
            GT_TILE => unsafe { intel_gt_tile_setup(gt, tile_phys) },
            GT_MEDIA => unsafe { intel_sa_mediagt_setup(gt, tile_phys, def.gsi_offset) },
            GT_PRIMARY => -crate::linux_config::ENODEV,
            _ => -crate::linux_config::ENODEV,
        };
        if ret != 0 {
            break;
        }
        unsafe {
            (*i915).gt[idx + 1] = gt;
        }
    }
    if ret != 0 {
        unsafe {
            i915_probe_error(
                i915,
                c"Failed to initialize %s! (%d)\n".as_ptr(),
                (*gt).name,
                ret,
            );
        }
    }
    ret
}

/// `intel_gt_tiles_init()` — discover and register local-memory regions for each GT.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_tiles_init()
pub unsafe extern "C" fn intel_gt_tiles_init(i915: *mut DrmI915Private) -> i32 {
    for gt in unsafe { (*i915).gt }.iter().copied() {
        if !gt.is_null() {
            let ret = unsafe { intel_gt_probe_lmem(gt) };
            if ret != 0 {
                return ret;
            }
        }
    }
    0
}

/// `intel_gt_info_print()` — print engine availability and topology information.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_info_print()
pub unsafe extern "C" fn intel_gt_info_print(info: *const IntelGtInfo, p: *mut DrmPrinter) {
    assert!(!info.is_null());
    drm_printf!(p, "available engines: %x\n", unsafe { (*info).engine_mask });
    unsafe {
        crate::intel_sseu_types_upstream::intel_sseu_dump(core::ptr::addr_of!((*info).sseu), p);
    }
}

/// `intel_gt_coherent_map_type()` — choose WB only when LLC/coherency allows it.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_coherent_map_type()
pub unsafe extern "C" fn intel_gt_coherent_map_type(
    gt: *mut IntelGt,
    obj: *mut DrmI915GemObject,
    always_coherent: bool,
) -> I915MapType {
    if unsafe { i915_gem_object_is_lmem(obj) || intel_gt_needs_wa_22016122933(gt) } {
        return I915_MAP_WC;
    }
    if unsafe { HAS_LLC((*gt).i915) } || always_coherent {
        I915_MAP_WB
    } else {
        I915_MAP_WC
    }
}

/// `intel_gt_needs_wa_16018031267()` — Wa_16018031267 / Wa_16018063123 predicate.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_needs_wa_16018031267()
pub unsafe extern "C" fn intel_gt_needs_wa_16018031267(gt: *mut IntelGt) -> bool {
    unsafe {
        crate::linux::i915::IS_GFX_GT_IP_RANGE(
            gt,
            crate::linux::i915::IP_VER(12, 55),
            crate::linux::i915::IP_VER(12, 71),
        )
    }
}

/// `intel_gt_needs_wa_22016122933()` — Media 13.0 mapping workaround predicate.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_needs_wa_22016122933()
pub unsafe extern "C" fn intel_gt_needs_wa_22016122933(gt: *mut IntelGt) -> bool {
    unsafe {
        crate::linux::i915::MEDIA_VER_FULL((*gt).i915) == crate::linux::i915::IP_VER(13, 0)
            && (*gt).type_ == GT_MEDIA
    }
}

// upstream: intel_gt.c __intel_gt_bind_context_set_ready()
unsafe fn __intel_gt_bind_context_set_ready(gt: *mut IntelGt, ready: bool) {
    let engine = unsafe { (*gt).engine[BCS0 as usize] };
    if !engine.is_null() && !unsafe { (*engine).bind_context.is_null() } {
        unsafe {
            (*engine).bind_context_ready = ready;
        }
    }
}

/// `intel_gt_bind_context_set_ready()` — publish the bind-context readiness state.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_bind_context_set_ready()
pub unsafe extern "C" fn intel_gt_bind_context_set_ready(gt: *mut IntelGt) {
    unsafe {
        __intel_gt_bind_context_set_ready(gt, true);
    }
}

/// `intel_gt_bind_context_set_unready()` — clear bind-context readiness before teardown.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_bind_context_set_unready()
pub unsafe extern "C" fn intel_gt_bind_context_set_unready(gt: *mut IntelGt) {
    unsafe {
        __intel_gt_bind_context_set_ready(gt, false);
    }
}

/// `intel_gt_is_bind_context_ready()` — query readiness of the BCS0 bind context.
#[unsafe(no_mangle)]
// upstream: intel_gt.c intel_gt_is_bind_context_ready()
pub unsafe extern "C" fn intel_gt_is_bind_context_ready(gt: *mut IntelGt) -> bool {
    let engine = unsafe { (*gt).engine[BCS0 as usize] };
    !engine.is_null() && unsafe { (*engine).bind_context_ready }
}
