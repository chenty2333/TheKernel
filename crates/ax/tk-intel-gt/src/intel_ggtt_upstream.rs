// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_ggtt.c.
// The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(
    unsafe_code,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code
)]

use core::{
    ffi::{c_int, c_long, c_ulong, c_void},
    mem::size_of,
    ptr,
};

use crate::{
    guc_submission_upstream::{
        intel_guc_invalidate_tlb_guc, intel_guc_tlb_invalidation_is_available,
    },
    i915_gem_core_upstream::i915_gem_drain_freed_objects,
    i915_gem_lmem_upstream::i915_gem_object_is_lmem,
    i915_gem_object_api_upstream::{
        i915_gem_object_get, i915_gem_object_lock, i915_gem_object_put, i915_gem_object_trylock,
        i915_gem_object_unlock,
    },
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_request_types_upstream::I915Request,
    i915_request_upstream::{
        __i915_request_commit, __i915_request_create, __i915_request_queue,
        i915_request_set_error_once, i915_request_wait,
    },
    i915_vma_api_upstream::{
        __i915_vma_evict, __i915_vma_unbind, i915_vma_is_bound, i915_vma_is_pinned,
        i915_vma_wait_for_bind,
    },
    i915_vma_resource_types_upstream::I915VmaResource,
    i915_vma_types_upstream::I915Vma,
    intel_context_api_upstream::{intel_context_enter, intel_context_exit},
    intel_context_types_upstream::{IntelContext, IntelWakerefT},
    intel_context_upstream::{DrmMmNode, SgTable},
    intel_engine_cs_upstream::BCS0,
    intel_ggtt_fencing_upstream::{
        intel_ggtt_fini_fences, intel_ggtt_init_fences, intel_ggtt_restore_fences,
    },
    intel_gt_api_upstream::{gt_to_guc, intel_gt_check_and_clear_faults},
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::{
        BYT_PTE_SNOOPED_BY_CPU_CACHES, BYT_PTE_WRITEABLE, GEN6_PTE_ADDR_ENCODE, GEN6_PTE_CACHE_LLC,
        GEN6_PTE_UNCACHED, GEN6_PTE_VALID, GEN7_PTE_CACHE_L3_LLC, GEN8_PAGE_PRESENT,
        GEN12_GGTT_PTE_ADDR_MASK, GEN12_GGTT_PTE_LM, HSW_PTE_ADDR_ENCODE, HSW_PTE_UNCACHED,
        HSW_WB_ELLC_LLC_AGE0, HSW_WB_ELLC_LLC_AGE3, HSW_WB_LLC_AGE3, HSW_WT_ELLC_LLC_AGE0,
        HSW_WT_ELLC_LLC_AGE3, I915_GTT_PAGE_SIZE, I915AddressSpace, I915Ggtt, I915Ppgtt,
        I915VmPtStash, I915VmaOpsLayout, MTL_GGTT_PTE_PAT0, MTL_GGTT_PTE_PAT1, PTE_LM,
        PTE_READ_ONLY, alloc_pt_dma, dma_addr_t, free_scratch, gen6_pte_t, gen8_pte_t,
        i915_address_space_fini, i915_address_space_init, i915_ggtt_require_binder, i915_vm_put,
        i915_vm_to_ggtt, intel_vm_no_concurrent_access_wa, setup_scratch_page,
    },
    intel_ring_upstream::intel_ring_begin,
    intel_runtime_pm_upstream::{intel_runtime_pm_get_if_active, intel_runtime_pm_put_raw},
    intel_uc_fw_types_upstream::{__intel_uc_fw_status, INTEL_UC_FIRMWARE_RUNNING},
    intel_uc_types_upstream::{intel_uc_uses_guc_submission, intel_uc_wants_guc_submission},
    intel_uncore_types_upstream::{
        IntelUncore, intel_uncore_posting_read_fw, intel_uncore_read_fw, intel_uncore_read64,
        intel_uncore_write_fw,
    },
    intel_wopcm_types_upstream::intel_wopcm_guc_size,
    linux::{
        gem_memory::{IoMapping, PgProt, Resource, drm_mm_node_allocated},
        i915::{
            GRAPHICS_VER, GRAPHICS_VER_FULL, HAS_LLC, HAS_LMEM, IP_VER, IS_CHERRYVIEW, IS_GEN9_LP,
            IS_HASWELL, IS_VALLEYVIEW, to_gt,
        },
        locks::{spin_lock_irq, spin_unlock_irq},
        mutex::{mutex_destroy, mutex_init, mutex_lock, mutex_unlock},
        pm::{intel_engine_pm_get, intel_engine_pm_put, intel_gt_pm_get_if_awake, intel_gt_pm_put},
        requests::{i915_request_get, i915_request_put, intel_ring_advance},
    },
    linux_config::{
        EIO, ENODEV, ENOMEM, ENXIO, ETIME, GFP_ATOMIC, GFP_KERNEL, GFP_NOWAIT,
        I915_BO_ALLOC_PM_EARLY, MAX_SCHEDULE_TIMEOUT, PAGE_SIZE, PIN_NOEVICT,
    },
    linux_i915_private::DrmI915Private,
};

const SZ_4G: u64 = 0x1_0000_0000;
const SZ_16M: u64 = 16 * 1024 * 1024;
const SZ_4M: u64 = 4 * 1024 * 1024;
const SZ_256K: u64 = 256 * 1024;
const GUC_GGTT_TOP: u64 = crate::intel_guc_types_upstream::GUC_GGTT_TOP as u64;
const GUC_TOP_RESERVE_SIZE: u64 = SZ_4G - GUC_GGTT_TOP;
const I915_COLOR_UNEVICTABLE: u64 = u64::MAX;
const DRM_MM_INSERT_LOW: u32 = 1;
const I915_VMA_BIND_MASK: u32 = crate::i915_vma_types_upstream::I915_VMA_BIND_MASK as u32;
const I915_VMA_GLOBAL_BIND: u32 = crate::i915_vma_types_upstream::I915_VMA_GLOBAL_BIND as u32;
const I915_VMA_LOCAL_BIND: u32 = crate::i915_vma_types_upstream::I915_VMA_LOCAL_BIND as u32;
const INTEL_PPGTT_ALIASING: i32 = 1;
const INTEL_PPGTT_FULL: i32 = 2;
const I915_GEM_DOMAIN_GTT: u32 = 0x40;
const I915_CACHE_NONE: u32 = 0;
const I915_CACHE_LLC: u32 = 1;
const I915_CACHE_L3_LLC: u32 = 2;
const I915_CACHE_WT: u32 = 3;
const GEN4_GTTMMADR_BAR: u32 = 0;
const GEN4_GMADR_BAR: u32 = 2;
const SNB_GMCH_CTRL: u32 = 0x50;
const SNB_GMCH_GGMS_SHIFT: u32 = 8;
const SNB_GMCH_GGMS_MASK: u16 = 0x3;
const BDW_GMCH_GGMS_SHIFT: u32 = 6;
const BDW_GMCH_GGMS_MASK: u16 = 0x3;
const GEN11_BDSM_MASK: u64 = u64::MAX << 20;
const GFX_FLSH_CNTL_GEN6: crate::intel_workarounds_types_upstream::I915RegT = reg(0x101008);
const GFX_FLSH_CNTL_EN: u32 = 1;
const GEN6_GSMBASE: crate::intel_workarounds_types_upstream::I915RegT = reg(0x108100);
const GEN8_GTCR: crate::intel_workarounds_types_upstream::I915RegT = reg(0x4274);
const GEN8_GTCR_INVALIDATE: u32 = 1;
const GEN12_GUC_TLB_INV_CR: crate::intel_workarounds_types_upstream::I915RegT = reg(0xcee8);
const GEN12_GUC_TLB_INV_CR_INVALIDATE: u32 = 1;
const GEN6_PTE_ADDR_MASK: u32 = 0x0000_0fff;
const I915_VMA_LOCAL_BIND_BIT_MASK: u32 = 1 << 11;
const I915_VMA_GLOBAL_BIND_BIT_MASK: u32 = 1 << 10;
const IORESOURCE_MEM: c_ulong = 0x200;
const GEN8_GGTT_PTE_RW: u64 = 1 << 1;
const GEN6_PTE_CACHE_LLC_VALUE: u32 = GEN6_PTE_CACHE_LLC;
const I915_PPGTT_NONE: i32 = 0;
const MI_UPDATE_GTT: u32 = 0x23 << 23;

const fn reg(offset: u32) -> crate::intel_workarounds_types_upstream::I915RegT {
    crate::intel_workarounds_types_upstream::I915RegT { reg: offset }
}

fn resource_size(resource: &Resource) -> u64 {
    resource.end.wrapping_sub(resource.start).wrapping_add(1)
}

unsafe fn intel_ppgtt_type(i915: *mut DrmI915Private) -> i32 {
    let info = unsafe {
        (*i915)
            .info
            .cast::<crate::linux::i915::IntelDeviceInfoOverlay>()
    };
    assert!(!info.is_null());
    unsafe { (*info).runtime.ppgtt_type }
}

pub(crate) unsafe fn has_ppgtt(i915: *mut DrmI915Private) -> bool {
    unsafe { intel_ppgtt_type(i915) != I915_PPGTT_NONE }
}

unsafe fn has_full_ppgtt(i915: *mut DrmI915Private) -> bool {
    unsafe { intel_ppgtt_type(i915) >= INTEL_PPGTT_FULL }
}

