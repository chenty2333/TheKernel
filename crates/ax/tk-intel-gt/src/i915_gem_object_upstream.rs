// SPDX-License-Identifier: MIT
// Copyright © 2017 Intel Corporation.
//
//! Source-order Rust transcription of Linux v7.2.3
//! `drivers/gpu/drm/i915/gem/i915_gem_object.c`.
//!
//! GEM allocation, lifetime, mmap, region, cache, and reservation operations
//! are integration bindings. This translation retains the upstream branch,
//! lock, reference, and error ordering rather than emulating those services.

use core::ffi::{c_int, c_ulong, c_void};

use crate::{
    i915_drm_client_upstream::i915_drm_client_remove_object,
    i915_gem_core_upstream::{io_mapping_map_wc, io_mapping_unmap},
    i915_gem_mman_upstream::i915_gem_object_release_mmap_gtt,
    linux::gem_memory::{INTEL_MEMORY_LOCAL, INTEL_MEMORY_SYSTEM},
    i915_gem_context_upstream::{i915_gem_context_get, i915_gem_context_put, i915_lut_handle_free},
    i915_gem_context_types_upstream::DrmI915FilePrivate,
    i915_gem_object_api_upstream::{
        i915_gem_object_has_pages, i915_gem_object_has_pinned_pages, i915_gem_object_lock,
        i915_gem_object_put, i915_gem_object_unlock, i915_gem_object_unpin_map,
    },
    i915_gem_object_header_upstream::{
        assert_object_held, assert_object_held_shared, i915_gem_object_flush_map,
    },
    i915_gem_pages_upstream::{
        i915_gem_object_pin_map, i915_gem_object_release_mmap_offset, radix_tree_delete,
        __i915_gem_object_put_pages, __i915_gem_object_get_page as i915_gem_object_get_page,
        __i915_gem_object_get_dma_address as i915_gem_object_get_dma_address,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, DrmI915GemObjectOps, I915LutHandle},
    i915_vma_api_upstream::*,
    intel_gtt_api_upstream::i915_vm_resv_put,
    intel_context_upstream::*,
    intel_engine_cs_upstream::*,
    linux::{
        i915_trace::trace_i915_gem_object_destroy,
        bitmap::bitmap_free,
        highmem::{kmap_local_page, kunmap_local},
        config::*,
        fields::{i915_gem_object_is_framebuffer, i915_gem_object_pat_set_by_user},
        gem::{
            dma_resv_fini, dma_resv_get_singleton, dma_resv_wait_timeout, drm_gem_is_imported,
            DrmFile, DrmGemObject, DmaResv, TtmBufferObjectLayout, TtmResource,
            drm_vma_offset_remove, i915_gem_to_ttm, i915_ttm_resource_visible_size,
        },
        gem_memory::*,
        heap::*,
        i915::{
            HAS_FLAT_CCS, HAS_LLC, INTEL_INFO, IS_DGFX, IS_ELKHARTLAKE, IS_JASPERLAKE, to_i915,
        },
        iosys_map::{iosys_map_set_vaddr, IosysMap},
        list::*,
        locks::*,
        memory::*,
        mutex::*,
        rbtree::*,
        rcu::*,
        workqueue::*,
    },
    linux_i915_private::{DrmDevicePrefix, DrmI915Private},
};

// Linux DRM API entry points whose C implementations live below this GEM
// translation; keep their source signatures at this integration boundary.
unsafe extern "C" {
    fn drm_vma_node_revoke(node: *mut DrmVmaOffsetNode, file: *mut DrmFile);
    fn drm_prime_gem_destroy(obj: *mut DrmGemObject, sg: *mut SgTable);
    fn drm_gem_free_mmap_offset(obj: *mut DrmGemObject);
    fn __cond_resched_lock(lock: *mut Spinlock) -> c_int;
    fn i915_memcpy_from_wc(dst: *mut c_void, src: *const c_void, len: c_ulong) -> bool;
    fn memcpy_fromio(dst: *mut c_void, src: *const c_void, len: usize);
}

#[inline]
fn cond_resched_lock(lock: &mut Spinlock) -> bool {
    unsafe { __cond_resched_lock(lock) != 0 }
}

// Linux i915 GEM allocation cache and the private GEM object callback table.
// The table layout is supplied by the surrounding DRM integration binding.
static mut slab_objects: *mut c_void = core::ptr::null_mut();

// Adjacent source C bitfields in `drm_i915_gem_object` are represented by
// one u32 in the canonical owner. Update only each field's own bit range.
#[inline]
pub(crate) unsafe fn object_pat_index(obj: *const DrmI915GemObject) -> u32 {
    ((*obj).cache_state_bits & 0x3f) as u32
}

#[inline]
unsafe fn object_set_pat_index(obj: *mut DrmI915GemObject, index: u32) {
    let bits = &mut (*obj).cache_state_bits;
    *bits = (*bits & !0x3f) | (index & 0x3f);
}

#[inline]
unsafe fn object_cache_coherent(obj: *const DrmI915GemObject) -> u32 {
    (((*obj).cache_state_bits >> 7) & 0x3) as u32
}

