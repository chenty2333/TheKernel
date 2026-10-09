// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/i915_vma.c.
// The upstream source is MIT-licensed. Framework/MM/GTT/resource operations
// below use their owning LinuxKPI/i915 APIs; no bind, pin, display, or eviction
// path is replaced by a fallback implementation.

#![allow(unsafe_code, non_snake_case, non_camel_case_types, dead_code)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicI32, AtomicPtr, Ordering},
};

use crate::{
    i915_active_upstream::{
        i915_active_acquire, i915_active_add_request, i915_active_fence_get, i915_active_fini,
        i915_active_init, i915_active_release, i915_active_set_exclusive,
        i915_sw_fence_await_active,
    },
    i915_gem_clflush_upstream::{
        DmaFenceWork, DmaFenceWorkOps, dma_fence_is_signaled, dma_fence_wait, dma_fence_work_chain,
        dma_fence_work_commit, dma_fence_work_commit_imm, dma_fence_work_init, dma_resv_add_fence,
        dma_resv_reserve_fences,
    },
    i915_gem_core_upstream::{io_mapping_map_wc, io_mapping_unmap},
    i915_gem_lmem_upstream::{i915_gem_object_is_lmem, i915_gem_object_lmem_io_map},
    i915_gem_object_api_upstream::{
        i915_gem_object_get, i915_gem_object_pin_pages, i915_gem_object_unpin_map,
        i915_gem_object_unpin_pages,
    },
    i915_gem_object_header_upstream::{
        assert_object_held, assert_object_held_shared, i915_gem_object_get_stride,
        i915_gem_object_get_tiling, i915_gem_object_has_pinned_pages, i915_gem_object_is_readonly,
        i915_gem_object_lock, i915_gem_object_put, i915_gem_object_trylock, i915_gem_object_unlock,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, I915_BO_ALLOC_GPU_ONLY, I915Frontbuffer},
    i915_gem_object_upstream::{
        i915_gem_object_get_moving_fence, i915_gem_object_wait_moving_fence,
    },
    i915_gem_pages_upstream::{
        __i915_gem_object_get_dma_address, __i915_gem_object_get_dma_address_len,
        __i915_gem_object_release_map, Scatterlist, i915_gem_object_pin_map, i915_sg_trim,
        page_mask_bits, page_pack_bits, page_unmask_bits,
    },
    i915_gem_shrinker_upstream::{
        i915_gem_object_make_purgeable, i915_gem_object_make_shrinkable,
        i915_gem_object_make_unshrinkable,
    },
    i915_gem_ww_upstream::{i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini, i915_gem_ww_ctx_init},
    i915_request_types_upstream::I915Request,
    i915_request_upstream::i915_request_await_object,
    i915_sw_fence_upstream::{__i915_sw_fence_await_dma_fence, I915SwDmaFenceCb},
    i915_vma_api_upstream::{
        __i915_vma_offset, __i915_vma_pin, __i915_vma_unpin, i915_vma_clear_scanout, i915_vma_get,
        i915_vma_get_iomap, i915_vma_has_userfault, i915_vma_is_active, i915_vma_is_bound,
        i915_vma_is_closed, i915_vma_is_ggtt, i915_vma_is_map_and_fenceable, i915_vma_is_pinned,
        i915_vma_pin_count, i915_vma_pin_fence, i915_vma_put, i915_vma_revoke_fence,
        i915_vma_set_ggtt_write, i915_vma_sync, i915_vma_tryget, i915_vma_unpin,
        i915_vma_unpin_fence, i915_vma_unset_ggtt_write, i915_vma_unset_userfault,
    },
    i915_vma_resource_types_upstream::{
        I915PageSizes, I915RefctSgt, I915VmaBindinfo, I915VmaResource,
    },
    i915_vma_types_upstream::{
        I915_VMA_BIND_MASK, I915_VMA_CAN_FENCE_BIT, I915_VMA_ERROR, I915_VMA_ERROR_BIT,
        I915_VMA_GGTT, I915_VMA_GGTT_BIT, I915_VMA_GGTT_WRITE, I915_VMA_GGTT_WRITE_BIT,
        I915_VMA_GLOBAL_BIND, I915_VMA_LOCAL_BIND, I915_VMA_OVERFLOW, I915_VMA_PAGES_ACTIVE,
        I915_VMA_PAGES_BIAS, I915_VMA_PIN_MASK, I915_VMA_SCANOUT_BIT, I915_VMA_USERFAULT_BIT,
        I915Vma,
    },
    intel_context_types_upstream::{I915Active, I915SwFence, IntelWakerefT},
    intel_context_upstream::{
        DmaFence, DrmGemObjectBaseLayout, DrmMmNode, I915GttView, Kref, RcuHead, SgTable,
    },
    intel_engine_cs_upstream::{
        AtomicT, IntelEngineCs, IntelGt, ListHead, Mutex, RbNode, RbRoot, RbRootCached, Spinlock,
        WorkStruct,
    },
    intel_engine_heartbeat_upstream::intel_engine_flush_barriers,
    intel_ggtt_fencing_types_upstream::I915FenceReg,
    intel_gt_api_upstream::{intel_gt_flush_ggtt_writes, intel_gt_wait_for_idle},
    intel_gtt_api_upstream::{
        I915_GTT_MIN_ALIGNMENT, I915_GTT_PAGE_MASK, I915_GTT_PAGE_SIZE, I915_GTT_PAGE_SIZE_2M,
        I915_GTT_PAGE_SIZE_64K, I915AddressSpace, I915Ggtt, I915VmPtStash, I915VmaOpsLayout,
        i915_is_ggtt, i915_is_ggtt_or_dpt, i915_vm_alloc_pt_stash, i915_vm_free_pt_stash,
        i915_vm_has_cache_coloring, i915_vm_lock_objects, i915_vm_map_pt_stash,
        i915_vm_obj_min_alignment, i915_vm_put, i915_vm_resv_put, i915_vm_to_ggtt, i915_vm_tryget,
        intel_vm_no_concurrent_access_wa,
    },
    intel_runtime_pm_upstream::{intel_runtime_pm_get, intel_runtime_pm_put_unchecked},
    intel_uncore_types_upstream::IntelRuntimePm,
    linux::{
        gem::{DmaResv, drm_vma_node_offset_addr},
        gem_memory::{IntelMemoryRegion, drm_mm_node_allocated},
        i915::INTEL_INFO,
        i915_private::DrmI915Private,
        list::{list_add, list_add_tail, list_del, list_del_init, list_move, list_move_tail},
        locks::{
            spin_lock as spin_lock_ref, spin_lock_irq as spin_lock_irq_ref,
            spin_unlock as spin_unlock_ref, spin_unlock_irq as spin_unlock_irq_ref,
            spin_unlock_irqrestore as spin_unlock_irqrestore_ref,
        },
        memory::{
            atomic_add, atomic_add_unless, atomic_dec, atomic_dec_and_lock_irqsave, atomic_inc,
            atomic_read, kfree, kmalloc, kref_get, kref_get_unless_zero, kref_put,
        },
        mutex::{mutex_lock, mutex_trylock, mutex_unlock},
        primitives::{fetch_and_zero, ilog2, is_power_of_2},
        rbtree::{rb_erase, rb_next},
        rcu::{rcu_access_pointer, rcu_dereference, rcu_read_lock, rcu_read_unlock},
        registers::PIN_GLOBAL,
        requests::{dma_fence_get, dma_fence_get_rcu, dma_fence_put},
        scatterlist as _,
    },
    linux_config::{
        __GFP_NOWARN, E2BIG, EAGAIN, EBUSY, EDEADLK, EFAULT, EINVAL, EIO, ENODEV, ENOENT, ENOMEM,
        ENOSPC, ERR_PTR, GFP_KERNEL, GFP_NOWAIT, IS_ERR, MAX_SCHEDULE_TIMEOUT, PAGE_SHIFT,
        PAGE_SIZE, PTR_ERR, PTR_ERR_OR_ZERO,
    },
};

const EXEC_OBJECT_WRITE: u32 = 1 << 2;
const EXEC_OBJECT_NEEDS_FENCE: u32 = 1 << 0;
const EXEC_OBJECT_NEEDS_MAP: u32 = 1 << 1;
const I915_GEM_DOMAIN_RENDER: u16 = 0x0002;
const I915_GEM_GPU_DOMAINS: u16 = 0x003e;

pub const I915_VMA_RELEASE_MAP: u32 = 1 << 0;

// i915_gem_gtt.h pin flags, retained as u64 so address bits remain in-band.
pub const PIN_NOEVICT: u64 = 1 << 0;
pub const PIN_NOSEARCH: u64 = 1 << 1;
pub const PIN_NONBLOCK: u64 = 1 << 2;
pub const PIN_MAPPABLE: u64 = 1 << 3;
pub const PIN_ZONE_4G: u64 = 1 << 4;
pub const PIN_HIGH: u64 = 1 << 5;
pub const PIN_OFFSET_BIAS: u64 = 1 << 6;
pub const PIN_OFFSET_FIXED: u64 = 1 << 7;
pub const PIN_OFFSET_GUARD: u64 = 1 << 8;
pub const PIN_VALIDATE: u64 = 1 << 9;
pub const PIN_USER: u64 = I915_VMA_LOCAL_BIND as u64;
pub const PIN_OFFSET_MASK: u64 = I915_GTT_PAGE_MASK;
const DMA_RESV_USAGE_WRITE: u32 = 1;
const DMA_RESV_USAGE_READ: u32 = 2;
const I915_ACTIVE_AWAIT_EXCL: u32 = 1 << 0;
const I915_ACTIVE_AWAIT_ACTIVE: u32 = 1 << 1;
const SG_CHAIN: usize = 1;
const SG_END: usize = 2;
const SG_PAGE_LINK_MASK: usize = SG_CHAIN | SG_END;
const I915_GTT_VIEW_NORMAL: u32 = 0;
const I915_GTT_VIEW_PARTIAL: u32 = 12;
const I915_GTT_VIEW_ROTATED: u32 = 24;
const I915_GTT_VIEW_REMAPPED: u32 = 52;
const I915_MAP_WC: u32 = 1;
const ORIGIN_CS: i32 = 1;
const __GFP_RETRY_MAYFAIL: u32 = 1 << 14;
const I915_VMA_RESOURCE_NEEDS_WAKEREF_BIT: u8 = 1 << 2;
const I915_VMA_RESOURCE_SKIP_PTE_REWRITE_BIT: u8 = 1 << 3;
// Both options are disabled in the configured Linux 7.2.3 build used by this
// target. Keep the full conditional code below source-faithful.
const DEBUG_VMA_ALLOCATOR: bool = false;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IntelRemappedPlaneInfo {
    offset_linear: u32,
    data: [u16; 4],
}

impl IntelRemappedPlaneInfo {
    #[inline]
    fn data(&self, index: usize) -> u16 {
        unsafe { ptr::read_unaligned(ptr::addr_of!(self.data).cast::<u16>().add(index)) }
    }
    #[inline]
    fn offset(&self) -> u32 {
        self.offset_linear & 0x7fff_ffff
    }
    #[inline]
    fn linear(&self) -> bool {
        self.offset_linear & 0x8000_0000 != 0
    }
    #[inline]
    fn width(&self) -> u16 {
        self.data(0)
    }
    #[inline]
    fn height(&self) -> u16 {
        self.data(1)
    }
    #[inline]
    fn src_stride(&self) -> u16 {
        self.data(2)
    }
    #[inline]
    fn dst_stride(&self) -> u16 {
        self.data(3)
    }
    #[inline]
    fn size(&self) -> u32 {
        self.data(0) as u32 | ((self.data(1) as u32) << 16)
    }
}

#[repr(C, packed)]
struct IntelRotationInfo {
    plane: [IntelRemappedPlaneInfo; 2],
}