unsafe fn has_lmembar_smem_stolen(i915: *mut DrmI915Private) -> bool {
    !unsafe { HAS_LMEM(i915) } && unsafe { GRAPHICS_VER_FULL(i915) >= IP_VER(12, 70) }
}

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn drmm_kzalloc(drm: *mut c_void, size: usize, flags: u32) -> *mut c_void;
    fn arch_phys_wc_add(base: u64, size: u64) -> c_int;
    fn arch_phys_wc_del(mtrr: c_int);
    fn ioremap(base: u64, size: u64) -> *mut c_void;
    fn ioremap_wc(base: u64, size: u64) -> *mut c_void;
    fn iounmap(addr: *mut c_void);
    fn pgprot_writecombine(prot: PgProt) -> PgProt;
    fn to_pci_dev(dev: *mut c_void) -> *mut c_void;
    fn pci_read_config_word(dev: *mut c_void, offset: u32, value: *mut u16) -> c_int;
    fn pci_resource_start(dev: *mut c_void, bar: u32) -> u64;
    fn pci_resource_len(dev: *mut c_void, bar: u32) -> u64;
    fn i915_pci_resource_valid(dev: *mut c_void, bar: c_int) -> bool;
    fn i915_direct_stolen_access(i915: *mut DrmI915Private) -> bool;
    fn intel_ggtt_gmch_probe(ggtt: *mut I915Ggtt) -> c_int;
    fn intel_ggtt_gmch_enable_hw(i915: *mut DrmI915Private) -> c_int;
    fn intel_vgt_balloon(ggtt: *mut I915Ggtt) -> c_int;
    fn intel_vgt_deballoon(ggtt: *mut I915Ggtt);
    fn drm_mm_reserve_node(mm: *mut c_void, node: *mut DrmMmNode) -> c_int;
    fn drm_mm_insert_node_in_range(
        mm: *mut c_void,
        node: *mut DrmMmNode,
        size: u64,
        alignment: u64,
        color: u64,
        start: u64,
        end: u64,
        flags: u32,
    ) -> c_int;
    fn drm_mm_remove_node(node: *mut DrmMmNode);
    fn i915_gem_gtt_reserve(
        vm: *mut I915AddressSpace,
        ww: *mut crate::i915_gem_ww_upstream::I915GemWwCtx,
        node: *mut DrmMmNode,
        size: u64,
        start: u64,
        color: u64,
        flags: u32,
    ) -> c_int;
    fn flush_workqueue(wq: *mut c_void);
    fn stop_machine(
        callback: unsafe extern "C" fn(*mut c_void) -> c_int,
        data: *mut c_void,
        cpus: *mut c_void,
    ) -> c_int;
    fn dma_resv_init(resv: *mut c_void);
    fn dma_resv_fini(resv: *mut c_void);
    fn wbinvd_on_all_cpus();
    static intel_graphics_stolen_res: Resource;
}

// Linux's x86_64 `io_mapping_init_wc()` and `io_mapping_fini()` are static
// inline helpers (and x86_64 does not enable CONFIG_HAVE_ATOMIC_IOMAP). Keep
// their non-atomic mapping behavior local instead of declaring nonexistent C
// linker symbols.
unsafe fn io_mapping_init_wc(iomap: *mut IoMapping, base: u64, size: c_ulong) -> bool {
    let iomem = unsafe { ioremap_wc(base, size as u64) };
    if iomem.is_null() {
        return false;
    }
    let page_kernel = PgProt {
        pgprot: ((1usize << 0)
            | (1usize << 1)
            | (1usize << 5)
            | (1usize << 6)
            | (1usize << 8)
            | (1usize << 63)) as c_ulong,
    };
    unsafe {
        (*iomap).iomem = iomem;
        (*iomap).base = base;
        (*iomap).size = size;
        (*iomap).prot = pgprot_writecombine(page_kernel);
    }
    true
}

unsafe fn io_mapping_fini(iomap: *mut IoMapping) {
    unsafe { iounmap((*iomap).iomem) };
}

unsafe fn vm_pte_encode(vm: *mut I915AddressSpace, addr: dma_addr_t, pat: u32, flags: u32) -> u64 {
    unsafe {
        (*vm)
            .pte_encode
            .expect("GGTT PTE encoder is not initialized")(addr, pat, flags)
    }
}
unsafe fn vm_pte_decode(
    vm: *mut I915AddressSpace,
    pte: u64,
    present: *mut bool,
    local: *mut bool,
) -> dma_addr_t {
    unsafe {
        (*vm)
            .pte_decode
            .expect("GGTT PTE decoder is not initialized")(pte, present, local)
    }
}
unsafe fn vm_clear_range(vm: *mut I915AddressSpace, start: u64, length: u64) {
    unsafe {
        (*vm)
            .clear_range
            .expect("GGTT clear callback is not initialized")(vm, start, length)
    }
}
unsafe fn ggtt_invalidate(ggtt: *mut I915Ggtt) {
    unsafe {
        (*ggtt)
            .invalidate
            .expect("GGTT invalidation callback is not initialized")(ggtt)
    }
}

// upstream: intel_ggtt.c i915_ggtt_color_adjust()
unsafe extern "C" fn i915_ggtt_color_adjust(
    node: *const DrmMmNode,
    color: c_ulong,
    start: *mut u64,
    end: *mut u64,
) {
    if unsafe { (*node).color != color } {
        unsafe { *start += I915_GTT_PAGE_SIZE };
    }
    let member = unsafe { core::ptr::addr_of!((*node).node_list) }
        as *const crate::intel_engine_cs_upstream::ListHead;
    let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).next)) };
    let offset = core::mem::offset_of!(DrmMmNode, node_list);
    let next_node = next.cast::<u8>().wrapping_sub(offset).cast::<DrmMmNode>();
    if unsafe { (*next_node).color != color } {
        unsafe { *end -= I915_GTT_PAGE_SIZE };
    }
}

// upstream: intel_ggtt.c ggtt_init_hw()
unsafe fn ggtt_init_hw(ggtt: *mut I915Ggtt) -> c_int {
    let vm = unsafe { core::ptr::addr_of_mut!((*ggtt).vm) };
    let i915 = unsafe { (*vm).i915 };
    unsafe { i915_address_space_init(vm, crate::intel_gtt_api_upstream::VM_CLASS_GGTT as c_int) };
    unsafe { (*vm).vm_flags |= 1 }; // is_ggtt
    if unsafe { IS_VALLEYVIEW(i915) } {
        unsafe { (*vm).vm_flags |= 1 << 2 };
    } // has_read_only
    if !unsafe { HAS_LLC(i915) || has_ppgtt(i915) } {
        unsafe {
            (*vm).mm.color_adjust = Some(i915_ggtt_color_adjust);
        }
    }
    if unsafe { (*ggtt).mappable_end != 0 } {
        if !unsafe {
            io_mapping_init_wc(
                &mut (*ggtt).iomap,
                (*ggtt).gmadr.start,
                (*ggtt).mappable_end as c_ulong,
            )
        } {
            unsafe {
                if let Some(cleanup) = (*vm).cleanup {
                    cleanup(vm);
                }
            }
            return -EIO;
        }
        unsafe {
            (*ggtt).mtrr = arch_phys_wc_add((*ggtt).gmadr.start, (*ggtt).mappable_end);
        }
    }
    unsafe {
        intel_ggtt_init_fences(ggtt);
    }
    0
}

// upstream: intel_ggtt.c i915_ggtt_init_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_init_hw(i915: *mut DrmI915Private) -> c_int {
    let gt = unsafe { to_gt(i915) };
    assert!(!gt.is_null() && !unsafe { (*gt).ggtt }.is_null());
    unsafe { ggtt_init_hw((*gt).ggtt) }
}

// upstream: intel_ggtt.c i915_ggtt_suspend_vm()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_suspend_vm(vm: *mut I915AddressSpace, evict_all: bool) {
    assert!(!vm.is_null());
    unsafe {
        drm_WARN_ON!(&(*(*vm).i915).drm, ((*vm).vm_flags & 3) == 0);
    }
    loop {
        unsafe {
            i915_gem_drain_freed_objects((*vm).i915);
        }
        unsafe {
            mutex_lock(core::ptr::addr_of_mut!((*vm).mutex));
        }
        let skip_pte_rewrite = unsafe { (*vm).vm_flags & (1 << 3) != 0 };
        unsafe {
            (*vm).vm_flags |= 1 << 3;
        }
        let mut vma: *mut I915Vma = ptr::null_mut();
        let mut vn: *mut I915Vma = ptr::null_mut();
        let mut retry = false;
        list_for_each_entry_safe!(
            vma,
            vn,
            core::ptr::addr_of_mut!((*vm).bound_list),
            vm_link,
            {
                let obj = unsafe { (*vma).obj };
                assert!(
                    unsafe { drm_mm_node_allocated(&(*vma).node) },
                    "GEM_BUG_ON: bound VMA without allocated node"
                );
                if unsafe {
                    i915_vma_is_pinned(vma) || !i915_vma_is_bound(vma, I915_VMA_GLOBAL_BIND)
                } {
                    continue;
                }
                if !unsafe { i915_gem_object_trylock(obj, ptr::null_mut()) } {
                    unsafe {
                        i915_gem_object_get(obj);
                        mutex_unlock(core::ptr::addr_of_mut!((*vm).mutex));
                        i915_gem_object_lock(obj, ptr::null_mut());
                        WARN_ON!(__i915_vma_unbind(vma) != 0);
                        i915_gem_object_unlock(obj);
                        i915_gem_object_put(obj);
                        if skip_pte_rewrite {
                            (*vm).vm_flags |= 1 << 3;
                        } else {
                            (*vm).vm_flags &= !(1 << 3);
                        }
                    }
                    retry = true;
                    break;
                }
                if evict_all || !unsafe { i915_vma_is_bound(vma, I915_VMA_GLOBAL_BIND) } {
                    unsafe {
                        i915_vma_wait_for_bind(vma);
                        __i915_vma_evict(vma, false);
                        drm_mm_remove_node(&mut (*vma).node);
                    }
                }
                unsafe {
                    i915_gem_object_unlock(obj);
                }
            }
        );
        if retry {
            continue;
        }
        unsafe {
            vm_clear_range(vm, 0, (*vm).total);
            if skip_pte_rewrite {
                (*vm).vm_flags |= 1 << 3;
            } else {
                (*vm).vm_flags &= !(1 << 3);
            }
            mutex_unlock(core::ptr::addr_of_mut!((*vm).mutex));
            drm_WARN_ON!(
                &(*(*vm).i915).drm,
                evict_all && !crate::linux_list::list_empty(&(*vm).bound_list)
            );
        }
        break;
    }
}

// upstream: intel_ggtt.c i915_ggtt_suspend()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_suspend(ggtt: *mut I915Ggtt) {
    unsafe {
        i915_ggtt_suspend_vm(core::ptr::addr_of_mut!((*ggtt).vm), false);
        ggtt_invalidate(ggtt);
    }
    let mut gt: *mut IntelGt = ptr::null_mut();
    list_for_each_entry!(gt, core::ptr::addr_of_mut!((*ggtt).gt_list), ggtt_link, {
        unsafe {
            intel_gt_check_and_clear_faults(gt);
        }
    });
}

// upstream: intel_ggtt.c gen6_ggtt_invalidate()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen6_ggtt_invalidate(ggtt: *mut I915Ggtt) {
    let uncore = unsafe { (*(*ggtt).vm.gt).uncore };
    unsafe {
        spin_lock_irq(&mut (*uncore).lock);
        intel_uncore_write_fw(uncore, GFX_FLSH_CNTL_GEN6, GFX_FLSH_CNTL_EN);
        intel_uncore_read_fw(uncore, GFX_FLSH_CNTL_GEN6);
        spin_unlock_irq(&mut (*uncore).lock);
    }
}

// upstream: intel_ggtt.c needs_wc_ggtt_mapping()
unsafe fn needs_wc_ggtt_mapping(i915: *mut DrmI915Private) -> bool {
    !unsafe { IS_GEN9_LP(i915) } && unsafe { GRAPHICS_VER(i915) } < 11
}