#[inline]
unsafe fn object_set_cache_coherent(obj: *mut DrmI915GemObject, coherent: u32) {
    let bits = &mut (*obj).cache_state_bits;
    *bits = (*bits & !(0x3 << 7)) | ((coherent & 0x3) << 7);
}

#[inline]
unsafe fn object_set_cache_dirty(obj: *mut DrmI915GemObject, dirty: bool) {
    let bits = &mut (*obj).cache_state_bits;
    if dirty {
        *bits |= 1 << 9;
    } else {
        *bits &= !(1 << 9);
    }
}

#[inline]
unsafe fn object_madv(obj: *const DrmI915GemObject) -> u32 {
    (*obj).mm.madv()
}

#[inline]
unsafe fn object_set_madv(obj: *mut DrmI915GemObject, madv: u32) {
    (*obj).mm.set_madv(madv);
}

#[inline]
unsafe fn to_intel_bo(gem: *mut DrmGemObject) -> *mut DrmI915GemObject {
    // The source `base` member is at offset zero; container_of also preserves
    // the source to_intel_bo(NULL) == NULL invariant.
    container_of!(gem, DrmI915GemObject, base)
}

// Linux's rb_first_postorder()/rb_next_postorder() iterator contract, used by
// both source postorder loops below. `next` is sampled before the callback so
// callers may remove/free the current containing record exactly as upstream.
unsafe fn rb_first_postorder(root: *mut RbRoot) -> *mut RbNode {
    let mut node = (*root).node;
    if node.is_null() {
        return core::ptr::null_mut();
    }
    loop {
        if !(*node).left.is_null() {
            node = (*node).left;
        } else if !(*node).right.is_null() {
            node = (*node).right;
        } else {
            return node;
        }
    }
}

unsafe fn rb_next_postorder(node: *mut RbNode) -> *mut RbNode {
    if node.is_null() {
        return core::ptr::null_mut();
    }
    let parent = ((*node).parent_color & !3) as *mut RbNode;
    if parent.is_null() || core::ptr::eq(node, (*parent).right) || (*parent).right.is_null() {
        parent
    } else {
        let mut next = (*parent).right;
        loop {
            if !(*next).left.is_null() {
                next = (*next).left;
            } else if !(*next).right.is_null() {
                next = (*next).right;
            } else {
                return next;
            }
        }
    }
}

unsafe fn rbtree_postorder_for_each_entry_safe<T>(
    root: *mut RbRoot,
    member_offset: usize,
    mut body: impl FnMut(*mut T, *mut T),
) {
    let mut node = rb_first_postorder(root);
    while !node.is_null() {
        let next_node = rb_next_postorder(node);
        let entry = node.wrapping_sub(member_offset) as *mut T;
        let next = if next_node.is_null() {
            core::ptr::null_mut()
        } else {
            next_node.wrapping_sub(member_offset) as *mut T
        };
        body(entry, next);
        node = next_node;
    }
}

// upstream: i915_gem_object.c i915_gem_get_pat_index()
pub unsafe fn i915_gem_get_pat_index(i915: *mut DrmI915Private, level: u32) -> u32 {
    if drm_WARN_ON!(
        core::ptr::addr_of_mut!((*i915).drm),
        level >= I915_MAX_CACHE_LEVEL,
    ) {
        return 0;
    }

    (*INTEL_INFO(i915)).cachelevel_to_pat[level as usize]
}

// upstream: i915_gem_object.c i915_gem_object_has_cache_level()
pub unsafe fn i915_gem_object_has_cache_level(obj: *const DrmI915GemObject, lvl: u32) -> bool {
    // In case the pat_index is set by user space, this kernel mode
    // driver should leave the coherency to be managed by user space,
    // simply return true here.
    if i915_gem_object_pat_set_by_user(obj) {
        return true;
    }

    // Otherwise compare the PAT index converted from cache_level.
    object_pat_index(obj) == i915_gem_get_pat_index(to_i915((&(*obj).base.base).dev), lvl)
}

// upstream: i915_gem_object.c i915_gem_object_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_alloc() -> *mut DrmI915GemObject {
    let obj = kmem_cache_zalloc(slab_objects.cast::<KmCache>(), GFP_KERNEL) as *mut DrmI915GemObject;
    if obj.is_null() {
        return core::ptr::null_mut();
    }
    core::ops::DerefMut::deref_mut(&mut (*obj).base.base).funcs =
        core::ptr::addr_of!(i915_gem_object_funcs).cast();

    obj
}

// upstream: i915_gem_object.c i915_gem_object_free()
pub unsafe fn i915_gem_object_free(obj: *mut DrmI915GemObject) {
    kmem_cache_free(slab_objects.cast::<KmCache>(), obj.cast());
}