#[repr(C, packed)]
struct IntelRemappedInfo {
    plane: [IntelRemappedPlaneInfo; 4],
    plane_alignment: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmI915VmaResourceOps {
    bind_vma: Option<
        unsafe extern "C" fn(
            *mut I915AddressSpace,
            *mut I915VmPtStash,
            *mut I915VmaResource,
            u32,
            u32,
        ),
    >,
    unbind_vma: Option<unsafe extern "C" fn(*mut I915AddressSpace, *mut I915VmaResource)>,
}

#[repr(C)]
struct I915VmaWork {
    base: DmaFenceWork,
    vm: *mut I915AddressSpace,
    stash: I915VmPtStash,
    vma_res: *mut I915VmaResource,
    obj: *mut DrmI915GemObject,
    cb: I915SwDmaFenceCb,
    pat_index: u32,
    flags: u32,
}

#[repr(C)]
struct I915RefctSgtView {
    kref: Kref,
    _pad: u32,
    table: SgTable,
    size: usize,
    ops: *const c_void,
}

#[repr(C)]
struct IntelFrontbufferView {
    display: *mut c_void,
    bits: AtomicT,
    _pad: u32,
    flush_work: WorkStruct,
}

#[repr(C)]
struct I915FrontbufferView {
    base: IntelFrontbufferView,
    obj: *mut DrmI915GemObject,
    write: I915Active,
    rcu: crate::intel_context_upstream::RcuHead,
    ref_: Kref,
}

#[repr(C)]
struct DmaFenceArrayView {
    base: DmaFence,
    num_fences: u32,
    _num_pending: AtomicT,
    fences: *mut *mut DmaFence,
}

const _: [(); 12] = [(); size_of::<IntelRemappedPlaneInfo>()];
const _: [(); 24] = [(); size_of::<IntelRotationInfo>()];
const _: [(); 52] = [(); size_of::<IntelRemappedInfo>()];
const _: [(); 40] = [(); size_of::<I915RefctSgtView>()];
const _: [(); 8] = [(); offset_of!(I915RefctSgtView, table)];
const _: [(); 224] = [(); offset_of!(I915FrontbufferView, ref_)];

static SLAB_VMAS: AtomicPtr<crate::linux_heap::KmCache> = AtomicPtr::new(ptr::null_mut());

static BIND_WORK_OPS: DmaFenceWorkOps = DmaFenceWorkOps {
    name: b"bind\0".as_ptr().cast(),
    work: Some(__vma_bind),
    release: Some(__vma_release),
};

unsafe extern "C" {
    fn mutex_lock_interruptible(lock: *mut Mutex) -> i32;
    fn mutex_lock_interruptible_nested(lock: *mut Mutex, subclass: u32) -> i32;
    fn rb_insert_color(node: *mut RbNode, root: *mut RbRoot);
    fn drm_mm_remove_node(node: *mut DrmMmNode);
    fn i915_gem_fence_size(i915: *mut DrmI915Private, size: u64, tiling: u32, stride: u32) -> u32;
    fn i915_gem_fence_alignment(
        i915: *mut DrmI915Private,
        size: u64,
        tiling: u32,
        stride: u32,
    ) -> u32;
    fn i915_gem_gtt_reserve(
        vm: *mut I915AddressSpace,
        ww: *mut c_void,
        node: *mut DrmMmNode,
        size: u64,
        start: u64,
        color: c_ulong,
        flags: u64,
    ) -> i32;
    fn i915_gem_gtt_insert(
        vm: *mut I915AddressSpace,
        ww: *mut c_void,
        node: *mut DrmMmNode,
        size: u64,
        alignment: u64,
        color: c_ulong,
        start: u64,
        end: u64,
        flags: u64,
    ) -> i32;
    fn i915_vma_resource_alloc() -> *mut I915VmaResource;
    fn i915_vma_resource_free(vma_res: *mut I915VmaResource);
    fn __i915_vma_resource_init(vma_res: *mut I915VmaResource);
    fn i915_vma_resource_hold(vma_res: *mut I915VmaResource, lockdep_cookie: *mut bool) -> bool;
    fn i915_vma_resource_unhold(vma_res: *mut I915VmaResource, lockdep_cookie: bool);
    fn i915_vma_resource_bind_dep_await(
        vm: *mut I915AddressSpace,
        chain: *mut I915SwFence,
        start: u64,
        size: u64,
        bind: bool,
        gfp: u32,
    ) -> i32;
    fn i915_vma_resource_bind_dep_sync(
        vm: *mut I915AddressSpace,
        start: u64,
        size: u64,
        bind: bool,
    ) -> i32;
    fn i915_vma_resource_unbind(vma_res: *mut I915VmaResource, tlb: *mut u32) -> *mut DmaFence;
    fn i915_gem_evict_vm(vm: *mut I915AddressSpace, ww: *mut c_void, target: *mut c_void) -> i32;
    fn i915_gem_object_frontbuffer_put(front: *mut I915Frontbuffer);
    fn __intel_frontbuffer_invalidate(front: *mut c_void, origin: i32, bits: u32);
    fn trace_i915_vma_unbind(vma: *mut I915Vma);
    fn stack_depot_snprint(handle: u32, buf: *mut c_char, size: usize, spaces: u32) -> usize;
    fn unmap_mapping_range(mapping: *mut c_void, start: u64, length: u64, even_cows: i32);
    fn sg_alloc_table(table: *mut SgTable, nents: u32, flags: u32) -> i32;
    fn sg_free_table(table: *mut SgTable);
    static dma_fence_array_ops: u8;
}

#[inline]
unsafe fn spin_lock(lock: *mut Spinlock) {
    unsafe { spin_lock_ref(&mut *lock) };
}

#[inline]
unsafe fn spin_unlock(lock: *mut Spinlock) {
    unsafe { spin_unlock_ref(&mut *lock) };
}

#[inline]
unsafe fn spin_lock_irq(lock: *mut Spinlock) {
    unsafe { spin_lock_irq_ref(&mut *lock) };
}

#[inline]
unsafe fn spin_unlock_irq(lock: *mut Spinlock) {
    unsafe { spin_unlock_irq_ref(&mut *lock) };
}

#[inline]
unsafe fn spin_unlock_irqrestore(lock: *mut Spinlock, flags: u64) {
    unsafe { spin_unlock_irqrestore_ref(&mut *lock, flags) };
}

#[inline]
unsafe fn vma_size(vma: *const I915Vma) -> u64 {
    unsafe { (*vma).node.size - 2 * (*vma).guard as u64 }
}

#[inline]
unsafe fn vma_offset(vma: *const I915Vma) -> u64 {
    unsafe { (*vma).node.start + (*vma).guard as u64 }
}

#[inline]
unsafe fn vma_ggtt_offset(vma: *const I915Vma) -> u32 {
    assert!(unsafe { i915_vma_is_ggtt(vma) });
    let offset = unsafe { vma_offset(vma) };
    assert_eq!(offset >> 32, 0);
    assert_eq!(
        unsafe { vma_size(vma) }
            .saturating_sub(1)
            .saturating_add(offset)
            >> 32,
        0
    );
    offset as u32
}

#[inline]
fn is_aligned(value: u64, alignment: u64) -> bool {
    alignment != 0 && value & (alignment - 1) == 0
}

#[inline]
fn align_up(value: u64, alignment: u64) -> u64 {
    value.saturating_add(alignment - 1) & !(alignment - 1)
}

#[inline]
fn range_overflows(start: u64, size: u64, limit: u64) -> bool {
    start.checked_add(size).map_or(true, |end| end > limit)
}

#[inline]
fn rounddown_pow_of_two(value: u64) -> u64 {
    if value == 0 {
        0
    } else {
        1u64 << (u64::BITS - 1 - value.leading_zeros())
    }
}

#[inline]
unsafe fn atomic_add_return(value: *mut AtomicT, addend: i32) -> i32 {
    let atomic = unsafe { AtomicI32::from_ptr(ptr::addr_of_mut!((*value).counter)) };
    atomic
        .fetch_add(addend, Ordering::SeqCst)
        .wrapping_add(addend)
}

#[inline]
unsafe fn atomic_sub_return(value: *mut AtomicT, subtrahend: i32) -> i32 {
    let atomic = unsafe { AtomicI32::from_ptr(ptr::addr_of_mut!((*value).counter)) };
    atomic
        .fetch_sub(subtrahend, Ordering::SeqCst)
        .wrapping_sub(subtrahend)
}

#[inline]
unsafe fn atomic_or(value: *mut AtomicT, mask: i32) {
    let atomic = unsafe { AtomicI32::from_ptr(ptr::addr_of_mut!((*value).counter)) };
    atomic.fetch_or(mask, Ordering::SeqCst);
}

#[inline]
unsafe fn atomic_and(value: *mut AtomicT, mask: i32) {
    let atomic = unsafe { AtomicI32::from_ptr(ptr::addr_of_mut!((*value).counter)) };
    atomic.fetch_and(mask, Ordering::SeqCst);
}

#[inline]
unsafe fn atomic_try_cmpxchg(value: *mut AtomicT, expected: &mut i32, new_value: i32) -> bool {
    let atomic = unsafe { AtomicI32::from_ptr(ptr::addr_of_mut!((*value).counter)) };
    match atomic.compare_exchange(*expected, new_value, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => true,
        Err(actual) => {
            *expected = actual;
            false
        }
    }
}

#[inline]
fn node_color_differs(node: *const DrmMmNode, color: c_ulong) -> bool {
    unsafe { drm_mm_node_allocated(node) && (*node).color != color }
}

#[inline]
unsafe fn drm_mm_hole_follows(node: *const DrmMmNode) -> bool {
    unsafe { (*node).hole_size != 0 }
}

#[inline]
unsafe fn intel_gt_next_invalidate_tlb_full(gt: *mut IntelGt) -> u32 {
    unsafe { ptr::read_volatile(ptr::addr_of!((*gt).tlb.seqno.sequence)) | 1 }
}

#[inline]
unsafe fn i915_request_await_exclusive(rq: *mut I915Request, active: *mut I915Active) -> i32 {
    let fence = unsafe { i915_active_fence_get(ptr::addr_of_mut!((*active).excl)) };
    if fence.is_null() {
        return 0;
    }
    let err = unsafe { crate::i915_request_upstream::i915_request_await_dma_fence(rq, fence) };
    unsafe { dma_fence_put(fence) };
    err
}

#[inline]
unsafe fn rb_clear_node(node: *mut RbNode) {
    unsafe {
        (*node).parent_color = node as usize;
        (*node).left = ptr::null_mut();
        (*node).right = ptr::null_mut();
    }
}

#[inline]
unsafe fn rb_empty_node(node: *const RbNode) -> bool {
    unsafe { (*node).parent_color == node as usize }
}

#[inline]
unsafe fn rb_link_node(node: *mut RbNode, parent: *mut RbNode, link: *mut *mut RbNode) {
    unsafe {
        (*node).parent_color = parent as usize | 1;
        (*node).left = ptr::null_mut();
        (*node).right = ptr::null_mut();
        *link = node;
    }
}

#[inline]
unsafe fn page_link_next(sg: *mut Scatterlist) -> *mut Scatterlist {
    let link = unsafe { (*sg).page_link };
    if link & SG_CHAIN != 0 {
        (link & !SG_PAGE_LINK_MASK) as *mut Scatterlist
    } else if link & SG_END != 0 {
        ptr::null_mut()
    } else {
        unsafe { sg.add(1) }
    }
}

#[inline]
unsafe fn sg_set_dma_page(sg: *mut Scatterlist, length: u32) {
    unsafe {
        (*sg).page_link &= SG_PAGE_LINK_MASK;
        (*sg).offset = 0;
        (*sg).length = length;
        (*sg).dma_address = 0;
        (*sg).dma_length = 0;
    }
}

#[inline]
unsafe fn set_sg_dma_address(sg: *mut Scatterlist, address: u64) {
    unsafe { (*sg).dma_address = address }
}

#[inline]
unsafe fn set_sg_dma_len(sg: *mut Scatterlist, length: u32) {
    unsafe { (*sg).dma_length = length }
}

#[inline]
unsafe fn dma_resv_of_object(obj: *mut DrmI915GemObject) -> *mut DmaResv {
    let base = unsafe { obj.cast::<DrmGemObjectBaseLayout>() };
    unsafe { (*base).resv.cast::<DmaResv>() }
}

#[inline]
unsafe fn has_64k_pages(i915: *mut DrmI915Private) -> bool {
    let info = unsafe { INTEL_INFO(i915) };
    !info.is_null() && unsafe { (*info).flags[1] & (1 << 1) != 0 }
}

#[inline]
unsafe fn compare_gtt_views(
    current: *const I915Vma,
    vm: *mut I915AddressSpace,
    view: *const I915GttView,
) -> isize {
    let current_vm = unsafe { (*current).vm } as usize;
    let target_vm = vm as usize;
    if current_vm != target_vm {
        return current_vm.wrapping_sub(target_vm) as isize;
    }
    let current_type = unsafe { (*current).gtt_view.r#type };
    let target_type = if view.is_null() {
        I915_GTT_VIEW_NORMAL
    } else {
        unsafe { (*view).r#type }
    };
    let type_cmp = current_type as i32 - target_type as i32;
    if type_cmp != 0 || view.is_null() {
        return type_cmp as isize;
    }
    let size = target_type as usize;
    if size == I915_GTT_VIEW_NORMAL as usize {
        return 0;
    }
    let left = unsafe { ptr::addr_of!((*current).gtt_view.info).cast::<u8>() };
    let right = unsafe { ptr::addr_of!((*view).info).cast::<u8>() };
    for index in 0..size {
        let a = unsafe { *left.add(index) };
        let b = unsafe { *right.add(index) };
        if a != b {
            return a as isize - b as isize;
        }
    }
    0
}

#[inline]
unsafe fn oa_refct_sgt_table(rsgt: *mut I915RefctSgt) -> *mut SgTable {
    unsafe { ptr::addr_of_mut!((*rsgt.cast::<I915RefctSgtView>()).table) }
}

#[inline]
unsafe fn frontbuffer_lookup(obj: *mut DrmI915GemObject) -> *mut I915Frontbuffer {
    let pointer = unsafe { rcu_access_pointer(ptr::addr_of!((*obj).frontbuffer)) };
    if pointer.is_null() {
        return pointer;
    }
    rcu_read_lock();
    let mut front = ptr::null_mut();
    loop {
        front = unsafe { rcu_dereference(ptr::addr_of!((*obj).frontbuffer)) };
        if front.is_null() {
            break;
        }
        let view = front.cast::<I915FrontbufferView>();
        if !unsafe { kref_get_unless_zero(&mut (*view).ref_) } {
            continue;
        }
        if front == unsafe { rcu_access_pointer(ptr::addr_of!((*obj).frontbuffer)) } {
            break;
        }
        unsafe { i915_gem_object_frontbuffer_put(front) };
    }
    rcu_read_unlock();
    front
}

#[inline]
unsafe fn intel_frontbuffer_invalidate(front: *mut I915Frontbuffer, origin: i32) -> bool {
    if front.is_null() {
        return false;
    }
    let view = front.cast::<I915FrontbufferView>();
    let bits = atomic_read(&unsafe { (*view).base.bits });
    if bits == 0 {
        return false;
    }
    unsafe { __intel_frontbuffer_invalidate(front.cast(), origin, bits as u32) };
    true
}

// upstream: i915_vma.c assert_vma_held_evict()
unsafe fn assert_vma_held_evict(vma: *const I915Vma) {
    // Dead VMs are the one case where forced cleanup may run after object
    // references were dropped; otherwise unbind requires the shared object lock.
    if crate::linux::memory::kref_read(unsafe { &(*(*vma).vm).r#ref }) != 0 {
        unsafe { assert_object_held_shared((*vma).obj) };
    }
}

// upstream: i915_vma.c i915_vma_alloc()
unsafe fn i915_vma_alloc() -> *mut I915Vma {
    let cache = SLAB_VMAS.load(Ordering::Acquire);
    if cache.is_null() {
        return ptr::null_mut();
    }
    unsafe { crate::linux_heap::kmem_cache_zalloc::<I915Vma>(cache, GFP_KERNEL) }
}

// upstream: i915_vma.c i915_vma_free()
unsafe fn i915_vma_free(vma: *mut I915Vma) {
    let cache = SLAB_VMAS.load(Ordering::Acquire);
    assert!(!cache.is_null());
    unsafe { crate::linux_heap::kmem_cache_free(cache, vma.cast()) };
}

// upstream: i915_vma.c vma_print_allocator() [CONFIG_DRM_I915_ERRLOG_GEM && CONFIG_DRM_DEBUG_MM]
unsafe fn vma_print_allocator_debug(vma: *mut I915Vma, reason: *const c_char) {
    if !DEBUG_VMA_ALLOCATOR {
        return;
    }
    let handle = unsafe {
        ptr::read_unaligned(
            vma.cast::<u8>()
                .add(offset_of!(I915Vma, node) + size_of::<DrmMmNode>())
                .cast::<u32>(),
        )
    };
    let drm = unsafe { (*vma).obj.cast::<DrmGemObjectBaseLayout>() };
    if handle == 0 {
        drm_dbg!(
            unsafe { (*drm).dev },
            "vma.node [%08llx + %08llx] %s: unknown owner\n",
            unsafe { (*vma).node.start },
            unsafe { (*vma).node.size },
            reason,
        );
        return;
    }
    let mut buf = [0 as c_char; 512];
    unsafe { stack_depot_snprint(handle, buf.as_mut_ptr(), buf.len(), 0) };
    drm_dbg!(
        unsafe { (*drm).dev },
        "vma.node [%08llx + %08llx] %s: inserted at %s\n",
        unsafe { (*vma).node.start },
        unsafe { (*vma).node.size },
        reason,
        buf.as_ptr(),
    );
}

// upstream: i915_vma.c vma_print_allocator() [other configurations]
unsafe fn vma_print_allocator(vma: *mut I915Vma, reason: *const c_char) {
    unsafe { vma_print_allocator_debug(vma, reason) }
}

// upstream: i915_vma.c active_to_vma()
#[inline]
unsafe fn active_to_vma(active: *mut I915Active) -> *mut I915Vma {
    unsafe {
        active
            .cast::<u8>()
            .sub(offset_of!(I915Vma, active))
            .cast::<I915Vma>()
    }
}

// upstream: i915_vma.c __i915_vma_active()
unsafe extern "C" fn __i915_vma_active(active: *mut I915Active) -> c_int {
    let vma = unsafe { active_to_vma(active) };
    if unsafe { i915_vma_tryget(vma) }.is_null() {
        return -ENOENT;
    }
    if !unsafe { i915_vma_is_ggtt(vma) } {
        unsafe { crate::linux::pm::intel_gt_pm_get_untracked((*(*vma).vm).gt) };
    }
    0
}

// upstream: i915_vma.c __i915_vma_retire()
unsafe extern "C" fn __i915_vma_retire(active: *mut I915Active) {
    let vma = unsafe { active_to_vma(active) };
    if !unsafe { i915_vma_is_ggtt(vma) } {
        unsafe { crate::linux::pm::intel_gt_pm_put_async_untracked((*(*vma).vm).gt) };
    }
    unsafe { i915_vma_put(vma) };
}

#[inline]
unsafe fn intel_rotation_info_size(info: *const IntelRotationInfo) -> u32 {
    let mut size = 0u32;
    for plane in 0..2 {
        let current = unsafe { &*ptr::addr_of!((*info).plane[plane]) };
        size =
            size.wrapping_add((current.dst_stride() as u32).wrapping_mul(current.width() as u32));
    }
    size
}

#[inline]
unsafe fn intel_remapped_info_size(info: *const IntelRemappedInfo) -> u32 {
    let alignment = unsafe { ptr::read_unaligned(ptr::addr_of!((*info).plane_alignment)) };
    let mut size = 0u32;
    for plane in 0..4 {
        let current = unsafe { &*ptr::addr_of!((*info).plane[plane]) };
        let plane_size = if current.linear() {
            current.size()
        } else {
            (current.dst_stride() as u32).wrapping_mul(current.height() as u32)
        };
        if plane_size == 0 {
            continue;
        }
        if alignment != 0 {
            size = size.saturating_add(alignment - 1) & !(alignment - 1);
        }
        size = size.wrapping_add(plane_size);
    }
    size
}

// upstream: i915_vma.c vma_create()
unsafe fn vma_create(
    obj: *mut DrmI915GemObject,
    vm: *mut I915AddressSpace,
    view: *const I915GttView,
) -> *mut I915Vma {
    let mut existing_or_error = ERR_PTR::<I915Vma>(-E2BIG);
    let alias_vm = unsafe { ptr::addr_of_mut!((*(*(*(*vm).gt).ggtt).alias).vm) };
    if vm == alias_vm {
        GEM_BUG_ON!(true);
    }
    let vma = unsafe { i915_vma_alloc() };
    if vma.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    unsafe {
        (*vma).ops = ptr::addr_of!((*vm).vma_ops).cast();
        (*vma).obj = obj;
        (*vma).size = (*obj.cast::<DrmGemObjectBaseLayout>()).size;
        (*vma).display_alignment = I915_GTT_MIN_ALIGNMENT as u32;
        i915_active_init(
            ptr::addr_of_mut!((*vma).active),
            __i915_vma_active,
            __i915_vma_retire,
            0,
        );
        INIT_LIST_HEAD!(ptr::addr_of_mut!((*vma).closed_link));
        INIT_LIST_HEAD!(ptr::addr_of_mut!((*vma).obj_link));
        rb_clear_node(ptr::addr_of_mut!((*vma).obj_node));
    }

    if !view.is_null() && unsafe { (*view).r#type != I915_GTT_VIEW_NORMAL } {
        unsafe { ptr::copy_nonoverlapping(view, ptr::addr_of_mut!((*vma).gtt_view), 1) };
        match unsafe { (*view).r#type } {
            I915_GTT_VIEW_PARTIAL => {
                let partial = unsafe { ptr::addr_of!((*view).info.partial) };
                let offset = unsafe { ptr::read_unaligned(ptr::addr_of!((*partial).offset)) };
                let pages = unsafe { ptr::read_unaligned(ptr::addr_of!((*partial).size)) } as u64;
                if range_overflows(offset, pages, unsafe {
                    (*obj.cast::<DrmGemObjectBaseLayout>()).size >> PAGE_SHIFT
                }) {
                    GEM_BUG_ON!(true);
                }
                unsafe { (*vma).size = pages << PAGE_SHIFT };
                GEM_BUG_ON!(unsafe { (*vma).size > (*obj.cast::<DrmGemObjectBaseLayout>()).size });
            }
            I915_GTT_VIEW_ROTATED => {
                let info = unsafe { view.cast::<u8>().add(4).cast::<IntelRotationInfo>() };
                unsafe { (*vma).size = (intel_rotation_info_size(info) as u64) << PAGE_SHIFT };
            }
            I915_GTT_VIEW_REMAPPED => {
                let info = unsafe { view.cast::<u8>().add(4).cast::<IntelRemappedInfo>() };
                unsafe { (*vma).size = (intel_remapped_info_size(info) as u64) << PAGE_SHIFT };
            }
            _ => GEM_BUG_ON!(true),
        }
    }
    if unsafe { (*vma).size > (*vm).total } {
        unsafe { i915_vma_free(vma) };
        return existing_or_error;
    }
    GEM_BUG_ON!(!is_aligned(unsafe { (*vma).size }, I915_GTT_PAGE_SIZE));

    let err = unsafe { mutex_lock_interruptible(ptr::addr_of_mut!((*vm).mutex)) };
    if err != 0 {
        unsafe { i915_vma_free(vma) };
        return ERR_PTR(err);
    }
    unsafe {
        (*vma).vm = vm;
        list_add_tail(
            ptr::addr_of_mut!((*vma).vm_link),
            ptr::addr_of_mut!((*vm).unbound_list),
        );
        spin_lock(ptr::addr_of_mut!((*obj).vma.lock));
    }

    if unsafe { i915_is_ggtt(vm) } {
        if unsafe { (*vma).size > u32::MAX as u64 } {
            existing_or_error = ERR_PTR(-E2BIG);
            unsafe { spin_unlock(ptr::addr_of_mut!((*obj).vma.lock)) };
            unsafe { list_del_init(ptr::addr_of_mut!((*vma).vm_link)) };
            unsafe { mutex_unlock(ptr::addr_of_mut!((*vm).mutex)) };
            unsafe { i915_vma_free(vma) };
            return existing_or_error;
        }
        let i915 = unsafe { (*vm).i915 };
        let tiling = unsafe { i915_gem_object_get_tiling(obj) };
        let stride = unsafe { i915_gem_object_get_stride(obj) };
        unsafe {
            (*vma).fence_size = i915_gem_fence_size(i915, (*vma).size, tiling, stride);
        }
        if unsafe {
            ((*vma).fence_size as u64) < (*vma).size || ((*vma).fence_size as u64) > (*vm).total
        } {
            existing_or_error = ERR_PTR(-E2BIG);
            unsafe { spin_unlock(ptr::addr_of_mut!((*obj).vma.lock)) };
            unsafe { list_del_init(ptr::addr_of_mut!((*vma).vm_link)) };
            unsafe { mutex_unlock(ptr::addr_of_mut!((*vm).mutex)) };
            unsafe { i915_vma_free(vma) };
            return existing_or_error;
        }
        GEM_BUG_ON!(!is_aligned(
            (*vma).fence_size as u64,
            I915_GTT_MIN_ALIGNMENT
        ));
        unsafe {
            (*vma).fence_alignment = i915_gem_fence_alignment(i915, (*vma).size, tiling, stride);
        }
        GEM_BUG_ON!(!is_power_of_2(unsafe { (*vma).fence_alignment }));
        unsafe { atomic_or(ptr::addr_of_mut!((*vma).flags), I915_VMA_GGTT) };
    }

    let mut parent: *mut RbNode = ptr::null_mut();
    let mut link = unsafe { ptr::addr_of_mut!((*obj).vma.tree.node) };
    while !unsafe { (*link).is_null() } {
        parent = unsafe { *link };
        let pos = unsafe {
            parent
                .cast::<u8>()
                .sub(offset_of!(I915Vma, obj_node))
                .cast::<I915Vma>()
        };
        existing_or_error = pos;
        let cmp = unsafe { compare_gtt_views(pos, vm, view) };
        if cmp < 0 {
            link = unsafe { ptr::addr_of_mut!((*parent).right) };
        } else if cmp > 0 {
            link = unsafe { ptr::addr_of_mut!((*parent).left) };
        } else {
            unsafe { spin_unlock(ptr::addr_of_mut!((*obj).vma.lock)) };
            unsafe { list_del_init(ptr::addr_of_mut!((*vma).vm_link)) };
            unsafe { mutex_unlock(ptr::addr_of_mut!((*vm).mutex)) };
            unsafe { i915_vma_free(vma) };
            return existing_or_error;
        }
    }
    unsafe {
        rb_link_node(ptr::addr_of_mut!((*vma).obj_node), parent, link);
        rb_insert_color(
            ptr::addr_of_mut!((*vma).obj_node),
            ptr::addr_of_mut!((*obj).vma.tree),
        );
    }
    if unsafe { i915_vma_is_ggtt(vma) } {
        unsafe {
            list_add(
                ptr::addr_of_mut!((*vma).obj_link),
                ptr::addr_of_mut!((*obj).vma.list),
            )
        };
    } else {
        unsafe {
            list_add_tail(
                ptr::addr_of_mut!((*vma).obj_link),
                ptr::addr_of_mut!((*obj).vma.list),
            )
        };
    }
    unsafe {
        spin_unlock(ptr::addr_of_mut!((*obj).vma.lock));
        mutex_unlock(ptr::addr_of_mut!((*vm).mutex));
    }
    vma
}

// upstream: i915_vma.c i915_vma_lookup()
unsafe fn i915_vma_lookup(
    obj: *mut DrmI915GemObject,
    vm: *mut I915AddressSpace,
    view: *const I915GttView,
) -> *mut I915Vma {
    let mut node = unsafe { (*obj).vma.tree.node };
    while !node.is_null() {
        let vma = unsafe {
            node.cast::<u8>()
                .sub(offset_of!(I915Vma, obj_node))
                .cast::<I915Vma>()
        };
        let cmp = unsafe { compare_gtt_views(vma, vm, view) };
        if cmp == 0 {
            return vma;
        }
        node = unsafe { if cmp < 0 { (*node).right } else { (*node).left } };
    }
    ptr::null_mut()
}

// upstream: i915_vma.c i915_vma_instance()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_instance(
    obj: *mut DrmI915GemObject,
    vm: *mut I915AddressSpace,
    view: *const I915GttView,
) -> *mut I915Vma {
    GEM_BUG_ON!(!view.is_null() && !unsafe { i915_is_ggtt_or_dpt(vm) });
    GEM_BUG_ON!(crate::linux::memory::kref_read(unsafe { &(*vm).r#ref }) == 0);
    unsafe { spin_lock(ptr::addr_of_mut!((*obj).vma.lock)) };
    let mut vma = unsafe { i915_vma_lookup(obj, vm, view) };
    unsafe { spin_unlock(ptr::addr_of_mut!((*obj).vma.lock)) };
    if vma.is_null() {
        vma = unsafe { vma_create(obj, vm, view) };
    }
    GEM_BUG_ON!(!IS_ERR(vma) && unsafe { compare_gtt_views(vma, vm, view) != 0 });
    vma
}

struct I915VmaBindinfoMarker;

// upstream: i915_vma.c __vma_bind()
unsafe extern "C" fn __vma_bind(work: *mut DmaFenceWork) {
    let vw = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(I915VmaWork, base))
            .cast::<I915VmaWork>()
    };
    let vma_res = unsafe { (*vw).vma_res };
    if unsafe { (*(*vw).obj).mm.unknown_state } {
        return;
    }
    let ops = unsafe { (*vma_res).ops.cast::<I915VmaOpsLayout>() };
    if let Some(bind) = unsafe { (*ops).bind_vma } {
        unsafe {
            bind(
                (*vma_res).vm,
                ptr::addr_of_mut!((*vw).stash),
                vma_res,
                (*vw).pat_index,
                (*vw).flags,
            )
        };
    }
}

// upstream: i915_vma.c __vma_release()
unsafe extern "C" fn __vma_release(work: *mut DmaFenceWork) {
    let vw = unsafe {
        work.cast::<u8>()
            .sub(offset_of!(I915VmaWork, base))
            .cast::<I915VmaWork>()
    };
    unsafe {
        if !(*vw).obj.is_null() {
            i915_gem_object_put((*vw).obj);
        }
        i915_vm_free_pt_stash((*vw).vm, ptr::addr_of_mut!((*vw).stash));
        if !(*vw).vma_res.is_null() {
            i915_vma_resource_put((*vw).vma_res);
        }
    }
}

// upstream: i915_vma.c i915_vma_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_work() -> *mut c_void {
    let vw = crate::linux::memory::kzalloc_obj_flags::<I915VmaWork>(GFP_KERNEL);
    if vw.is_null() {
        return ptr::null_mut();
    }
    unsafe { dma_fence_work_init(ptr::addr_of_mut!((*vw).base), &BIND_WORK_OPS) };
    unsafe { (*vw).base.dma.error = -EAGAIN };
    vw.cast()
}

// upstream: i915_vma.c i915_vma_wait_for_bind()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_wait_for_bind(vma: *mut I915Vma) -> c_int {
    let mut err = 0;
    if !unsafe { rcu_access_pointer(ptr::addr_of!((*vma).active.excl.fence)) }.is_null() {
        rcu_read_lock();
        let fence =
            unsafe { dma_fence_get_rcu(ptr::read(ptr::addr_of!((*vma).active.excl.fence))) };
        rcu_read_unlock();
        if !fence.is_null() {
            err = unsafe { dma_fence_wait(fence, true) };
            unsafe { dma_fence_put(fence) };
        }
    }
    err
}

// upstream: i915_vma.c i915_vma_verify_bind_complete()
unsafe fn i915_vma_verify_bind_complete(vma: *mut I915Vma) -> c_int {
    if !crate::linux_config::CONFIG_DRM_I915_DEBUG_GEM {
        return 0;
    }
    let fence = unsafe { i915_active_fence_get(ptr::addr_of_mut!((*vma).active.excl)) };
    if fence.is_null() {
        return 0;
    }
    let err = if unsafe { dma_fence_is_signaled(fence) } {
        unsafe { (*fence).error }
    } else {
        -EBUSY
    };
    unsafe { dma_fence_put(fence) };
    err
}

#[inline]
unsafe fn i915_vma_resource_get(vma_res: *mut I915VmaResource) -> *mut I915VmaResource {
    unsafe { dma_fence_get(ptr::addr_of_mut!((*vma_res).unbind_fence)) };
    vma_res
}

#[inline]
unsafe fn i915_vma_resource_put(vma_res: *mut I915VmaResource) {
    unsafe { dma_fence_put(ptr::addr_of_mut!((*vma_res).unbind_fence)) };
}

#[inline]
unsafe fn i915_vma_resource_init(
    vma_res: *mut I915VmaResource,
    vm: *mut I915AddressSpace,
    pages: *mut SgTable,
    page_sizes: *const I915PageSizes,
    pages_rsgt: *mut I915RefctSgt,
    readonly: bool,
    lmem: bool,
    mr: *mut IntelMemoryRegion,
    ops: *const crate::intel_context_upstream::I915VmaOps,
    private: *mut c_void,
    start: u64,
    node_size: u64,
    size: u64,
    guard: u32,
) {
    unsafe {
        __i915_vma_resource_init(vma_res);
        (*vma_res).vm = vm;
        (*vma_res).bi.pages = pages;
        (*vma_res).bi.page_sizes = *page_sizes;
        if !pages_rsgt.is_null() {
            let view = pages_rsgt.cast::<I915RefctSgtView>();
            crate::linux::memory::kref_get(ptr::addr_of_mut!((*view).kref));
            (*vma_res).bi.pages_rsgt = pages_rsgt;
        }
        (*vma_res).bi.flags = (readonly as u8) | ((lmem as u8) << 1);
        (*vma_res).mr = mr;
        (*vma_res).ops = ops;
        (*vma_res).private = private;
        (*vma_res).start = start;
        (*vma_res).node_size = node_size;
        (*vma_res).vma_size = size;
        (*vma_res).guard = guard;
    }
}

// upstream: i915_vma.c i915_vma_resource_init_from_vma()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_resource_init_from_vma(
    vma_res: *mut I915VmaResource,
    vma: *mut I915Vma,
) {
    let obj = unsafe { (*vma).obj };
    unsafe {
        i915_vma_resource_init(
            vma_res,
            (*vma).vm,
            (*vma).pages,
            ptr::addr_of!((*vma).page_sizes),
            (*obj).mm.rsgt,
            i915_gem_object_is_readonly(obj),
            i915_gem_object_is_lmem(obj),
            (*obj).mm.region,
            (*vma).ops,
            (*vma).private,
            vma_offset(vma),
            vma_size(vma),
            (*vma).size,
            (*vma).guard,
        );
    }
}

// upstream: i915_vma.c i915_vma_bind()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_bind(
    vma: *mut I915Vma,
    pat_index: u32,
    flags: u32,
    work: *mut c_void,
    vma_res: *mut I915VmaResource,
) -> c_int {
    let work = work.cast::<I915VmaWork>();
    let vm = unsafe { (*vma).vm };
    GEM_BUG_ON!(!unsafe { drm_mm_node_allocated(&(*vma).node) });
    GEM_BUG_ON!(unsafe { (*vma).size > vma_size(vma) });
    if range_overflows(
        unsafe { (*vma).node.start },
        unsafe { (*vma).node.size },
        unsafe { (*vm).total },
    ) {
        unsafe { i915_vma_resource_free(vma_res) };
        return -ENODEV;
    }
    if flags == 0 {
        unsafe { i915_vma_resource_free(vma_res) };
        return -EINVAL;
    }

    let mut bind_flags = flags & (I915_VMA_GLOBAL_BIND | I915_VMA_LOCAL_BIND) as u32;
    let vma_flags = unsafe { atomic_read(&(*vma).flags) } as u32
        & (I915_VMA_GLOBAL_BIND | I915_VMA_LOCAL_BIND) as u32;
    bind_flags &= !vma_flags;
    if bind_flags == 0 {
        unsafe { i915_vma_resource_free(vma_res) };
        return 0;
    }
    GEM_BUG_ON!(unsafe { atomic_read(&(*vma).pages_count) == 0 });

    let ret = if !work.is_null() && bind_flags & unsafe { (*vm).bind_async_flags } != 0 {
        unsafe {
            i915_vma_resource_bind_dep_await(
                vm,
                ptr::addr_of_mut!((*work).base.chain),
                (*vma).node.start,
                (*vma).node.size,
                true,
                GFP_NOWAIT | __GFP_RETRY_MAYFAIL | __GFP_NOWARN,
            )
        }
    } else {
        unsafe { i915_vma_resource_bind_dep_sync(vm, (*vma).node.start, (*vma).node.size, true) }
    };
    if ret != 0 {
        unsafe { i915_vma_resource_free(vma_res) };
        return ret;
    }

    if unsafe { !(*vma).resource.is_null() || vma_res.is_null() } {
        GEM_WARN_ON!(vma_flags == 0);
        unsafe { i915_vma_resource_free(vma_res) };
    } else {
        unsafe { i915_vma_resource_init_from_vma(vma_res, vma) };
        unsafe { (*vma).resource = vma_res };
    }

    if !work.is_null() && bind_flags & unsafe { (*vm).bind_async_flags } != 0 {
        let prev;
        unsafe {
            (*work).vma_res = i915_vma_resource_get((*vma).resource);
            (*work).pat_index = pat_index;
            (*work).flags = bind_flags;
            prev =
                i915_active_set_exclusive(&mut (*vma).active, ptr::addr_of_mut!((*work).base.dma));
        }
        if !prev.is_null() {
            unsafe {
                __i915_sw_fence_await_dma_fence(
                    ptr::addr_of_mut!((*work).base.chain),
                    prev,
                    ptr::addr_of_mut!((*work).cb),
                );
                dma_fence_put(prev);
            }
        }
        unsafe {
            (*work).base.dma.error = 0;
            (*work).obj = i915_gem_object_get((*vma).obj);
        }
    } else {
        let ret = unsafe { i915_gem_object_wait_moving_fence((*vma).obj, true) };
        if ret != 0 {
            unsafe {
                i915_vma_resource_free((*vma).resource);
                (*vma).resource = ptr::null_mut();
            }
            return ret;
        }
        let ops = unsafe { (*vma).ops.cast::<I915VmaOpsLayout>() };
        if let Some(bind) = unsafe { (*ops).bind_vma } {
            unsafe { bind(vm, ptr::null_mut(), (*vma).resource, pat_index, bind_flags) };
        }
    }
    unsafe { atomic_or(ptr::addr_of_mut!((*vma).flags), bind_flags as i32) };
    0
}

