// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation.
//
// Source-faithful Rust transcription of Linux v7.2.3
// drivers/gpu/drm/i915/gem/i915_gem_shmem.c. Keep source order, allocation
// fallback order, folio/page lifetime, cache-domain behavior and writeback
// semantics intact. GEM, shmem, folio, scatterlist, DMA, file and memory-region
// operations below are Linux/i915 integration bindings, not substitute
// implementations.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

use core::{
    ffi::{c_char, c_int, c_long, c_ulong, c_void},
    ptr,
};

use crate::{
    i915_gem_object_types_upstream::{
        DrmI915GemObject, DrmI915GemObjectOps, I915_GEM_OBJECT_IS_SHRINKABLE,
        I915_GEM_OBJECT_SHRINK_WRITEBACK, VmOperationsStruct,
    },
    i915_gem_object_upstream::{
        i915_gem_object_can_bypass_llc, i915_gem_object_has_struct_page, i915_gem_object_init,
        i915_gem_object_set_cache_coherency,
    },
    i915_gem_tiling_upstream::i915_gem_object_needs_bit17_swizzle,
    intel_context_upstream::SgTable,
    linux::i915::{GRAPHICS_VER, GRAPHICS_VER_FULL, IP_VER, IS_DGFX, IS_I965G, IS_I965GM},
    linux_config::*,
    linux_i915_private::DrmI915Private,
};

// Values from i915_gem_shrinker.h, i915_gem_object_types.h,
// i915_gem.h, i915_drm.h, gfp_types.h, swap.h and fs.h.
const I915_SHRINK_UNBOUND: u32 = 1 << 0;
const I915_SHRINK_BOUND: u32 = 1 << 1;
const I915_BO_ALLOC_NOTHP: u32 = 1 << 8;
const I915_BO_FLAG_STRUCT_PAGE: u32 = 1 << 0;
const I915_MADV_WILLNEED: u32 = 0;
const I915_MADV_DONTNEED: u32 = 1;
const __I915_MADV_PURGED: u32 = 2;
const I915_GEM_DOMAIN_CPU: u32 = 0x0000_0001;
const I915_GEM_DOMAIN_RENDER: u32 = 0x0000_0002;
const I915_GEM_DOMAIN_SAMPLER: u32 = 0x0000_0004;
const I915_GEM_DOMAIN_COMMAND: u32 = 0x0000_0008;
const I915_GEM_DOMAIN_INSTRUCTION: u32 = 0x0000_0010;
const I915_GEM_DOMAIN_VERTEX: u32 = 0x0000_0020;
const I915_GEM_GPU_DOMAINS: u32 = I915_GEM_DOMAIN_RENDER
    | I915_GEM_DOMAIN_SAMPLER
    | I915_GEM_DOMAIN_COMMAND
    | I915_GEM_DOMAIN_INSTRUCTION
    | I915_GEM_DOMAIN_VERTEX;
const I915_BO_CACHE_COHERENT_FOR_READ: u32 = 1 << 0;
const I915_CACHE_NONE: u32 = 0;
const I915_CACHE_LLC: u32 = 1;
const VMA_NORESERVE_BIT: u32 = 21;
const PAGE_SHIFT: u32 = 12;
const E2BIG: c_int = 7;
const ENOMEM: c_int = 12;
const EFAULT: c_int = 14;
const ENODEV: c_int = 19;
const EINVAL: c_int = 22;
const EIO: c_int = 5;
const EFBIG: c_int = 27;
const ENOSPC: c_int = 28;
const __GFP_DMA32: u32 = 1 << 2;
const __GFP_HIGHMEM: u32 = 1 << 1;
const __GFP_RECLAIMABLE: u32 = 1 << 4;
const __GFP_RECLAIM: u32 = (1 << 10) | (1 << 11);
const __GFP_IO: u32 = 1 << 6;
const __GFP_FS: u32 = 1 << 7;
const __GFP_HARDWALL: u32 = 1 << 20;
const __GFP_NOWARN: u32 = 1 << 13;
const __GFP_NORETRY: u32 = 1 << 16;
const __GFP_RETRY_MAYFAIL: u32 = 1 << 14;
const GFP_HIGHUSER: u32 = __GFP_RECLAIM | __GFP_IO | __GFP_FS | __GFP_HARDWALL | __GFP_HIGHMEM;
const MAX_RW_COUNT: u64 = 0x7fff_f000;
const SWAP_CLUSTER_MAX: c_long = 32;
const WB_SYNC_NONE: c_int = 0;
const MAX_LFS_FILESIZE: u64 = i64::MAX as u64;
const O_LARGEFILE: c_int = 0x8000;
const ITER_SOURCE: c_int = 1;
const EIOCBQUEUED: c_int = 529;
const INTEL_REGION_SMEM: usize = 0;