// upstream: i915_gem_object.c i915_gem_object_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_init(
    obj: *mut DrmI915GemObject,
    ops: *const DrmI915GemObjectOps,
    key: *mut LockClassKey,
    flags: u32,
) {
    // The GEM base is embedded in both TTM and i915 objects and must alias.
    BUILD_BUG_ON!(
        core::mem::offset_of!(DrmI915GemObject, base)
            != core::mem::offset_of!(TtmBufferObjectLayout, base)
    );

    spin_lock_init(&mut (*obj).vma.lock);
    INIT_LIST_HEAD!(core::ptr::addr_of_mut!((*obj).vma.list));

    INIT_LIST_HEAD!(core::ptr::addr_of_mut!((*obj).mm.link));

    #[cfg(CONFIG_PROC_FS)]
    INIT_LIST_HEAD!(core::ptr::addr_of_mut!((*obj).client_link));

    INIT_LIST_HEAD!(core::ptr::addr_of_mut!((*obj).lut_list));
    spin_lock_init(&mut (*obj).lut_lock);

    spin_lock_init(&mut (*obj).mmo.lock);
    (*obj).mmo.offsets = RB_ROOT;

    unsafe {
        init_rcu_head(&mut *core::ptr::addr_of_mut!((*obj).rcu_or_freed.rcu).cast::<RcuHead>());
    }

    (*obj).ops = ops;
    GEM_BUG_ON!(flags & !I915_BO_ALLOC_FLAGS != 0);
    (*obj).flags = flags as c_ulong;

    object_set_madv(obj, I915_MADV_WILLNEED);
    INIT_RADIX_TREE!(&mut (*obj).mm.get_page.radix, GFP_KERNEL | __GFP_NOWARN);
    mutex_init(&mut (*obj).mm.get_page.lock);
    INIT_RADIX_TREE!(&mut (*obj).mm.get_dma_page.radix, GFP_KERNEL | __GFP_NOWARN);
    mutex_init(&mut (*obj).mm.get_dma_page.lock);
    let _ = key;
}

// upstream: i915_gem_object.c __i915_gem_object_fini()
pub unsafe fn __i915_gem_object_fini(obj: *mut DrmI915GemObject) {
    mutex_destroy(&mut (*obj).mm.get_page.lock);
    mutex_destroy(&mut (*obj).mm.get_dma_page.lock);
    dma_resv_fini(core::ptr::addr_of_mut!(
        core::ops::DerefMut::deref_mut(&mut (*obj).base.base)._resv
    ).cast());
}

// upstream: i915_gem_object.c i915_gem_object_set_cache_coherency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_set_cache_coherency(obj: *mut DrmI915GemObject, cache_level: u32) {
    let i915 = to_i915((&(*obj).base.base).dev);

    object_set_pat_index(obj, i915_gem_get_pat_index(i915, cache_level));

    if cache_level != I915_CACHE_NONE {
        object_set_cache_coherent(
            obj,
            I915_BO_CACHE_COHERENT_FOR_READ | I915_BO_CACHE_COHERENT_FOR_WRITE,
        );
    } else if HAS_LLC(i915) {
        object_set_cache_coherent(obj, I915_BO_CACHE_COHERENT_FOR_READ);
    } else {
        object_set_cache_coherent(obj, 0);
    }

    object_set_cache_dirty(
        obj,
        object_cache_coherent(obj) & I915_BO_CACHE_COHERENT_FOR_WRITE == 0 && !IS_DGFX(i915),
    );
}

// upstream: i915_gem_object.c i915_gem_object_set_pat_index()
pub unsafe fn i915_gem_object_set_pat_index(obj: *mut DrmI915GemObject, pat_index: u32) {
    let i915 = to_i915((&(*obj).base.base).dev);

    if object_pat_index(obj) == pat_index {
        return;
    }

    object_set_pat_index(obj, pat_index);

    if pat_index != i915_gem_get_pat_index(i915, I915_CACHE_NONE) {
        object_set_cache_coherent(
            obj,
            I915_BO_CACHE_COHERENT_FOR_READ | I915_BO_CACHE_COHERENT_FOR_WRITE,
        );
    } else if HAS_LLC(i915) {
        object_set_cache_coherent(obj, I915_BO_CACHE_COHERENT_FOR_READ);
    } else {
        object_set_cache_coherent(obj, 0);
    }

    object_set_cache_dirty(
        obj,
        object_cache_coherent(obj) & I915_BO_CACHE_COHERENT_FOR_WRITE == 0 && !IS_DGFX(i915),
    );
}

// upstream: i915_gem_object.c i915_gem_object_can_bypass_llc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_gem_object_can_bypass_llc(obj: *mut DrmI915GemObject) -> bool {
    let i915 = to_i915((&(*obj).base.base).dev);

    // This is purely from a security perspective, so ignore non-user objects.
    if (*obj).flags & I915_BO_ALLOC_USER as c_ulong == 0 {
        return false;
    }

    // Always flush cache for UMD objects at creation time.
    if i915_gem_object_pat_set_by_user(obj) {
        return true;
    }

    // EHL and JSL expose the Bypass LLC MOCS entry.
    IS_JASPERLAKE(i915) || IS_ELKHARTLAKE(i915)
}