// upstream: i915_vma.c i915_vma_pin_iomap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_pin_iomap(vma: *mut I915Vma) -> *mut c_void {
    if WARN_ON_ONCE!(unsafe { (*(*vma).obj).flags & I915_BO_ALLOC_GPU_ONLY != 0 }) {
        return ERR_PTR(-EINVAL);
    }
    GEM_BUG_ON!(!unsafe { i915_vma_is_ggtt(vma) });
    GEM_BUG_ON!(!unsafe { i915_vma_is_bound(vma, I915_VMA_GLOBAL_BIND as u32) });
    GEM_BUG_ON!(unsafe { i915_vma_verify_bind_complete(vma) != 0 });

    let mut iomap = unsafe { ptr::read_volatile(ptr::addr_of!((*vma).iomap)) };
    if iomap.is_null() {
        if unsafe { i915_gem_object_is_lmem((*vma).obj) } {
            iomap = unsafe {
                i915_gem_object_lmem_io_map(
                    (*vma).obj,
                    0,
                    (*(*vma).obj.cast::<DrmGemObjectBaseLayout>()).size as usize,
                )
            };
        } else if unsafe { i915_vma_is_map_and_fenceable(vma) } {
            let ggtt = unsafe { i915_vm_to_ggtt((*vma).vm) };
            iomap = unsafe {
                io_mapping_map_wc(
                    ptr::addr_of_mut!((*ggtt).iomap).cast(),
                    vma_offset(vma) as i64,
                    vma_size(vma) as usize,
                )
            };
        } else {
            iomap = unsafe { i915_gem_object_pin_map((*vma).obj, I915_MAP_WC) };
            if IS_ERR(iomap) {
                return iomap;
            }
            iomap = page_pack_bits(iomap, I915_MAP_WC);
        }
        if iomap.is_null() {
            return ERR_PTR(-ENOMEM);
        }
        let slot = unsafe { ptr::addr_of_mut!((*vma).iomap) };
        let atomic = unsafe { AtomicPtr::<c_void>::from_ptr(slot) };
        match atomic.compare_exchange(ptr::null_mut(), iomap, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {}
            Err(existing) => {
                if iomap as usize & (PAGE_SIZE - 1) != 0 {
                    unsafe { __i915_gem_object_release_map((*vma).obj) };
                } else {
                    unsafe { io_mapping_unmap(iomap) };
                }
                iomap = existing;
            }
        }
    }

    unsafe { __i915_vma_pin(vma) };
    let err = unsafe { i915_vma_pin_fence(vma) };
    if err != 0 {
        unsafe { __i915_vma_unpin(vma) };
        return ERR_PTR(err);
    }
    unsafe { i915_vma_set_ggtt_write(vma) };
    page_mask_bits(iomap)
}

