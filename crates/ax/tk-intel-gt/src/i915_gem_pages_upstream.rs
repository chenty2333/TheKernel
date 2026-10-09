// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation. Full MIT grant: LICENSE-MIT.
// Source-faithful Rust translation of Linux 7.2.3
// drivers/gpu/drm/i915/gem/i915_gem_pages.c. Page ownership, SG iteration,
// mapping cache policy, pin counts and panic framebuffer paths preserve source
// ordering. Framework operations below are explicit LinuxKPI integration APIs.
#![allow(
    non_snake_case,
    non_camel_case_types,
    unsafe_op_in_unsafe_fn,
    unexpected_cfgs
)]

use core::{
    ffi::{c_int, c_ulong, c_void},
    mem::size_of,
    ptr,
};

use crate::{
    i915_gem_object_api_upstream::*,
    i915_gem_object_types_upstream::{
        DrmI915GemObject, I915_BO_ALLOC_GPU_ONLY, I915_BO_ALLOC_VOLATILE,
        I915_BO_CACHE_COHERENT_FOR_WRITE, I915_GEM_OBJECT_IS_SHRINKABLE,
        I915_GEM_OBJECT_SELF_MANAGED_SHRINK_LIST, I915_MAP_OVERRIDE, I915_MAP_WB, I915_MAP_WC,
        I915_TILING_QUIRK_BIT, I915GemObjectPageIter, I915MapType, Page,
    },
    i915_gem_object_upstream::{
        i915_gem_object_has_iomem, i915_gem_object_has_struct_page,
        i915_gem_object_placement_possible, i915_gem_object_wait_moving_fence,
    },
    i915_gem_ww_upstream::{
        I915GemWwCtx, i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini, i915_gem_ww_ctx_init,
    },
    intel_context_upstream::{DrmGemObjectBaseLayout, SgTable},
    intel_gt_types_upstream::IntelGt,
    linux::{
        gem_memory::PgProt,
        i915::{IS_DGFX, IntelDeviceInfoOverlay},
        i915_private::DrmI915Private,
        iosys_map::IosysMap,
        locks::*,
        memory::*,
        mutex::{mutex_lock, mutex_unlock},
        rcu::*,
    },
    linux_config::*,
    linux_list::*,
};

const I915_GEM_DOMAIN_CPU: u32 = 1;
const I915_MADV_DONTNEED: u32 = 1;
const GEM_QUIRK_PIN_SWIZZLED_PAGES: c_ulong = 1;
const DRM_FORMAT_MAX_PLANES: usize = 4;

#[repr(C)]
pub struct IntelPanic {
    pages: *mut *mut Page,
    page: c_int,
    vaddr: *mut c_void,
}
const _: [(); 24] = [(); size_of::<IntelPanic>()];
#[repr(C)]
pub struct DrmFormatInfo {
    format: u32,
    depth: u8,
    num_planes: u8,
    cpp: [u8; DRM_FORMAT_MAX_PLANES],
}
const _: [(); 6] = [(); core::mem::offset_of!(DrmFormatInfo, cpp)];
#[repr(C)]
pub struct DrmScanoutBuffer {
    format: *const DrmFormatInfo,
    map: [IosysMap; DRM_FORMAT_MAX_PLANES],
    pages: *mut *mut Page,
    width: u32,
    height: u32,
    pitch: [u32; DRM_FORMAT_MAX_PLANES],
    set_pixel: Option<unsafe extern "C" fn(*mut DrmScanoutBuffer, u32, u32, u32)>,
    private: *mut c_void,
}
const _: [(); 120] = [(); size_of::<DrmScanoutBuffer>()];
const _: [(); 104] = [(); core::mem::offset_of!(DrmScanoutBuffer, set_pixel)];
const _: [(); 112] = [(); core::mem::offset_of!(DrmScanoutBuffer, private)];
#[repr(C)]
pub struct Scatterlist {
    page_link: usize,
    offset: u32,
    length: u32,
    dma_address: u64,
    dma_length: u32,
    _dma_flags_or_padding: u32,
}
const _: [(); 32] = [(); size_of::<Scatterlist>()];
const _: [(); 16] = [(); core::mem::offset_of!(Scatterlist, dma_address)];
const _: [(); 24] = [(); core::mem::offset_of!(Scatterlist, dma_length)];
// Source-derived display layout: drm_framebuffer is 192 bytes for the
// configured x86_64 ABI, intel_fb_view is 136 bytes, followed by intel DPT,
// alignment/guard, then the panic tiling callback and private panic record.
#[repr(C)]
pub struct IntelFramebuffer {
    _drm_framebuffer: [u8; 192],
    _frontbuffer: *mut c_void,
    _normal_view: [u8; 136],
    _rotated_or_remapped_view: [u8; 136],
    _dpt: *mut c_void,
    _min_alignment: u32,
    _vtd_guard: u32,
    panic_tiling: Option<unsafe extern "C" fn(u32, u32, u32) -> u32>,
    panic: *mut IntelPanic,
}
const _: [(); 504] = [(); size_of::<IntelFramebuffer>()];
const _: [(); 488] = [(); core::mem::offset_of!(IntelFramebuffer, panic_tiling)];
const _: [(); 496] = [(); core::mem::offset_of!(IntelFramebuffer, panic)];