// upstream: i915_gem_object.c i915_gem_close_object()
unsafe extern "C" fn i915_gem_close_object(gem: *mut DrmGemObject, file: *mut DrmFile) {
    let obj = to_intel_bo(gem);
    let fpriv = (*file).driver_priv;
    let mut bookmark: I915LutHandle = core::mem::zeroed();
    let mut close: ListHead = core::mem::zeroed();
    INIT_LIST_HEAD!(&mut close);

    spin_lock(&mut (*obj).lut_lock);
    let head = core::ptr::addr_of_mut!((*obj).lut_list);
    let member_offset = core::mem::offset_of!(I915LutHandle, obj_link);
    let mut cursor = (*head).next;
    while cursor != head {
        let next_link = (*cursor).next;
        let lut = cursor.wrapping_sub(member_offset) as *mut I915LutHandle;
        let ctx = (*lut).ctx;

        if !ctx.is_null() && (*ctx).file_priv == fpriv.cast::<DrmI915FilePrivate>() {
            i915_gem_context_get(ctx);
            list_move(&mut (*lut).obj_link, &mut close);
        }

        // Break long locks, and carefully continue on from this spot.
        if next_link != head {
            list_add_tail(&mut bookmark.obj_link, next_link);
            let mut resume = next_link;
            if cond_resched_lock(&mut (*obj).lut_lock) {
                resume = bookmark.obj_link.next;
            }
            __list_del_entry(&mut bookmark.obj_link);
            cursor = resume;
        } else {
            cursor = next_link;
        }
    }
    spin_unlock(&mut (*obj).lut_lock);

    spin_lock(&mut (*obj).mmo.lock);
    let mmo_offset = core::mem::offset_of!(I915MmapOffset, offset);
    rbtree_postorder_for_each_entry_safe(
        &mut (*obj).mmo.offsets,
        mmo_offset,
        |mmo: *mut I915MmapOffset, _mn| {
            drm_vma_node_revoke(&mut (*mmo).vma_node, file);
        },
    );
    spin_unlock(&mut (*obj).mmo.lock);

    let mut lut: *mut I915LutHandle = core::ptr::null_mut();
    let mut ln: *mut I915LutHandle = core::ptr::null_mut();
    list_for_each_entry_safe!(lut, ln, &mut close, obj_link, {
        let ctx = (*lut).ctx;
        let mut vma: *mut I915Vma;

        // flink/open may give one process multiple handles to the same VMA.
        mutex_lock(&mut (*ctx).lut_mutex);
        vma = radix_tree_delete(&mut (*ctx).handles_vma, u64::from((*lut).handle)) as *mut I915Vma;
        if !vma.is_null() {
            GEM_BUG_ON!((*vma).obj != obj);
            GEM_BUG_ON!(atomic_read(&(*vma).open_count) == 0);
            i915_vma_close(vma);
        }
        mutex_unlock(&mut (*ctx).lut_mutex);

        i915_gem_context_put((*lut).ctx);
        i915_lut_handle_free(lut);
        i915_gem_object_put(obj);
    });
}

// upstream: i915_gem_object.c __i915_gem_free_object_rcu()
pub unsafe extern "C" fn __i915_gem_free_object_rcu(head: *mut RcuHead) {
    let obj = container_of!(head, DrmI915GemObject, rcu_or_freed.rcu);
    let i915 = to_i915((&(*obj).base.base).dev);

    // Keep placement storage alive for RCU reads from fdinfo.
    if (*obj).mm.n_placements > 1 {
        kfree((*obj).mm.placements.cast::<c_void>());
    }

    i915_gem_object_free(obj);

    GEM_BUG_ON!(atomic_read(&(*i915).mm.free_count) == 0);
    atomic_dec(&mut (*i915).mm.free_count);
}

// upstream: i915_gem_object.c __i915_gem_object_free_mmaps()
unsafe fn __i915_gem_object_free_mmaps(obj: *mut DrmI915GemObject) {
    // Skip serialisation and waking the device if known not to be used.
    if (*obj).userfault_count != 0 && !IS_DGFX(to_i915((&(*obj).base.base).dev)) {
        i915_gem_object_release_mmap_gtt(obj);
    }

    if !RB_EMPTY_ROOT(&(*obj).mmo.offsets) {
        i915_gem_object_release_mmap_offset(obj);

        let mmo_offset = core::mem::offset_of!(I915MmapOffset, offset);
        rbtree_postorder_for_each_entry_safe(
            &mut (*obj).mmo.offsets,
            mmo_offset,
            |mmo: *mut I915MmapOffset, _mn| {
                drm_vma_offset_remove(
                    (*(&(*obj).base.base).dev.cast::<DrmDevicePrefix>()).vma_offset_manager,
                    &mut (*mmo).vma_node,
                );
                kfree(mmo.cast::<c_void>());
            },
        );
        (*obj).mmo.offsets = RB_ROOT;
    }
}