// upstream: i915_vma.c i915_vma_flush_writes()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_flush_writes(vma: *mut I915Vma) {
    if unsafe { i915_vma_unset_ggtt_write(vma) } {
        unsafe { intel_gt_flush_ggtt_writes((*(*vma).vm).gt) };
    }
}

// upstream: i915_vma.c i915_vma_unpin_iomap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_unpin_iomap(vma: *mut I915Vma) {
    GEM_BUG_ON!(unsafe { (*vma).iomap.is_null() });
    unsafe { i915_vma_flush_writes(vma) };
    unsafe {
        i915_vma_unpin_fence(vma);
        __i915_vma_unpin(vma);
    }
}

// upstream: i915_vma.c i915_vma_unpin_and_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_unpin_and_release(vma: *mut *mut I915Vma, flags: u32) {
    let vma = unsafe { fetch_and_zero(&mut *vma) };
    if vma.is_null() {
        return;
    }
    let obj = unsafe { (*vma).obj };
    GEM_BUG_ON!(obj.is_null());
    unsafe { __i915_vma_unpin(vma) };
    if flags & I915_VMA_RELEASE_MAP != 0 {
        unsafe { i915_gem_object_unpin_map(obj) };
    }
    unsafe { i915_gem_object_put(obj) };
}

// upstream: i915_vma.c i915_vma_misplaced()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_misplaced(
    vma: *const I915Vma,
    size: u64,
    alignment: u64,
    flags: u64,
) -> bool {
    if !unsafe { drm_mm_node_allocated(&(*vma).node) } {
        return false;
    }
    if unsafe { crate::linux::bits::test_bit(I915_VMA_ERROR_BIT, &(*vma).flags.counter) } {
        return true;
    }
    if unsafe { vma_size(vma) < size } {
        return true;
    }
    GEM_BUG_ON!(alignment != 0 && !is_power_of_2(alignment));
    if alignment != 0 && !is_aligned(unsafe { vma_offset(vma) }, alignment) {
        return true;
    }
    if flags & PIN_MAPPABLE != 0 && !unsafe { i915_vma_is_map_and_fenceable(vma) } {
        return true;
    }
    if flags & PIN_OFFSET_BIAS != 0 && unsafe { vma_offset(vma) } < (flags & PIN_OFFSET_MASK) {
        return true;
    }
    if flags & PIN_OFFSET_FIXED != 0 && unsafe { vma_offset(vma) } != (flags & PIN_OFFSET_MASK) {
        return true;
    }
    flags & PIN_OFFSET_GUARD != 0 && unsafe { (*vma).guard as u64 } < (flags & PIN_OFFSET_MASK)
}

// upstream: i915_vma.c __i915_vma_set_map_and_fenceable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_vma_set_map_and_fenceable(vma: *mut I915Vma) {
    GEM_BUG_ON!(!unsafe { i915_vma_is_ggtt(vma) });
    GEM_BUG_ON!(unsafe { (*vma).fence_size == 0 });
    let fenceable = unsafe {
        vma_size(vma) >= (*vma).fence_size as u64
            && is_aligned(vma_offset(vma), (*vma).fence_alignment as u64)
    };
    let ggtt = unsafe { i915_vm_to_ggtt((*vma).vm) };
    let mappable =
        unsafe { vma_ggtt_offset(vma) as u64 + (*vma).fence_size as u64 <= (*ggtt).mappable_end };
    if mappable && fenceable {
        unsafe { crate::linux::bits::set_bit(I915_VMA_CAN_FENCE_BIT, &mut (*vma).flags.counter) };
    } else {
        unsafe { crate::linux::bits::clear_bit(I915_VMA_CAN_FENCE_BIT, &mut (*vma).flags.counter) };
    }
}

// upstream: i915_vma.c i915_gem_valid_gtt_space()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_valid_gtt_space(vma: *mut I915Vma, color: c_ulong) -> bool {
    if !unsafe { i915_vm_has_cache_coloring((*vma).vm) } {
        return true;
    }
    let node = ptr::addr_of!((*vma).node);
    GEM_BUG_ON!(!unsafe { drm_mm_node_allocated(node) });
    let link = ptr::addr_of!((*vma).node.node_list);
    GEM_BUG_ON!(unsafe { (*link).next == link.cast_mut() });
    let prev = unsafe {
        (*link)
            .prev
            .cast::<u8>()
            .sub(offset_of!(DrmMmNode, node_list))
            .cast::<DrmMmNode>()
    };
    if node_color_differs(prev, color) && !unsafe { drm_mm_hole_follows(prev) } {
        return false;
    }
    let next = unsafe {
        (*link)
            .next
            .cast::<u8>()
            .sub(offset_of!(DrmMmNode, node_list))
            .cast::<DrmMmNode>()
    };
    if node_color_differs(next, color) && !unsafe { drm_mm_hole_follows(node) } {
        return false;
    }
    true
}