// upstream: intel_ggtt.c gen8_ggtt_invalidate()
unsafe extern "C" fn gen8_ggtt_invalidate(ggtt: *mut I915Ggtt) {
    let uncore = unsafe { (*(*ggtt).vm.gt).uncore };
    if unsafe { needs_wc_ggtt_mapping((*ggtt).vm.i915) } {
        unsafe {
            intel_uncore_write_fw(uncore, GFX_FLSH_CNTL_GEN6, GFX_FLSH_CNTL_EN);
        }
    }
}

// upstream: intel_ggtt.c guc_ggtt_ct_invalidate()
unsafe fn guc_ggtt_ct_invalidate(gt: *mut IntelGt) {
    let uncore = unsafe { (*gt).uncore };
    let rpm = unsafe { (*uncore).rpm };
    let wakeref = unsafe { intel_runtime_pm_get_if_active(rpm) };
    if !wakeref.is_null() {
        let guc = unsafe { gt_to_guc(gt) };
        unsafe {
            intel_guc_invalidate_tlb_guc(&mut *guc);
            intel_runtime_pm_put_raw(rpm, wakeref);
        }
    }
}

// upstream: intel_ggtt.c guc_ggtt_invalidate()
unsafe extern "C" fn guc_ggtt_invalidate(ggtt: *mut I915Ggtt) {
    let i915 = unsafe { (*ggtt).vm.i915 };
    unsafe {
        gen8_ggtt_invalidate(ggtt);
    }
    let mut gt: *mut IntelGt = ptr::null_mut();
    list_for_each_entry!(gt, core::ptr::addr_of_mut!((*ggtt).gt_list), ggtt_link, {
        let guc = unsafe { gt_to_guc(gt) };
        if unsafe { intel_guc_tlb_invalidation_is_available(&*guc) } {
            unsafe {
                guc_ggtt_ct_invalidate(gt);
            }
        } else if unsafe { GRAPHICS_VER(i915) } >= 12 {
            let uncore = unsafe { (*gt).uncore };
            unsafe {
                intel_uncore_write_fw(
                    uncore,
                    GEN12_GUC_TLB_INV_CR,
                    GEN12_GUC_TLB_INV_CR_INVALIDATE,
                );
            }
        } else {
            let uncore = unsafe { (*gt).uncore };
            unsafe {
                intel_uncore_write_fw(uncore, GEN8_GTCR, GEN8_GTCR_INVALIDATE);
            }
        }
    });
}

// upstream: intel_ggtt.c mtl_ggtt_pte_encode()
unsafe extern "C" fn mtl_ggtt_pte_encode(addr: dma_addr_t, pat_index: u32, flags: u32) -> u64 {
    let mut pte = addr | GEN8_PAGE_PRESENT;
    WARN_ON_ONCE!(addr & !GEN12_GGTT_PTE_ADDR_MASK != 0);
    if flags & PTE_LM != 0 {
        pte |= GEN12_GGTT_PTE_LM;
    }
    if pat_index & 1 != 0 {
        pte |= MTL_GGTT_PTE_PAT0;
    }
    if pat_index & 2 != 0 {
        pte |= MTL_GGTT_PTE_PAT1;
    }
    pte
}

// upstream: intel_ggtt.c gen8_ggtt_pte_encode()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_ggtt_pte_encode(
    addr: dma_addr_t,
    _pat_index: u32,
    flags: u32,
) -> u64 {
    let mut pte = addr | GEN8_PAGE_PRESENT;
    if flags & PTE_LM != 0 {
        pte |= GEN12_GGTT_PTE_LM;
    }
    pte
}

// upstream: intel_ggtt.c gen8_ggtt_pte_decode()
unsafe extern "C" fn gen8_ggtt_pte_decode(
    pte: u64,
    is_present: *mut bool,
    is_local: *mut bool,
) -> dma_addr_t {
    unsafe {
        *is_present = pte & GEN8_PAGE_PRESENT != 0;
        *is_local = pte & GEN12_GGTT_PTE_LM != 0;
    }
    pte & GEN12_GGTT_PTE_ADDR_MASK
}

// upstream: intel_ggtt.c should_update_ggtt_with_bind()
unsafe fn should_update_ggtt_with_bind(ggtt: *mut I915Ggtt) -> bool {
    unsafe { crate::intel_gt_api_upstream::intel_gt_is_bind_context_ready((*ggtt).vm.gt) }
}

// upstream: intel_ggtt.c gen8_ggtt_bind_get_ce()
unsafe fn gen8_ggtt_bind_get_ce(
    ggtt: *mut I915Ggtt,
    wakeref: *mut IntelWakerefT,
) -> *mut IntelContext {
    let gt = unsafe { (*ggtt).vm.gt };
    if unsafe { crate::intel_gt_api_upstream::intel_gt_is_wedged(gt) } {
        return ptr::null_mut();
    }
    let engine = unsafe { (*gt).engine[BCS0 as usize] };
    assert!(
        !engine.is_null(),
        "GGTT binder BCS0 engine is not initialized"
    );
    let ce = unsafe { (*engine).bind_context };
    assert!(!ce.is_null(), "GEM_BUG_ON: bind context is missing");
    unsafe {
        *wakeref = intel_gt_pm_get_if_awake(gt);
    }
    if unsafe { (*wakeref).is_null() } {
        return ptr::null_mut();
    }
    unsafe {
        intel_engine_pm_get(engine);
    }
    ce
}

// upstream: intel_ggtt.c gen8_ggtt_bind_put_ce()
unsafe fn gen8_ggtt_bind_put_ce(ce: *mut IntelContext, wakeref: IntelWakerefT) {
    let engine = unsafe { (*ce).engine };
    unsafe {
        intel_engine_pm_put(engine);
        intel_gt_pm_put((*engine).gt, wakeref);
    }
}

// upstream: intel_ggtt.c gen8_ggtt_bind_ptes()
unsafe fn gen8_ggtt_bind_ptes(
    ggtt: *mut I915Ggtt,
    mut offset: u32,
    pages: *mut SgTable,
    mut num_entries: u32,
    pte: u64,
) -> bool {
    let mut attr: crate::i915_scheduler_types_upstream::I915SchedAttr =
        unsafe { core::mem::zeroed() };
    let gt = unsafe { (*ggtt).vm.gt };
    let scratch_pte = unsafe { (*(*ggtt).vm.scratch[0]).backing.encode };
    let mut page_iter = if pages.is_null() {
        None
    } else {
        Some(unsafe { crate::intel_gtt_api_upstream::for_each_sgt_daddr(pages) })
    };
    if num_entries == 0 {
        return true;
    }
    let mut wakeref: IntelWakerefT = ptr::null_mut();
    let ce = unsafe { gen8_ggtt_bind_get_ce(ggtt, &mut wakeref) };
    if ce.is_null() {
        return false;
    }
    while num_entries != 0 {
        let mut count = 0u32;
        let n_ptes = core::cmp::min(511, num_entries);
        let timeline = unsafe { (*ce).timeline };
        if unsafe {
            crate::intel_context_api_upstream::mutex_lock_interruptible(core::ptr::addr_of_mut!(
                (*timeline).mutex
            ))
        } != 0
        {
            unsafe {
                gen8_ggtt_bind_put_ce(ce, wakeref);
            }
            return false;
        }
        unsafe {
            intel_context_enter(ce);
        }
        let rq = unsafe { __i915_request_create(ce, GFP_NOWAIT | GFP_ATOMIC) };
        unsafe {
            intel_context_exit(ce);
        }
        if crate::linux_config::IS_ERR(rq) {
            crate::GTT_TRACE!("Failed to get bind request\n");
            unsafe {
                mutex_unlock(core::ptr::addr_of_mut!((*timeline).mutex));
                gen8_ggtt_bind_put_ce(ce, wakeref);
            }
            return false;
        }
        let mut cs = unsafe { intel_ring_begin(rq, 2 * n_ptes + 2) };
        if crate::linux_config::IS_ERR(cs) {
            crate::GTT_TRACE!("Failed to ring space for GGTT bind\n");
            unsafe {
                i915_request_set_error_once(rq, crate::linux_config::PTR_ERR(cs));
            }
        } else {
            unsafe {
                *cs = MI_UPDATE_GTT | (2 * n_ptes);
                cs = cs.add(1);
                *cs = offset << 12;
                cs = cs.add(1);
            }
            if let Some(iter) = page_iter.as_mut() {
                for _ in 0..n_ptes {
                    if let Some(addr) = iter.next() {
                        let entry = pte | addr;
                        unsafe {
                            cs.write(entry as u32);
                            cs.add(1).write((entry >> 32) as u32);
                            cs = cs.add(2);
                        }
                        count += 1;
                    } else {
                        break;
                    }
                }
                while count < n_ptes {
                    unsafe {
                        cs.cast::<u64>().write(scratch_pte);
                        cs = cs.add(2);
                    }
                    count += 1;
                }
            } else {
                for _ in 0..n_ptes {
                    unsafe {
                        cs.cast::<u64>().write(pte);
                        cs = cs.add(2);
                    }
                }
            }
            unsafe {
                intel_ring_advance(rq, cs);
            }
        }
        unsafe {
            i915_request_get(rq);
            __i915_request_commit(rq);
            __i915_request_queue(rq, &attr);
            mutex_unlock(core::ptr::addr_of_mut!((*timeline).mutex));
            i915_request_wait(rq, 0, MAX_SCHEDULE_TIMEOUT as c_long);
            if (*rq).fence.error != 0 {
                i915_request_put(rq);
                gen8_ggtt_bind_put_ce(ce, wakeref);
                return false;
            }
            i915_request_put(rq);
        }
        num_entries -= n_ptes;
        offset += n_ptes;
    }
    unsafe {
        gen8_ggtt_bind_put_ce(ce, wakeref);
    }
    let _ = gt;
    true
}

// upstream: intel_ggtt.c gen8_set_pte()
unsafe extern "C" fn gen8_set_pte(addr: *mut u64, pte: u64) {
    unsafe {
        ptr::write_volatile(addr, pte);
    }
}

// upstream: intel_ggtt.c gen8_get_pte()
unsafe extern "C" fn gen8_get_pte(addr: *const u64) -> u64 {
    unsafe { ptr::read_volatile(addr) }
}

// upstream: intel_ggtt.c gen8_ggtt_insert_page()
unsafe extern "C" fn gen8_ggtt_insert_page(
    vm: *mut I915AddressSpace,
    addr: dma_addr_t,
    offset: u64,
    pat_index: u32,
    flags: u32,
) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let pte = unsafe {
        (*ggtt)
            .gsm
            .cast::<u64>()
            .add((offset / I915_GTT_PAGE_SIZE) as usize)
    };
    unsafe {
        gen8_set_pte(pte, vm_pte_encode(vm, addr, pat_index, flags));
        ggtt_invalidate(ggtt);
    }
}