// upstream: i915_gem_object.c __i915_gem_object_pages_fini()
pub unsafe fn __i915_gem_object_pages_fini(obj: *mut DrmI915GemObject) {
    assert_object_held_shared(obj);

    if !list_empty(&(*obj).vma.list) {
        let mut vma: *mut I915Vma;

        spin_lock(&mut (*obj).vma.lock);
        loop {
            vma = list_first_entry_or_null!(&mut (*obj).vma.list, I915Vma, obj_link);
            if vma.is_null() {
                break;
            }
            GEM_BUG_ON!((*vma).obj != obj);
            spin_unlock(&mut (*obj).vma.lock);

            i915_vma_destroy(vma);

            spin_lock(&mut (*obj).vma.lock);
        }
        spin_unlock(&mut (*obj).vma.lock);
    }

    __i915_gem_object_free_mmaps(obj);

    atomic_set(&mut (*obj).mm.pages_pin_count, 0);

    // Imported dma-buf unmap requires the reservation to be locked.
    if drm_gem_is_imported(core::ptr::addr_of!((*obj).base).cast::<DrmGemObject>()) {
        i915_gem_object_lock(obj, core::ptr::null_mut());
    }

    __i915_gem_object_put_pages(obj);

    if drm_gem_is_imported(core::ptr::addr_of!((*obj).base).cast::<DrmGemObject>()) {
        i915_gem_object_unlock(obj);
    }

    GEM_BUG_ON!(i915_gem_object_has_pages(obj));
}

// upstream: i915_gem_object.c __i915_gem_free_object()
pub unsafe fn __i915_gem_free_object(obj: *mut DrmI915GemObject) {
    trace_i915_gem_object_destroy(obj);

    GEM_BUG_ON!(!list_empty(&(*obj).lut_list));

    unsafe { bitmap_free((*obj).bit_17.cast()) };

    if drm_gem_is_imported(core::ptr::addr_of!((*obj).base).cast::<DrmGemObject>()) {
        drm_prime_gem_destroy(core::ptr::addr_of_mut!((*obj).base).cast::<DrmGemObject>(), core::ptr::null_mut());
    }

    drm_gem_free_mmap_offset(core::ptr::addr_of_mut!((*obj).base).cast::<DrmGemObject>());

    if let Some(release) = (*(*obj).ops).release {
        release(obj);
    }

    if !(*obj).shares_resv_from.is_null() {
        i915_vm_resv_put((*obj).shares_resv_from);
    }

    __i915_gem_object_fini(obj);
}

// upstream: i915_gem_object.c __i915_gem_free_objects()
unsafe fn __i915_gem_free_objects(i915: *mut DrmI915Private, freed: *mut LlistNode) {
    let member_offset = core::mem::offset_of!(DrmI915GemObject, rcu_or_freed.freed);
    let mut node = freed;

    while !node.is_null() {
        let next = (*node).next;
        let obj = node.wrapping_sub(member_offset) as *mut DrmI915GemObject;

        might_sleep();
        if let Some(delayed_free) = (*(*obj).ops).delayed_free {
            delayed_free(obj);
            node = next;
            continue;
        }

        __i915_gem_object_pages_fini(obj);
        __i915_gem_free_object(obj);

        // Keep the pointer alive for RCU-protected lookups.
        call_rcu(
            core::ptr::addr_of_mut!((*obj).rcu_or_freed.rcu).cast::<RcuHead>(),
            __i915_gem_free_object_rcu,
        );
        cond_resched();
        node = next;
    }
    let _ = i915;
}

// upstream: i915_gem_object.c i915_gem_flush_free_objects()
pub unsafe fn i915_gem_flush_free_objects(i915: *mut DrmI915Private) {
    let freed = llist_del_all(&mut (*i915).mm.free_list);

    if !freed.is_null() {
        __i915_gem_free_objects(i915, freed);
    }
}

// upstream: i915_gem_object.c __i915_gem_free_work()
unsafe extern "C" fn __i915_gem_free_work(work: *mut WorkStruct) {
    let i915 = container_of!(work, DrmI915Private, mm.free_work);

    i915_gem_flush_free_objects(i915);
}

// upstream: i915_gem_object.c i915_gem_free_object()
unsafe extern "C" fn i915_gem_free_object(gem_obj: *mut DrmGemObject) {
    let obj = to_intel_bo(gem_obj);
    let i915 = to_i915((&(*obj).base.base).dev);

    GEM_BUG_ON!(i915_gem_object_is_framebuffer(obj));

    i915_drm_client_remove_object(obj);

    // Complete pure-RCU readers before deferring the release work.
    atomic_inc(&mut (*i915).mm.free_count);

    // VMA unbind may sleep, so defer it to the free worker.
    if llist_add(
        core::ptr::addr_of_mut!((*obj).rcu_or_freed.freed).cast::<LlistNode>(),
        &mut (*i915).mm.free_list,
    ) {
        queue_work((*i915).wq, &mut (*i915).mm.free_work);
    }
}