// upstream: i915_vma.c i915_vma_insert()
unsafe fn i915_vma_insert(
    vma: *mut I915Vma,
    ww: *mut c_void,
    mut size: u64,
    mut alignment: u64,
    flags: u64,
) -> c_int {
    GEM_BUG_ON!(unsafe { i915_vma_is_bound(vma, I915_VMA_BIND_MASK as u32) });
    GEM_BUG_ON!(unsafe { drm_mm_node_allocated(&(*vma).node) });
    GEM_BUG_ON!((flags & (PIN_OFFSET_GUARD | PIN_OFFSET_FIXED | PIN_OFFSET_BIAS)).count_ones() > 1);

    size = core::cmp::max(size, unsafe { (*vma).size });
    alignment = core::cmp::max(alignment, unsafe { (*vma).display_alignment as u64 });
    if flags & PIN_MAPPABLE != 0 {
        size = core::cmp::max(size, unsafe { (*vma).fence_size as u64 });
        alignment = core::cmp::max(alignment, unsafe { (*vma).fence_alignment as u64 });
    }
    GEM_BUG_ON!(!is_aligned(size, I915_GTT_PAGE_SIZE));
    GEM_BUG_ON!(!is_aligned(alignment, I915_GTT_MIN_ALIGNMENT));
    GEM_BUG_ON!(!is_power_of_2(alignment));

    let mut guard = unsafe { (*vma).guard as u64 };
    if flags & PIN_OFFSET_GUARD != 0 {
        GEM_BUG_ON!((flags & PIN_OFFSET_MASK) > u32::MAX as u64);
        guard = core::cmp::max(guard, flags & PIN_OFFSET_MASK);
    }
    guard = align_up(guard, alignment);
    let start = if flags & PIN_OFFSET_BIAS != 0 {
        flags & PIN_OFFSET_MASK
    } else {
        0
    };
    GEM_BUG_ON!(!is_aligned(start, I915_GTT_PAGE_SIZE));

    let vm = unsafe { (*vma).vm };
    let mut end = unsafe { (*vm).total };
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    if flags & PIN_MAPPABLE != 0 {
        end = core::cmp::min(end, unsafe { (*ggtt).mappable_end });
    }
    if flags & PIN_ZONE_4G != 0 {
        end = core::cmp::min(end, (1u64 << 32) - I915_GTT_PAGE_SIZE);
    }
    GEM_BUG_ON!(!is_aligned(end, I915_GTT_PAGE_SIZE));

    alignment = core::cmp::max(alignment, unsafe {
        crate::intel_gtt_api_upstream::i915_vm_obj_min_alignment(vm, (*vma).obj)
    });
    if size > end.saturating_sub(guard.saturating_mul(2)) {
        return -ENOSPC;
    }
    let color = if unsafe { i915_vm_has_cache_coloring(vm) } {
        (unsafe { (*(*vma).obj).cache_state_bits } & 0x3f) as c_ulong
    } else {
        0
    };

    if flags & PIN_OFFSET_FIXED != 0 {
        let offset = flags & PIN_OFFSET_MASK;
        if !is_aligned(offset, alignment) || range_overflows(offset, size, end) {
            return -EINVAL;
        }
        if offset < guard || offset.saturating_add(size) > end.saturating_sub(guard) {
            return -ENOSPC;
        }
        let ret = unsafe {
            i915_gem_gtt_reserve(
                vm,
                ww,
                ptr::addr_of_mut!((*vma).node),
                size + 2 * guard,
                offset - guard,
                color,
                flags,
            )
        };
        if ret != 0 {
            return ret;
        }
    } else {
        size += 2 * guard;
        if end.saturating_sub(1) >> 32 != 0
            && unsafe { (*vma).page_sizes.sg as u64 > I915_GTT_PAGE_SIZE }
            && !unsafe { has_64k_pages((*vm).i915) }
        {
            GEM_BUG_ON!(unsafe { i915_vma_is_ggtt(vma) });
            let page_alignment = rounddown_pow_of_two(
                unsafe { (*vma).page_sizes.sg as u64 } | I915_GTT_PAGE_SIZE_2M,
            );
            alignment = core::cmp::max(alignment, page_alignment);
            if unsafe { (*vma).page_sizes.sg as u64 } & I915_GTT_PAGE_SIZE_64K != 0 {
                size = align_up(size, I915_GTT_PAGE_SIZE_2M);
            }
        }
        let ret = unsafe {
            i915_gem_gtt_insert(
                vm,
                ww,
                ptr::addr_of_mut!((*vma).node),
                size,
                alignment,
                color,
                start,
                end,
                flags,
            )
        };
        if ret != 0 {
            return ret;
        }
        GEM_BUG_ON!(unsafe { (*vma).node.start < start });
        GEM_BUG_ON!(unsafe { (*vma).node.start + (*vma).node.size > end });
    }
    GEM_BUG_ON!(!unsafe { drm_mm_node_allocated(&(*vma).node) });
    GEM_BUG_ON!(!unsafe { i915_gem_valid_gtt_space(vma, color) });
    unsafe {
        list_move_tail(
            ptr::addr_of_mut!((*vma).vm_link),
            ptr::addr_of_mut!((*vm).bound_list),
        );
        (*vma).guard = guard as u32;
    }
    0
}

// upstream: i915_vma.c i915_vma_detach()
unsafe fn i915_vma_detach(vma: *mut I915Vma) {
    GEM_BUG_ON!(!unsafe { drm_mm_node_allocated(&(*vma).node) });
    GEM_BUG_ON!(unsafe { i915_vma_is_bound(vma, I915_VMA_BIND_MASK as u32) });
    let vm = unsafe { (*vma).vm };
    unsafe {
        list_move_tail(
            ptr::addr_of_mut!((*vma).vm_link),
            ptr::addr_of_mut!((*vm).unbound_list),
        )
    };
}

// upstream: i915_vma.c try_qad_pin()
unsafe fn try_qad_pin(vma: *mut I915Vma, mut flags: u32) -> bool {
    let mut bound = unsafe { atomic_read(&(*vma).flags) };
    if flags as u64 & PIN_VALIDATE != 0 {
        flags &= I915_VMA_BIND_MASK as u32;
        return (flags as i32 & bound) == flags as i32;
    }
    flags &= I915_VMA_BIND_MASK as u32;
    loop {
        if flags as i32 & !bound != 0 {
            return false;
        }
        if bound & (I915_VMA_OVERFLOW | I915_VMA_ERROR) != 0 {
            return false;
        }
        GEM_BUG_ON!(((bound + 1) & I915_VMA_PIN_MASK) == 0);
        let next = bound + 1;
        if unsafe { atomic_try_cmpxchg(ptr::addr_of_mut!((*vma).flags), &mut bound, next) } {
            return true;
        }
    }
}

// upstream: i915_vma.c rotate_pages()
unsafe fn rotate_pages(
    obj: *mut DrmI915GemObject,
    offset: u32,
    width: u16,
    height: u16,
    src_stride: u16,
    dst_stride: u16,
    st: *mut SgTable,
    mut sg: *mut Scatterlist,
) -> *mut Scatterlist {
    for column in 0..width as u32 {
        let mut src_idx = (src_stride as u32)
            .wrapping_mul(height as u32 - 1)
            .wrapping_add(column)
            .wrapping_add(offset);
        for _row in 0..height as u32 {
            unsafe {
                (*st).nents += 1;
                sg_set_dma_page(sg, I915_GTT_PAGE_SIZE as u32);
                set_sg_dma_address(sg, __i915_gem_object_get_dma_address(obj, src_idx as u64));
                set_sg_dma_len(sg, I915_GTT_PAGE_SIZE as u32);
                sg = page_link_next(sg);
            }
            src_idx = src_idx.wrapping_sub(src_stride as u32);
        }
        let left = (dst_stride as u32).wrapping_sub(height as u32) * I915_GTT_PAGE_SIZE as u32;
        if left == 0 {
            continue;
        }
        unsafe {
            (*st).nents += 1;
            sg_set_dma_page(sg, left);
            set_sg_dma_address(sg, 0);
            set_sg_dma_len(sg, left);
            sg = page_link_next(sg);
        }
    }
    sg
}

// upstream: i915_vma.c intel_rotate_pages()
unsafe fn intel_rotate_pages(
    rot_info: *mut IntelRotationInfo,
    obj: *mut DrmI915GemObject,
) -> *mut SgTable {
    let size = unsafe { intel_rotation_info_size(rot_info) };
    let i915 = unsafe { crate::linux::i915::to_i915((*obj.cast::<DrmGemObjectBaseLayout>()).dev) };
    let st = crate::linux::memory::kmalloc_obj::<SgTable>(GFP_KERNEL);
    if st.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    let ret = unsafe { sg_alloc_table(st, size, GFP_KERNEL) };
    if ret != 0 {
        unsafe { kfree(st) };
        return ERR_PTR(ret);
    }
    unsafe {
        (*st).nents = 0;
        let mut sg = (*st).sgl;
        for plane in 0..2 {
            let current = ptr::addr_of!((*rot_info).plane[plane]);
            let offset = unsafe { (*current).offset() };
            let width = unsafe { (*current).width() };
            let height = unsafe { (*current).height() };
            let src_stride = unsafe { (*current).src_stride() };
            let dst_stride = unsafe { (*current).dst_stride() };
            sg = rotate_pages(obj, offset, width, height, src_stride, dst_stride, st, sg);
        }
    }
    let _ = i915; // used by the source-only diagnostic below when logging is enabled
    st
}

// upstream: i915_vma.c add_padding_pages()
unsafe fn add_padding_pages(
    count: u32,
    st: *mut SgTable,
    sg: *mut Scatterlist,
) -> *mut Scatterlist {
    let len = count.wrapping_mul(I915_GTT_PAGE_SIZE as u32);
    unsafe {
        (*st).nents += 1;
        sg_set_dma_page(sg, len);
        set_sg_dma_address(sg, 0);
        set_sg_dma_len(sg, len);
        page_link_next(sg)
    }
}

// upstream: i915_vma.c remap_tiled_color_plane_pages()
unsafe fn remap_tiled_color_plane_pages(
    obj: *mut DrmI915GemObject,
    mut offset: u64,
    alignment_pad: u32,
    width: u16,
    height: u16,
    src_stride: u16,
    dst_stride: u16,
    st: *mut SgTable,
    mut sg: *mut Scatterlist,
    gtt_offset: *mut u32,
) -> *mut Scatterlist {
    if width == 0 || height == 0 {
        return sg;
    }
    if alignment_pad != 0 {
        sg = unsafe { add_padding_pages(alignment_pad, st, sg) };
    }
    for _row in 0..height {
        let mut left = width as u32 * I915_GTT_PAGE_SIZE as u32;
        while left != 0 {
            let mut length = 0u32;
            let address =
                unsafe { __i915_gem_object_get_dma_address_len(obj, offset, &mut length) };
            length = core::cmp::min(left, length);
            unsafe {
                (*st).nents += 1;
                sg_set_dma_page(sg, length);
                set_sg_dma_address(sg, address);
                set_sg_dma_len(sg, length);
                sg = page_link_next(sg);
            }
            let pages = length / I915_GTT_PAGE_SIZE as u32;
            offset = offset.wrapping_add(pages as u64);
            left -= length;
        }
        offset = offset.wrapping_add(src_stride.wrapping_sub(width) as u64);
        let padding = dst_stride.wrapping_sub(width) as u32;
        if padding != 0 {
            sg = unsafe { add_padding_pages(padding, st, sg) };
        }
    }
    unsafe {
        *gtt_offset = (*gtt_offset).wrapping_add(alignment_pad + dst_stride as u32 * height as u32)
    };
    sg
}

// upstream: i915_vma.c remap_contiguous_pages()
unsafe fn remap_contiguous_pages(
    obj: *mut DrmI915GemObject,
    mut obj_offset: u64,
    mut count: u32,
    st: *mut SgTable,
    mut sg: *mut Scatterlist,
) -> *mut Scatterlist {
    while count != 0 {
        let mut length = 0u32;
        let address =
            unsafe { __i915_gem_object_get_dma_address_len(obj, obj_offset, &mut length) };
        let requested = count.saturating_mul(I915_GTT_PAGE_SIZE as u32);
        length = core::cmp::min(length, requested);
        GEM_BUG_ON!(length == 0 || length % I915_GTT_PAGE_SIZE as u32 != 0);
        unsafe {
            sg_set_dma_page(sg, length);
            set_sg_dma_address(sg, address);
            set_sg_dma_len(sg, length);
            (*st).nents += 1;
        }
        let pages = length >> PAGE_SHIFT;
        count -= pages;
        obj_offset += pages as u64;
        if count == 0 {
            return sg;
        }
        sg = unsafe { page_link_next(sg) };
    }
    sg
}

// upstream: i915_vma.c remap_linear_color_plane_pages()
unsafe fn remap_linear_color_plane_pages(
    obj: *mut DrmI915GemObject,
    obj_offset: u64,
    alignment_pad: u32,
    size: u32,
    st: *mut SgTable,
    mut sg: *mut Scatterlist,
    gtt_offset: *mut u32,
) -> *mut Scatterlist {
    if size == 0 {
        return sg;
    }
    if alignment_pad != 0 {
        sg = unsafe { add_padding_pages(alignment_pad, st, sg) };
    }
    sg = unsafe { remap_contiguous_pages(obj, obj_offset, size, st, sg) };
    sg = unsafe { page_link_next(sg) };
    unsafe { *gtt_offset = (*gtt_offset).wrapping_add(alignment_pad + size) };
    sg
}

// upstream: i915_vma.c remap_color_plane_pages()
unsafe fn remap_color_plane_pages(
    rem_info: *const IntelRemappedInfo,
    obj: *mut DrmI915GemObject,
    color_plane: usize,
    st: *mut SgTable,
    mut sg: *mut Scatterlist,
    gtt_offset: *mut u32,
) -> *mut Scatterlist {
    let alignment = unsafe { ptr::read_unaligned(ptr::addr_of!((*rem_info).plane_alignment)) };
    let alignment_pad = if alignment == 0 {
        0
    } else {
        align_up(*gtt_offset as u64, alignment as u64) as u32 - *gtt_offset
    };
    let plane = unsafe { &*ptr::addr_of!((*rem_info).plane[color_plane]) };
    if plane.linear() {
        unsafe {
            remap_linear_color_plane_pages(
                obj,
                plane.offset() as u64,
                alignment_pad,
                plane.size(),
                st,
                sg,
                gtt_offset,
            )
        }
    } else {
        unsafe {
            remap_tiled_color_plane_pages(
                obj,
                plane.offset() as u64,
                alignment_pad,
                plane.width(),
                plane.height(),
                plane.src_stride(),
                plane.dst_stride(),
                st,
                sg,
                gtt_offset,
            )
        }
    }
}

// upstream: i915_vma.c intel_remap_pages()
unsafe fn intel_remap_pages(
    rem_info: *mut IntelRemappedInfo,
    obj: *mut DrmI915GemObject,
) -> *mut SgTable {
    let size = unsafe { intel_remapped_info_size(rem_info) };
    let i915 = unsafe { crate::linux::i915::to_i915((*obj.cast::<DrmGemObjectBaseLayout>()).dev) };
    let st = crate::linux::memory::kmalloc_obj::<SgTable>(GFP_KERNEL);
    if st.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    let ret = unsafe { sg_alloc_table(st, size, GFP_KERNEL) };
    if ret != 0 {
        unsafe { kfree(st) };
        return ERR_PTR(ret);
    }
    unsafe {
        (*st).nents = 0;
        let mut gtt_offset = 0;
        let mut sg = (*st).sgl;
        for plane in 0..4 {
            sg = remap_color_plane_pages(rem_info, obj, plane, st, sg, &mut gtt_offset);
        }
        i915_sg_trim(st);
    }
    let _ = i915;
    st
}

// upstream: i915_vma.c intel_partial_pages()
unsafe fn intel_partial_pages(
    view: *const I915GttView,
    obj: *mut DrmI915GemObject,
) -> *mut SgTable {
    let partial = unsafe { ptr::addr_of!((*view).info.partial) };
    let count = unsafe { ptr::read_unaligned(ptr::addr_of!((*partial).size)) };
    let offset = unsafe { ptr::read_unaligned(ptr::addr_of!((*partial).offset)) };
    let st = crate::linux::memory::kmalloc_obj::<SgTable>(GFP_KERNEL);
    if st.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    let ret = unsafe { sg_alloc_table(st, count, GFP_KERNEL) };
    if ret != 0 {
        unsafe { kfree(st) };
        return ERR_PTR(ret);
    }
    unsafe {
        (*st).nents = 0;
        let sg = remap_contiguous_pages(obj, offset, count, st, (*st).sgl);
        (*sg).page_link |= SG_END;
        i915_sg_trim(st);
    }
    st
}