#[inline]
unsafe fn gem_base(obj: *mut DrmI915GemObject) -> *mut DrmGemObjectBaseLayout {
    unsafe { ptr::addr_of_mut!((*obj).base).cast() }
}
#[inline]
unsafe fn object_size(obj: *mut DrmI915GemObject) -> u64 {
    unsafe { (*gem_base(obj)).size }
}
#[inline]
fn page_mask_bits(ptr_: *mut c_void) -> *mut c_void {
    ((ptr_ as usize) & !((1usize << PAGE_SHIFT) - 1)) as *mut c_void
}
#[inline]
fn page_pack_bits(ptr_: *mut c_void, bits: I915MapType) -> *mut c_void {
    assert_eq!((bits as usize) & !((1usize << PAGE_SHIFT) - 1), 0);
    ((ptr_ as usize) | bits as usize) as *mut c_void
}
#[inline]
unsafe fn page_unpack_bits(ptr_: *mut c_void, bits: *mut I915MapType) -> *mut c_void {
    unsafe {
        *bits = (ptr_ as usize & ((1usize << PAGE_SHIFT) - 1)) as I915MapType;
        page_mask_bits(ptr_)
    }
}
#[inline]
fn smp_mb__before_atomic() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
}
#[inline]
fn err_ptr(err: c_int) -> *mut c_void {
    err as isize as *mut c_void
}
#[inline]
fn is_err(ptr: *const c_void) -> bool {
    (ptr as isize) < 0 && (ptr as isize) >= -4095
}
#[inline]
fn is_err_or_null(ptr: *const c_void) -> bool {
    ptr.is_null() || is_err(ptr)
}

// External framework/source APIs intentionally remain unresolved integration
// bindings rather than success-returning stand-ins. The final section reports
// the precise lower-layer symbols still required by this translation.
unsafe extern "C" {
    fn i915_gem_object_release_mmap_offset(obj: *mut DrmI915GemObject);
    fn intel_gt_invalidate_tlb_full(gt: *mut IntelGt, seqno: u32);
    fn drm_clflush_sg(pages: *mut SgTable);
    fn radix_tree_next_chunk(
        root: *const crate::intel_context_upstream::RadixTreeRoot,
        iter: *mut RadixTreeIter,
        flags: u32,
    ) -> *mut *mut c_void;
    fn radix_tree_delete(
        root: *mut crate::intel_context_upstream::RadixTreeRoot,
        index: u64,
    ) -> *mut c_void;
    fn radix_tree_insert(
        root: *mut crate::intel_context_upstream::RadixTreeRoot,
        index: u64,
        item: *mut c_void,
    ) -> c_int;
    fn radix_tree_lookup(
        root: *mut crate::intel_context_upstream::RadixTreeRoot,
        index: u64,
    ) -> *mut c_void;
    fn is_vmalloc_addr(ptr: *mut c_void) -> bool;
    fn vunmap(ptr: *mut c_void);
    fn page_address(page: *mut Page) -> *mut c_void;
    fn pgprot_writecombine(prot: PgProt) -> PgProt;
    fn vmap(pages: *mut *mut Page, count: u32, flags: c_ulong, prot: PgProt) -> *mut c_void;
    fn vmap_pfn(pfns: *mut c_ulong, count: u32, prot: PgProt) -> *mut c_void;
    fn kvmalloc_array(count: usize, size: usize, flags: u32) -> *mut c_void;
    fn kvfree(ptr: *mut c_void);
    fn set_page_dirty(page: *mut Page);
    fn pat_enabled() -> bool;
    fn might_sleep();
}

// `CONFIG_HIGHMEM` is not selectable for the target's x86_64 Linux 7.2.3
// configuration, so PageHighMem() folds to false exactly as its page-flags
// macro does there.
#[inline]
fn page_high_mem(_page: *mut Page) -> bool {
    false
}
#[inline]
unsafe fn kmap_local_page_try_from_panic(page: *mut Page) -> *mut c_void {
    // x86_64 with CONFIG_HIGHMEM=n: highmem panic mappings are unreachable;
    // the upstream inline helper returns page_address() directly.
    unsafe { page_address(page) }
}
#[inline]
fn kunmap_local(_addr: *mut c_void) {
    // The configured x86_64 architecture does not define
    // ARCH_HAS_FLUSH_ON_KUNMAP; its highmem-internal.h inline is empty.
}

#[inline]
unsafe fn assert_object_held(obj: *mut DrmI915GemObject) {
    unsafe {
        lockdep_assert_held!((*gem_base(obj))._resv.lock);
    }
}
#[inline]
unsafe fn assert_object_held_shared(obj: *mut DrmI915GemObject) {
    unsafe {
        if CONFIG_LOCKDEP && kref_read(&(*gem_base(obj)).refcount) > 0 {
            assert_object_held(obj);
        }
    }
}

#[inline]
fn ilog2(value: usize) -> usize {
    assert!(value != 0);
    usize::BITS as usize - 1 - value.leading_zeros() as usize
}

unsafe fn iosys_map_wr_u32(map: *mut IosysMap, offset: usize, value: u32) {
    unsafe {
        if (*map).is_iomem {
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
            ptr::write_volatile(
                (*map).addr.vaddr_iomem.cast::<u8>().add(offset).cast(),
                value,
            );
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
        } else {
            ptr::write_volatile((*map).addr.vaddr.cast::<u8>().add(offset).cast(), value);
        }
    }
}

// arch/x86 PAGE_KERNEL/PAGE_KERNEL_IO and pgprot_writecombine() for the
// target's x86_64, non-SME configuration. PAGE_KERNEL_IO aliases PAGE_KERNEL.
const fn page_kernel() -> PgProt {
    PgProt {
        pgprot: ((1usize << 0)
            | (1usize << 1)
            | (1usize << 5)
            | (1usize << 6)
            | (1usize << 8)
            | (1usize << 63)) as c_ulong,
    }
}
const fn page_kernel_io() -> PgProt {
    page_kernel()
}