// upstream: i915_gem_object.c i915_gem_object_read_from_page_kmap()
unsafe fn i915_gem_object_read_from_page_kmap(
    obj: *mut DrmI915GemObject,
    offset: u64,
    dst: *mut c_void,
    size: c_int,
) {
    let idx = (offset >> PAGE_SHIFT) as PgoffT;
    let mut src_ptr = kmap_local_page(i915_gem_object_get_page(obj, idx));
    src_ptr = src_ptr
        .cast::<u8>()
        .add(offset_in_page(offset) as usize)
        .cast();
    if object_cache_coherent(obj) & I915_BO_CACHE_COHERENT_FOR_READ == 0 {
        drm_clflush_virt_range(src_ptr, size as u64);
    }
    memcpy(dst, src_ptr, size as usize);

    kunmap_local(src_ptr);
}

// upstream: i915_gem_object.c i915_gem_object_read_from_page_iomap()
unsafe fn i915_gem_object_read_from_page_iomap(
    obj: *mut DrmI915GemObject,
    offset: u64,
    dst: *mut c_void,
    size: c_int,
) {
    let idx = (offset >> PAGE_SHIFT) as PgoffT;
    let dma = i915_gem_object_get_dma_address(obj, idx);
    let src_map = io_mapping_map_wc(
        (&mut (*(*obj).mm.region).iomap as *mut crate::linux::gem_memory::IoMapping)
            .cast::<c_void>(),
        (dma - (*(*obj).mm.region).region.start) as c_ulong,
        PAGE_SIZE as usize,
    );
    let src_ptr: *mut u8 = src_map.cast::<u8>().add(offset_in_page(offset) as usize);

    if !i915_memcpy_from_wc(dst, src_ptr.cast(), size as c_ulong) {
        memcpy_fromio(dst, src_ptr.cast(), size as usize);
    }

    io_mapping_unmap(src_map);
}

// upstream: i915_gem_object.c object_has_mappable_iomem()
unsafe fn object_has_mappable_iomem(obj: *mut DrmI915GemObject) -> bool {
    GEM_BUG_ON!(!i915_gem_object_has_iomem(obj));

    if IS_DGFX(to_i915((&(*obj).base.base).dev)) {
        return i915_ttm_resource_mappable((*i915_gem_to_ttm(obj)).resource);
    }

    true
}

/// The source owner is `i915_gem_ttm.c`; this is its target-layout-equivalent
/// predicate over `ttm_resource` and `i915_ttm_buddy_resource`.
unsafe fn i915_ttm_resource_mappable(res: *mut TtmResource) -> bool {
    if (*res).mem_type == 0 /* TTM_PL_SYSTEM */ {
        return true;
    }

    let pfn_up = ((*res).size + crate::linux_config::PAGE_SIZE - 1)
        >> crate::linux_config::PAGE_SHIFT;
    i915_ttm_resource_visible_size(res) == pfn_up
}

// upstream: i915_gem_object.c i915_gem_object_read_from_page()
pub unsafe fn i915_gem_object_read_from_page(
    obj: *mut DrmI915GemObject,
    offset: u64,
    dst: *mut c_void,
    size: c_int,
) -> c_int {
    GEM_BUG_ON!(overflows_type!(offset >> PAGE_SHIFT, PgoffT));
    GEM_BUG_ON!(offset >= (&(*obj).base.base).size);
    GEM_BUG_ON!(offset_in_page(offset) > PAGE_SIZE as u64 - size as u64);
    GEM_BUG_ON!(!i915_gem_object_has_pinned_pages(obj));

    if i915_gem_object_has_struct_page(obj) {
        i915_gem_object_read_from_page_kmap(obj, offset, dst, size);
    } else if i915_gem_object_has_iomem(obj) && object_has_mappable_iomem(obj) {
        i915_gem_object_read_from_page_iomap(obj, offset, dst, size);
    } else {
        return -ENODEV;
    }

    0
}

// upstream: i915_gem_object.c i915_gem_object_evictable()
pub unsafe fn i915_gem_object_evictable(obj: *mut DrmI915GemObject) -> bool {
    let mut pin_count = atomic_read(&(*obj).mm.pages_pin_count);

    if pin_count == 0 {
        return true;
    }

    let mut vma: *mut I915Vma = core::ptr::null_mut();
    spin_lock(&mut (*obj).vma.lock);
    list_for_each_entry!(vma, &mut (*obj).vma.list, obj_link, {
        if i915_vma_is_pinned(vma) {
            spin_unlock(&mut (*obj).vma.lock);
            return false;
        }
        if atomic_read(&(*vma).pages_count) != 0 {
            pin_count -= 1;
        }
    });
    spin_unlock(&mut (*obj).vma.lock);
    GEM_WARN_ON!(pin_count < 0);

    pin_count == 0
}

// upstream: i915_gem_object.c i915_gem_object_migratable()
pub unsafe fn i915_gem_object_migratable(obj: *mut DrmI915GemObject) -> bool {
    let mr = core::ptr::read_volatile(core::ptr::addr_of!((*obj).mm.region));

    if mr.is_null() {
        return false;
    }

    (*obj).mm.n_placements > 1
}

// upstream: i915_gem_object.c i915_gem_object_has_struct_page()
pub unsafe fn i915_gem_object_has_struct_page(obj: *const DrmI915GemObject) -> bool {
    #[cfg(CONFIG_LOCKDEP)]
    if IS_DGFX(to_i915((*obj).base.base.dev))
        && i915_gem_object_evictable(obj as *mut DrmI915GemObject)
    {
        assert_object_held_shared(obj as *mut DrmI915GemObject);
    }
    (*obj).mem_flags & I915_BO_FLAG_STRUCT_PAGE != 0
}