// upstream: i915_vma.c __i915_vma_get_pages()
unsafe fn __i915_vma_get_pages(vma: *mut I915Vma) -> c_int {
    GEM_BUG_ON!(!unsafe { i915_gem_object_has_pinned_pages((*vma).obj) });
    let pages = match unsafe { (*vma).gtt_view.r#type } {
        I915_GTT_VIEW_NORMAL => unsafe { (*(*vma).obj).mm.pages },
        I915_GTT_VIEW_ROTATED => {
            let info = unsafe {
                ptr::addr_of_mut!((*vma).gtt_view.info)
                    .cast::<u8>()
                    .add(0)
                    .cast::<IntelRotationInfo>()
            };
            unsafe { intel_rotate_pages(info, (*vma).obj) }
        }
        I915_GTT_VIEW_REMAPPED => {
            let info = unsafe {
                ptr::addr_of_mut!((*vma).gtt_view.info)
                    .cast::<u8>()
                    .add(0)
                    .cast::<IntelRemappedInfo>()
            };
            unsafe { intel_remap_pages(info, (*vma).obj) }
        }
        I915_GTT_VIEW_PARTIAL => unsafe {
            intel_partial_pages(ptr::addr_of!((*vma).gtt_view), (*vma).obj)
        },
        _ => {
            GEM_BUG_ON!(true);
            ptr::null_mut()
        }
    };
    if IS_ERR(pages) {
        return PTR_ERR(pages);
    }
    unsafe { (*vma).pages = pages };
    0
}

// upstream: i915_vma.c i915_vma_get_pages()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_get_pages(vma: *mut I915Vma) -> c_int {
    if crate::linux::memory::atomic_add_unless(&mut unsafe { (*vma).pages_count }, 1, 0) {
        return 0;
    }
    let err = unsafe { i915_gem_object_pin_pages((*vma).obj) };
    if err != 0 {
        return err;
    }
    let err = unsafe { __i915_vma_get_pages(vma) };
    if err != 0 {
        unsafe { i915_gem_object_unpin_pages((*vma).obj) };
        return err;
    }
    unsafe {
        (*vma).page_sizes = (*(*vma).obj).mm.page_sizes;
        atomic_inc(&mut (*vma).pages_count);
    }
    0
}

// upstream: i915_vma.c vma_invalidate_tlb()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vma_invalidate_tlb(vm: *mut I915AddressSpace, tlb: *mut u32) {
    if tlb.is_null() {
        return;
    }
    let i915 = unsafe { (*vm).i915 };
    for gt in unsafe { (*i915).gt } {
        if gt.is_null() {
            continue;
        }
        let id = unsafe { (*gt).info.id as usize };
        unsafe { ptr::write_volatile(tlb.add(id), intel_gt_next_invalidate_tlb_full(gt)) };
    }
}

// upstream: i915_vma.c __vma_put_pages()
unsafe fn __vma_put_pages(vma: *mut I915Vma, count: u32) {
    GEM_BUG_ON!(unsafe { atomic_read(&(*vma).pages_count) < count as i32 });
    if unsafe { atomic_sub_return(ptr::addr_of_mut!((*vma).pages_count), count as i32) } == 0 {
        if unsafe { (*vma).pages != (*(*vma).obj).mm.pages } {
            unsafe {
                sg_free_table((*vma).pages);
                kfree((*vma).pages);
            }
        }
        unsafe {
            (*vma).pages = ptr::null_mut();
            i915_gem_object_unpin_pages((*vma).obj);
        }
    }
}

// upstream: i915_vma.c i915_vma_put_pages()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_put_pages(vma: *mut I915Vma) {
    if crate::linux::memory::atomic_add_unless(&mut unsafe { (*vma).pages_count }, -1, 1) {
        return;
    }
    unsafe { __vma_put_pages(vma, 1) };
}

// upstream: i915_vma.c vma_unbind_pages()
unsafe fn vma_unbind_pages(vma: *mut I915Vma) {
    let mut count = unsafe { atomic_read(&(*vma).pages_count) } as u32;
    count >>= I915_VMA_PAGES_BIAS;
    GEM_BUG_ON!(count == 0);
    unsafe { __vma_put_pages(vma, count | (count << I915_VMA_PAGES_BIAS)) };
}

// upstream: i915_vma.c i915_vma_pin_ww()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_pin_ww(
    vma: *mut I915Vma,
    ww: *mut crate::intel_context_upstream::I915GemWwCtx,
    size: u64,
    alignment: u64,
    flags: u64,
) -> c_int {
    unsafe { assert_object_held((*vma).obj) };
    GEM_BUG_ON!(ww.is_null());
    GEM_BUG_ON!(PIN_GLOBAL != I915_VMA_GLOBAL_BIND as u64);
    GEM_BUG_ON!(PIN_USER != I915_VMA_LOCAL_BIND as u64);
    GEM_BUG_ON!(flags & (PIN_USER | PIN_GLOBAL) == 0);

    if unsafe { try_qad_pin(vma, flags as u32) } {
        return 0;
    }
    let err = unsafe { i915_vma_get_pages(vma) };
    if err != 0 {
        return err;
    }

    let i915 = unsafe { (*(*vma).vm).i915 };
    let rpm = unsafe { ptr::addr_of_mut!((*i915).runtime_pm).cast::<IntelRuntimePm>() };
    let wakeref = unsafe { intel_runtime_pm_get(rpm) };
    let mut work: *mut I915VmaWork = ptr::null_mut();
    let mut moving: *mut DmaFence = ptr::null_mut();
    let mut vma_res: *mut I915VmaResource = ptr::null_mut();
    let mut bound: u32;
    let mut err;

    if flags & unsafe { (*(*vma).vm).bind_async_flags as u64 } != 0 {
        err = unsafe { i915_vm_lock_objects((*vma).vm, ww) };
        if err != 0 {
            unsafe {
                goto_pin_ww_cleanup(
                    vma,
                    rpm,
                    wakeref,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    err,
                    true,
                )
            };
            return err;
        }
        work = unsafe { i915_vma_work() }.cast::<I915VmaWork>();
        if work.is_null() {
            err = -ENOMEM;
            goto_pin_ww_cleanup(vma, rpm, wakeref, work, moving, vma_res, err, true);
            return err;
        }
        unsafe { (*work).vm = (*vma).vm };
        err = unsafe { i915_gem_object_get_moving_fence((*vma).obj, &mut moving) };
        if err != 0 {
            // The C source jumps directly to `err_rpm` here, bypassing the
            // `err_fence` work-commit label.
            unsafe { goto_pin_ww_cleanup(vma, rpm, wakeref, work, moving, vma_res, err, false) };
            return err;
        }
        unsafe { dma_fence_work_chain(ptr::addr_of_mut!((*work).base), moving) };
        if unsafe { (*(*vma).vm).allocate_va_range.is_some() } {
            err = unsafe {
                i915_vm_alloc_pt_stash((*vma).vm, ptr::addr_of_mut!((*work).stash), (*vma).size)
            };
            if err != 0 {
                unsafe { goto_pin_ww_cleanup(vma, rpm, wakeref, work, moving, vma_res, err, true) };
                return err;
            }
            err = unsafe { i915_vm_map_pt_stash((*vma).vm, ptr::addr_of_mut!((*work).stash)) };
            if err != 0 {
                unsafe { goto_pin_ww_cleanup(vma, rpm, wakeref, work, moving, vma_res, err, true) };
                return err;
            }
        }
    }

    vma_res = unsafe { i915_vma_resource_alloc() };
    if IS_ERR(vma_res) {
        err = PTR_ERR(vma_res);
        vma_res = ptr::null_mut();
        unsafe { goto_pin_ww_cleanup(vma, rpm, wakeref, work, moving, vma_res, err, true) };
        return err;
    }

    err = unsafe {
        mutex_lock_interruptible_nested(
            ptr::addr_of_mut!((*(*vma).vm).mutex),
            (flags & PIN_GLOBAL == 0) as u32,
        )
    };
    if err != 0 {
        unsafe { goto_pin_ww_cleanup(vma, rpm, wakeref, work, moving, vma_res, err, true) };
        return err;
    }

    if unsafe { i915_vma_is_closed(vma) } {
        err = -ENOENT;
        unsafe {
            goto_pin_locked_cleanup(
                vma, ww, work, moving, vma_res, rpm, wakeref, err, false, false,
            )
        };
        return err;
    }

    bound = unsafe { atomic_read(&(*vma).flags) } as u32;
    if bound & I915_VMA_ERROR as u32 != 0 {
        err = -ENOMEM;
        unsafe {
            goto_pin_locked_cleanup(
                vma, ww, work, moving, vma_res, rpm, wakeref, err, false, false,
            )
        };
        return err;
    }
    if bound.wrapping_add(1) & I915_VMA_PIN_MASK as u32 == 0 {
        err = -EAGAIN;
        unsafe {
            goto_pin_locked_cleanup(
                vma, ww, work, moving, vma_res, rpm, wakeref, err, false, false,
            )
        };
        return err;
    }
    if flags as u32 & !bound & I915_VMA_BIND_MASK as u32 == 0 {
        if flags & PIN_VALIDATE == 0 {
            unsafe { __i915_vma_pin(vma) };
        }
        err = 0;
        unsafe {
            goto_pin_locked_cleanup(
                vma, ww, work, moving, vma_res, rpm, wakeref, err, false, false,
            )
        };
        return err;
    }

    err = unsafe { i915_active_acquire(ptr::addr_of_mut!((*vma).active)) };
    if err != 0 {
        unsafe {
            goto_pin_locked_cleanup(
                vma, ww, work, moving, vma_res, rpm, wakeref, err, false, false,
            )
        };
        return err;
    }

    let mut acquired_active = true;
    if bound & I915_VMA_BIND_MASK as u32 == 0 {
        err = unsafe { i915_vma_insert(vma, ww.cast(), size, alignment, flags) };
        if err != 0 {
            unsafe {
                goto_pin_locked_cleanup(
                    vma,
                    ww,
                    work,
                    moving,
                    vma_res,
                    rpm,
                    wakeref,
                    err,
                    acquired_active,
                    false,
                )
            };
            return err;
        }
        if unsafe { i915_is_ggtt((*vma).vm) } {
            unsafe { __i915_vma_set_map_and_fenceable(vma) };
        }
    }

    GEM_BUG_ON!(unsafe { (*vma).pages.is_null() });
    err = unsafe {
        i915_vma_bind(
            vma,
            (*(*vma).obj).cache_state_bits & 0x3f,
            flags as u32,
            work.cast(),
            vma_res,
        )
    };
    vma_res = ptr::null_mut();
    if err != 0 {
        unsafe {
            goto_pin_locked_cleanup(
                vma,
                ww,
                work,
                moving,
                vma_res,
                rpm,
                wakeref,
                err,
                acquired_active,
                true,
            )
        };
        return err;
    }
    GEM_BUG_ON!(bound.wrapping_add(I915_VMA_PAGES_ACTIVE as u32) < bound);
    unsafe {
        atomic_add(I915_VMA_PAGES_ACTIVE as i32, &mut (*vma).pages_count);
        list_move_tail(
            ptr::addr_of_mut!((*vma).vm_link),
            ptr::addr_of_mut!((*(*vma).vm).bound_list),
        );
    }
    if flags & PIN_VALIDATE == 0 {
        unsafe { __i915_vma_pin(vma) };
        GEM_BUG_ON!(!unsafe { i915_vma_is_pinned(vma) });
    }
    GEM_BUG_ON!(!unsafe { i915_vma_is_bound(vma, flags as u32) });
    GEM_BUG_ON!(unsafe { i915_vma_misplaced(vma, size, alignment, flags) });

    err = 0;
    if !unsafe { i915_vma_is_bound(vma, I915_VMA_BIND_MASK as u32) } {
        unsafe {
            i915_vma_detach(vma);
            drm_mm_remove_node(ptr::addr_of_mut!((*vma).node));
        }
    }
    if acquired_active {
        unsafe { i915_active_release(ptr::addr_of_mut!((*vma).active)) };
    }
    unsafe { mutex_unlock(ptr::addr_of_mut!((*(*vma).vm).mutex)) };
    unsafe { i915_vma_resource_free(vma_res) };
    if !work.is_null() {
        if unsafe { i915_vma_is_ggtt(vma) && intel_vm_no_concurrent_access_wa(i915) } {
            unsafe { dma_fence_work_commit(ptr::addr_of_mut!((*work).base)) };
        } else {
            unsafe { dma_fence_work_commit_imm(ptr::addr_of_mut!((*work).base)) };
        }
    }
    unsafe { intel_runtime_pm_put_unchecked(rpm) };
    if !moving.is_null() {
        unsafe { dma_fence_put(moving) };
    }
    unsafe { i915_vma_put_pages(vma) };
    err
}

// Cleanup helper for early `i915_vma_pin_ww()` exits, before vm->mutex is held.
unsafe fn goto_pin_ww_cleanup(
    vma: *mut I915Vma,
    rpm: *mut IntelRuntimePm,
    _wakeref: IntelWakerefT,
    work: *mut I915VmaWork,
    moving: *mut DmaFence,
    vma_res: *mut I915VmaResource,
    err: c_int,
    commit_work: bool,
) {
    let _ = err;
    unsafe {
        i915_vma_resource_free(vma_res);
        if commit_work && !work.is_null() {
            if i915_vma_is_ggtt(vma) && intel_vm_no_concurrent_access_wa((*(*vma).vm).i915) {
                dma_fence_work_commit(ptr::addr_of_mut!((*work).base));
            } else {
                dma_fence_work_commit_imm(ptr::addr_of_mut!((*work).base));
            }
        }
        intel_runtime_pm_put_unchecked(rpm);
        if !moving.is_null() {
            dma_fence_put(moving);
        }
        i915_vma_put_pages(vma);
    }
}

// Cleanup helper for `i915_vma_pin_ww()` exits after vm->mutex is locked.
unsafe fn goto_pin_locked_cleanup(
    vma: *mut I915Vma,
    _ww: *mut crate::intel_context_upstream::I915GemWwCtx,
    work: *mut I915VmaWork,
    moving: *mut DmaFence,
    vma_res: *mut I915VmaResource,
    rpm: *mut IntelRuntimePm,
    wakeref: IntelWakerefT,
    err: c_int,
    active_held: bool,
    detach_on_error: bool,
) {
    let _ = wakeref;
    unsafe {
        if detach_on_error && !i915_vma_is_bound(vma, I915_VMA_BIND_MASK as u32) {
            i915_vma_detach(vma);
            drm_mm_remove_node(ptr::addr_of_mut!((*vma).node));
        }
        if active_held {
            i915_active_release(ptr::addr_of_mut!((*vma).active));
        }
        mutex_unlock(ptr::addr_of_mut!((*(*vma).vm).mutex));
        i915_vma_resource_free(vma_res);
        if !work.is_null() {
            if i915_vma_is_ggtt(vma) && intel_vm_no_concurrent_access_wa((*(*vma).vm).i915) {
                dma_fence_work_commit(ptr::addr_of_mut!((*work).base));
            } else {
                dma_fence_work_commit_imm(ptr::addr_of_mut!((*work).base));
            }
        }
        intel_runtime_pm_put_unchecked(rpm);
        if !moving.is_null() {
            dma_fence_put(moving);
        }
        i915_vma_put_pages(vma);
    }
    let _ = err;
}