// upstream: intel_ggtt.c gen8_ggtt_read_entry()
unsafe extern "C" fn gen8_ggtt_read_entry(
    vm: *mut I915AddressSpace,
    offset: u64,
    present: *mut bool,
    local: *mut bool,
) -> dma_addr_t {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let pte = unsafe {
        (*ggtt)
            .gsm
            .cast::<u64>()
            .add((offset / I915_GTT_PAGE_SIZE) as usize)
    };
    unsafe { vm_pte_decode(vm, gen8_get_pte(pte), present, local) }
}

// upstream: intel_ggtt.c gen8_ggtt_insert_page_bind()
unsafe extern "C" fn gen8_ggtt_insert_page_bind(
    vm: *mut I915AddressSpace,
    addr: dma_addr_t,
    offset: u64,
    pat_index: u32,
    flags: u32,
) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let pte = unsafe { vm_pte_encode(vm, addr, pat_index, flags) };
    if unsafe {
        should_update_ggtt_with_bind(ggtt)
            && gen8_ggtt_bind_ptes(ggtt, offset as u32, ptr::null_mut(), 1, pte)
    } {
        unsafe {
            ggtt_invalidate(ggtt);
        }
        return;
    }
    unsafe {
        gen8_ggtt_insert_page(vm, addr, offset, pat_index, flags);
    }
}

// upstream: intel_ggtt.c gen8_ggtt_insert_entries()
unsafe extern "C" fn gen8_ggtt_insert_entries(
    vm: *mut I915AddressSpace,
    res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let pte_encode = unsafe { vm_pte_encode(vm, 0, pat_index, flags) };
    let mut gte = unsafe {
        (*ggtt)
            .gsm
            .cast::<u64>()
            .add((((*res).start - (*res).guard as u64) / I915_GTT_PAGE_SIZE) as usize)
    };
    let mut end = unsafe { gte.add(((*res).guard as u64 / I915_GTT_PAGE_SIZE) as usize) };
    while gte < end {
        unsafe {
            gen8_set_pte(gte, (*(*vm).scratch[0]).backing.encode);
            gte = gte.add(1);
        }
    }
    end = unsafe {
        end.add((((*res).node_size + (*res).guard as u64) / I915_GTT_PAGE_SIZE) as usize)
    };
    let mut iter = unsafe { crate::intel_gtt_api_upstream::for_each_sgt_daddr((*res).bi.pages) };
    for addr in iter.by_ref() {
        unsafe {
            gen8_set_pte(gte, pte_encode | addr);
            gte = gte.add(1);
        }
    }
    assert!(
        gte <= end,
        "GEM_BUG_ON: PTE iterator exceeded VMA allocation"
    );
    while gte < end {
        unsafe {
            gen8_set_pte(gte, (*(*vm).scratch[0]).backing.encode);
            gte = gte.add(1);
        }
    }
    unsafe {
        ggtt_invalidate(ggtt);
    }
}

// upstream: intel_ggtt.c __gen8_ggtt_insert_entries_bind()
unsafe extern "C" fn __gen8_ggtt_insert_entries_bind(
    vm: *mut I915AddressSpace,
    res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
) -> bool {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let scratch = unsafe { (*(*vm).scratch[0]).backing.encode };
    let pte = unsafe { vm_pte_encode(vm, 0, pat_index, flags) };
    let mut start = unsafe { ((*res).start - (*res).guard as u64) / I915_GTT_PAGE_SIZE };
    let mut end = start + unsafe { (*res).guard as u64 / I915_GTT_PAGE_SIZE };
    if !unsafe {
        gen8_ggtt_bind_ptes(
            ggtt,
            start as u32,
            ptr::null_mut(),
            (end - start) as u32,
            scratch,
        )
    } {
        return false;
    }
    start = end;
    end += unsafe { ((*res).node_size + (*res).guard as u64) / I915_GTT_PAGE_SIZE };
    if !unsafe {
        gen8_ggtt_bind_ptes(
            ggtt,
            start as u32,
            (*res).bi.pages.cast(),
            ((*res).node_size / I915_GTT_PAGE_SIZE) as u32,
            pte,
        )
    } {
        return false;
    }
    start += unsafe { (*res).node_size / I915_GTT_PAGE_SIZE };
    unsafe {
        gen8_ggtt_bind_ptes(
            ggtt,
            start as u32,
            ptr::null_mut(),
            (end - start) as u32,
            scratch,
        )
    }
}

// upstream: intel_ggtt.c gen8_ggtt_insert_entries_bind()
unsafe extern "C" fn gen8_ggtt_insert_entries_bind(
    vm: *mut I915AddressSpace,
    res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    if unsafe {
        should_update_ggtt_with_bind(ggtt)
            && __gen8_ggtt_insert_entries_bind(vm, res, pat_index, flags)
    } {
        unsafe {
            ggtt_invalidate(ggtt);
        }
    } else {
        unsafe {
            gen8_ggtt_insert_entries(vm, res, pat_index, flags);
        }
    }
}

// upstream: intel_ggtt.c gen8_ggtt_clear_range()
unsafe extern "C" fn gen8_ggtt_clear_range(vm: *mut I915AddressSpace, start: u64, length: u64) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let first = start / I915_GTT_PAGE_SIZE;
    let mut count = length / I915_GTT_PAGE_SIZE;
    let scratch = unsafe { (*(*vm).scratch[0]).backing.encode };
    let base = unsafe { (*ggtt).gsm.cast::<u64>().add(first as usize) };
    let max = unsafe { crate::intel_gtt_api_upstream::ggtt_total_entries(ggtt) - first };
    if WARN_ON!(count > max) {
        drm_warn!(
            &(*(*vm).i915).drm,
            "First entry = %d; Num entries = %d (max=%d)\n",
            first,
            count,
            max
        );
        count = max;
    }
    for i in 0..count {
        unsafe {
            gen8_set_pte(base.add(i as usize), scratch);
        }
    }
}

// upstream: intel_ggtt.c gen8_ggtt_scratch_range_bind()
unsafe extern "C" fn gen8_ggtt_scratch_range_bind(
    vm: *mut I915AddressSpace,
    start: u64,
    length: u64,
) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let first = start / I915_GTT_PAGE_SIZE;
    let mut count = length / I915_GTT_PAGE_SIZE;
    let scratch = unsafe { (*(*vm).scratch[0]).backing.encode };
    let max = unsafe { crate::intel_gtt_api_upstream::ggtt_total_entries(ggtt) - first };
    if WARN_ON!(count > max) {
        drm_warn!(
            &(*(*vm).i915).drm,
            "First entry = %d; Num entries = %d (max=%d)\n",
            first,
            count,
            max
        );
        count = max;
    }
    if unsafe {
        should_update_ggtt_with_bind(ggtt)
            && gen8_ggtt_bind_ptes(ggtt, first as u32, ptr::null_mut(), count as u32, scratch)
    } {
        unsafe {
            ggtt_invalidate(ggtt);
        }
    } else {
        unsafe {
            gen8_ggtt_clear_range(vm, start, length);
        }
    }
}

// upstream: intel_ggtt.c gen6_ggtt_insert_page()
unsafe extern "C" fn gen6_ggtt_insert_page(
    vm: *mut I915AddressSpace,
    addr: dma_addr_t,
    offset: u64,
    pat_index: u32,
    flags: u32,
) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let pte = unsafe {
        (*ggtt)
            .gsm
            .cast::<u32>()
            .add((offset / I915_GTT_PAGE_SIZE) as usize)
    };
    let encoded = unsafe { vm_pte_encode(vm, addr, pat_index, flags) } as u32;
    unsafe {
        ptr::write_volatile(pte, encoded);
        ggtt_invalidate(ggtt);
    }
}

// upstream: intel_ggtt.c gen6_ggtt_read_entry()
unsafe extern "C" fn gen6_ggtt_read_entry(
    vm: *mut I915AddressSpace,
    offset: u64,
    present: *mut bool,
    local: *mut bool,
) -> dma_addr_t {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let pte = unsafe {
        (*ggtt)
            .gsm
            .cast::<u32>()
            .add((offset / I915_GTT_PAGE_SIZE) as usize)
    };
    unsafe { vm_pte_decode(vm, ptr::read_volatile(pte) as u64, present, local) }
}

// upstream: intel_ggtt.c gen6_ggtt_insert_entries()
unsafe extern "C" fn gen6_ggtt_insert_entries(
    vm: *mut I915AddressSpace,
    res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let encode = unsafe { vm_pte_encode(vm, 0, pat_index, flags) as u32 };
    let mut gte = unsafe {
        (*ggtt).gsm.cast::<u32>().add(
            ((*res).start - (*res).guard as u64)
                .checked_div(I915_GTT_PAGE_SIZE)
                .unwrap() as usize,
        )
    };
    let mut end = unsafe { gte.add(((*res).guard as u64 / I915_GTT_PAGE_SIZE) as usize) };
    let scratch = unsafe { (*(*vm).scratch[0]).backing.encode as u32 };
    while gte < end {
        unsafe {
            ptr::write_volatile(gte, scratch);
            gte = gte.add(1);
        }
    }
    end = unsafe {
        end.add(((*res).node_size + (*res).guard as u64) as usize / I915_GTT_PAGE_SIZE as usize)
    };
    let mut iter = unsafe { crate::intel_gtt_api_upstream::for_each_sgt_daddr((*res).bi.pages) };
    for addr in iter.by_ref() {
        unsafe {
            ptr::write_volatile(gte, (encode as u64 | addr) as u32);
            gte = gte.add(1);
        }
    }
    assert!(
        gte <= end,
        "GEM_BUG_ON: PTE iterator exceeded VMA allocation"
    );
    while gte < end {
        unsafe {
            ptr::write_volatile(gte, scratch);
            gte = gte.add(1);
        }
    }
    unsafe {
        ggtt_invalidate(ggtt);
    }
}

// upstream: intel_ggtt.c nop_clear_range()
unsafe extern "C" fn nop_clear_range(_vm: *mut I915AddressSpace, _start: u64, _length: u64) {}

// upstream: intel_ggtt.c bxt_vtd_ggtt_wa()
unsafe fn bxt_vtd_ggtt_wa(vm: *mut I915AddressSpace) {
    let uncore = unsafe { (*(*vm).gt).uncore };
    unsafe {
        intel_uncore_posting_read_fw(uncore, GFX_FLSH_CNTL_GEN6);
    }
}

#[repr(C)]
struct InsertPageCbArg {
    vm: *mut I915AddressSpace,
    addr: dma_addr_t,
    offset: u64,
    pat_index: u32,
}

// upstream: intel_ggtt.c bxt_vtd_ggtt_insert_page__cb()
unsafe extern "C" fn bxt_vtd_ggtt_insert_page__cb(data: *mut c_void) -> c_int {
    let arg = unsafe { &mut *data.cast::<InsertPageCbArg>() };
    unsafe {
        gen8_ggtt_insert_page(arg.vm, arg.addr, arg.offset, arg.pat_index, 0);
        bxt_vtd_ggtt_wa(arg.vm);
    }
    0
}