// Linux records passed through pointers remain opaque here. The records whose
// source members this file accesses are represented by source-derived prefixes.
#[repr(C)]
struct Folio {
    _opaque: [u8; 0],
}
#[repr(C)]
struct Page {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct AddressSpace {
    _opaque: [u8; 0],
}
#[repr(C)]
struct File {
    _f_lock: [u8; 4],
    f_mode: u32,
    f_op: *const FileOperations,
    f_mapping: *mut AddressSpace,
    _private_data: *mut c_void,
    f_inode: *mut Inode,
    f_flags: u32,
    _f_iocb_flags: u32,
}
#[repr(C)]
struct VfsMount {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct IntelMemoryRegion {
    i915: *mut DrmI915Private,
    ops: *const IntelMemoryRegionOps,
    iomap: IoMapping,
    region: Resource,
}
#[repr(C)]
struct IoMapping {
    base: u64,
    size: c_ulong,
    prot: c_ulong,
    iomem: *mut c_void,
}
#[repr(C)]
struct Resource {
    start: u64,
    end: u64,
    name: *const c_char,
    flags: c_ulong,
    desc: c_ulong,
    parent: *mut Resource,
    sibling: *mut Resource,
    child: *mut Resource,
}
#[repr(C)]
struct DrmGemObject {
    _refcount_and_handle_count: [u8; 8],
    dev: *mut c_void,
    filp: *mut File,
    _vma_node: [u8; 192],
    size: usize,
}
const _: [(); 216] = [(); core::mem::offset_of!(DrmGemObject, size)];
#[repr(C)]
struct FolioBatch {
    nr: u8,
    i: u8,
    percpu_pvec_drained: bool,
    _pad: [u8; 5],
    folios: [*mut Folio; 15],
}
#[repr(C)]
struct SgtIter {
    sgp: *mut ScatterList,
    pfn: c_ulong,
    curr: u32,
    max: u32,
}
#[repr(C)]
struct ScatterList {
    _opaque: [usize; 4],
}
#[repr(C)]
struct WritebackControl {
    nr_to_write: c_long,
    pages_skipped: c_long,
    range_start: u64,
    range_end: u64,
    sync_mode: c_int,
    _flags: u32,
    fbatch: FolioBatch,
    index: u64,
    saved_err: c_int,
    _cgroup_writeback: [u8; 64],
}
#[repr(C)]
struct Kiocb {
    ki_filp: *mut File,
    ki_pos: i64,
    ki_complete: Option<unsafe extern "C" fn(*mut Kiocb, c_long)>,
    private: *mut c_void,
    ki_flags: c_int,
    ki_ioprio: u16,
    ki_write_stream: u8,
    _pad: u8,
    ki_waitq: *mut c_void,
}
#[repr(C)]
struct FileOperations {
    owner: *mut c_void,
    fop_flags: u32,
    _pad: u32,
    _llseek: *const c_void,
    _read: *const c_void,
    _write: *const c_void,
    _read_iter: *const c_void,
    write_iter: Option<unsafe extern "C" fn(*mut Kiocb, *mut IovIter) -> isize>,
}
const _: [(); 8] = [(); core::mem::offset_of!(File, f_op)];
const _: [(); 16] = [(); core::mem::offset_of!(File, f_mapping)];
const _: [(); 40] = [(); core::mem::offset_of!(File, f_flags)];
const _: [(); 48] = [(); core::mem::offset_of!(FileOperations, write_iter)];
#[repr(C)]
struct Inode {
    _opaque: [u8; 0],
}
#[repr(C)]
struct IovIter {
    _opaque: [usize; 8],
}
#[repr(C)]
pub struct DrmI915GemPwrite {
    handle: u32,
    _pad: u32,
    offset: u64,
    size: u64,
    data_ptr: u64,
}
#[repr(C)]
pub struct DrmI915GemPread {
    handle: u32,
    _pad: u32,
    offset: u64,
    size: u64,
    data_ptr: u64,
}

#[repr(C)]
struct SgTableLayout {
    sgl: *mut ScatterList,
    nents: u32,
    orig_nents: u32,
}
#[repr(C)]
struct ScatterListLayout {
    page_link: c_ulong,
    offset: u32,
    length: u32,
    dma_address: u64,
    dma_length: u32,
    dma_flags: u32,
}
#[repr(C)]
struct LockClassKey {
    _opaque: [u8; 0],
}
const _: [(); 16] = [(); core::mem::size_of::<SgTableLayout>()];
const _: [(); 24] = [(); core::mem::size_of::<SgtIter>()];
const _: [(); 128] = [(); core::mem::size_of::<FolioBatch>()];
const _: [(); 48] = [(); core::mem::offset_of!(IntelMemoryRegion, region)];

#[inline]
unsafe fn sgt_layout(st: *mut SgTable) -> *mut SgTableLayout {
    st.cast()
}

#[inline]
unsafe fn sg_layout(sg: *mut ScatterList) -> *mut ScatterListLayout {
    sg.cast()
}

#[inline]
unsafe fn drm_gem_base(obj: *mut DrmI915GemObject) -> *mut DrmGemObject {
    ptr::addr_of_mut!((*obj).base).cast()
}

#[inline]
unsafe fn object_file(obj: *mut DrmI915GemObject) -> *mut File {
    (*drm_gem_base(obj)).filp
}

#[inline]
unsafe fn object_size(obj: *mut DrmI915GemObject) -> usize {
    (*drm_gem_base(obj)).size
}

#[inline]
unsafe fn object_madv(obj: *const DrmI915GemObject) -> u32 {
    (*obj).mm.madv()
}

#[inline]
unsafe fn set_object_madv(obj: *mut DrmI915GemObject, madv: u32) {
    (*obj).mm.set_madv(madv);
}

#[inline]
unsafe fn object_dirty(obj: *const DrmI915GemObject) -> bool {
    (*obj).mm.is_dirty()
}

#[inline]
unsafe fn set_object_dirty(obj: *mut DrmI915GemObject, dirty: bool) {
    (*obj).mm.set_dirty(dirty);
}

#[inline]
unsafe fn file_inode_mapping(file: *mut File) -> *mut AddressSpace {
    // shmem's file->f_mapping is its file_inode(file)->i_mapping.
    (*file).f_mapping
}

#[inline]
unsafe fn file_inode(file: *mut File) -> *mut Inode {
    (*file).f_inode
}

#[inline]
fn resource_size(resource: &Resource) -> u64 {
    resource.end.wrapping_sub(resource.start).wrapping_add(1)
}

#[inline]
unsafe fn sgt_iter_init(sg: *mut ScatterList) -> SgtIter {
    let mut iter = SgtIter {
        sgp: sg,
        pfn: 0,
        curr: 0,
        max: 0,
    };
    if !sg.is_null() {
        iter.curr = (*sg_layout(sg)).offset;
        iter.max = iter.curr.wrapping_add((*sg_layout(sg)).length);
        iter.pfn = page_to_pfn(sg_page(sg));
    }
    iter
}

/// Rust expansion of i915's `for_each_sgt_page` macro. This preserves its
/// PFN sentinel, byte offset, page-size advance and sg-next transition.
#[inline]
unsafe fn sgt_iter_next_page(iter: &mut SgtIter) -> *mut Page {
    if iter.sgp.is_null() || iter.pfn == 0 {
        return ptr::null_mut();
    }

    let page = pfn_to_page(iter.pfn.wrapping_add((iter.curr >> PAGE_SHIFT) as c_ulong));
    iter.curr = iter.curr.wrapping_add(PAGE_SIZE as u32);
    if iter.curr >= iter.max {
        *iter = sgt_iter_init(sg_next(iter.sgp));
    }
    page
}

// Linux memory-management, shmem, DRM/GEM, scatterlist and i915 helpers below
// intentionally retain upstream names and argument order. They are supplied
// by the integration layer; this file does not replace them with host shims.

// upstream: i915_gem_shmem.c check_release_folio_batch()
unsafe fn check_release_folio_batch(fbatch: *mut FolioBatch) {
    check_move_unevictable_folios(fbatch);
    __folio_batch_release(fbatch);
    cond_resched();
}

// upstream: i915_gem_shmem.c shmem_sg_free_table()
pub unsafe fn shmem_sg_free_table(
    st: *mut SgTable,
    mapping: *mut AddressSpace,
    dirty: bool,
    backup: bool,
) {
    let mut sgt_iter = sgt_iter_init((*sgt_layout(st)).sgl);
    let mut fbatch = FolioBatch {
        nr: 0,
        i: 0,
        percpu_pvec_drained: false,
        _pad: [0; 5],
        folios: [ptr::null_mut(); 15],
    };
    let mut last: *mut Folio = ptr::null_mut();

    mapping_clear_unevictable(mapping);

    folio_batch_init(&mut fbatch);
    loop {
        let page = sgt_iter_next_page(&mut sgt_iter);
        if page.is_null() {
            break;
        }
        let folio = page_folio(page);

        if folio == last {
            continue;
        }
        last = folio;
        if dirty {
            folio_mark_dirty(folio);
        }
        if backup {
            folio_mark_accessed(folio);
        }

        if folio_batch_add(&mut fbatch, folio) == 0 {
            check_release_folio_batch(&mut fbatch);
        }
    }
    if fbatch.nr != 0 {
        check_release_folio_batch(&mut fbatch);
    }

    sg_free_table(st);
}

// upstream: i915_gem_shmem.c shmem_sg_alloc_table()
pub unsafe fn shmem_sg_alloc_table(
    i915: *mut DrmI915Private,
    st: *mut SgTable,
    size: usize,
    mr: *mut IntelMemoryRegion,
    mapping: *mut AddressSpace,
    max_segment: u32,
) -> c_int {
    // `page_count` is restricted by sg_alloc_table's unsigned-int argument.
    if size / PAGE_SIZE > u32::MAX as usize {
        return -E2BIG;
    }

    let page_count = (size / PAGE_SIZE) as u32;
    // If there's no chance of allocating enough pages for the whole
    // object, bail early.
    if size as u64 > resource_size(&(*mr).region) {
        return -ENOMEM;
    }

    if sg_alloc_table(st, page_count, GFP_KERNEL | __GFP_NOWARN) != 0 {
        return -ENOMEM;
    }

    // Get the list of pages out of our struct file.  They'll be pinned
    // at this point until we release them.
    //
    // Fail silently without starting the shrinker
    mapping_set_unevictable(mapping);
    let mut noreclaim = mapping_gfp_constraint(mapping, !__GFP_RECLAIM);
    noreclaim |= __GFP_NORETRY | __GFP_NOWARN;

    let mut sg = (*sgt_layout(st)).sgl;
    (*sgt_layout(st)).nents = 0;
    let mut next_pfn: c_ulong = 0; // suppress gcc warning
    let mut i: c_ulong = 0;
    while i < page_count as c_ulong {
        let mut gfp = noreclaim;
        let shrink = [I915_SHRINK_BOUND | I915_SHRINK_UNBOUND, 0];
        let mut s = shrink.as_ptr();
        let folio: *mut Folio;

        loop {
            cond_resched();
            let candidate = shmem_read_folio_gfp(mapping, i, gfp);
            if !IS_ERR(candidate) {
                folio = candidate;
                break;
            }

            if unsafe { *s } == 0 {
                let mut ret = PTR_ERR(candidate);
                sg_mark_end(sg);
                if sg != (*sgt_layout(st)).sgl {
                    shmem_sg_free_table(st, mapping, false, false);
                } else {
                    mapping_clear_unevictable(mapping);
                    sg_free_table(st);
                }

                // shmemfs first checks if there is enough memory to allocate
                // the page and reports ENOSPC should there be insufficient,
                // along with the usual ENOMEM for a genuine allocation
                // failure. i915 uses ENOSPC for exhausted aperture space, so
                // translate shmemfs ENOSPC back to ENOMEM.
                if ret == -ENOSPC {
                    ret = -ENOMEM;
                }
                return ret;
            }

            i915_gem_shrink(
                ptr::null_mut(),
                i915,
                2 * page_count as c_ulong,
                ptr::null_mut(),
                *s,
            );
            s = s.add(1);

            // We've tried hard to allocate the memory by reaping our own
            // buffer, now let the real VM do its job and go down in flames if
            // truly OOM. However, graphics tend to be disposable, so defer the
            // oom here by reporting ENOMEM back to userspace.
            if unsafe { *s } == 0 {
                // reclaim and warn, but no oom
                gfp = mapping_gfp_mask(mapping);

                // Our BOs are always dirty and so we require kswapd to
                // reclaim our pages (direct reclaim does not effectively
                // begin pageout of our buffers on its own). Direct reclaim
                // only waits for kswapd when under allocation congestion,
                // making __GFP_RECLAIM unreliable here. Retry without
                // __GFP_NORETRY while still avoiding the OOM killer.
                gfp |= __GFP_RETRY_MAYFAIL | __GFP_NOWARN;
            }
        }

        let folio_pages = folio_nr_pages(folio) as c_ulong;
        let remaining = page_count as c_ulong - i;
        let segment_pages = (max_segment / PAGE_SIZE as u32) as c_ulong;
        let mut nr_pages = core::cmp::min(core::cmp::min(folio_pages, remaining), segment_pages);

        if i == 0 || (*sg_layout(sg)).length >= max_segment || folio_pfn(folio) != next_pfn {
            if i != 0 {
                sg = sg_next(sg);
            }

            (*sgt_layout(st)).nents += 1;
            sg_set_folio(sg, folio, nr_pages * PAGE_SIZE as c_ulong, 0);
        } else {
            nr_pages = core::cmp::min(
                nr_pages,
                ((max_segment - (*sg_layout(sg)).length) / PAGE_SIZE as u32) as c_ulong,
            );
            let append_len = (nr_pages * PAGE_SIZE as c_ulong) as u32;
            (*sg_layout(sg)).length = (*sg_layout(sg)).length.wrapping_add(append_len);
        }
        next_pfn = folio_pfn(folio).wrapping_add(nr_pages);
        i = i.wrapping_add(nr_pages.wrapping_sub(1));

        // Check that the i965g/gm workaround works.
        GEM_BUG_ON!(gfp & __GFP_DMA32 != 0 && next_pfn >= 0x0010_0000);
    }
    if !sg.is_null() {
        // loop terminated early; short sg table
        sg_mark_end(sg);
    }

    // Trim unused sg entries to avoid wasting memory.
    i915_sg_trim(st);

    0
}

// upstream: i915_gem_shmem.c shmem_get_pages()
unsafe extern "C" fn shmem_get_pages(obj: *mut DrmI915GemObject) -> c_int {
    let i915 = to_i915((*obj).base.base.dev);
    let mem = (*obj).mm.region as *mut IntelMemoryRegion;
    let mapping = (*object_file(obj)).f_mapping;
    let mut max_segment = i915_sg_segment_size((*i915).drm.dev);
    let mut st: *mut SgTable;
    let mut ret: c_int;

    // Assert that the object is not currently in any GPU domain. As it
    // wasn't in the GTT, there shouldn't be any way it could have been in
    // a GPU cache.
    GEM_BUG_ON!((*obj).read_domains as u32 & I915_GEM_GPU_DOMAINS != 0);
    GEM_BUG_ON!((*obj).write_domain as u32 & I915_GEM_GPU_DOMAINS != 0);

    loop {
        st = kmalloc_obj::<SgTable>(GFP_KERNEL | __GFP_NOWARN);
        if st.is_null() {
            return -ENOMEM;
        }

        ret = shmem_sg_alloc_table(i915, st, object_size(obj), mem, mapping, max_segment);
        if ret != 0 {
            // `shmem_sg_alloc_table` has already released any partial table.
            if ret == -ENOSPC {
                ret = -ENOMEM;
            }
            kfree(st.cast());
            return ret;
        }

        ret = i915_gem_gtt_prepare_pages(obj, st);
        if ret != 0 {
            // DMA remapping failed? One possible cause is that it could not
            // reserve enough large entries, asking for PAGE_SIZE chunks
            // instead may help.
            if max_segment > PAGE_SIZE as u32 {
                shmem_sg_free_table(st, mapping, false, false);
                kfree(st.cast());

                max_segment = PAGE_SIZE as u32;
                continue;
            }

            dev_warn(
                (*i915).drm.dev,
                "Failed to DMA remap %zu pages\n",
                object_size(obj) >> PAGE_SHIFT,
            );
            shmem_sg_free_table(st, mapping, false, false);
            if ret == -ENOSPC {
                ret = -ENOMEM;
            }
            kfree(st.cast());
            return ret;
        }

        if i915_gem_object_needs_bit17_swizzle(obj) {
            i915_gem_object_do_bit_17_swizzle(obj, st);
        }

        if i915_gem_object_can_bypass_llc(obj) {
            i915_gem_object_set_cache_dirty(obj, true);
        }

        __i915_gem_object_set_pages(obj, st);

        return 0;
    }
}

// upstream: i915_gem_shmem.c shmem_truncate()
unsafe extern "C" fn shmem_truncate(obj: *mut DrmI915GemObject) -> c_int {
    // Our goal here is to return as much of the memory as is possible back
    // to the system as we are called from OOM. To do this we must instruct
    // the shmfs to drop all of its backing pages, *now*.
    shmem_truncate_range(file_inode(object_file(obj)), 0, -1);
    set_object_madv(obj, __I915_MADV_PURGED);
    (*obj).mm.pages = ERR_PTR(-EFAULT);

    0
}

// upstream: i915_gem_shmem.c __shmem_writeback()
pub unsafe fn __shmem_writeback(size: usize, mapping: *mut AddressSpace) {
    let mut wbc = WritebackControl {
        nr_to_write: SWAP_CLUSTER_MAX,
        pages_skipped: 0,
        range_start: 0,
        range_end: i64::MAX as u64,
        sync_mode: WB_SYNC_NONE,
        _flags: 0,
        fbatch: FolioBatch {
            nr: 0,
            i: 0,
            percpu_pvec_drained: false,
            _pad: [0; 5],
            folios: [ptr::null_mut(); 15],
        },
        index: 0,
        saved_err: 0,
        _cgroup_writeback: [0; 64],
    };
    let mut folio: *mut Folio = ptr::null_mut();
    let mut error: c_int = 0;

    // Leave mmapings intact (GTT will have been revoked on unbinding,
    // leaving only CPU mmapings around) and add those folios to the LRU
    // instead of invoking writeback so they are aged and paged out as
    // normal.
    loop {
        folio = writeback_iter(mapping, &mut wbc, folio, &mut error);
        if folio.is_null() {
            break;
        }
        if folio_mapped(folio) {
            folio_redirty_for_writepage(&mut wbc, folio);
        } else {
            error = shmem_writeout(folio, ptr::null_mut(), ptr::null_mut());
        }
    }
    let _ = size;
}

// upstream: i915_gem_shmem.c shmem_writeback()
unsafe fn shmem_writeback(obj: *mut DrmI915GemObject) {
    __shmem_writeback(object_size(obj), (*object_file(obj)).f_mapping);
}

// upstream: i915_gem_shmem.c shmem_shrink()
unsafe extern "C" fn shmem_shrink(obj: *mut DrmI915GemObject, flags: u32) -> c_int {
    match object_madv(obj) {
        I915_MADV_DONTNEED => return i915_gem_object_truncate(obj),
        __I915_MADV_PURGED => return 0,
        _ => {}
    }

    if flags & I915_GEM_OBJECT_SHRINK_WRITEBACK != 0 {
        shmem_writeback(obj);
    }

    0
}

// upstream: i915_gem_shmem.c __i915_gem_object_release_shmem()
pub unsafe fn __i915_gem_object_release_shmem(
    obj: *mut DrmI915GemObject,
    pages: *mut SgTable,
    needs_clflush: bool,
) {
    let i915 = to_i915((*obj).base.base.dev);

    GEM_BUG_ON!(object_madv(obj) == __I915_MADV_PURGED);

    if object_madv(obj) == I915_MADV_DONTNEED {
        set_object_dirty(obj, false);
    }

    if needs_clflush
        && ((*obj).read_domains as u32 & I915_GEM_DOMAIN_CPU) == 0
        && (i915_gem_object_cache_coherent(obj) & I915_BO_CACHE_COHERENT_FOR_READ) == 0
    {
        drm_clflush_sg(pages);
    }

    __start_cpu_write(obj);
    // On non-LLC igfx platforms, force the flush-on-acquire if this is ever
    // swapped-in. Our async flush path is not trustworthy enough yet (and
    // happens in the wrong order), and userspace can conceivably change the
    // cache level to I915_CACHE_NONE after swapin, racing execbuf's bind and
    // async flush.
    if !HAS_LLC(i915) && !IS_DGFX(i915) {
        i915_gem_object_set_cache_dirty(obj, true);
    }
}

// upstream: i915_gem_shmem.c i915_gem_object_put_pages_shmem()
pub unsafe fn i915_gem_object_put_pages_shmem(obj: *mut DrmI915GemObject, pages: *mut SgTable) {
    __i915_gem_object_release_shmem(obj, pages, true);

    i915_gem_gtt_finish_pages(obj, pages);

    if i915_gem_object_needs_bit17_swizzle(obj) {
        i915_gem_object_save_bit_17_swizzle(obj, pages);
    }

    shmem_sg_free_table(
        pages,
        file_inode_mapping(object_file(obj)),
        object_dirty(obj),
        object_madv(obj) == I915_MADV_WILLNEED,
    );
    kfree(pages.cast());
    set_object_dirty(obj, false);
}

// upstream: i915_gem_shmem.c shmem_put_pages()
unsafe extern "C" fn shmem_put_pages(obj: *mut DrmI915GemObject, pages: *mut SgTable) {
    if likely(i915_gem_object_has_struct_page(obj)) {
        i915_gem_object_put_pages_shmem(obj, pages);
    } else {
        i915_gem_object_put_pages_phys(obj, pages);
    }
}

// upstream: i915_gem_shmem.c shmem_pwrite()
unsafe extern "C" fn shmem_pwrite(
    obj: *mut DrmI915GemObject,
    arg: *const DrmI915GemPwrite,
) -> c_int {
    let user_data = u64_to_user_ptr((*arg).data_ptr).cast::<c_char>();
    let file = object_file(obj);
    let mut kiocb = Kiocb {
        ki_filp: ptr::null_mut(),
        ki_pos: 0,
        ki_complete: None,
        private: ptr::null_mut(),
        ki_flags: 0,
        ki_ioprio: 0,
        ki_write_stream: 0,
        _pad: 0,
        ki_waitq: ptr::null_mut(),
    };
    let mut iter = IovIter { _opaque: [0; 8] };
    let mut written: isize;
    let size = (*arg).size;

    // Caller already validated user args.
    GEM_BUG_ON!(!access_ok(user_data, (*arg).size as usize));

    if !i915_gem_object_has_struct_page(obj) {
        return i915_gem_object_pwrite_phys(obj, arg);
    }

    // Before we instantiate/pin the backing store, prepopulate shmemfs's
    // pagecache efficiently. This avoids instantiating every page when the
    // user writes only a few pages and never uses the object on the GPU, and
    // allows direct overwrite without retrieving or clearing the old page.
    if i915_gem_object_has_pages(obj) {
        return -ENODEV;
    }

    if object_madv(obj) != I915_MADV_WILLNEED {
        return -EFAULT;
    }

    if size > MAX_RW_COUNT {
        return -EFBIG;
    }

    if (*(*file).f_op).write_iter.is_none() {
        return -EINVAL;
    }

    init_sync_kiocb(&mut kiocb, file);
    kiocb.ki_pos = (*arg).offset as i64;
    iov_iter_ubuf(
        &mut iter,
        ITER_SOURCE,
        user_data.cast::<c_void>(),
        size as usize,
    );

    written = (*(*file).f_op).write_iter.unwrap()(&mut kiocb, &mut iter);
    BUG_ON!(written == -(EIOCBQUEUED as isize));

    // Preserve the real write_iter error before checking for a short write.
    if written < 0 {
        return written as c_int;
    }
    if written as u64 != size {
        return -EIO;
    }

    0
}

// upstream: i915_gem_shmem.c shmem_pread()
unsafe extern "C" fn shmem_pread(obj: *mut DrmI915GemObject, arg: *const DrmI915GemPread) -> c_int {
    if !i915_gem_object_has_struct_page(obj) {
        return i915_gem_object_pread_phys(obj, arg);
    }

    -ENODEV
}

// upstream: i915_gem_shmem.c shmem_release()
unsafe extern "C" fn shmem_release(obj: *mut DrmI915GemObject) {
    if i915_gem_object_has_struct_page(obj) {
        i915_gem_object_release_memory_region(obj);
    }

    fput(object_file(obj));
}

// The operation-table record and callback ABI come from the canonical
// i915_gem_object_types.h owner.
// SAFETY: the ops table is immutable after initialization and contains only
// immutable function pointers and a static string pointer.
unsafe impl Sync for DrmI915GemObjectOps {}

pub static i915_gem_shmem_ops: DrmI915GemObjectOps = DrmI915GemObjectOps {
    flags: I915_GEM_OBJECT_IS_SHRINKABLE,
    get_pages: Some(shmem_get_pages),
    put_pages: Some(shmem_put_pages),
    truncate: Some(shmem_truncate),
    shrink: Some(shmem_shrink),
    pread: Some(shmem_pread),
    pwrite: Some(shmem_pwrite),
    mmap_offset: None,
    unmap_virtual: None,
    dmabuf_export: None,
    adjust_lru: None,
    delayed_free: None,
    migrate: None,
    release: Some(shmem_release),
    mmap_ops: ptr::null(),
    name: b"i915_gem_object_shmem\0".as_ptr().cast::<c_char>(),
};

// upstream: i915_gem_shmem.c __create_shmem()
unsafe fn __create_shmem(
    i915: *mut DrmI915Private,
    obj: *mut DrmGemObject,
    size: u64,
    flags: u32,
) -> c_int {
    let shmem_flags: c_ulong = 1 << VMA_NORESERVE_BIT;
    let mut huge_mnt: *mut VfsMount;
    let filp: *mut File;

    drm_gem_private_object_init(&mut (*i915).drm, obj, size);

    // __shmem_file_setup() returns -EINVAL when size exceeds MAX_LFS_FILESIZE.
    // Match other i915 size checks by returning -E2BIG for oversized objects.
    // On 32-bit, size > MAX_LFS_FILESIZE is impossible, and the usual
    // i915_gem_object_size_2big() check runs before this init_object callback.
    if BITS_PER_LONG == 64 && size > MAX_LFS_FILESIZE {
        return -E2BIG;
    }

    huge_mnt = drm_gem_get_huge_mnt(&mut (*i915).drm);
    let created = if flags & I915_BO_ALLOC_NOTHP == 0 && !huge_mnt.is_null() {
        shmem_file_setup_with_mnt(huge_mnt, b"i915\0".as_ptr().cast(), size, shmem_flags)
    } else {
        shmem_file_setup(b"i915\0".as_ptr().cast(), size, shmem_flags)
    };
    if IS_ERR(created) {
        return PTR_ERR(created);
    }
    filp = created;

    // Prevent -EFBIG for large writes beyond MAX_NON_LFS by setting
    // O_LARGEFILE on shmem objects.
    if force_o_largefile() {
        (*filp).f_flags |= O_LARGEFILE as u32;
    }

    (*obj).filp = filp;
    0
}

// upstream: i915_gem_shmem.c shmem_object_init()
unsafe fn shmem_object_init(
    mem: *mut IntelMemoryRegion,
    obj: *mut DrmI915GemObject,
    offset: u64,
    size: u64,
    page_size: u64,
    flags: u32,
) -> c_int {
    static mut LOCK_CLASS: LockClassKey = LockClassKey { _opaque: [0; 0] };
    let i915 = (*mem).i915;
    let mut mapping: *mut AddressSpace;
    let mut cache_level: u32;
    let mut mask: u32;
    let ret = __create_shmem(
        i915,
        ptr::addr_of_mut!((*obj).base).cast::<DrmGemObject>(),
        size,
        flags,
    );
    if ret != 0 {
        return ret;
    }

    mask = GFP_HIGHUSER | __GFP_RECLAIMABLE;
    if IS_I965GM(i915) || IS_I965G(i915) {
        // 965gm cannot relocate objects above 4GiB.
        mask &= !__GFP_HIGHMEM;
        mask |= __GFP_DMA32;
    }

    mapping = (*object_file(obj)).f_mapping;
    mapping_set_gfp_mask(mapping, mask);
    GEM_BUG_ON!(mapping_gfp_mask(mapping) & __GFP_RECLAIM == 0);

    i915_gem_object_init(
        obj,
        &i915_gem_shmem_ops,
        ptr::addr_of_mut!(LOCK_CLASS),
        flags,
    );
    (*obj).mem_flags |= I915_BO_FLAG_STRUCT_PAGE;
    (*obj).write_domain = I915_GEM_DOMAIN_CPU as u16;
    (*obj).read_domains = I915_GEM_DOMAIN_CPU as u16;

    // MTL does not snoop the CPU cache by default for GPU access (one-way
    // coherency), but existing userspace depends on snooping. Default to
    // one-way coherent on MTL; GEM_CREATE will gain an explicit setting later.
    if HAS_LLC(i915) || GRAPHICS_VER_FULL(i915) >= IP_VER(12, 70) as u32 {
        // LLC caching can improve performance by roughly 10% over uncached
        // access. Non-display graphics accesses are CPU coherent; display
        // planes remain UC and are rebound when first used as such.
        cache_level = I915_CACHE_LLC;
    } else {
        cache_level = I915_CACHE_NONE;
    }

    i915_gem_object_set_cache_coherency(obj, cache_level);

    i915_gem_object_init_memory_region(obj, mem);
    let _ = (offset, page_size);

    0
}

// upstream: i915_gem_shmem.c i915_gem_object_create_shmem()
pub unsafe fn i915_gem_object_create_shmem(
    i915: *mut DrmI915Private,
    size: u64,
) -> *mut DrmI915GemObject {
    i915_gem_object_create_region((*i915).mm.regions[INTEL_REGION_SMEM], size, 0, 0)
}

// upstream: i915_gem_shmem.c i915_gem_object_create_shmem_from_data()
pub unsafe fn i915_gem_object_create_shmem_from_data(
    i915: *mut DrmI915Private,
    data: *const c_void,
    size: u64,
) -> *mut DrmI915GemObject {
    let mut obj: *mut DrmI915GemObject;
    let file: *mut File;
    let mut pos: i64 = 0;
    let mut err: isize;

    GEM_WARN_ON!(IS_DGFX(i915));
    obj = i915_gem_object_create_shmem(i915, round_up(size, PAGE_SIZE as u64));
    if IS_ERR(obj) {
        return obj;
    }

    GEM_BUG_ON!((*obj).write_domain as u32 != I915_GEM_DOMAIN_CPU);

    file = object_file(obj);
    err = kernel_write(file, data, size as usize, &mut pos);

    if err < 0 {
        i915_gem_object_put(obj);
        return ERR_PTR(err as c_int);
    }

    if err as u64 != size {
        i915_gem_object_put(obj);
        return ERR_PTR(-EIO);
    }

    obj
}

// upstream: i915_gem_shmem.c init_shmem()
unsafe fn init_shmem(mem: *mut IntelMemoryRegion) -> c_int {
    let i915 = (*mem).i915;

    // By creating our own shmemfs mountpoint, pass mount flags that better
    // match this use case. Huge pages can help on supported platforms and can
    // offset IOMMU lookup overhead, although some older platforms otherwise
    // regress on slow reads.
    if GRAPHICS_VER(i915) >= 11 || i915_vtd_active(i915) {
        drm_gem_huge_mnt_create(&mut (*i915).drm, b"within_size\0".as_ptr().cast());
        if !drm_gem_get_huge_mnt(&mut (*i915).drm).is_null() {
            drm_info(
                &mut (*i915).drm,
                b"Using Transparent Hugepages\n\0".as_ptr().cast(),
            );
        } else {
            drm_notice(
                &mut (*i915).drm,
                b"Transparent Hugepage support is recommended for optimal performance%s\n\0"
                    .as_ptr()
                    .cast(),
                if GRAPHICS_VER(i915) >= 11 {
                    b" on this platform!\0".as_ptr().cast()
                } else {
                    b" when IOMMU is enabled!\0".as_ptr().cast()
                },
            );
        }
    }

    intel_memory_region_set_name(mem, b"system\0".as_ptr().cast());

    0 // Fall back to the kernel mount if huge mount creation failed.
}

// Source order follows struct intel_memory_region_ops in intel_memory_region.h.
#[repr(C)]
struct IntelMemoryRegionOps {
    init: Option<unsafe fn(*mut IntelMemoryRegion) -> c_int>,
    release: Option<unsafe fn(*mut IntelMemoryRegion) -> c_int>,
    init_object: Option<
        unsafe fn(*mut IntelMemoryRegion, *mut DrmI915GemObject, u64, u64, u64, u32) -> c_int,
    >,
}

// SAFETY: this ops table is immutable and contains only function pointers.
unsafe impl Sync for IntelMemoryRegionOps {}

static shmem_region_ops: IntelMemoryRegionOps = IntelMemoryRegionOps {
    init: Some(init_shmem),
    release: None,
    init_object: Some(shmem_object_init),
};

// upstream: i915_gem_shmem.c i915_gem_shmem_setup()
pub unsafe fn i915_gem_shmem_setup(
    i915: *mut DrmI915Private,
    type_: u16,
    instance: u16,
) -> *mut IntelMemoryRegion {
    intel_memory_region_create(
        i915,
        0,
        totalram_pages() << PAGE_SHIFT,
        PAGE_SIZE as u64,
        0,
        0,
        type_,
        instance,
        &shmem_region_ops,
    )
}

// upstream: i915_gem_shmem.c i915_gem_object_is_shmem()
pub unsafe fn i915_gem_object_is_shmem(obj: *const DrmI915GemObject) -> bool {
    (*obj).ops == ptr::addr_of!(i915_gem_shmem_ops).cast::<c_void>()
}