// upstream: i915_vma.c i915_vma_pin()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_pin(
    vma: *mut I915Vma,
    size: u64,
    alignment: u64,
    flags: u64,
) -> c_int {
    let mut ww: crate::intel_context_upstream::I915GemWwCtx = unsafe { core::mem::zeroed() };
    unsafe { i915_gem_ww_ctx_init(&mut ww, true) };
    let mut err;
    loop {
        err = unsafe { i915_gem_object_lock((*vma).obj, &mut ww) };
        if err == 0 {
            err = unsafe { i915_vma_pin_ww(vma, &mut ww, size, alignment, flags) };
        }
        if err != -EDEADLK {
            break;
        }
        err = unsafe { i915_gem_ww_ctx_backoff(&mut ww) };
        if err != 0 {
            break;
        }
    }
    unsafe { i915_gem_ww_ctx_fini(&mut ww) };
    err
}

// upstream: i915_vma.c flush_idle_contexts()
unsafe fn flush_idle_contexts(gt: *mut IntelGt) {
    for engine in unsafe { (*gt).engine } {
        if !engine.is_null() {
            unsafe { intel_engine_flush_barriers(engine) };
        }
    }
    unsafe { intel_gt_wait_for_idle(gt, MAX_SCHEDULE_TIMEOUT as _) };
}

// upstream: i915_vma.c __i915_ggtt_pin()
unsafe fn __i915_ggtt_pin(
    vma: *mut I915Vma,
    ww: *mut crate::intel_context_upstream::I915GemWwCtx,
    align: u32,
    flags: u32,
) -> c_int {
    let vm = unsafe { (*vma).vm };
    let ggtt = unsafe { i915_vm_to_ggtt(vm) };
    loop {
        let mut err =
            unsafe { i915_vma_pin_ww(vma, ww, 0, align as u64, (flags as u64) | PIN_GLOBAL) };
        if err != -ENOSPC {
            if err == 0 {
                err = unsafe { i915_vma_wait_for_bind(vma) };
                if err != 0 {
                    unsafe { __i915_vma_unpin(vma) };
                }
            }
            return err;
        }
        let head = ptr::addr_of_mut!((*ggtt).gt_list);
        let mut link = unsafe { (*head).next };
        while link != head {
            let gt = unsafe {
                link.cast::<u8>()
                    .sub(offset_of!(IntelGt, ggtt_link))
                    .cast::<IntelGt>()
            };
            unsafe { flush_idle_contexts(gt) };
            link = unsafe { (*link).next };
        }
        if unsafe { mutex_lock_interruptible(ptr::addr_of_mut!((*vm).mutex)) } == 0 {
            unsafe { i915_gem_evict_vm(vm, ptr::null_mut(), ptr::null_mut()) };
            unsafe { mutex_unlock(ptr::addr_of_mut!((*vm).mutex)) };
        }
    }
}

// upstream: i915_vma.c i915_ggtt_pin()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_pin(
    vma: *mut I915Vma,
    ww: *mut crate::intel_context_upstream::I915GemWwCtx,
    align: u32,
    flags: u32,
) -> c_int {
    GEM_BUG_ON!(!unsafe { i915_vma_is_ggtt(vma) });
    if !ww.is_null() {
        return unsafe { __i915_ggtt_pin(vma, ww, align, flags) };
    }
    let mut local_ww: crate::intel_context_upstream::I915GemWwCtx = unsafe { core::mem::zeroed() };
    let mut err;
    unsafe { i915_gem_ww_ctx_init(&mut local_ww, true) };
    loop {
        err = unsafe { i915_gem_object_lock((*vma).obj, &mut local_ww) };
        if err == 0 {
            err = unsafe { __i915_ggtt_pin(vma, &mut local_ww, align, flags) };
        }
        if err != -EDEADLK {
            break;
        }
        err = unsafe { i915_gem_ww_ctx_backoff(&mut local_ww) };
        if err != 0 {
            break;
        }
    }
    unsafe { i915_gem_ww_ctx_fini(&mut local_ww) };
    err
}

// upstream: i915_vma.c i915_ggtt_clear_scanout()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_ggtt_clear_scanout(obj: *mut DrmI915GemObject) {
    let head = unsafe { ptr::addr_of_mut!((*obj).vma.list) };
    unsafe { spin_lock(ptr::addr_of_mut!((*obj).vma.lock)) };
    let mut link = unsafe { (*head).next };
    while link != head {
        let vma = unsafe {
            link.cast::<u8>()
                .sub(offset_of!(I915Vma, obj_link))
                .cast::<I915Vma>()
        };
        let next = unsafe { (*link).next };
        if unsafe { i915_vma_is_ggtt(vma) } {
            unsafe { i915_vma_clear_scanout(vma) };
            unsafe { (*vma).display_alignment = I915_GTT_MIN_ALIGNMENT as u32 };
        }
        link = next;
    }
    unsafe { spin_unlock(ptr::addr_of_mut!((*obj).vma.lock)) };
}

// upstream: i915_vma.c __vma_close()
unsafe fn __vma_close(vma: *mut I915Vma, gt: *mut IntelGt) {
    GEM_BUG_ON!(unsafe { i915_vma_is_closed(vma) });
    unsafe {
        list_add(
            ptr::addr_of_mut!((*vma).closed_link),
            ptr::addr_of_mut!((*gt).closed_vma),
        )
    };
}

// upstream: i915_vma.c i915_vma_close()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_close(vma: *mut I915Vma) {
    if unsafe { i915_vma_is_ggtt(vma) } {
        return;
    }
    let gt = unsafe { (*(*vma).vm).gt };
    GEM_BUG_ON!(atomic_read(unsafe { &(*vma).open_count }) == 0);
    let mut irq_flags = 0u64;
    if unsafe {
        atomic_dec_and_lock_irqsave(
            ptr::addr_of_mut!((*vma).open_count),
            ptr::addr_of_mut!((*gt).closed_lock),
            ptr::addr_of_mut!(irq_flags),
        )
    } {
        unsafe { __vma_close(vma, gt) };
        unsafe { spin_unlock_irqrestore(ptr::addr_of_mut!((*gt).closed_lock), irq_flags) };
    }
}

// upstream: i915_vma.c __i915_vma_remove_closed()
unsafe fn __i915_vma_remove_closed(vma: *mut I915Vma) {
    unsafe { list_del_init(ptr::addr_of_mut!((*vma).closed_link)) };
}

// upstream: i915_vma.c i915_vma_reopen()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_reopen(vma: *mut I915Vma) {
    let gt = unsafe { (*(*vma).vm).gt };
    unsafe { spin_lock_irq(ptr::addr_of_mut!((*gt).closed_lock)) };
    if unsafe { i915_vma_is_closed(vma) } {
        unsafe { __i915_vma_remove_closed(vma) };
    }
    unsafe { spin_unlock_irq(ptr::addr_of_mut!((*gt).closed_lock)) };
}

// upstream: i915_vma.c force_unbind()
unsafe fn force_unbind(vma: *mut I915Vma) {
    if !unsafe { drm_mm_node_allocated(ptr::addr_of!((*vma).node)) } {
        return;
    }
    unsafe { atomic_and(ptr::addr_of_mut!((*vma).flags), !I915_VMA_PIN_MASK) };
    WARN_ON!(unsafe { __i915_vma_unbind(vma) } != 0);
    GEM_BUG_ON!(unsafe { drm_mm_node_allocated(ptr::addr_of!((*vma).node)) });
}

// upstream: i915_vma.c release_references()
unsafe fn release_references(vma: *mut I915Vma, gt: *mut IntelGt, vm_ddestroy: bool) {
    let obj = unsafe { (*vma).obj };
    GEM_BUG_ON!(unsafe { i915_vma_is_active(vma) });

    unsafe { spin_lock(ptr::addr_of_mut!((*obj).vma.lock)) };
    unsafe { list_del(ptr::addr_of_mut!((*vma).obj_link)) };
    let obj_node = ptr::addr_of_mut!((*vma).obj_node);
    if !unsafe { rb_empty_node(obj_node) } {
        unsafe {
            rb_erase(obj_node, &mut (*obj).vma.tree);
            rb_clear_node(obj_node);
        }
    }
    unsafe { spin_unlock(ptr::addr_of_mut!((*obj).vma.lock)) };

    unsafe { spin_lock_irq(ptr::addr_of_mut!((*gt).closed_lock)) };
    unsafe { __i915_vma_remove_closed(vma) };
    unsafe { spin_unlock_irq(ptr::addr_of_mut!((*gt).closed_lock)) };

    if vm_ddestroy {
        unsafe { i915_vm_resv_put((*vma).vm) };
    }
    unsafe { i915_active_fini(ptr::addr_of_mut!((*vma).active)) };
    GEM_WARN_ON!(!unsafe { (*vma).resource }.is_null());
    unsafe { i915_vma_free(vma) };
}

// upstream: i915_vma.c i915_vma_destroy_locked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_destroy_locked(vma: *mut I915Vma) {
    lockdep_assert_held!(unsafe { &(*(*vma).vm).mutex });
    unsafe { force_unbind(vma) };
    unsafe { list_del_init(ptr::addr_of_mut!((*vma).vm_link)) };
    let gt = unsafe { (*(*vma).vm).gt };
    unsafe { release_references(vma, gt, false) };
}

// upstream: i915_vma.c i915_vma_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_destroy(vma: *mut I915Vma) {
    let vm = unsafe { (*vma).vm };
    unsafe { mutex_lock(ptr::addr_of_mut!((*vm).mutex)) };
    unsafe { force_unbind(vma) };
    unsafe { list_del_init(ptr::addr_of_mut!((*vma).vm_link)) };
    let vm_ddestroy = unsafe { (*vma).vm_ddestroy };
    unsafe { (*vma).vm_ddestroy = false };
    let gt = unsafe { (*vm).gt };
    unsafe { mutex_unlock(ptr::addr_of_mut!((*vm).mutex)) };
    unsafe { release_references(vma, gt, vm_ddestroy) };
}

// upstream: i915_vma.c i915_vma_parked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_parked(gt: *mut IntelGt) {
    let mut closed = crate::intel_engine_cs_upstream::ListHead {
        next: ptr::null_mut(),
        prev: ptr::null_mut(),
    };
    let closed_head = ptr::addr_of_mut!(closed);
    unsafe {
        (*closed_head).next = closed_head;
        (*closed_head).prev = closed_head;
        spin_lock_irq(ptr::addr_of_mut!((*gt).closed_lock));
    }
    let head = unsafe { ptr::addr_of_mut!((*gt).closed_vma) };
    let mut link = unsafe { (*head).next };
    while link != head {
        let vma = unsafe {
            link.cast::<u8>()
                .sub(offset_of!(I915Vma, closed_link))
                .cast::<I915Vma>()
        };
        let next = unsafe { (*link).next };
        let obj = unsafe { (*vma).obj };
        let vm = unsafe { (*vma).vm };
        let base = obj.cast::<DrmGemObjectBaseLayout>();
        if unsafe { kref_get_unless_zero(&mut (*base).refcount) } {
            if !unsafe { i915_vm_tryget(vm) }.is_null() {
                unsafe { list_move(ptr::addr_of_mut!((*vma).closed_link), closed_head) };
            } else {
                unsafe { i915_gem_object_put(obj) };
            }
        }
        link = next;
    }
    unsafe { spin_unlock_irq(ptr::addr_of_mut!((*gt).closed_lock)) };

    let mut link = unsafe { (*closed_head).next };
    while link != closed_head {
        let vma = unsafe {
            link.cast::<u8>()
                .sub(offset_of!(I915Vma, closed_link))
                .cast::<I915Vma>()
        };
        let next = unsafe { (*link).next };
        let obj = unsafe { (*vma).obj };
        let vm = unsafe { (*vma).vm };
        if unsafe { i915_gem_object_trylock(obj, ptr::null_mut()) } {
            unsafe {
                INIT_LIST_HEAD!(ptr::addr_of_mut!((*vma).closed_link));
                i915_vma_destroy(vma);
                i915_gem_object_unlock(obj);
            }
        } else {
            unsafe {
                spin_lock_irq(ptr::addr_of_mut!((*gt).closed_lock));
                list_add(
                    ptr::addr_of_mut!((*vma).closed_link),
                    ptr::addr_of_mut!((*gt).closed_vma),
                );
                spin_unlock_irq(ptr::addr_of_mut!((*gt).closed_lock));
            }
        }
        unsafe {
            i915_gem_object_put(obj);
            i915_vm_put(vm);
        }
        link = next;
    }
}

// upstream: i915_vma.c __i915_vma_iounmap()
unsafe fn __i915_vma_iounmap(vma: *mut I915Vma) {
    GEM_BUG_ON!(unsafe { i915_vma_is_pinned(vma) });
    let iomap = unsafe { (*vma).iomap };
    if iomap.is_null() {
        return;
    }
    if !unsafe { page_unmask_bits(iomap) }.is_null() {
        unsafe { __i915_gem_object_release_map((*vma).obj) };
    } else {
        unsafe { io_mapping_unmap(iomap) };
    }
    unsafe { (*vma).iomap = ptr::null_mut() };
}

// upstream: i915_vma.c i915_vma_revoke_mmap()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_revoke_mmap(vma: *mut I915Vma) {
    if !unsafe { i915_vma_has_userfault(vma) } {
        return;
    }
    GEM_BUG_ON!(!unsafe { i915_vma_is_map_and_fenceable(vma) });
    let obj = unsafe { (*vma).obj };
    GEM_BUG_ON!(unsafe { (*obj).userfault_count == 0 });
    let node = unsafe { ptr::addr_of_mut!((*(*vma).mmo).vma_node) };
    let partial = unsafe { ptr::addr_of!((*vma).gtt_view.info.partial) };
    let partial_offset = unsafe { ptr::read_unaligned(ptr::addr_of!((*partial).offset)) };
    let vma_offset = (partial_offset as u64) << PAGE_SHIFT;
    let i915 = unsafe { (*(*vma).vm).i915 };
    let inode = unsafe { (*i915).drm.anon_inode };
    unsafe {
        unmap_mapping_range(
            (*inode).i_mapping,
            drm_vma_node_offset_addr(node) + vma_offset,
            (*vma).size,
            1,
        )
    };
    unsafe { i915_vma_unset_userfault(vma) };
    let count = unsafe { (*obj).userfault_count } - 1;
    unsafe { (*obj).userfault_count = count };
    if count == 0 {
        unsafe { list_del(ptr::addr_of_mut!((*obj).userfault_link)) };
    }
}

// upstream: i915_vma.c __i915_request_await_bind()
unsafe fn __i915_request_await_bind(rq: *mut I915Request, vma: *mut I915Vma) -> c_int {
    unsafe { i915_request_await_exclusive(rq, ptr::addr_of_mut!((*vma).active)) }
}

// upstream: i915_vma.c __i915_vma_move_to_active()
unsafe fn __i915_vma_move_to_active(vma: *mut I915Vma, rq: *mut I915Request) -> c_int {
    let err = unsafe { __i915_request_await_bind(rq, vma) };
    if err != 0 {
        return err;
    }
    unsafe { i915_active_add_request(ptr::addr_of_mut!((*vma).active), rq) }
}