// upstream: intel_ggtt.c bxt_vtd_ggtt_insert_page__BKL()
unsafe extern "C" fn bxt_vtd_ggtt_insert_page__BKL(
    vm: *mut I915AddressSpace,
    addr: dma_addr_t,
    offset: u64,
    pat_index: u32,
    _unused: u32,
) {
    let mut arg = InsertPageCbArg {
        vm,
        addr,
        offset,
        pat_index,
    };
    unsafe {
        stop_machine(
            bxt_vtd_ggtt_insert_page__cb,
            (&mut arg as *mut InsertPageCbArg).cast(),
            ptr::null_mut(),
        );
    }
}

#[repr(C)]
struct InsertEntriesCbArg {
    vm: *mut I915AddressSpace,
    res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
}

// upstream: intel_ggtt.c bxt_vtd_ggtt_insert_entries__cb()
unsafe extern "C" fn bxt_vtd_ggtt_insert_entries__cb(data: *mut c_void) -> c_int {
    let arg = unsafe { &mut *data.cast::<InsertEntriesCbArg>() };
    unsafe {
        gen8_ggtt_insert_entries(arg.vm, arg.res, arg.pat_index, arg.flags);
        bxt_vtd_ggtt_wa(arg.vm);
    }
    0
}

// upstream: intel_ggtt.c bxt_vtd_ggtt_insert_entries__BKL()
unsafe extern "C" fn bxt_vtd_ggtt_insert_entries__BKL(
    vm: *mut I915AddressSpace,
    res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
) {
    let mut arg = InsertEntriesCbArg {
        vm,
        res,
        pat_index,
        flags,
    };
    unsafe {
        stop_machine(
            bxt_vtd_ggtt_insert_entries__cb,
            (&mut arg as *mut InsertEntriesCbArg).cast(),
            ptr::null_mut(),
        );
    }
}

// upstream: intel_ggtt.c gen6_ggtt_clear_range()
unsafe extern "C" fn gen6_ggtt_clear_range(vm: *mut I915AddressSpace, start: u64, length: u64) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    let first = start / I915_GTT_PAGE_SIZE;
    let mut count = length / I915_GTT_PAGE_SIZE;
    let base = unsafe { (*ggtt).gsm.cast::<u32>().add(first as usize) };
    let max = unsafe { crate::intel_gtt_api_upstream::ggtt_total_entries(ggtt) - first };
    if WARN_ON!(count > max) {
        drm_warn!(
            &(*(*vm).i915).drm,
            "First entry = %d; Num entries = %d (max=%d)\n",
            first,
            count,
            max
        );
        count = max;
    }
    let scratch = unsafe { (*(*vm).scratch[0]).backing.encode as u32 };
    for i in 0..count {
        unsafe {
            ptr::write_volatile(base.add(i as usize), scratch);
        }
    }
}

// upstream: intel_ggtt.c intel_ggtt_bind_vma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ggtt_bind_vma(
    vm: *mut I915AddressSpace,
    stash: *mut I915VmPtStash,
    res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
) {
    if unsafe { (*res).bound_flags & (!flags & I915_VMA_BIND_MASK) != 0 } {
        return;
    }
    unsafe {
        (*res).bound_flags |= flags;
    }
    let mut pte_flags = 0;
    if unsafe { (*res).bi.flags & 1 != 0 } {
        pte_flags |= PTE_READ_ONLY;
    }
    if unsafe { (*res).bi.flags & 2 != 0 } {
        pte_flags |= PTE_LM;
    }
    let insert = unsafe {
        (*vm)
            .insert_entries
            .expect("GGTT insert_entries callback is not initialized")
    };
    unsafe {
        insert(vm, res, pat_index, pte_flags);
        (*res).page_sizes_gtt = I915_GTT_PAGE_SIZE as u32;
    }
    let _ = stash;
}

// upstream: intel_ggtt.c intel_ggtt_unbind_vma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ggtt_unbind_vma(
    vm: *mut I915AddressSpace,
    res: *mut I915VmaResource,
) {
    unsafe {
        vm_clear_range(vm, (*res).start, (*res).vma_size);
    }
}

// upstream: intel_ggtt.c intel_ggtt_read_entry()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_ggtt_read_entry(
    vm: *mut I915AddressSpace,
    offset: u64,
    present: *mut bool,
    local: *mut bool,
) -> dma_addr_t {
    let read = unsafe {
        (*vm)
            .read_entry
            .expect("GGTT read_entry callback is not initialized")
    };
    unsafe { read(vm, offset, present, local) }
}

// upstream: intel_ggtt.c ggtt_reserve_guc_top()
unsafe fn ggtt_reserve_guc_top(ggtt: *mut I915Ggtt) -> c_int {
    let vm = unsafe { core::ptr::addr_of_mut!((*ggtt).vm) };
    let gt = unsafe { (*vm).gt };
    if !unsafe { intel_uc_uses_guc_submission(core::ptr::addr_of_mut!((*gt).uc)) } {
        return 0;
    }
    assert!(
        unsafe { (*vm).total > GUC_TOP_RESERVE_SIZE },
        "GEM_BUG_ON: GGTT too small for GuC reserve"
    );
    let offset = unsafe { (*vm).total - GUC_TOP_RESERVE_SIZE };
    let ret = unsafe {
        i915_gem_gtt_reserve(
            vm,
            ptr::null_mut(),
            core::ptr::addr_of_mut!((*ggtt).uc_fw),
            GUC_TOP_RESERVE_SIZE,
            offset,
            I915_COLOR_UNEVICTABLE,
            PIN_NOEVICT as u32,
        )
    };
    if ret != 0 {
        drm_dbg!(
            &(*(*vm).i915).drm,
            "Failed to reserve top of GGTT for GuC\n"
        );
    }
    ret
}

// upstream: intel_ggtt.c ggtt_release_guc_top()
unsafe fn ggtt_release_guc_top(ggtt: *mut I915Ggtt) {
    if unsafe { drm_mm_node_allocated(&(*ggtt).uc_fw) } {
        unsafe {
            drm_mm_remove_node(core::ptr::addr_of_mut!((*ggtt).uc_fw));
        }
    }
}

// upstream: intel_ggtt.c cleanup_init_ggtt()
unsafe fn cleanup_init_ggtt(ggtt: *mut I915Ggtt) {
    unsafe {
        ggtt_release_guc_top(ggtt);
        if drm_mm_node_allocated(&(*ggtt).error_capture) {
            drm_mm_remove_node(core::ptr::addr_of_mut!((*ggtt).error_capture));
        }
        mutex_destroy(&mut (*ggtt).error_mutex);
    }
}

// upstream: intel_ggtt.c init_ggtt()
unsafe fn init_ggtt(ggtt: *mut I915Ggtt) -> c_int {
    let vm = unsafe { core::ptr::addr_of_mut!((*ggtt).vm) };
    unsafe {
        (*ggtt).pin_bias = core::cmp::max(
            I915_GTT_PAGE_SIZE as u32,
            intel_wopcm_guc_size(&(*(*vm).gt).wopcm),
        );
    }
    let ret = unsafe { intel_vgt_balloon(ggtt) };
    if ret != 0 {
        return ret;
    }
    unsafe {
        mutex_init(core::ptr::addr_of_mut!((*ggtt).error_mutex));
    }
    if unsafe { (*ggtt).mappable_end != 0 } {
        unsafe {
            (*ggtt).error_capture.size = 2 * I915_GTT_PAGE_SIZE;
            (*ggtt).error_capture.color = I915_COLOR_UNEVICTABLE as c_ulong;
        }
        let mm = unsafe { core::ptr::addr_of_mut!((*vm).mm).cast::<c_void>() };
        let node = unsafe { core::ptr::addr_of_mut!((*ggtt).error_capture) };
        if unsafe { drm_mm_reserve_node(mm, node) } != 0 {
            unsafe {
                drm_mm_insert_node_in_range(
                    mm,
                    node,
                    2 * I915_GTT_PAGE_SIZE,
                    0,
                    I915_COLOR_UNEVICTABLE,
                    0,
                    (*ggtt).mappable_end,
                    DRM_MM_INSERT_LOW,
                );
            }
        }
    }
    if unsafe { drm_mm_node_allocated(&(*ggtt).error_capture) } {
        let start = unsafe { (*ggtt).error_capture.start };
        let size = unsafe { (*ggtt).error_capture.size };
        let scratch = unsafe {
            (*vm)
                .scratch_range
                .expect("GGTT scratch_range callback missing")
        };
        unsafe {
            scratch(vm, start, size);
            drm_dbg!(
                &(*(*vm).i915).drm,
                "Reserved GGTT:[%llx, %llx] for use by error capture\n",
                start,
                start + size
            );
        }
    }
    let ret = unsafe { ggtt_reserve_guc_top(ggtt) };
    if ret != 0 {
        unsafe {
            cleanup_init_ggtt(ggtt);
        }
        return ret;
    }
    let mm = unsafe { core::ptr::addr_of_mut!((*vm).mm) };
    let mut entry: *mut DrmMmNode = ptr::null_mut();
    let mut hole_start = 0u64;
    let mut hole_end = 0u64;
    list_for_each_entry!(
        entry,
        core::ptr::addr_of_mut!((*mm).hole_stack),
        hole_stack,
        {
            if unsafe { (*entry).hole_size != 0 } {
                hole_start = unsafe { (*entry).start + (*entry).size };
                hole_end = hole_start + unsafe { (*entry).hole_size };
                drm_dbg!(
                    &(*(*vm).i915).drm,
                    "clearing unused GTT space: [%lx, %lx]\n",
                    hole_start,
                    hole_end
                );
                unsafe {
                    vm_clear_range(vm, hole_start, hole_end - hole_start);
                }
            }
        }
    );
    unsafe {
        vm_clear_range(vm, (*vm).total - PAGE_SIZE as u64, PAGE_SIZE as u64);
    }
    0
}

// upstream: intel_ggtt.c aliasing_gtt_bind_vma()
unsafe extern "C" fn aliasing_gtt_bind_vma(
    vm: *mut I915AddressSpace,
    stash: *mut I915VmPtStash,
    res: *mut I915VmaResource,
    pat_index: u32,
    flags: u32,
) {
    let mut pte_flags = 0;
    if unsafe { (*res).bi.flags & 1 != 0 } {
        pte_flags |= PTE_READ_ONLY;
    }
    if flags & I915_VMA_LOCAL_BIND != 0 {
        unsafe {
            crate::intel_gtt_api_upstream::ppgtt_bind_vma(
                &mut (*i915_vm_to_ggtt(vm)).alias.as_mut().unwrap().vm,
                stash,
                res,
                pat_index,
                flags,
            );
        }
    }
    if flags & I915_VMA_GLOBAL_BIND != 0 {
        let insert = unsafe {
            (*vm)
                .insert_entries
                .expect("GGTT insert_entries callback is not initialized")
        };
        unsafe {
            insert(vm, res, pat_index, pte_flags);
        }
    }
    unsafe {
        (*res).bound_flags |= flags;
    }
}