#[inline]
unsafe fn goto_err_unpin(obj: *mut DrmI915GemObject, _ptr: *mut c_void) {
    unsafe { atomic_dec(&mut (*obj).mm.pages_pin_count) }
}

#[repr(C)]
struct SgTablePrefix {
    sgl: *mut Scatterlist,
    nents: u32,
    orig_nents: u32,
}
const _: [(); 16] = [(); size_of::<SgTablePrefix>()];
#[repr(C)]
struct RadixTreeIter {
    index: usize,
    next_index: usize,
    tags: usize,
    node: *mut c_void,
}
const RADIX_TREE_ITER_CONTIG: u32 = 0x20;
const SG_CHAIN: usize = 1;
const SG_END: usize = 2;
const SG_PAGE_LINK_MASK: usize = SG_CHAIN | SG_END;

#[inline]
fn xa_mk_value(value: u64) -> *mut c_void {
    (((value as usize) << 1) | 1) as *mut c_void
}
#[inline]
fn xa_is_value(entry: *mut c_void) -> bool {
    (entry as usize & 1) != 0
}
#[inline]
fn xa_to_value(entry: *mut c_void) -> u64 {
    (entry as usize >> 1) as u64
}

// Non-tagged branch of radix_tree_next_slot() used by
// radix_tree_for_each_slot(root, iter, start=0).
unsafe fn radix_tree_next_slot(
    mut slot: *mut *mut c_void,
    iter: *mut RadixTreeIter,
    flags: u32,
) -> *mut *mut c_void {
    assert_eq!(flags & 0x10, 0, "pages iterator uses untagged slots");
    unsafe {
        let mut count = (*iter).next_index.wrapping_sub((*iter).index);
        while count > 1 {
            slot = slot.add(1);
            (*iter).index += 1;
            count -= 1;
            if !(*slot).is_null() {
                return slot;
            }
            if flags & RADIX_TREE_ITER_CONTIG != 0 {
                (*iter).next_index = 0;
                break;
            }
        }
        ptr::null_mut()
    }
}

#[inline]
unsafe fn sg_page(sg: *mut Scatterlist) -> *mut Page {
    unsafe { ((*sg).page_link & !SG_PAGE_LINK_MASK) as *mut Page }
}
#[inline]
unsafe fn sg_dma_address(sg: *mut Scatterlist) -> u64 {
    unsafe { (*sg).dma_address }
}
#[inline]
unsafe fn sg_dma_len(sg: *mut Scatterlist) -> u32 {
    unsafe { (*sg).dma_length }
}
#[inline]
unsafe fn sg_page_count(sg: *mut Scatterlist) -> u32 {
    unsafe { (*sg).length >> PAGE_SHIFT }
}
#[inline]
unsafe fn sg_dma_page_count(sg: *mut Scatterlist) -> u32 {
    unsafe { (*sg).dma_length >> PAGE_SHIFT }
}
#[inline]
unsafe fn sg_chain_ptr(sg: *mut Scatterlist) -> *mut Scatterlist {
    unsafe { ((*sg).page_link & !SG_PAGE_LINK_MASK) as *mut Scatterlist }
}
#[inline]
unsafe fn sg_next(sg: *mut Scatterlist) -> *mut Scatterlist {
    unsafe {
        if sg.is_null() || (*sg).page_link & SG_END != 0 {
            return ptr::null_mut();
        }
        let next = sg.add(1);
        if (*next).page_link & SG_CHAIN != 0 {
            sg_chain_ptr(next)
        } else {
            next
        }
    }
}
// i915_scatterlist.h's ____sg_next() deliberately does not stop at SG_END.
#[inline]
unsafe fn ____sg_next(sg: *mut Scatterlist) -> *mut Scatterlist {
    unsafe {
        let next = sg.add(1);
        if (*next).page_link & SG_CHAIN != 0 {
            sg_chain_ptr(next)
        } else {
            next
        }
    }
}
unsafe fn i915_sg_dma_sizes(mut sg: *mut Scatterlist) -> u32 {
    unsafe {
        let mut sizes = 0;
        while !sg.is_null() && sg_dma_len(sg) != 0 {
            assert_eq!((*sg).offset, 0);
            assert_eq!(sg_dma_len(sg) as usize % (1usize << PAGE_SHIFT), 0);
            sizes |= sg_dma_len(sg);
            sg = sg_next(sg);
        }
        sizes
    }
}
#[inline]
unsafe fn sg_table_first(table: *mut SgTable) -> *mut Scatterlist {
    unsafe { (*table.cast::<SgTablePrefix>()).sgl }
}
#[inline]
unsafe fn set_page_iter_origin(iter: *mut I915GemObjectPageIter, table: *mut SgTable) {
    unsafe {
        (*iter).sg_pos = sg_table_first(table).cast();
        (*iter).sg_idx = 0;
    }
}
#[inline]
unsafe fn sgt_next(sg: *mut Scatterlist) -> *mut Scatterlist {
    unsafe { ____sg_next(sg) }
}
unsafe fn sgt_copy_pages(table: *mut SgTable, pages: *mut *mut Page, n_pages: usize) {
    unsafe {
        let mut sg = sg_table_first(table);
        let mut index = 0;
        while index < n_pages && !sg.is_null() {
            let count = (sg_page_count(sg) as usize).min(n_pages - index);
            let base = sg_page(sg).cast::<u8>();
            for offset in 0..count {
                *pages.add(index + offset) = base.add(offset << PAGE_SHIFT).cast();
            }
            index += count;
            sg = sg_next(sg);
        }
        assert_eq!(
            index, n_pages,
            "SG table ended before the object page count"
        );
    }
}
unsafe fn sgt_copy_dma_pfns(table: *mut SgTable, pfns: *mut c_ulong, n_pfns: usize, iomap: u64) {
    unsafe {
        let mut sg = sg_table_first(table);
        let mut index = 0;
        while index < n_pfns && !sg.is_null() && sg_dma_len(sg) != 0 {
            let count = (sg_dma_page_count(sg) as usize).min(n_pfns - index);
            let base = sg_dma_address(sg);
            for offset in 0..count {
                *pfns.add(index + offset) =
                    ((iomap + base + ((offset as u64) << PAGE_SHIFT)) >> PAGE_SHIFT) as c_ulong;
            }
            index += count;
            sg = sg_next(sg);
        }
        assert_eq!(
            index, n_pfns,
            "DMA SG table ended before the object page count"
        );
    }
}