// upstream: i915_vma.c _i915_vma_move_to_active()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _i915_vma_move_to_active(
    vma: *mut I915Vma,
    rq: *mut I915Request,
    fence: *mut DmaFence,
    flags: u32,
) -> c_int {
    let obj = unsafe { (*vma).obj };
    unsafe { assert_object_held(obj) };
    GEM_BUG_ON!(unsafe { (*vma).pages.is_null() });

    if flags & (1 << 30) == 0 {
        let err = unsafe { i915_request_await_object(rq, obj, (flags & EXEC_OBJECT_WRITE) != 0) };
        if err != 0 {
            return err;
        }
    }
    let err = unsafe { __i915_vma_move_to_active(vma, rq) };
    if err != 0 {
        return err;
    }

    let resv = unsafe { dma_resv_of_object(obj) };
    if !fence.is_null() && flags & (1 << 31) == 0 {
        let count =
            if unsafe { (*fence).ops == ptr::addr_of!(dma_fence_array_ops).cast::<c_void>() } {
                unsafe { (*fence.cast::<DmaFenceArrayView>()).num_fences }
            } else {
                1
            };
        let err = unsafe { dma_resv_reserve_fences(resv, count) };
        if err != 0 {
            return err;
        }
    }

    if flags & EXEC_OBJECT_WRITE != 0 {
        let front = unsafe { frontbuffer_lookup(obj) };
        if !front.is_null() {
            if unsafe { intel_frontbuffer_invalidate(front, ORIGIN_CS) } {
                let front_view = front.cast::<I915FrontbufferView>();
                unsafe { i915_active_add_request(ptr::addr_of_mut!((*front_view).write), rq) };
            }
            unsafe { i915_gem_object_frontbuffer_put(front) };
        }
    }

    if !fence.is_null() {
        let usage = if flags & EXEC_OBJECT_WRITE != 0 {
            unsafe {
                (*obj).write_domain = I915_GEM_DOMAIN_RENDER;
                (*obj).read_domains = 0;
            }
            DMA_RESV_USAGE_READ
        } else {
            unsafe { (*obj).write_domain = 0 };
            DMA_RESV_USAGE_WRITE
        };
        if unsafe { (*fence).ops == ptr::addr_of!(dma_fence_array_ops).cast::<c_void>() } {
            let array = fence.cast::<DmaFenceArrayView>();
            let mut index = 0;
            while index < unsafe { (*array).num_fences } {
                let current = unsafe { *(*array).fences.add(index as usize) };
                unsafe { dma_resv_add_fence(resv, current, usage) };
                index += 1;
            }
        } else {
            unsafe { dma_resv_add_fence(resv, fence, usage) };
        }
    }

    if flags & EXEC_OBJECT_NEEDS_FENCE != 0 && !unsafe { (*vma).fence }.is_null() {
        unsafe { i915_active_add_request(&mut (*(*vma).fence).active, rq) };
    }
    unsafe {
        (*obj).read_domains |= I915_GEM_GPU_DOMAINS;
        (*obj).mm.madv_dirty_bits |= 1 << 2;
    }
    GEM_BUG_ON!(!unsafe { i915_vma_is_active(vma) });
    0
}

// upstream: i915_vma.c __i915_vma_evict()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_vma_evict(vma: *mut I915Vma, async_: bool) -> *mut DmaFence {
    let vma_res = unsafe { (*vma).resource };
    GEM_BUG_ON!(unsafe { i915_vma_is_pinned(vma) });
    unsafe { assert_vma_held_evict(vma) };

    if unsafe { i915_vma_is_map_and_fenceable(vma) } {
        unsafe { i915_vma_revoke_mmap(vma) };
        unsafe { i915_vma_flush_writes(vma) };
        unsafe { i915_vma_revoke_fence(vma) };
        unsafe {
            atomic_and(
                ptr::addr_of_mut!((*vma).flags),
                !(1 << I915_VMA_CAN_FENCE_BIT),
            )
        };
    }
    unsafe { __i915_vma_iounmap(vma) };
    GEM_BUG_ON!(!unsafe { (*vma).fence }.is_null());
    GEM_BUG_ON!(unsafe { i915_vma_has_userfault(vma) });
    GEM_WARN_ON!(async_ && unsafe { (*vma_res).bi.pages_rsgt.is_null() });

    let vm = unsafe { (*vma).vm };
    let vm_open = unsafe { crate::linux::memory::kref_read(&(*vm).r#ref) != 0 };
    let needs_wakeref = unsafe { i915_vma_is_bound(vma, I915_VMA_GLOBAL_BIND as u32) } && vm_open;
    let skip_pte_rewrite = !vm_open || unsafe { (*vm).vm_flags & (1 << 3) != 0 };
    unsafe {
        let state = ptr::addr_of_mut!((*vma_res).state_bits);
        if needs_wakeref {
            *state |= I915_VMA_RESOURCE_NEEDS_WAKEREF_BIT;
        } else {
            *state &= !I915_VMA_RESOURCE_NEEDS_WAKEREF_BIT;
        }
        if skip_pte_rewrite {
            *state |= I915_VMA_RESOURCE_SKIP_PTE_REWRITE_BIT;
        } else {
            *state &= !I915_VMA_RESOURCE_SKIP_PTE_REWRITE_BIT;
        }
    }
    unsafe { trace_i915_vma_unbind(vma) };
    let fence = unsafe {
        if async_ {
            i915_vma_resource_unbind(
                vma_res,
                ptr::addr_of_mut!((*(*vma).obj).mm.tlb).cast::<u32>(),
            )
        } else {
            i915_vma_resource_unbind(vma_res, ptr::null_mut())
        }
    };
    unsafe { (*vma).resource = ptr::null_mut() };
    unsafe {
        atomic_and(
            ptr::addr_of_mut!((*vma).flags),
            !(I915_VMA_BIND_MASK | I915_VMA_ERROR | I915_VMA_GGTT_WRITE),
        );
        i915_vma_detach(vma);
    }

    let mut unbind_fence = fence;
    if !async_ {
        if !unbind_fence.is_null() {
            unsafe {
                dma_fence_wait(unbind_fence, false);
                dma_fence_put(unbind_fence);
            }
            unbind_fence = ptr::null_mut();
        }
        unsafe { vma_invalidate_tlb(vm, ptr::addr_of_mut!((*(*vma).obj).mm.tlb).cast::<u32>()) };
    }
    unsafe { vma_unbind_pages(vma) };
    unbind_fence
}

// upstream: i915_vma.c __i915_vma_unbind()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_vma_unbind(vma: *mut I915Vma) -> c_int {
    lockdep_assert_held!(unsafe { &(*(*vma).vm).mutex });
    unsafe { assert_vma_held_evict(vma) };
    if !unsafe { drm_mm_node_allocated(ptr::addr_of!((*vma).node)) } {
        return 0;
    }
    if unsafe { i915_vma_is_pinned(vma) } {
        unsafe { vma_print_allocator(vma, c"is pinned".as_ptr()) };
        return -EAGAIN;
    }
    let err = unsafe { i915_vma_sync(vma) };
    if err != 0 {
        return err;
    }
    GEM_BUG_ON!(unsafe { i915_vma_is_active(vma) });
    unsafe { __i915_vma_evict(vma, false) };
    unsafe { drm_mm_remove_node(ptr::addr_of_mut!((*vma).node)) };
    0
}

// upstream: i915_vma.c __i915_vma_unbind_async()
unsafe fn __i915_vma_unbind_async(vma: *mut I915Vma) -> *mut DmaFence {
    lockdep_assert_held!(unsafe { &(*(*vma).vm).mutex });
    if !unsafe { drm_mm_node_allocated(ptr::addr_of!((*vma).node)) } {
        return ptr::null_mut();
    }
    let obj = unsafe { (*vma).obj };
    let rsgt = unsafe { (*obj).mm.rsgt };
    let vma_res = unsafe { (*vma).resource };
    if unsafe { i915_vma_is_pinned(vma) }
        || unsafe { oa_refct_sgt_table(rsgt) != (*vma_res).bi.pages }
    {
        return ERR_PTR(-EAGAIN);
    }
    let err = unsafe {
        i915_sw_fence_await_active(
            ptr::addr_of_mut!((*vma_res).chain),
            ptr::addr_of_mut!((*vma).active),
            I915_ACTIVE_AWAIT_EXCL | I915_ACTIVE_AWAIT_ACTIVE,
        )
    };
    if err < 0 {
        return ERR_PTR(-EBUSY);
    }

    let fence = unsafe { __i915_vma_evict(vma, true) };
    unsafe { drm_mm_remove_node(ptr::addr_of_mut!((*vma).node)) };
    fence
}

// upstream: i915_vma.c i915_vma_unbind()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_unbind(vma: *mut I915Vma) -> c_int {
    let vm = unsafe { (*vma).vm };
    let obj = unsafe { (*vma).obj };
    let mut wakeref: IntelWakerefT = ptr::null_mut();
    unsafe { assert_object_held_shared(obj) };

    let mut err = unsafe { i915_vma_sync(vma) };
    if err != 0 {
        return err;
    }
    if !unsafe { drm_mm_node_allocated(ptr::addr_of!((*vma).node)) } {
        return 0;
    }
    if unsafe { i915_vma_is_pinned(vma) } {
        unsafe { vma_print_allocator(vma, c"is pinned".as_ptr()) };
        return -EAGAIN;
    }
    if unsafe { i915_vma_is_bound(vma, I915_VMA_GLOBAL_BIND as u32) } {
        let rpm = unsafe { ptr::addr_of_mut!((*(*vm).i915).runtime_pm).cast::<IntelRuntimePm>() };
        wakeref = unsafe { intel_runtime_pm_get(rpm) };
    }

    err = unsafe {
        mutex_lock_interruptible_nested(ptr::addr_of_mut!((*vm).mutex), wakeref.is_null() as u32)
    };
    if err != 0 {
        if !wakeref.is_null() {
            let rpm =
                unsafe { ptr::addr_of_mut!((*(*vm).i915).runtime_pm).cast::<IntelRuntimePm>() };
            unsafe { intel_runtime_pm_put_unchecked(rpm) };
        }
        return err;
    }
    err = unsafe { __i915_vma_unbind(vma) };
    unsafe { mutex_unlock(ptr::addr_of_mut!((*vm).mutex)) };
    if !wakeref.is_null() {
        let rpm = unsafe { ptr::addr_of_mut!((*(*vm).i915).runtime_pm).cast::<IntelRuntimePm>() };
        unsafe { intel_runtime_pm_put_unchecked(rpm) };
    }
    err
}

// upstream: i915_vma.c i915_vma_unbind_async()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_unbind_async(vma: *mut I915Vma, trylock_vm: bool) -> c_int {
    let obj = unsafe { (*vma).obj };
    let vm = unsafe { (*vma).vm };
    let mut wakeref: IntelWakerefT = ptr::null_mut();
    let mut err = 0;
    unsafe { assert_object_held(obj) };

    if !unsafe { drm_mm_node_allocated(ptr::addr_of!((*vma).node)) } {
        return 0;
    }
    if unsafe { i915_vma_is_pinned(vma) } {
        unsafe { vma_print_allocator(vma, c"is pinned".as_ptr()) };
        return -EAGAIN;
    }
    if unsafe { (*obj).mm.rsgt.is_null() } {
        return -EBUSY;
    }
    let reserve_err = unsafe { dma_resv_reserve_fences(dma_resv_of_object(obj), 2) };
    if reserve_err != 0 {
        return -EBUSY;
    }

    if unsafe { i915_vma_is_bound(vma, I915_VMA_GLOBAL_BIND as u32) } {
        let rpm = unsafe { ptr::addr_of_mut!((*(*vm).i915).runtime_pm).cast::<IntelRuntimePm>() };
        wakeref = unsafe { intel_runtime_pm_get(rpm) };
    }
    if trylock_vm {
        if !unsafe { mutex_trylock(ptr::addr_of_mut!((*vm).mutex)) } {
            err = -EBUSY;
            if !wakeref.is_null() {
                let rpm =
                    unsafe { ptr::addr_of_mut!((*(*vm).i915).runtime_pm).cast::<IntelRuntimePm>() };
                unsafe { intel_runtime_pm_put_unchecked(rpm) };
            }
            return err;
        }
    } else {
        err = unsafe {
            mutex_lock_interruptible_nested(
                ptr::addr_of_mut!((*vm).mutex),
                wakeref.is_null() as u32,
            )
        };
        if err != 0 {
            if !wakeref.is_null() {
                let rpm =
                    unsafe { ptr::addr_of_mut!((*(*vm).i915).runtime_pm).cast::<IntelRuntimePm>() };
                unsafe { intel_runtime_pm_put_unchecked(rpm) };
            }
            return err;
        }
    }

    let fence = unsafe { __i915_vma_unbind_async(vma) };
    unsafe { mutex_unlock(ptr::addr_of_mut!((*vm).mutex)) };
    if fence.is_null() || unsafe { IS_ERR(fence) } {
        err = PTR_ERR_OR_ZERO(fence);
    } else {
        unsafe {
            dma_resv_add_fence(dma_resv_of_object(obj), fence, DMA_RESV_USAGE_READ);
            dma_fence_put(fence);
        }
    }
    if !wakeref.is_null() {
        let rpm = unsafe { ptr::addr_of_mut!((*(*vm).i915).runtime_pm).cast::<IntelRuntimePm>() };
        unsafe { intel_runtime_pm_put_unchecked(rpm) };
    }
    err
}

// upstream: i915_vma.c i915_vma_unbind_unlocked()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_unbind_unlocked(vma: *mut I915Vma) -> c_int {
    unsafe { i915_gem_object_lock((*vma).obj, ptr::null_mut()) };
    let err = unsafe { i915_vma_unbind(vma) };
    unsafe { i915_gem_object_unlock((*vma).obj) };
    err
}

// upstream: i915_vma.c i915_vma_make_unshrinkable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_make_unshrinkable(vma: *mut I915Vma) -> *mut I915Vma {
    unsafe { i915_gem_object_make_unshrinkable((*vma).obj) };
    vma
}

// upstream: i915_vma.c i915_vma_make_shrinkable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_make_shrinkable(vma: *mut I915Vma) {
    unsafe { i915_gem_object_make_shrinkable((*vma).obj) };
}

// upstream: i915_vma.c i915_vma_make_purgeable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_make_purgeable(vma: *mut I915Vma) {
    unsafe { i915_gem_object_make_purgeable((*vma).obj) };
}

// upstream: i915_vma.c i915_vma_module_exit()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_module_exit() {
    let cache = SLAB_VMAS.swap(ptr::null_mut(), Ordering::AcqRel);
    assert!(!cache.is_null());
    unsafe { crate::linux_heap::kmem_cache_destroy(cache) };
}

// upstream: i915_vma.c i915_vma_module_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_vma_module_init() -> c_int {
    let cache = unsafe {
        crate::linux_heap::kmem_cache_create::<I915Vma>(crate::linux_heap::SLAB_HWCACHE_ALIGN)
    };
    if cache.is_null() {
        return -ENOMEM;
    }
    SLAB_VMAS.store(cache, Ordering::Release);
    0
}