// upstream: intel_ggtt.c aliasing_gtt_unbind_vma()
unsafe extern "C" fn aliasing_gtt_unbind_vma(vm: *mut I915AddressSpace, res: *mut I915VmaResource) {
    let flags = unsafe { (*res).bound_flags };
    if flags & I915_VMA_GLOBAL_BIND != 0 {
        unsafe {
            vm_clear_range(vm, (*res).start, (*res).vma_size);
        }
    }
    if flags & I915_VMA_LOCAL_BIND != 0 {
        let ggtt = unsafe { i915_vm_to_ggtt(vm) };
        let alias = unsafe { (*ggtt).alias };
        assert!(!alias.is_null(), "aliasing GGTT has no PPGTT");
        unsafe {
            crate::intel_gtt_api_upstream::ppgtt_unbind_vma(
                core::ptr::addr_of_mut!((*alias).vm),
                res,
            );
        }
    }
}

// upstream: intel_ggtt.c init_aliasing_ppgtt()
unsafe fn init_aliasing_ppgtt(ggtt: *mut I915Ggtt) -> c_int {
    let mut stash: I915VmPtStash = unsafe { core::mem::zeroed() };
    let vm = unsafe { core::ptr::addr_of_mut!((*ggtt).vm) };
    let gt = unsafe { (*vm).gt };
    let ppgtt = unsafe { crate::intel_gtt_api_upstream::i915_ppgtt_create(gt, 0) };
    if crate::linux_config::IS_ERR(ppgtt) {
        return crate::linux_config::PTR_ERR(ppgtt);
    }
    let mut err = 0;
    if WARN_ON!(unsafe { (*ppgtt).vm.total < (*vm).total }) {
        err = -ENODEV;
    }
    if err == 0 {
        err = unsafe {
            crate::intel_gtt_api_upstream::i915_vm_alloc_pt_stash(
                core::ptr::addr_of_mut!((*ppgtt).vm),
                &mut stash,
                (*vm).total,
            )
        };
    }
    if err != 0 {
        unsafe {
            i915_vm_put(core::ptr::addr_of_mut!((*ppgtt).vm));
        }
        return err;
    }
    let scratch = unsafe { (*ppgtt).vm.scratch[0] };
    unsafe {
        i915_gem_object_lock(scratch, ptr::null_mut());
    }
    err = unsafe {
        crate::intel_gtt_api_upstream::i915_vm_map_pt_stash(
            core::ptr::addr_of_mut!((*ppgtt).vm),
            &mut stash,
        )
    };
    unsafe {
        i915_gem_object_unlock(scratch);
    }
    if err != 0 {
        unsafe {
            crate::intel_gtt_api_upstream::i915_vm_free_pt_stash(
                core::ptr::addr_of_mut!((*ppgtt).vm),
                &mut stash,
            );
            i915_vm_put(core::ptr::addr_of_mut!((*ppgtt).vm));
        }
        return err;
    }
    let allocate = unsafe {
        (*ppgtt)
            .vm
            .allocate_va_range
            .expect("PPGTT VA allocator is not initialized")
    };
    unsafe {
        allocate(
            core::ptr::addr_of_mut!((*ppgtt).vm),
            &mut stash,
            0,
            (*vm).total,
        );
    }
    unsafe {
        (*ggtt).alias = ppgtt;
        (*vm).bind_async_flags |= (*ppgtt).vm.bind_async_flags;
    }
    let expected_bind = intel_ggtt_bind_vma as usize;
    let current_bind = unsafe { (*vm).vma_ops.bind_vma.map(|f| f as usize).unwrap_or(0) };
    assert_eq!(
        current_bind, expected_bind,
        "GEM_BUG_ON: GGTT bind callback changed before aliasing mode"
    );
    unsafe {
        (*vm).vma_ops.bind_vma = Some(aliasing_gtt_bind_vma);
    }
    let expected_unbind = intel_ggtt_unbind_vma as usize;
    let current_unbind = unsafe { (*vm).vma_ops.unbind_vma.map(|f| f as usize).unwrap_or(0) };
    assert_eq!(
        current_unbind, expected_unbind,
        "GEM_BUG_ON: GGTT unbind callback changed before aliasing mode"
    );
    unsafe {
        (*vm).vma_ops.unbind_vma = Some(aliasing_gtt_unbind_vma);
    }
    unsafe {
        crate::intel_gtt_api_upstream::i915_vm_free_pt_stash(
            core::ptr::addr_of_mut!((*ppgtt).vm),
            &mut stash,
        );
    }
    0
}

// upstream: intel_ggtt.c fini_aliasing_ppgtt()
unsafe fn fini_aliasing_ppgtt(ggtt: *mut I915Ggtt) {
    let ppgtt = unsafe { core::mem::replace(&mut (*ggtt).alias, ptr::null_mut()) };
    if ppgtt.is_null() {
        return;
    }
    unsafe {
        i915_vm_put(core::ptr::addr_of_mut!((*ppgtt).vm));
        (*ggtt).vm.vma_ops.bind_vma = Some(intel_ggtt_bind_vma);
        (*ggtt).vm.vma_ops.unbind_vma = Some(intel_ggtt_unbind_vma);
    }
}

// upstream: intel_ggtt.c i915_init_ggtt()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_init_ggtt(i915: *mut DrmI915Private) -> c_int {
    let gt = unsafe { to_gt(i915) };
    let ggtt = unsafe { (*gt).ggtt };
    let mut ret = unsafe { init_ggtt(ggtt) };
    if ret != 0 {
        return ret;
    }
    let info = unsafe {
        (*i915)
            .info
            .cast::<crate::linux::i915::IntelDeviceInfoOverlay>()
    };
    if unsafe { (*info).runtime.ppgtt_type == INTEL_PPGTT_ALIASING } {
        ret = unsafe { init_aliasing_ppgtt(ggtt) };
        if ret != 0 {
            unsafe {
                cleanup_init_ggtt(ggtt);
            }
        }
    }
    0
}

// upstream: intel_ggtt.c ggtt_cleanup_hw()
unsafe fn ggtt_cleanup_hw(ggtt: *mut I915Ggtt) {
    let vm = unsafe { core::ptr::addr_of_mut!((*ggtt).vm) };
    let i915 = unsafe { (*vm).i915 };
    unsafe {
        flush_workqueue((*i915).wq);
        i915_gem_drain_freed_objects(i915);
        mutex_lock(core::ptr::addr_of_mut!((*vm).mutex));
        (*vm).vm_flags |= 1 << 3;
    }
    let mut vma: *mut I915Vma = ptr::null_mut();
    let mut next: *mut I915Vma = ptr::null_mut();
    list_for_each_entry_safe!(
        vma,
        next,
        core::ptr::addr_of_mut!((*vm).bound_list),
        vm_link,
        {
            let obj = unsafe { (*vma).obj };
            let trylock = unsafe { i915_gem_object_trylock(obj, ptr::null_mut()) };
            WARN_ON!(!trylock);
            WARN_ON!(unsafe { __i915_vma_unbind(vma) } != 0);
            if trylock {
                unsafe {
                    i915_gem_object_unlock(obj);
                }
            }
        }
    );
    unsafe {
        if drm_mm_node_allocated(&(*ggtt).error_capture) {
            drm_mm_remove_node(core::ptr::addr_of_mut!((*ggtt).error_capture));
        }
        mutex_destroy(&mut (*ggtt).error_mutex);
        ggtt_release_guc_top(ggtt);
        intel_vgt_deballoon(ggtt);
        if let Some(cleanup) = (*vm).cleanup {
            cleanup(vm);
        }
        mutex_unlock(core::ptr::addr_of_mut!((*vm).mutex));
        i915_address_space_fini(vm);
        arch_phys_wc_del((*ggtt).mtrr);
        if (*ggtt).iomap.size != 0 {
            io_mapping_fini(core::ptr::addr_of_mut!((*ggtt).iomap));
        }
    }
}

// upstream: intel_ggtt.c i915_ggtt_driver_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_driver_release(i915: *mut DrmI915Private) {
    let ggtt = unsafe { (*to_gt(i915)).ggtt };
    unsafe {
        fini_aliasing_ppgtt(ggtt);
        intel_ggtt_fini_fences(ggtt);
        ggtt_cleanup_hw(ggtt);
    }
}

// upstream: intel_ggtt.c i915_ggtt_driver_late_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_driver_late_release(i915: *mut DrmI915Private) {
    let ggtt = unsafe { (*to_gt(i915)).ggtt };
    GEM_WARN_ON!(crate::linux::memory::kref_read(unsafe { &(*ggtt).vm.resv_ref }) != 1);
    unsafe {
        dma_resv_fini(core::ptr::addr_of_mut!((*ggtt).vm._resv).cast());
    }
}

// upstream: intel_ggtt.c gen6_get_total_gtt_size()
unsafe fn gen6_get_total_gtt_size(mut gmch_ctl: u16) -> u32 {
    gmch_ctl >>= SNB_GMCH_GGMS_SHIFT;
    gmch_ctl &= SNB_GMCH_GGMS_MASK;
    (gmch_ctl as u32) << 20
}

// upstream: intel_ggtt.c gen8_get_total_gtt_size()
unsafe fn gen8_get_total_gtt_size(mut gmch_ctl: u16) -> u32 {
    gmch_ctl >>= BDW_GMCH_GGMS_SHIFT;
    gmch_ctl &= BDW_GMCH_GGMS_MASK;
    if gmch_ctl != 0 {
        gmch_ctl = 1u16 << gmch_ctl;
    }
    (gmch_ctl as u32) << 20
}

// upstream: intel_ggtt.c chv_get_total_gtt_size()
unsafe fn chv_get_total_gtt_size(mut gmch_ctrl: u16) -> u32 {
    gmch_ctrl >>= SNB_GMCH_GGMS_SHIFT;
    gmch_ctrl &= SNB_GMCH_GGMS_MASK;
    if gmch_ctrl != 0 {
        return 1u32 << (20 + gmch_ctrl as u32);
    }
    0
}

// upstream: intel_ggtt.c gen6_gttmmadr_size()
unsafe fn gen6_gttmmadr_size(i915: *mut DrmI915Private) -> u32 {
    assert!(
        unsafe { GRAPHICS_VER(i915) >= 6 },
        "GEM_BUG_ON: GTTMMADR size queried before Gen6"
    );
    if unsafe { GRAPHICS_VER(i915) < 8 } {
        SZ_4M as u32
    } else {
        SZ_16M as u32
    }
}