// upstream: i915_gem_object.c i915_gem_object_has_iomem()
pub unsafe fn i915_gem_object_has_iomem(obj: *const DrmI915GemObject) -> bool {
    #[cfg(CONFIG_LOCKDEP)]
    if IS_DGFX(to_i915((*obj).base.base.dev))
        && i915_gem_object_evictable(obj as *mut DrmI915GemObject)
    {
        assert_object_held_shared(obj as *mut DrmI915GemObject);
    }
    (*obj).mem_flags & I915_BO_FLAG_IOMEM != 0
}

// upstream: i915_gem_object.c i915_gem_object_can_migrate()
pub unsafe fn i915_gem_object_can_migrate(obj: *mut DrmI915GemObject, id: IntelRegionId) -> bool {
    let i915 = to_i915((&(*obj).base.base).dev);
    let num_allowed = (*obj).mm.n_placements;
    let mut mr: *mut IntelMemoryRegion;

    GEM_BUG_ON!(id >= INTEL_REGION_UNKNOWN);
    GEM_BUG_ON!(object_madv(obj) != I915_MADV_WILLNEED);

    mr = (*i915).mm.regions[id as usize];
    if mr.is_null() {
        return false;
    }

    if !IS_ALIGNED!((&(*obj).base.base).size, (*mr).min_page_size) {
        return false;
    }

    if (*obj).mm.region == mr {
        return true;
    }

    if !i915_gem_object_evictable(obj) {
        return false;
    }

    if (*(*obj).ops).migrate.is_none() {
        return false;
    }

    if (*obj).flags & I915_BO_ALLOC_USER as c_ulong == 0 {
        return true;
    }

    if num_allowed == 0 {
        return false;
    }

    for i in 0..num_allowed as usize {
        if mr == *(*obj).mm.placements.add(i) {
            return true;
        }
    }

    false
}

// upstream: i915_gem_object.c i915_gem_object_migrate()
pub unsafe fn i915_gem_object_migrate(
    obj: *mut DrmI915GemObject,
    ww: *mut I915GemWwCtx,
    id: IntelRegionId,
) -> c_int {
    __i915_gem_object_migrate(obj, ww, id, (*obj).flags as u32)
}

// upstream: i915_gem_object.c __i915_gem_object_migrate()
pub unsafe fn __i915_gem_object_migrate(
    obj: *mut DrmI915GemObject,
    ww: *mut I915GemWwCtx,
    id: IntelRegionId,
    flags: u32,
) -> c_int {
    let i915 = to_i915((&(*obj).base.base).dev);
    let mut mr: *mut IntelMemoryRegion;

    GEM_BUG_ON!(id >= INTEL_REGION_UNKNOWN);
    GEM_BUG_ON!(object_madv(obj) != I915_MADV_WILLNEED);
    assert_object_held(obj);

    mr = (*i915).mm.regions[id as usize];
    GEM_BUG_ON!(mr.is_null());

    if !i915_gem_object_can_migrate(obj, id) {
        return -EINVAL;
    }

    if (*(*obj).ops).migrate.is_none() {
        if GEM_WARN_ON!((*obj).mm.region != mr) {
            return -EINVAL;
        }
        return 0;
    }

    (*(*obj).ops).migrate.unwrap()(obj, mr, flags)
}

// upstream: i915_gem_object.c i915_gem_object_placement_possible()
pub unsafe fn i915_gem_object_placement_possible(
    obj: *mut DrmI915GemObject,
    memory_type: IntelMemoryType,
) -> bool {
    if (*obj).mm.n_placements == 0 {
        match memory_type as u16 {
            INTEL_MEMORY_LOCAL => return i915_gem_object_has_iomem(obj),
            INTEL_MEMORY_SYSTEM => return i915_gem_object_has_pages(obj),
            _ => {
                // Ignore stolen for now.
                GEM_BUG_ON!(true);
                return false;
            }
        }
    }

    for i in 0..(*obj).mm.n_placements as usize {
        if (**(*obj).mm.placements.add(i)).r#type == memory_type as u16 {
            return true;
        }
    }

    false
}

// upstream: i915_gem_object.c i915_gem_object_needs_ccs_pages()
pub unsafe fn i915_gem_object_needs_ccs_pages(obj: *mut DrmI915GemObject) -> bool {
    let mut lmem_placement = false;
    let mut i = 0;

    if !HAS_FLAT_CCS(to_i915((&(*obj).base.base).dev)) {
        return false;
    }

    if (*obj).flags & I915_BO_ALLOC_CCS_AUX as c_ulong != 0 {
        return true;
    }

    while i < (*obj).mm.n_placements as c_int {
        // Compression is not allowed with a system-memory placement.
        let memory_type = (**(*obj).mm.placements.add(i as usize)).r#type;
        if memory_type == INTEL_MEMORY_SYSTEM {
            return false;
        }
        if !lmem_placement && memory_type == INTEL_MEMORY_LOCAL {
            lmem_placement = true;
        }
        i += 1;
    }

    lmem_placement
}