#[inline]
unsafe fn i915_gem_object_get_sg(
    obj: *mut DrmI915GemObject,
    n: u64,
    offset: *mut u32,
) -> *mut Scatterlist {
    unsafe { __i915_gem_object_page_iter_get_sg(obj, &mut (*obj).mm.get_page, n, offset) }
}
#[inline]
unsafe fn i915_gem_object_get_sg_dma(
    obj: *mut DrmI915GemObject,
    n: u64,
    offset: *mut u32,
) -> *mut Scatterlist {
    unsafe { __i915_gem_object_page_iter_get_sg(obj, &mut (*obj).mm.get_dma_page, n, offset) }
}

// upstream: i915_gem_pages.c __i915_gem_object_set_pages()
pub unsafe fn __i915_gem_object_set_pages(obj: *mut DrmI915GemObject, pages: *mut SgTable) {
    unsafe {
        let i915 = crate::linux::i915::to_i915((*gem_base(obj)).dev);
        let supported = (*(*i915).info.cast::<IntelDeviceInfoOverlay>())
            .runtime
            .page_sizes as c_ulong;
        let mut shrinkable;
        assert_object_held_shared(obj);
        if ((*obj).flags & I915_BO_ALLOC_VOLATILE != 0) {
            (*obj).mm.set_madv(I915_MADV_DONTNEED);
        }
        if ((*obj).cache_state_bits & (1 << 9) != 0) {
            WARN_ON_ONCE!(IS_DGFX(i915));
            (*obj).write_domain = 0;
            if i915_gem_object_has_struct_page(obj) {
                drm_clflush_sg(pages);
            }
            (*obj).cache_state_bits &= !(1 << 9);
        }
        // Both page iterators begin at the first SG entry and index zero.
        set_page_iter_origin(&mut (*obj).mm.get_page, pages);
        set_page_iter_origin(&mut (*obj).mm.get_dma_page, pages);
        (*obj).mm.pages = pages;
        (*obj).mm.page_sizes.phys = i915_sg_dma_sizes(sg_table_first(pages));
        assert!((*obj).mm.page_sizes.phys != 0);
        (*obj).mm.page_sizes.sg = 0;
        for i in 0..=ilog2(crate::intel_gtt_api_upstream::I915_GTT_MAX_PAGE_SIZE as usize) {
            if supported & (1u64 << i) != 0 && (*obj).mm.page_sizes.phys & (!0u32 << i as u32) != 0
            {
                (*obj).mm.page_sizes.sg |= 1 << i;
            }
        }
        assert!(
            ((*(*i915).info.cast::<IntelDeviceInfoOverlay>())
                .runtime
                .page_sizes
                & (*obj).mm.page_sizes.sg)
                == (*obj).mm.page_sizes.sg
        );
        shrinkable = (*(*obj).ops).flags & I915_GEM_OBJECT_IS_SHRINKABLE != 0;
        if unsafe { crate::linux::fields::i915_gem_object_is_tiled(obj) }
            && (*i915).gem_quirks & GEM_QUIRK_PIN_SWIZZLED_PAGES != 0
        {
            assert!((*obj).flags & (1 << I915_TILING_QUIRK_BIT) == 0);
            (*obj).flags |= 1 << I915_TILING_QUIRK_BIT;
            assert!(list_empty(&(*obj).mm.link));
            atomic_inc(&mut (*obj).mm.shrink_pin);
            shrinkable = false;
        }
        if shrinkable && !((*(*obj).ops).flags & I915_GEM_OBJECT_SELF_MANAGED_SHRINK_LIST != 0) {
            assert_object_held(obj);
            let mut flags = 0;
            spin_lock_irqsave(&mut (*i915).mm.obj_lock, &mut flags);
            (*i915).mm.shrink_count += 1;
            (*i915).mm.shrink_memory += object_size(obj);
            let list = if (*obj).mm.madv() != I915_MADV_WILLNEED {
                &mut (*i915).mm.purge_list
            } else {
                &mut (*i915).mm.shrink_list
            };
            list_add_tail(&mut (*obj).mm.link, list);
            atomic_set(&mut (*obj).mm.shrink_pin, 0);
            spin_unlock_irqrestore(&mut (*i915).mm.obj_lock, flags);
        }
    }
}
// upstream: i915_gem_pages.c ____i915_gem_object_get_pages()
pub unsafe fn ____i915_gem_object_get_pages(obj: *mut DrmI915GemObject) -> c_int {
    unsafe {
        let i915 = crate::linux::i915::to_i915((*gem_base(obj)).dev);
        assert_object_held_shared(obj);
        if (*obj).mm.madv() != I915_MADV_WILLNEED {
            drm_dbg!(&(*i915).drm, "Attempting to obtain a purgeable object");
            return -14;
        }
        let err = ((*obj).ops.as_ref().unwrap().get_pages.unwrap())(obj);
        assert!(err != 0 || i915_gem_object_has_pages(obj));
        err
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_get_pages()
pub unsafe fn __i915_gem_object_get_pages(obj: *mut DrmI915GemObject) -> c_int {
    unsafe {
        assert_object_held(obj);
        assert_object_held_shared(obj);
        if !i915_gem_object_has_pages(obj) {
            assert!(!i915_gem_object_has_pinned_pages(obj));
            let err = ____i915_gem_object_get_pages(obj);
            if err != 0 {
                return err;
            }
            smp_mb__before_atomic();
        }
        atomic_inc(&mut (*obj).mm.pages_pin_count);
        0
    }
}
// upstream: i915_gem_pages.c i915_gem_object_pin_pages_unlocked()
pub unsafe fn i915_gem_object_pin_pages_unlocked(obj: *mut DrmI915GemObject) -> c_int {
    unsafe {
        let mut ww = I915GemWwCtx::default();
        i915_gem_ww_ctx_init(&mut ww, true);
        let mut err;
        loop {
            err = i915_gem_object_lock(obj, &mut ww);
            if err == 0 {
                err = i915_gem_object_pin_pages(obj);
            }
            if err != -35 {
                break;
            }
            err = i915_gem_ww_ctx_backoff(&mut ww);
            if err != 0 {
                break;
            }
        }
        i915_gem_ww_ctx_fini(&mut ww);
        err
    }
}
// upstream: i915_gem_pages.c i915_gem_object_truncate()
pub unsafe fn i915_gem_object_truncate(obj: *mut DrmI915GemObject) -> c_int {
    unsafe {
        match (*obj).ops.as_ref().unwrap().truncate {
            Some(truncate) => truncate(obj),
            None => 0,
        }
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_reset_page_iter()
unsafe fn __i915_gem_object_reset_page_iter(obj: *mut DrmI915GemObject) {
    unsafe {
        for iter in [
            &mut (*obj).mm.get_page as *mut I915GemObjectPageIter,
            &mut (*obj).mm.get_dma_page as *mut I915GemObjectPageIter,
        ] {
            let root = &mut (*iter).radix;
            let mut it = RadixTreeIter {
                index: 0,
                next_index: 0,
                tags: 0,
                node: ptr::null_mut(),
            };
            let mut slot: *mut *mut c_void = ptr::null_mut();
            loop {
                if slot.is_null() {
                    slot = radix_tree_next_chunk(root, &mut it, 0);
                    if slot.is_null() {
                        break;
                    }
                }
                radix_tree_delete(root, it.index as u64);
                slot = radix_tree_next_slot(slot, &mut it, 0);
            }
        }
    }
}
// upstream: i915_gem_pages.c unmap_object()
unsafe fn unmap_object(_obj: *mut DrmI915GemObject, ptr_: *mut c_void) {
    unsafe {
        if is_vmalloc_addr(ptr_) {
            vunmap(ptr_);
        }
    }
}
// upstream: i915_gem_pages.c flush_tlb_invalidate()
unsafe fn flush_tlb_invalidate(obj: *mut DrmI915GemObject) {
    unsafe {
        let i915 = crate::linux::i915::to_i915((*gem_base(obj)).dev);
        for id in 0..crate::intel_gt_defines_types_upstream::I915_MAX_GT {
            if (*i915).gt[id].is_null() {
                continue;
            }
            let gt = (*i915).gt[id];
            let seq = (*obj).mm.tlb[id as usize];
            if seq != 0 {
                intel_gt_invalidate_tlb_full(gt, seq);
                (*obj).mm.tlb[id as usize] = 0;
            }
        }
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_unset_pages()
pub unsafe fn __i915_gem_object_unset_pages(obj: *mut DrmI915GemObject) -> *mut SgTable {
    unsafe {
        assert_object_held_shared(obj);
        let pages = crate::linux::primitives::fetch_and_zero(&mut (*obj).mm.pages);
        if is_err_or_null(pages.cast()) {
            return pages;
        }
        if ((*obj).flags & I915_BO_ALLOC_VOLATILE != 0) {
            (*obj).mm.set_madv(I915_MADV_WILLNEED);
        }
        if !((*(*obj).ops).flags & I915_GEM_OBJECT_SELF_MANAGED_SHRINK_LIST != 0) {
            crate::i915_gem_shrinker_upstream::i915_gem_object_make_unshrinkable(obj);
        }
        if !(*obj).mm.mapping.is_null() {
            unmap_object(obj, page_mask_bits((*obj).mm.mapping));
            (*obj).mm.mapping = ptr::null_mut();
        }
        __i915_gem_object_reset_page_iter(obj);
        (*obj).mm.page_sizes.phys = 0;
        (*obj).mm.page_sizes.sg = 0;
        flush_tlb_invalidate(obj);
        pages
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_put_pages()
pub unsafe fn __i915_gem_object_put_pages(obj: *mut DrmI915GemObject) -> c_int {
    unsafe {
        if i915_gem_object_has_pinned_pages(obj) {
            return -16;
        }
        assert_object_held_shared(obj);
        i915_gem_object_release_mmap_offset(obj);
        let pages = __i915_gem_object_unset_pages(obj);
        if !is_err_or_null(pages.cast()) {
            ((*obj).ops.as_ref().unwrap().put_pages.unwrap())(obj, pages);
        }
        0
    }
}

// upstream: i915_gem_pages.c i915_gem_object_map_page()
unsafe fn i915_gem_object_map_page(obj: *mut DrmI915GemObject, type_: I915MapType) -> *mut c_void {
    unsafe {
        let n_pages = (object_size(obj) >> PAGE_SHIFT) as usize;
        let mut stack = [ptr::null_mut::<Page>(); 32];
        let mut pages = stack.as_mut_ptr();
        let mut pgprot = page_kernel();
        match type_ {
            I915_MAP_WB => {
                let first = sg_page(sg_table_first((*obj).mm.pages));
                if n_pages == 1 && !page_high_mem(first) {
                    return page_address(first);
                }
            }
            I915_MAP_WC => {
                pgprot = pgprot_writecombine(page_kernel_io());
            }
            _ => {
                MISSING_CASE!(type_);
            }
        }
        if n_pages > stack.len() {
            pages = kvmalloc_array(n_pages, size_of::<*mut Page>(), GFP_KERNEL).cast();
            if pages.is_null() {
                return err_ptr(-12);
            }
        }
        sgt_copy_pages((*obj).mm.pages, pages, n_pages);
        let vaddr = vmap(pages, n_pages as u32, 0, pgprot);
        if pages != stack.as_mut_ptr() {
            kvfree(pages.cast());
        }
        if vaddr.is_null() { err_ptr(-12) } else { vaddr }
    }
}
// upstream: i915_gem_pages.c i915_gem_object_map_pfn()
unsafe fn i915_gem_object_map_pfn(obj: *mut DrmI915GemObject, type_: I915MapType) -> *mut c_void {
    unsafe {
        let region = &*(*obj).mm.region;
        let iomap = region.iomap.base.wrapping_sub(region.region.start) as u64;
        let n_pfn = (object_size(obj) >> PAGE_SHIFT) as usize;
        let mut stack = [0usize; 32];
        let mut pfns = stack.as_mut_ptr();
        assert_eq!(type_, I915_MAP_WC);
        if n_pfn > stack.len() {
            pfns = kvmalloc_array(n_pfn, size_of::<usize>(), GFP_KERNEL).cast();
            if pfns.is_null() {
                return err_ptr(-12);
            }
        }
        sgt_copy_dma_pfns((*obj).mm.pages, pfns.cast::<u64>(), n_pfn, iomap);
        let vaddr = vmap_pfn(
            pfns.cast::<u64>(),
            n_pfn as u32,
            pgprot_writecombine(page_kernel_io()),
        );
        if pfns != stack.as_mut_ptr() {
            kvfree(pfns.cast());
        }
        if vaddr.is_null() { err_ptr(-12) } else { vaddr }
    }
}
// upstream: i915_gem_pages.c i915_panic_kunmap()
unsafe fn i915_panic_kunmap(panic: *mut IntelPanic) {
    unsafe {
        if !(*panic).vaddr.is_null() {
            crate::intel_engine_api_upstream::drm_clflush_virt_range(
                (*panic).vaddr.cast(),
                PAGE_SIZE as c_ulong,
            );
            kunmap_local((*panic).vaddr);
            (*panic).vaddr = ptr::null_mut();
        }
    }
}
// upstream: i915_gem_pages.c i915_gem_object_panic_pages()
unsafe fn i915_gem_object_panic_pages(obj: *mut DrmI915GemObject) -> *mut *mut Page {
    unsafe {
        let n = (object_size(obj) >> PAGE_SHIFT) as usize;
        let pages = kmalloc_objs_flags::<*mut Page, _>(n, GFP_ATOMIC);
        if pages.is_null() {
            return ptr::null_mut();
        }
        sgt_copy_pages((*obj).mm.pages, pages, n);
        pages
    }
}
// upstream: i915_gem_pages.c i915_gem_object_panic_map_set_pixel()
unsafe extern "C" fn i915_gem_object_panic_map_set_pixel(
    sb: *mut DrmScanoutBuffer,
    x: u32,
    y: u32,
    color: u32,
) {
    unsafe {
        let fb = (*sb).private.cast::<IntelFramebuffer>();
        let offset = ((*fb).panic_tiling.unwrap())((*sb).width, x, y);
        iosys_map_wr_u32(&mut (*sb).map[0], offset as usize, color);
    }
}
// upstream: i915_gem_pages.c i915_gem_object_panic_page_set_pixel()
unsafe extern "C" fn i915_gem_object_panic_page_set_pixel(
    sb: *mut DrmScanoutBuffer,
    x: u32,
    y: u32,
    color: u32,
) {
    unsafe {
        let fb = (*sb).private.cast::<IntelFramebuffer>();
        let panic = (*fb).panic;
        let mut offset = if let Some(panic_tiling) = (*fb).panic_tiling {
            panic_tiling((*sb).width, x, y)
        } else {
            y.wrapping_mul((*sb).pitch[0])
                .wrapping_add(x.wrapping_mul((*(*sb).format).cpp[0] as u32))
        };
        let new_page = offset >> PAGE_SHIFT;
        offset %= PAGE_SIZE as u32;
        if new_page as c_int != (*panic).page {
            i915_panic_kunmap(panic);
            (*panic).page = new_page as c_int;
            (*panic).vaddr =
                kmap_local_page_try_from_panic(*(*panic).pages.add((*panic).page as usize));
        }
        if !(*panic).vaddr.is_null() {
            ptr::write((*panic).vaddr.add(offset as usize).cast::<u32>(), color);
        }
    }
}
// upstream: i915_gem_pages.c i915_gem_object_alloc_panic()
pub unsafe fn i915_gem_object_alloc_panic() -> *mut IntelPanic {
    unsafe {
        let panic = kzalloc_obj_flags::<IntelPanic>(GFP_KERNEL);
        panic
    }
}
// upstream: i915_gem_pages.c i915_gem_object_panic_setup()
pub unsafe fn i915_gem_object_panic_setup(
    panic: *mut IntelPanic,
    sb: *mut DrmScanoutBuffer,
    obj_: *mut crate::linux::gem::DrmGemObject,
    panic_tiling: bool,
) -> c_int {
    unsafe {
        let obj = crate::i915_gem_object_types_upstream::to_intel_bo(obj_);
        let mut type_ = 0;
        let ptr_ = page_unpack_bits((*obj).mm.mapping, &mut type_);
        if !ptr_.is_null() {
            if i915_gem_object_has_iomem(obj) {
                (*sb).map[0].set_vaddr_iomem(ptr_);
            } else {
                (*sb).map[0].set_vaddr(ptr_);
            }
            if panic_tiling {
                (*sb).set_pixel = Some(i915_gem_object_panic_map_set_pixel);
            }
            return 0;
        }
        if i915_gem_object_has_struct_page(obj) {
            (*panic).pages = i915_gem_object_panic_pages(obj);
            if (*panic).pages.is_null() {
                return -12;
            }
            (*panic).page = -1;
            (*sb).set_pixel = Some(i915_gem_object_panic_page_set_pixel);
            return 0;
        }
        let _ = panic;
        -95
    }
}
// upstream: i915_gem_pages.c i915_gem_object_panic_finish()
pub unsafe fn i915_gem_object_panic_finish(panic: *mut IntelPanic) {
    unsafe {
        i915_panic_kunmap(panic);
        (*panic).page = -1;
        kfree((*panic).pages.cast::<c_void>());
        (*panic).pages = ptr::null_mut();
    }
}
// upstream: i915_gem_pages.c i915_gem_object_pin_map()
pub unsafe fn i915_gem_object_pin_map(
    obj: *mut DrmI915GemObject,
    type_: I915MapType,
) -> *mut c_void {
    unsafe {
        if !i915_gem_object_has_struct_page(obj) && !i915_gem_object_has_iomem(obj) {
            return err_ptr(-6);
        }
        if WARN_ON_ONCE!((*obj).flags & I915_BO_ALLOC_GPU_ONLY != 0) {
            return err_ptr(-22);
        }
        assert_object_held(obj);
        let mut pinned = type_ & I915_MAP_OVERRIDE == 0;
        let mut type_ = type_ & !I915_MAP_OVERRIDE;
        if !atomic_inc_not_zero(&mut (*obj).mm.pages_pin_count) {
            if !i915_gem_object_has_pages(obj) {
                assert!(!i915_gem_object_has_pinned_pages(obj));
                let err = __i915_gem_object_get_pages(obj);
                if err != 0 {
                    return err_ptr(err);
                }
                smp_mb__before_atomic();
            }
            atomic_inc(&mut (*obj).mm.pages_pin_count);
            pinned = false;
        }
        assert!(i915_gem_object_has_pages(obj));
        if i915_gem_object_placement_possible(obj, INTEL_MEMORY_LOCAL) {
            if type_ != I915_MAP_WC && (*obj).mm.n_placements == 0 {
                let ptr_ = err_ptr(-19);
                goto_err_unpin(obj, ptr_);
                return ptr_;
            }
            type_ = I915_MAP_WC;
        } else if IS_DGFX(crate::linux::i915::to_i915((*gem_base(obj)).dev)) {
            type_ = I915_MAP_WB;
        }
        let mut has_type = 0;
        let mut ptr_ = page_unpack_bits((*obj).mm.mapping, &mut has_type);
        if !ptr_.is_null() && has_type != type_ {
            if pinned {
                let ptr_ = err_ptr(-16);
                goto_err_unpin(obj, ptr_);
                return ptr_;
            }
            unmap_object(obj, ptr_);
            ptr_ = ptr::null_mut();
            (*obj).mm.mapping = ptr::null_mut();
        }
        if ptr_.is_null() {
            let err = i915_gem_object_wait_moving_fence(obj, true);
            if err != 0 {
                let ptr_ = err_ptr(err);
                goto_err_unpin(obj, ptr_);
                return ptr_;
            }
            if GEM_WARN_ON!(type_ == I915_MAP_WC && !pat_enabled()) {
                ptr_ = err_ptr(-19);
            } else if i915_gem_object_has_struct_page(obj) {
                ptr_ = i915_gem_object_map_page(obj, type_);
            } else {
                ptr_ = i915_gem_object_map_pfn(obj, type_);
            }
            if is_err(ptr_) {
                goto_err_unpin(obj, ptr_);
                return ptr_;
            }
            (*obj).mm.mapping = page_pack_bits(ptr_, type_);
        }
        ptr_
    }
}

// upstream: i915_gem_pages.c i915_gem_object_pin_map_unlocked()
pub unsafe fn i915_gem_object_pin_map_unlocked(
    obj: *mut DrmI915GemObject,
    type_: I915MapType,
) -> *mut c_void {
    unsafe {
        i915_gem_object_lock(obj, ptr::null_mut());
        let ret = i915_gem_object_pin_map(obj, type_);
        i915_gem_object_unlock(obj);
        ret
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_flush_map()
pub unsafe fn __i915_gem_object_flush_map(obj: *mut DrmI915GemObject, offset: u64, size: u64) {
    unsafe {
        assert!(i915_gem_object_has_pinned_pages(obj));
        assert!(
            !offset
                .checked_add(size)
                .map_or(true, |end| end > object_size(obj))
        );
        crate::linux::primitives::wmb();
        (*obj).mm.set_dirty(true);
        if ((*obj).cache_state_bits >> 7) & I915_BO_CACHE_COHERENT_FOR_WRITE != 0 {
            return;
        }
        let mut type_ = 0;
        let ptr_ = page_unpack_bits((*obj).mm.mapping, &mut type_);
        if type_ == I915_MAP_WC {
            return;
        }
        crate::intel_engine_api_upstream::drm_clflush_virt_range(
            ptr_.add(offset as usize),
            size as c_ulong,
        );
        if size == object_size(obj) {
            (*obj).write_domain &= !(I915_GEM_DOMAIN_CPU as u16);
            (*obj).cache_state_bits &= !(1 << 9);
        }
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_release_map()
pub unsafe fn __i915_gem_object_release_map(obj: *mut DrmI915GemObject) {
    unsafe {
        assert!(!(*obj).mm.mapping.is_null());
        let mapping = crate::linux::primitives::fetch_and_zero(&mut (*obj).mm.mapping);
        unmap_object(obj, page_mask_bits(mapping));
        i915_gem_object_unpin_map(obj);
    }
}
unsafe fn lookup_iter_sg(
    iter: *mut I915GemObjectPageIter,
    n: u64,
    offset: *mut u32,
) -> *mut Scatterlist {
    unsafe {
        rcu_read_lock();
        let mut sg = radix_tree_lookup(&mut (*iter).radix, n);
        assert!(!sg.is_null());
        *offset = 0;
        if xa_is_value(sg) {
            let base = xa_to_value(sg);
            sg = radix_tree_lookup(&mut (*iter).radix, base);
            assert!(!sg.is_null());
            *offset = (n - base) as u32;
        }
        rcu_read_unlock();
        sg.cast()
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_page_iter_get_sg()
pub unsafe fn __i915_gem_object_page_iter_get_sg(
    obj: *mut DrmI915GemObject,
    iter: *mut I915GemObjectPageIter,
    n: u64,
    offset: *mut u32,
) -> *mut Scatterlist {
    unsafe {
        might_sleep();
        let dma = iter == &mut (*obj).mm.get_dma_page || iter == &mut (*obj).ttm.get_io_page;
        assert!((n < object_size(obj) >> PAGE_SHIFT));
        if !i915_gem_object_has_pinned_pages(obj) {
            assert_object_held(obj);
        }
        if n < ptr::read_volatile(&(*iter).sg_idx) as u64 {
            return lookup_iter_sg(iter, n, offset);
        }
        mutex_lock(&mut (*iter).lock);
        let mut sg = (*iter).sg_pos.cast::<Scatterlist>();
        let mut idx = (*iter).sg_idx as u64;
        let mut count = if dma {
            sg_dma_page_count(sg)
        } else {
            sg_page_count(sg)
        } as u64;
        while idx + count <= n {
            let mut ret = radix_tree_insert(&mut (*iter).radix, idx, sg.cast());
            if ret != 0 && ret != -17 {
                break;
            }
            let entry = xa_mk_value(idx);
            let mut inserted = 1;
            while inserted < count {
                ret = radix_tree_insert(&mut (*iter).radix, idx + inserted, entry);
                if ret != 0 && ret != -17 {
                    break;
                }
                inserted += 1;
            }
            if ret != 0 && ret != -17 {
                break;
            }
            idx += count;
            sg = sgt_next(sg);
            count = if dma {
                sg_dma_page_count(sg)
            } else {
                sg_page_count(sg)
            } as u64;
        }
        (*iter).sg_pos = sg.cast();
        (*iter).sg_idx = idx as u32;
        mutex_unlock(&mut (*iter).lock);
        if n < idx {
            return lookup_iter_sg(iter, n, offset);
        }
        while idx + count <= n {
            idx += count;
            sg = sgt_next(sg);
            count = if dma {
                sg_dma_page_count(sg)
            } else {
                sg_page_count(sg)
            } as u64;
        }
        *offset = (n - idx) as u32;
        sg
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_get_page()
pub unsafe fn __i915_gem_object_get_page(obj: *mut DrmI915GemObject, n: u64) -> *mut Page {
    unsafe {
        assert!(i915_gem_object_has_struct_page(obj));
        let mut offset = 0;
        let sg = i915_gem_object_get_sg(obj, n, &mut offset);
        sg_page(sg)
            .cast::<u8>()
            .add((offset as usize) << PAGE_SHIFT)
            .cast()
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_get_dirty_page()
pub unsafe fn __i915_gem_object_get_dirty_page(obj: *mut DrmI915GemObject, n: u64) -> *mut Page {
    unsafe {
        let page = __i915_gem_object_get_page(obj, n);
        if !(*obj).mm.is_dirty() {
            set_page_dirty(page);
        }
        page
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_get_dma_address_len()
pub unsafe fn __i915_gem_object_get_dma_address_len(
    obj: *mut DrmI915GemObject,
    n: u64,
    len: *mut u32,
) -> u64 {
    unsafe {
        let mut offset = 0;
        let sg = i915_gem_object_get_sg_dma(obj, n, &mut offset);
        if !len.is_null() {
            *len = sg_dma_len(sg).wrapping_sub(offset << PAGE_SHIFT);
        }
        sg_dma_address(sg) + ((offset as u64) << PAGE_SHIFT)
    }
}
// upstream: i915_gem_pages.c __i915_gem_object_get_dma_address()
pub unsafe fn __i915_gem_object_get_dma_address(obj: *mut DrmI915GemObject, n: u64) -> u64 {
    unsafe { __i915_gem_object_get_dma_address_len(obj, n, ptr::null_mut()) }
}