// upstream: intel_ggtt.c gen6_gttadr_offset()
unsafe fn gen6_gttadr_offset(i915: *mut DrmI915Private) -> u32 {
    unsafe { gen6_gttmmadr_size(i915) / 2 }
}

// upstream: intel_ggtt.c ggtt_probe_common()
unsafe fn ggtt_probe_common(ggtt: *mut I915Ggtt, size: u64) -> c_int {
    let vm = unsafe { core::ptr::addr_of_mut!((*ggtt).vm) };
    let i915 = unsafe { (*vm).i915 };
    let uncore = unsafe { (*(*vm).gt).uncore };
    let pdev = unsafe { to_pci_dev((*i915).drm.dev) };
    if GEM_WARN_ON!(unsafe {
        pci_resource_len(pdev, GEN4_GTTMMADR_BAR) != gen6_gttmmadr_size(i915) as u64
    }) {}
    let phys_addr = if unsafe { i915_direct_stolen_access(i915) } {
        drm_dbg!(&(*i915).drm, "Using direct GSM access\n");
        unsafe { intel_uncore_read64(uncore, GEN6_GSMBASE) & GEN11_BDSM_MASK }
    } else {
        unsafe { pci_resource_start(pdev, GEN4_GTTMMADR_BAR) + gen6_gttadr_offset(i915) as u64 }
    };
    unsafe {
        (*ggtt).gsm = if needs_wc_ggtt_mapping(i915) {
            ioremap_wc(phys_addr, size)
        } else {
            ioremap(phys_addr, size)
        };
    }
    if unsafe { (*ggtt).gsm.is_null() } {
        drm_err!(&(*i915).drm, "Failed to map the ggtt page table\n");
        return -ENOMEM;
    }
    unsafe {
        crate::linux_memory::kref_init(core::ptr::addr_of_mut!((*vm).resv_ref));
    }
    let ret = unsafe { setup_scratch_page(vm) };
    if ret != 0 {
        drm_err!(&(*i915).drm, "Scratch setup failed\n");
        unsafe {
            iounmap((*ggtt).gsm);
        }
        return ret;
    }
    let mut pte_flags = 0;
    if unsafe { i915_gem_object_is_lmem((*vm).scratch[0]) } {
        pte_flags |= PTE_LM;
    }
    let pat = unsafe { crate::linux::i915::i915_gem_get_pat_index(i915, I915_CACHE_NONE) };
    let encode = unsafe { (*vm).pte_encode.expect("GGTT PTE encoder not selected") };
    unsafe {
        (*(*vm).scratch[0]).backing.encode = encode(
            crate::intel_gtt_api_upstream::__px_dma((*vm).scratch[0]),
            pat,
            pte_flags,
        );
    }
    0
}

// upstream: intel_ggtt.c gen6_gmch_remove()
unsafe extern "C" fn gen6_gmch_remove(vm: *mut I915AddressSpace) {
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    unsafe {
        iounmap((*ggtt).gsm);
        free_scratch(vm);
    }
}

// upstream: intel_ggtt.c pci_resource()
unsafe fn pci_resource(pdev: *mut c_void, bar: c_int) -> Resource {
    let start = unsafe { pci_resource_start(pdev, bar as u32) };
    let len = unsafe { pci_resource_len(pdev, bar as u32) };
    Resource {
        start,
        end: start.saturating_add(len).saturating_sub(1),
        name: ptr::null(),
        flags: IORESOURCE_MEM,
        desc: 0,
        parent: ptr::null_mut(),
        sibling: ptr::null_mut(),
        child: ptr::null_mut(),
    }
}

// upstream: intel_ggtt.c gen8_gmch_probe()
unsafe fn gen8_gmch_probe(ggtt: *mut I915Ggtt) -> c_int {
    let vm = unsafe { core::ptr::addr_of_mut!((*ggtt).vm) };
    let i915 = unsafe { (*vm).i915 };
    let pdev = unsafe { to_pci_dev((*i915).drm.dev) };
    if !unsafe { HAS_LMEM(i915) || has_lmembar_smem_stolen(i915) } {
        if !unsafe { i915_pci_resource_valid(pdev, GEN4_GMADR_BAR as c_int) } {
            return -ENXIO;
        }
        unsafe {
            (*ggtt).gmadr = pci_resource(pdev, GEN4_GMADR_BAR as c_int);
            (*ggtt).mappable_end = resource_size(&(*ggtt).gmadr);
        }
    }
    let mut gmch_ctl = 0u16;
    unsafe {
        pci_read_config_word(pdev, SNB_GMCH_CTRL, &mut gmch_ctl);
    }
    let size = if unsafe { IS_CHERRYVIEW(i915) } {
        chv_get_total_gtt_size(gmch_ctl)
    } else {
        gen8_get_total_gtt_size(gmch_ctl)
    };
    unsafe {
        (*vm).alloc_pt_dma = Some(alloc_pt_dma);
        (*vm).alloc_scratch_dma = Some(alloc_pt_dma);
        (*vm).lmem_pt_obj_flags = I915_BO_ALLOC_PM_EARLY as c_ulong;
        (*vm).total = (size as u64 / size_of::<gen8_pte_t>() as u64) * I915_GTT_PAGE_SIZE;
        (*vm).cleanup = Some(gen6_gmch_remove);
        (*vm).insert_page = Some(gen8_ggtt_insert_page);
        (*vm).clear_range = Some(nop_clear_range);
        (*vm).scratch_range = Some(gen8_ggtt_clear_range);
        (*vm).insert_entries = Some(gen8_ggtt_insert_entries);
        (*vm).read_entry = Some(gen8_ggtt_read_entry);
    }
    if unsafe { intel_vm_no_concurrent_access_wa(i915) } {
        unsafe {
            (*vm).insert_entries = Some(bxt_vtd_ggtt_insert_entries__BKL);
            (*vm).insert_page = Some(bxt_vtd_ggtt_insert_page__BKL);
            (*vm).raw_insert_page = Some(gen8_ggtt_insert_page);
            (*vm).raw_insert_entries = Some(gen8_ggtt_insert_entries);
            (*vm).bind_async_flags = I915_VMA_GLOBAL_BIND | I915_VMA_LOCAL_BIND;
        }
    }
    if unsafe { i915_ggtt_require_binder(i915) } {
        unsafe {
            (*vm).scratch_range = Some(gen8_ggtt_scratch_range_bind);
            (*vm).insert_page = Some(gen8_ggtt_insert_page_bind);
            (*vm).insert_entries = Some(gen8_ggtt_insert_entries_bind);
            (*vm).raw_insert_page = Some(gen8_ggtt_insert_page);
        }
    }
    unsafe {
        (*ggtt).invalidate =
            if intel_uc_wants_guc_submission(core::ptr::addr_of_mut!((*(*vm).gt).uc)) {
                Some(guc_ggtt_invalidate)
            } else {
                Some(gen8_ggtt_invalidate)
            };
        (*vm).vma_ops.bind_vma = Some(intel_ggtt_bind_vma);
        (*vm).vma_ops.unbind_vma = Some(intel_ggtt_unbind_vma);
        (*vm).pte_encode = if GRAPHICS_VER_FULL(i915) >= IP_VER(12, 70) {
            Some(mtl_ggtt_pte_encode)
        } else {
            Some(gen8_ggtt_pte_encode)
        };
        (*vm).pte_decode = Some(gen8_ggtt_pte_decode);
    }
    unsafe { ggtt_probe_common(ggtt, size as u64) }
}

// upstream: intel_ggtt.c snb_pte_encode()
unsafe extern "C" fn snb_pte_encode(addr: dma_addr_t, pat_index: u32, _flags: u32) -> u64 {
    let mut pte = crate::intel_gtt_api_upstream::GEN6_PTE_ADDR_ENCODE(addr) as u32 | GEN6_PTE_VALID;
    match pat_index {
        I915_CACHE_L3_LLC | I915_CACHE_LLC => pte |= GEN6_PTE_CACHE_LLC,
        I915_CACHE_NONE => pte |= GEN6_PTE_UNCACHED,
        _ => MISSING_CASE!(pat_index),
    }
    pte as u64
}

// upstream: intel_ggtt.c ivb_pte_encode()
unsafe extern "C" fn ivb_pte_encode(addr: dma_addr_t, pat_index: u32, _flags: u32) -> u64 {
    let mut pte = crate::intel_gtt_api_upstream::GEN6_PTE_ADDR_ENCODE(addr) as u32 | GEN6_PTE_VALID;
    match pat_index {
        I915_CACHE_L3_LLC => pte |= GEN7_PTE_CACHE_L3_LLC,
        I915_CACHE_LLC => pte |= GEN6_PTE_CACHE_LLC,
        I915_CACHE_NONE => pte |= GEN6_PTE_UNCACHED,
        _ => MISSING_CASE!(pat_index),
    }
    pte as u64
}

// upstream: intel_ggtt.c byt_pte_encode()
unsafe extern "C" fn byt_pte_encode(addr: dma_addr_t, pat_index: u32, flags: u32) -> u64 {
    let mut pte = crate::intel_gtt_api_upstream::GEN6_PTE_ADDR_ENCODE(addr) as u32 | GEN6_PTE_VALID;
    if flags & PTE_READ_ONLY == 0 {
        pte |= BYT_PTE_WRITEABLE;
    }
    if pat_index != I915_CACHE_NONE {
        pte |= BYT_PTE_SNOOPED_BY_CPU_CACHES;
    }
    pte as u64
}

// upstream: intel_ggtt.c hsw_pte_encode()
unsafe extern "C" fn hsw_pte_encode(addr: dma_addr_t, pat_index: u32, _flags: u32) -> u64 {
    let mut pte = crate::intel_gtt_api_upstream::HSW_PTE_ADDR_ENCODE(addr) as u32 | GEN6_PTE_VALID;
    if pat_index != I915_CACHE_NONE {
        pte |= HSW_WB_LLC_AGE3;
    }
    pte as u64
}

// upstream: intel_ggtt.c iris_pte_encode()
unsafe extern "C" fn iris_pte_encode(addr: dma_addr_t, pat_index: u32, _flags: u32) -> u64 {
    let mut pte = crate::intel_gtt_api_upstream::HSW_PTE_ADDR_ENCODE(addr) as u32 | GEN6_PTE_VALID;
    pte |= match pat_index {
        I915_CACHE_NONE => 0,
        I915_CACHE_WT => HSW_WT_ELLC_LLC_AGE3,
        _ => HSW_WB_ELLC_LLC_AGE3,
    };
    pte as u64
}

// upstream: intel_ggtt.c gen6_pte_decode()
unsafe extern "C" fn gen6_pte_decode(pte: u64, present: *mut bool, local: *mut bool) -> dma_addr_t {
    let pte = pte as u32;
    unsafe {
        *present = pte & GEN6_PTE_VALID != 0;
        *local = false;
    }
    (((pte & 0xff0) as u64) << 28) | ((pte & !0xfff) as u64)
}