// upstream: i915_gem_object.c i915_gem_vmap_object()
unsafe extern "C" fn i915_gem_vmap_object(gem_obj: *mut DrmGemObject, map: *mut IosysMap) -> c_int {
    let obj = to_intel_bo(gem_obj);
    let vaddr = i915_gem_object_pin_map(obj, I915_MAP_WB);
    if IS_ERR(vaddr) {
        return PTR_ERR(vaddr);
    }

    iosys_map_set_vaddr(map, vaddr);

    0
}

// upstream: i915_gem_object.c i915_gem_vunmap_object()
unsafe extern "C" fn i915_gem_vunmap_object(gem_obj: *mut DrmGemObject, map: *mut IosysMap) {
    let obj = to_intel_bo(gem_obj);

    i915_gem_object_flush_map(obj);
    i915_gem_object_unpin_map(obj);
    let _ = map;
}

// upstream: i915_gem_object.c i915_gem_init__objects()
pub unsafe fn i915_gem_init__objects(i915: *mut DrmI915Private) {
    INIT_WORK_C(&mut (*i915).mm.free_work, __i915_gem_free_work);
}

// upstream: i915_gem_object.c i915_objects_module_exit()
pub unsafe fn i915_objects_module_exit() {
    kmem_cache_destroy(slab_objects.cast::<KmCache>());
}

// upstream: i915_gem_object.c i915_objects_module_init()
pub unsafe fn i915_objects_module_init() -> c_int {
    slab_objects = KMEM_CACHE!(DrmI915GemObject, SLAB_HWCACHE_ALIGN);
    if slab_objects.is_null() {
        return -ENOMEM;
    }

    0
}

// Full Linux `drm_gem_object_funcs` member order. Unspecified C aggregate
// members are null, while the five source-assigned callbacks retain their
// exact callback slots.
#[repr(C)]
struct I915GemObjectFuncs {
    free: *const c_void,
    open: *const c_void,
    close: *const c_void,
    print_info: *const c_void,
    export: *const c_void,
    pin: *const c_void,
    unpin: *const c_void,
    get_sg_table: *const c_void,
    vmap: *const c_void,
    vunmap: *const c_void,
    mmap: *const c_void,
    evict: *const c_void,
    status: *const c_void,
    rss: *const c_void,
    vm_ops: *const c_void,
}
unsafe impl Sync for I915GemObjectFuncs {}

unsafe extern "C" {
    fn i915_gem_prime_export(gem: *mut DrmGemObject, flags: c_int) -> *mut c_void;
}

static i915_gem_object_funcs: I915GemObjectFuncs = I915GemObjectFuncs {
    free: i915_gem_free_object as *const () as *const c_void,
    open: core::ptr::null(),
    close: i915_gem_close_object as *const () as *const c_void,
    print_info: core::ptr::null(),
    export: i915_gem_prime_export as *const () as *const c_void,
    pin: core::ptr::null(),
    unpin: core::ptr::null(),
    get_sg_table: core::ptr::null(),
    vmap: i915_gem_vmap_object as *const () as *const c_void,
    vunmap: i915_gem_vunmap_object as *const () as *const c_void,
    mmap: core::ptr::null(),
    evict: core::ptr::null(),
    status: core::ptr::null(),
    rss: core::ptr::null(),
    vm_ops: core::ptr::null(),
};

// upstream: i915_gem_object.c i915_gem_object_get_moving_fence()
pub unsafe fn i915_gem_object_get_moving_fence(
    obj: *mut DrmI915GemObject,
    fence: *mut *mut DmaFence,
) -> c_int {
    dma_resv_get_singleton((&(*obj).base.base).resv.cast::<DmaResv>(), DMA_RESV_USAGE_KERNEL, fence)
}

// upstream: i915_gem_object.c i915_gem_object_wait_moving_fence()
pub unsafe fn i915_gem_object_wait_moving_fence(obj: *mut DrmI915GemObject, intr: bool) -> c_int {
    let mut ret: isize;

    assert_object_held(obj);

    ret = dma_resv_wait_timeout(
        (&(*obj).base.base).resv.cast::<DmaResv>(),
        DMA_RESV_USAGE_KERNEL,
        intr,
        MAX_SCHEDULE_TIMEOUT as core::ffi::c_long,
    ) as isize;
    if ret == 0 {
        ret = -ETIME as isize;
    } else if ret > 0 && i915_gem_object_has_unknown_state(obj) {
        ret = -EIO as isize;
    }

    if ret < 0 { ret as c_int } else { 0 }
}

// upstream: i915_gem_object.c i915_gem_object_has_unknown_state()
pub unsafe fn i915_gem_object_has_unknown_state(obj: *mut DrmI915GemObject) -> bool {
    // Pairs with dma_fence_signal() in __memcpy_work().
    smp_rmb();
    (*obj).mm.unknown_state
}