// upstream: intel_ggtt.c gen6_gmch_probe()
unsafe fn gen6_gmch_probe(ggtt: *mut I915Ggtt) -> c_int {
    let vm = unsafe { core::ptr::addr_of_mut!((*ggtt).vm) };
    let i915 = unsafe { (*vm).i915 };
    let pdev = unsafe { to_pci_dev((*i915).drm.dev) };
    if !unsafe { i915_pci_resource_valid(pdev, GEN4_GMADR_BAR as c_int) } {
        return -ENXIO;
    }
    unsafe {
        (*ggtt).gmadr = pci_resource(pdev, GEN4_GMADR_BAR as c_int);
        (*ggtt).mappable_end = resource_size(&(*ggtt).gmadr);
    }
    if unsafe { (*ggtt).mappable_end < (64 << 20) || (*ggtt).mappable_end > (512 << 20) } {
        drm_err!(
            &(*i915).drm,
            "Unknown GMADR size (%pa)\n",
            &(*ggtt).mappable_end
        );
        return -ENXIO;
    }
    let mut gmch_ctl = 0u16;
    unsafe {
        pci_read_config_word(pdev, SNB_GMCH_CTRL, &mut gmch_ctl);
    }
    let size = unsafe { gen6_get_total_gtt_size(gmch_ctl) };
    unsafe {
        (*vm).total = (size as u64 / size_of::<gen6_pte_t>() as u64) * I915_GTT_PAGE_SIZE;
        (*vm).alloc_pt_dma = Some(alloc_pt_dma);
        (*vm).alloc_scratch_dma = Some(alloc_pt_dma);
        (*vm).clear_range = Some(if has_full_ppgtt(i915) {
            nop_clear_range
        } else {
            gen6_ggtt_clear_range
        });
        (*vm).scratch_range = Some(gen6_ggtt_clear_range);
        (*vm).insert_page = Some(gen6_ggtt_insert_page);
        (*vm).insert_entries = Some(gen6_ggtt_insert_entries);
        (*vm).read_entry = Some(gen6_ggtt_read_entry);
        (*vm).cleanup = Some(gen6_gmch_remove);
        (*ggtt).invalidate = Some(gen6_ggtt_invalidate);
        (*vm).pte_encode = Some(if (*i915).edram_size_mb != 0 {
            iris_pte_encode
        } else if IS_HASWELL(i915) {
            hsw_pte_encode
        } else if IS_VALLEYVIEW(i915) {
            byt_pte_encode
        } else if GRAPHICS_VER(i915) >= 7 {
            ivb_pte_encode
        } else {
            snb_pte_encode
        });
        (*vm).pte_decode = Some(gen6_pte_decode);
        (*vm).vma_ops.bind_vma = Some(intel_ggtt_bind_vma);
        (*vm).vma_ops.unbind_vma = Some(intel_ggtt_unbind_vma);
    }
    unsafe { ggtt_probe_common(ggtt, size as u64) }
}

// upstream: intel_ggtt.c ggtt_probe_hw()
unsafe fn ggtt_probe_hw(ggtt: *mut I915Ggtt, gt: *mut IntelGt) -> c_int {
    let i915 = unsafe { (*gt).i915 };
    let drm = unsafe { &*core::ptr::addr_of!((*i915).drm) };
    unsafe {
        (*ggtt).vm.gt = gt;
        (*ggtt).vm.i915 = i915;
        (*ggtt).vm.dma = (*i915).drm.dev;
        dma_resv_init(core::ptr::addr_of_mut!((*ggtt).vm._resv).cast());
    }
    let ret = if unsafe { GRAPHICS_VER(i915) >= 8 } {
        unsafe { gen8_gmch_probe(ggtt) }
    } else if unsafe { GRAPHICS_VER(i915) >= 6 } {
        unsafe { gen6_gmch_probe(ggtt) }
    } else {
        unsafe { intel_ggtt_gmch_probe(ggtt) }
    };
    if ret != 0 {
        unsafe {
            dma_resv_fini(core::ptr::addr_of_mut!((*ggtt).vm._resv).cast());
        }
        return ret;
    }
    if unsafe { ((*ggtt).vm.total - 1) >> 32 != 0 } {
        drm_err!(
            drm,
            "We never expected a Global GTT with more than 32bits of address space! Found %lldM!\n",
            unsafe { (*ggtt).vm.total >> 20 }
        );
        unsafe {
            (*ggtt).vm.total = 1u64 << 32;
            (*ggtt).mappable_end = core::cmp::min((*ggtt).mappable_end, (*ggtt).vm.total);
        }
    }
    if unsafe { (*ggtt).mappable_end > (*ggtt).vm.total } {
        drm_err!(
            drm,
            "mappable aperture extends past end of GGTT, aperture=%pa, total=%llx\n",
            &unsafe { (*ggtt).mappable_end },
            unsafe { (*ggtt).vm.total }
        );
        unsafe {
            (*ggtt).mappable_end = (*ggtt).vm.total;
        }
    }
    drm_dbg!(drm, "GGTT size = %lluM\n", unsafe {
        (*ggtt).vm.total >> 20
    });
    drm_dbg!(drm, "GMADR size = %lluM\n", unsafe {
        (*ggtt).mappable_end >> 20
    });
    let stolen = unsafe { core::ptr::addr_of!(intel_graphics_stolen_res) };
    drm_dbg!(drm, "DSM size = %lluM\n", unsafe {
        resource_size(&*stolen) >> 20
    });
    0
}

// upstream: intel_ggtt.c i915_ggtt_probe_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_probe_hw(i915: *mut DrmI915Private) -> c_int {
    for gt in unsafe { (*i915).gt }
        .iter()
        .copied()
        .filter(|gt| !gt.is_null())
    {
        let ret = unsafe { crate::intel_gt_api_upstream::intel_gt_assign_ggtt(gt) };
        if ret != 0 {
            return ret;
        }
    }
    let gt = unsafe { to_gt(i915) };
    let ret = unsafe { ggtt_probe_hw((*gt).ggtt, gt) };
    if ret != 0 {
        return ret;
    }
    if unsafe { crate::i915_utils_upstream::i915_vtd_active(i915) } {
        let drm = unsafe { &*core::ptr::addr_of!((*i915).drm) };
        drm_notice!(drm, "VT-d active for gfx access\n");
    }
    0
}

// upstream: intel_ggtt.c i915_ggtt_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_create(i915: *mut DrmI915Private) -> *mut I915Ggtt {
    let drm = unsafe { core::ptr::addr_of_mut!((*i915).drm).cast::<c_void>() };
    let ggtt = unsafe { drmm_kzalloc(drm, size_of::<I915Ggtt>(), GFP_KERNEL) }.cast::<I915Ggtt>();
    if ggtt.is_null() {
        return crate::linux_config::ERR_PTR(-ENOMEM);
    }
    unsafe {
        crate::linux_list::INIT_LIST_HEAD(core::ptr::addr_of_mut!((*ggtt).gt_list));
    }
    ggtt
}

// upstream: intel_ggtt.c i915_ggtt_enable_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_enable_hw(i915: *mut DrmI915Private) -> c_int {
    if unsafe { GRAPHICS_VER(i915) < 6 } {
        unsafe { intel_ggtt_gmch_enable_hw(i915) }
    } else {
        0
    }
}

// upstream: intel_ggtt.c i915_ggtt_resume_vm()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_resume_vm(vm: *mut I915AddressSpace, all_evicted: bool) -> bool {
    unsafe {
        drm_WARN_ON!(&(*(*vm).i915).drm, (*vm).vm_flags & 3 == 0);
    }
    if all_evicted {
        unsafe {
            drm_WARN_ON!(
                &(*(*vm).i915).drm,
                !crate::linux_list::list_empty(&(*vm).bound_list)
            );
        }
        return false;
    }
    unsafe {
        vm_clear_range(vm, 0, (*vm).total);
    }
    let mut write_domain_objs = false;
    let mut vma: *mut I915Vma = ptr::null_mut();
    list_for_each_entry!(vma, core::ptr::addr_of_mut!((*vm).bound_list), vm_link, {
        let obj = unsafe { (*vma).obj };
        let was_bound =
            unsafe { crate::linux::memory::atomic_read(&(*vma).flags) as u32 & I915_VMA_BIND_MASK };
        assert!(
            was_bound != 0,
            "GEM_BUG_ON: resume encountered unbound VMA on bound list"
        );
        let res = unsafe { (*vma).resource };
        unsafe {
            (*res).bound_flags = 0;
        }
        let ops = unsafe { (*res).ops.cast::<I915VmaOpsLayout>() };
        let bind = unsafe { (*ops).bind_vma.expect("VMA bind callback missing") };
        let pat = if !obj.is_null() {
            unsafe { (*obj).cache_state_bits & 0x3f }
        } else {
            unsafe { crate::linux::i915::i915_gem_get_pat_index((*vm).i915, I915_CACHE_NONE) }
        };
        unsafe {
            bind(vm, ptr::null_mut(), res, pat, was_bound);
        }
        if !obj.is_null() {
            write_domain_objs |= unsafe { core::mem::replace(&mut (*obj).write_domain, 0) != 0 };
            unsafe {
                (*obj).read_domains |= I915_GEM_DOMAIN_GTT as u16;
            }
        }
    });
    write_domain_objs
}

// upstream: intel_ggtt.c i915_ggtt_resume()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_resume(ggtt: *mut I915Ggtt) {
    let mut gt: *mut IntelGt = ptr::null_mut();
    list_for_each_entry!(gt, core::ptr::addr_of_mut!((*ggtt).gt_list), ggtt_link, {
        unsafe {
            intel_gt_check_and_clear_faults(gt);
        }
    });
    let flush = unsafe { i915_ggtt_resume_vm(core::ptr::addr_of_mut!((*ggtt).vm), false) };
    if unsafe { drm_mm_node_allocated(&(*ggtt).error_capture) } {
        let scratch = unsafe {
            (*ggtt)
                .vm
                .scratch_range
                .expect("GGTT scratch_range callback missing")
        };
        unsafe {
            scratch(
                core::ptr::addr_of_mut!((*ggtt).vm),
                (*ggtt).error_capture.start,
                (*ggtt).error_capture.size,
            );
        }
    }
    gt = ptr::null_mut();
    list_for_each_entry!(gt, core::ptr::addr_of_mut!((*ggtt).gt_list), ggtt_link, {
        unsafe {
            crate::intel_uc_types_upstream::intel_uc_resume_mappings(core::ptr::addr_of_mut!(
                (*gt).uc
            ));
        }
    });
    unsafe {
        ggtt_invalidate(ggtt);
    }
    if flush {
        unsafe {
            wbinvd_on_all_cpus();
        }
    }
    unsafe {
        intel_ggtt_restore_fences(ggtt);
    }
}
