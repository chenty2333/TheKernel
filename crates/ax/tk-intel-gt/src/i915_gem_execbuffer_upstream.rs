// SPDX-License-Identifier: MIT
// Copyright © 2008,2010 Intel Corporation.
//
//! Translated from Linux v7.2.3 `drivers/gpu/drm/i915/gem/i915_gem_execbuffer.c`.
//! The upstream file is MIT-licensed. This file preserves the source's
//! execbuffer function order and branch/error ordering. Kernel, DRM, GEM, VM,
//! user-copy, dma-resv, and syncobj operations are integration bindings; they
//! are intentionally not replaced by weaker local policy.

use core::ffi::c_void;

// Linux i915 ABI/internal flags from i915_gem_execbuffer.c and i915_vma.h.
pub const FORCE_CPU_RELOC: i32 = 1;
pub const FORCE_GTT_RELOC: i32 = 2;
pub const FORCE_GPU_RELOC: i32 = 3;
pub const DBG_FORCE_RELOC: i32 = 0;
pub const EXEC_OBJECT_HAS_PIN: u32 = 1 << 29;
pub const EXEC_OBJECT_HAS_FENCE: u32 = 1 << 28;
pub const EXEC_OBJECT_USERPTR_INIT: u32 = 1 << 27;
pub const EXEC_OBJECT_NEEDS_MAP: u32 = 1 << 26;
pub const EXEC_OBJECT_NEEDS_BIAS: u32 = 1 << 25;
pub const EXEC_OBJECT_INTERNAL_FLAGS: u32 = !0u32 << 25;
pub const EXEC_OBJECT_RESERVED: u32 = EXEC_OBJECT_HAS_PIN | EXEC_OBJECT_HAS_FENCE;
pub const EXEC_HAS_RELOC: u32 = 1 << 31;
pub const EXEC_ENGINE_PINNED: u32 = 1 << 30;
pub const EXEC_USERPTR_USED: u32 = 1 << 29;
pub const EXEC_INTERNAL_FLAGS: u32 = !0u32 << 29;
pub const UPDATE: u64 = PIN_OFFSET_FIXED;
pub const BATCH_OFFSET_BIAS: u64 = 256 * 1024;
pub const I915_EXEC_ILLEGAL_FLAGS: u64 =
    I915_EXEC_UNKNOWN_FLAGS | I915_EXEC_CONSTANTS_MASK | I915_EXEC_RESOURCE_STREAMER;

// Binding-layout records corresponding to the C records in this source and
// its i915 headers. Pointers deliberately mirror the source ownership rules.
#[repr(C)]
pub struct EbVma {
    pub vma: *mut I915Vma,
    pub flags: u32,
    pub exec: *mut DrmI915GemExecObject2,
    pub bind_link: ListHead,
    pub reloc_link: ListHead,
    pub node: HlistNode,
    pub handle: u32,
}
#[repr(C)]
pub struct EbFence {
    pub syncobj: *mut DrmSyncobj,
    pub dma_fence: *mut DmaFence,
    pub value: u64,
    pub chain_fence: *mut DmaFenceChain,
}
#[repr(C)]
pub struct RelocCache {
    pub node: DrmMmNode,
    pub vaddr: usize,
    pub page: usize,
    pub graphics_ver: u32,
    pub use_64bit_reloc: bool,
    pub has_llc: bool,
    pub has_fence: bool,
    pub needs_unfenced: bool,
}
#[repr(C)]
pub struct I915Execbuffer {
    pub i915: *mut DrmI915Private,
    pub file: *mut DrmFile,
    pub args: *mut DrmI915GemExecbuffer2,
    pub exec: *mut DrmI915GemExecObject2,
    pub vma: *mut EbVma,
    pub gt: *mut IntelGt,
    pub context: *mut IntelContext,
    pub gem_context: *mut I915GemContext,
    pub wakeref: u64,
    pub wakeref_gt0: u64,
    pub requests: [*mut I915Request; MAX_ENGINE_INSTANCE + 1],
    pub batches: [*mut EbVma; MAX_ENGINE_INSTANCE + 1],
    pub trampoline: *mut I915Vma,
    pub composite_fence: *mut DmaFence,
    pub buffer_count: u32,
    pub num_batches: u32,
    pub unbound: ListHead,
    pub relocs: ListHead,
    pub ww: I915GemWwCtx,
    pub reloc_cache: RelocCache,
    pub invalid_flags: u64,
    pub batch_len: [u64; MAX_ENGINE_INSTANCE + 1],
    pub batch_start_offset: u32,
    pub batch_flags: u32,
    pub batch_pool: *mut IntelGtBufferPoolNode,
    pub lut_size: i32,
    pub buckets: *mut HlistHead,
    pub fences: *mut EbFence,
    pub num_fences: usize,
    pub capture_lists: [*mut I915CaptureList; MAX_ENGINE_INSTANCE + 1],
}

// UAPI and header records are supplied by the surrounding i915 translation
// modules; declarations here are intentionally absent to avoid duplicate ABI
// layouts. The signatures below retain their upstream C argument order.

// upstream: i915_gem_execbuffer.c eb_use_cmdparser()
#[inline]
pub unsafe fn eb_use_cmdparser(eb: *const I915Execbuffer) -> bool {
    intel_engine_requires_cmd_parser((*(*eb).context).engine)
        || (intel_engine_using_cmd_parser((*(*eb).context).engine) && (*(*eb).args).batch_len != 0)
}

// upstream: i915_gem_execbuffer.c eb_create()
pub unsafe fn eb_create(eb: *mut I915Execbuffer) -> i32 {
    if ((*(*eb).args).flags & I915_EXEC_HANDLE_LUT) == 0 {
        let mut size = 1 + ilog2((*eb).buffer_count);
        loop {
            let mut flags = GFP_KERNEL;
            if size > 1 {
                flags |= __GFP_NORETRY | __GFP_NOWARN;
            }
            (*eb).buckets =
                kzalloc(core::mem::size_of::<HlistHead>() << size, flags) as *mut HlistHead;
            if !(*eb).buckets.is_null() {
                break;
            }
            if size == 0 {
                break;
            }
            size -= 1;
        }
        if size == 0 {
            return -ENOMEM;
        }
        (*eb).lut_size = size as i32;
    } else {
        (*eb).lut_size = -((*eb).buffer_count as i32);
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_vma_misplaced()
#[inline]
pub unsafe fn eb_vma_misplaced(
    entry: *const DrmI915GemExecObject2,
    vma: *const I915Vma,
    flags: u32,
) -> bool {
    let start = i915_vma_offset(vma);
    let size = i915_vma_size(vma);
    if size < (*entry).pad_to_size {
        return true;
    }
    if (*entry).alignment != 0 && !IS_ALIGNED(start, (*entry).alignment) {
        return true;
    }
    if flags & EXEC_OBJECT_PINNED != 0 && start != (*entry).offset {
        return true;
    }
    if flags & EXEC_OBJECT_NEEDS_BIAS != 0 && start < BATCH_OFFSET_BIAS {
        return true;
    }
    if flags & EXEC_OBJECT_SUPPORTS_48B_ADDRESS == 0 && ((start + size + 4095) >> 32) != 0 {
        return true;
    }
    if flags & EXEC_OBJECT_NEEDS_MAP != 0 && !i915_vma_is_map_and_fenceable(vma) {
        return true;
    }
    false
}

// upstream: i915_gem_execbuffer.c eb_pin_flags()
#[inline]
pub unsafe fn eb_pin_flags(entry: *const DrmI915GemExecObject2, exec_flags: u32) -> u64 {
    let mut pin_flags = 0;
    if exec_flags & EXEC_OBJECT_NEEDS_GTT != 0 {
        pin_flags |= PIN_GLOBAL;
    }
    if exec_flags & EXEC_OBJECT_SUPPORTS_48B_ADDRESS == 0 {
        pin_flags |= PIN_ZONE_4G;
    }
    if exec_flags & EXEC_OBJECT_NEEDS_MAP != 0 {
        pin_flags |= PIN_MAPPABLE;
    }
    if exec_flags & EXEC_OBJECT_PINNED != 0 {
        pin_flags |= (*entry).offset | PIN_OFFSET_FIXED;
    } else if exec_flags & EXEC_OBJECT_NEEDS_BIAS != 0 {
        pin_flags |= BATCH_OFFSET_BIAS | PIN_OFFSET_BIAS;
    }
    pin_flags
}

// upstream: i915_gem_execbuffer.c eb_pin_vma()
pub unsafe fn eb_pin_vma(
    eb: *mut I915Execbuffer,
    entry: *const DrmI915GemExecObject2,
    ev: *mut EbVma,
) -> i32 {
    let vma = (*ev).vma;
    let mut pin_flags = if (*vma).node.size != 0 {
        __i915_vma_offset(vma)
    } else {
        (*entry).offset & PIN_OFFSET_MASK
    };
    pin_flags |= PIN_USER | PIN_NOEVICT | PIN_OFFSET_FIXED | PIN_VALIDATE;
    if (*ev).flags & EXEC_OBJECT_NEEDS_GTT != 0 {
        pin_flags |= PIN_GLOBAL;
    }
    let mut err = i915_vma_pin_ww(vma, &mut (*eb).ww, 0, 0, pin_flags);
    if err == -EDEADLK {
        return err;
    }
    if err != 0 {
        if (*entry).flags & EXEC_OBJECT_PINNED != 0 {
            return err;
        }
        err = i915_vma_pin_ww(
            vma,
            &mut (*eb).ww,
            (*entry).pad_to_size,
            (*entry).alignment,
            eb_pin_flags(entry, (*ev).flags) | PIN_USER | PIN_NOEVICT | PIN_VALIDATE,
        );
        if err != 0 {
            return err;
        }
    }
    if (*ev).flags & EXEC_OBJECT_NEEDS_FENCE != 0 {
        err = i915_vma_pin_fence(vma);
        if err != 0 {
            return err;
        }
        if !(*vma).fence.is_null() {
            (*ev).flags |= EXEC_OBJECT_HAS_FENCE;
        }
    }
    (*ev).flags |= EXEC_OBJECT_HAS_PIN;
    if eb_vma_misplaced(entry, vma, (*ev).flags) {
        return -EBADSLT;
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_unreserve_vma()
#[inline]
pub unsafe fn eb_unreserve_vma(ev: *mut EbVma) {
    if (*ev).flags & EXEC_OBJECT_HAS_FENCE != 0 {
        __i915_vma_unpin_fence((*ev).vma);
    }
    (*ev).flags &= !EXEC_OBJECT_RESERVED;
}

// upstream: i915_gem_execbuffer.c eb_validate_vma()
pub unsafe fn eb_validate_vma(
    eb: *mut I915Execbuffer,
    entry: *mut DrmI915GemExecObject2,
    vma: *mut I915Vma,
) -> i32 {
    if (*entry).relocation_count != 0 && GRAPHICS_VER((*eb).i915) >= 12 && !IS_TIGERLAKE((*eb).i915)
    {
        return -EINVAL;
    }
    if (*entry).flags as u64 & (*eb).invalid_flags != 0 {
        return -EINVAL;
    }
    if (*entry).alignment != 0 && !is_power_of_2_u64((*entry).alignment) {
        return -EINVAL;
    }
    if (*entry).flags & EXEC_OBJECT_PINNED != 0
        && (*entry).offset != gen8_canonical_addr((*entry).offset & I915_GTT_PAGE_MASK)
    {
        return -EINVAL;
    }
    if (*entry).flags & EXEC_OBJECT_PAD_TO_SIZE != 0 {
        if offset_in_page((*entry).pad_to_size) != 0 {
            return -EINVAL;
        }
    } else {
        (*entry).pad_to_size = 0;
    }
    (*entry).offset = gen8_noncanonical_addr((*entry).offset);
    if !(*eb).reloc_cache.has_fence {
        (*entry).flags &= !EXEC_OBJECT_NEEDS_FENCE;
    } else if ((*entry).flags & EXEC_OBJECT_NEEDS_FENCE != 0 || (*eb).reloc_cache.needs_unfenced)
        && i915_gem_object_is_tiled((*vma).obj)
    {
        (*entry).flags |= EXEC_OBJECT_NEEDS_GTT | EXEC_OBJECT_NEEDS_MAP;
    }
    0
}

// upstream: i915_gem_execbuffer.c is_batch_buffer()
#[inline]
pub unsafe fn is_batch_buffer(eb: *const I915Execbuffer, buffer_idx: u32) -> bool {
    if (*(*eb).args).flags & I915_EXEC_BATCH_FIRST != 0 {
        buffer_idx < (*eb).num_batches
    } else {
        buffer_idx >= (*(*eb).args).buffer_count - (*eb).num_batches
    }
}

// upstream: i915_gem_execbuffer.c eb_add_vma()
pub unsafe fn eb_add_vma(
    eb: *mut I915Execbuffer,
    current_batch: *mut u32,
    i: u32,
    vma: *mut I915Vma,
) -> i32 {
    let i915 = (*eb).i915;
    let entry = (*eb).exec.add(i as usize);
    let ev = (*eb).vma.add(i as usize);
    (*ev).vma = vma;
    (*ev).exec = entry;
    (*ev).flags = (*entry).flags;
    if (*eb).lut_size > 0 {
        (*ev).handle = (*entry).handle;
        hlist_add_head(
            &mut (*ev).node,
            (*eb)
                .buckets
                .add(hash_32((*entry).handle, (*eb).lut_size as u32) as usize),
        );
    }
    if (*entry).relocation_count != 0 {
        list_add_tail(&mut (*ev).reloc_link, &mut (*eb).relocs);
    }
    if is_batch_buffer(eb, i) {
        if (*entry).relocation_count != 0 && (*ev).flags & EXEC_OBJECT_PINNED == 0 {
            (*ev).flags |= EXEC_OBJECT_NEEDS_BIAS;
        }
        if (*eb).reloc_cache.has_fence {
            (*ev).flags |= EXEC_OBJECT_NEEDS_FENCE;
        }
        (*eb).batches[*current_batch as usize] = ev;
        if (*ev).flags & EXEC_OBJECT_WRITE != 0 {
            drm_dbg(
                &mut (*i915).drm,
                "Attempting to use self-modifying batch buffer\n",
            );
            return -EINVAL;
        }
        if range_overflows_t_u64(
            (*eb).batch_start_offset as u64,
            (*(*eb).args).batch_len,
            (*vma).size,
        ) {
            drm_dbg(&mut (*i915).drm, "Attempting to use out-of-bounds batch\n");
            return -EINVAL;
        }
        if (*(*eb).args).batch_len == 0 {
            (*eb).batch_len[*current_batch as usize] =
                (*vma).size - (*eb).batch_start_offset as u64;
        } else {
            (*eb).batch_len[*current_batch as usize] = (*(*eb).args).batch_len;
        }
        if (*eb).batch_len[*current_batch as usize] == 0 {
            drm_dbg(&mut (*i915).drm, "Invalid batch length\n");
            return -EINVAL;
        }
        *current_batch += 1;
    }
    0
}

// upstream: i915_gem_execbuffer.c use_cpu_reloc()
pub unsafe fn use_cpu_reloc(cache: *const RelocCache, obj: *const DrmI915GemObject) -> i32 {
    if !i915_gem_object_has_struct_page(obj) {
        return 0;
    }
    if DBG_FORCE_RELOC == FORCE_CPU_RELOC {
        return 1;
    }
    if DBG_FORCE_RELOC == FORCE_GTT_RELOC {
        return 0;
    }
    if (*cache).has_llc
        || (*obj).cache_dirty
        || !i915_gem_object_has_cache_level(obj, I915_CACHE_NONE)
    {
        1
    } else {
        0
    }
}

// upstream: i915_gem_execbuffer.c eb_reserve_vma()
pub unsafe fn eb_reserve_vma(eb: *mut I915Execbuffer, ev: *mut EbVma, pin_flags: u64) -> i32 {
    let entry = (*ev).exec;
    let vma = (*ev).vma;
    let mut err;
    if drm_mm_node_allocated(&(*vma).node) && eb_vma_misplaced(entry, vma, (*ev).flags) {
        err = i915_vma_unbind(vma);
        if err != 0 {
            return err;
        }
    }
    err = i915_vma_pin_ww(
        vma,
        &mut (*eb).ww,
        (*entry).pad_to_size,
        (*entry).alignment,
        eb_pin_flags(entry, (*ev).flags) | pin_flags,
    );
    if err != 0 {
        return err;
    }
    if (*entry).offset != i915_vma_offset(vma) {
        (*entry).offset = i915_vma_offset(vma) | UPDATE;
        (*(*eb).args).flags |= EXEC_HAS_RELOC as u64;
    }
    if (*ev).flags & EXEC_OBJECT_NEEDS_FENCE != 0 {
        err = i915_vma_pin_fence(vma);
        if err != 0 {
            return err;
        }
        if !(*vma).fence.is_null() {
            (*ev).flags |= EXEC_OBJECT_HAS_FENCE;
        }
    }
    (*ev).flags |= EXEC_OBJECT_HAS_PIN;
    GEM_BUG_ON(eb_vma_misplaced(entry, vma, (*ev).flags));
    0
}

// upstream: i915_gem_execbuffer.c eb_unbind()
pub unsafe fn eb_unbind(eb: *mut I915Execbuffer, force: bool) -> bool {
    let count = (*eb).buffer_count;
    let mut unpinned = false;
    let mut last = ListHead::default();
    INIT_LIST_HEAD(&mut (*eb).unbound);
    INIT_LIST_HEAD(&mut last);
    for i in 0..count {
        let ev = (*eb).vma.add(i as usize);
        let flags = (*ev).flags;
        if !force && flags & EXEC_OBJECT_PINNED != 0 && flags & EXEC_OBJECT_HAS_PIN != 0 {
            continue;
        }
        unpinned = true;
        eb_unreserve_vma(ev);
        if flags & EXEC_OBJECT_PINNED != 0 {
            list_add(&mut (*ev).bind_link, &mut (*eb).unbound);
        } else if flags & EXEC_OBJECT_NEEDS_MAP != 0 {
            list_add_tail(&mut (*ev).bind_link, &mut (*eb).unbound);
        } else if flags & EXEC_OBJECT_SUPPORTS_48B_ADDRESS == 0 {
            list_add(&mut (*ev).bind_link, &mut last);
        } else {
            list_add_tail(&mut (*ev).bind_link, &mut last);
        }
    }
    list_splice_tail(&mut last, &mut (*eb).unbound);
    unpinned
}

// upstream: i915_gem_execbuffer.c eb_reserve()
pub unsafe fn eb_reserve(eb: *mut I915Execbuffer) -> i32 {
    let mut err = 0;
    for pass in 0..=3 {
        let mut pin_flags = PIN_USER | PIN_VALIDATE;
        if pass == 0 {
            pin_flags |= PIN_NONBLOCK;
        }
        if pass >= 1 {
            eb_unbind(eb, pass >= 2);
        }
        if pass == 2 {
            err = mutex_lock_interruptible(&mut (*(*eb).context).vm.mutex);
            if err == 0 {
                err = i915_gem_evict_vm((*(*eb).context).vm, &mut (*eb).ww, core::ptr::null_mut());
                mutex_unlock(&mut (*(*eb).context).vm.mutex);
            }
            if err != 0 {
                return err;
            }
        }
        if pass == 3 {
            loop {
                err = mutex_lock_interruptible(&mut (*(*eb).context).vm.mutex);
                if err == 0 {
                    let mut busy_bo: *mut DrmI915GemObject = core::ptr::null_mut();
                    err = i915_gem_evict_vm((*(*eb).context).vm, &mut (*eb).ww, &mut busy_bo);
                    mutex_unlock(&mut (*(*eb).context).vm.mutex);
                    if err != 0 && !busy_bo.is_null() {
                        err = i915_gem_object_lock(busy_bo, &mut (*eb).ww);
                        i915_gem_object_put(busy_bo);
                        if err == 0 {
                            continue;
                        }
                    }
                }
                break;
            }
            if err != 0 {
                return err;
            }
        }
        let mut ev = (*eb).unbound.next;
        while ev != &mut (*eb).unbound as *mut ListHead {
            let cur = container_of!(ev, EbVma, bind_link);
            err = eb_reserve_vma(eb, cur, pin_flags as u64);
            if err != 0 {
                break;
            }
            ev = (*ev).next;
        }
        if err != -ENOSPC {
            break;
        }
    }
    err
}

// upstream: i915_gem_execbuffer.c eb_select_context()
pub unsafe fn eb_select_context(eb: *mut I915Execbuffer) -> i32 {
    let ctx = i915_gem_context_lookup((*(*eb).file).driver_priv, (*(*eb).args).rsvd1);
    if IS_ERR(ctx) {
        return PTR_ERR(ctx);
    }
    (*eb).gem_context = ctx;
    if i915_gem_context_has_full_ppgtt(ctx) {
        (*eb).invalid_flags |= EXEC_OBJECT_NEEDS_GTT as u64;
    }
    0
}

// upstream: i915_gem_execbuffer.c __eb_add_lut()
pub unsafe fn __eb_add_lut(eb: *mut I915Execbuffer, handle: u32, vma: *mut I915Vma) -> i32 {
    let ctx = (*eb).gem_context;
    let lut = i915_lut_handle_alloc();
    if lut.is_null() {
        return -ENOMEM;
    }
    i915_vma_get(vma);
    if atomic_fetch_inc(&mut (*vma).open_count) == 0 {
        i915_vma_reopen(vma);
    }
    (*lut).handle = handle;
    (*lut).ctx = ctx;
    let mut err = -EINTR;
    if mutex_lock_interruptible(&mut (*ctx).lut_mutex) == 0 {
        if !i915_gem_context_is_closed(ctx) {
            err = radix_tree_insert(&mut (*ctx).handles_vma, handle, vma);
        } else {
            err = -ENOENT;
        }
        if err == 0 {
            let obj = (*vma).obj;
            spin_lock(&mut (*obj).lut_lock);
            if idr_find(&(*(*eb).file).object_idr, handle) == obj {
                list_add(&mut (*lut).obj_link, &mut (*obj).lut_list);
            } else {
                radix_tree_delete(&mut (*ctx).handles_vma, handle);
                err = -ENOENT;
            }
            spin_unlock(&mut (*obj).lut_lock);
        }
        mutex_unlock(&mut (*ctx).lut_mutex);
    }
    if err == 0 {
        return 0;
    }
    i915_vma_close(vma);
    i915_vma_put(vma);
    i915_lut_handle_free(lut);
    err
}

// upstream: i915_gem_execbuffer.c eb_lookup_vma()
pub unsafe fn eb_lookup_vma(eb: *mut I915Execbuffer, handle: u32) -> *mut I915Vma {
    let vm = (*(*eb).context).vm;
    loop {
        rcu_read_lock();
        let mut vma = radix_tree_lookup(&(*(*eb).gem_context).handles_vma, handle);
        if !vma.is_null() {
            vma = i915_vma_tryget(vma);
        }
        rcu_read_unlock();
        if !vma.is_null() {
            return vma;
        }
        let obj = i915_gem_object_lookup((*eb).file, handle);
        if obj.is_null() {
            return ERR_PTR(-ENOENT);
        }
        if i915_gem_context_uses_protected_content((*eb).gem_context)
            && i915_gem_object_is_protected(obj)
        {
            let err = intel_pxp_key_check(intel_bo_to_drm_bo(obj), true);
            if err != 0 {
                i915_gem_object_put(obj);
                return ERR_PTR(err);
            }
        }
        vma = i915_vma_instance(obj, vm, core::ptr::null_mut());
        if IS_ERR(vma) {
            i915_gem_object_put(obj);
            return vma;
        }
        let err = __eb_add_lut(eb, handle, vma);
        if err == 0 {
            return vma;
        }
        i915_gem_object_put(obj);
        if err != -EEXIST {
            return ERR_PTR(err);
        }
    }
}

// upstream: i915_gem_execbuffer.c eb_lookup_vmas()
pub unsafe fn eb_lookup_vmas(eb: *mut I915Execbuffer) -> i32 {
    let mut current_batch = 0;
    INIT_LIST_HEAD(&mut (*eb).relocs);
    for i in 0..(*eb).buffer_count {
        let entry = (*eb).exec.add(i as usize);
        let vma = eb_lookup_vma(eb, (*entry).handle);
        if IS_ERR(vma) {
            return PTR_ERR(vma);
        }
        let mut err = eb_validate_vma(eb, entry, vma);
        if err != 0 {
            i915_vma_put(vma);
            return err;
        }
        err = eb_add_vma(eb, &mut current_batch, i, vma);
        if err != 0 {
            return err;
        }
        if i915_gem_object_is_userptr((*vma).obj) {
            err = i915_gem_object_userptr_submit_init((*vma).obj);
            if err != 0 {
                return err;
            }
            (*(*eb).vma.add(i as usize)).flags |= EXEC_OBJECT_USERPTR_INIT;
            (*(*eb).args).flags |= EXEC_USERPTR_USED as u64;
        }
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_lock_vmas()
pub unsafe fn eb_lock_vmas(eb: *mut I915Execbuffer) -> i32 {
    for i in 0..(*eb).buffer_count {
        let vma = (*(*eb).vma.add(i as usize)).vma;
        let err = i915_gem_object_lock((*vma).obj, &mut (*eb).ww);
        if err != 0 {
            return err;
        }
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_validate_vmas()
pub unsafe fn eb_validate_vmas(eb: *mut I915Execbuffer) -> i32 {
    INIT_LIST_HEAD(&mut (*eb).unbound);
    let mut err = eb_lock_vmas(eb);
    if err != 0 {
        return err;
    }
    for i in 0..(*eb).buffer_count {
        let entry = (*eb).exec.add(i as usize);
        let ev = (*eb).vma.add(i as usize);
        let vma = (*ev).vma;
        err = eb_pin_vma(eb, entry, ev);
        if err == -EDEADLK {
            return err;
        }
        if err == 0 {
            if (*entry).offset != i915_vma_offset(vma) {
                (*entry).offset = i915_vma_offset(vma) | UPDATE;
                (*(*eb).args).flags |= EXEC_HAS_RELOC as u64;
            }
        } else {
            eb_unreserve_vma(ev);
            list_add_tail(&mut (*ev).bind_link, &mut (*eb).unbound);
            if drm_mm_node_allocated(&(*vma).node) {
                err = i915_vma_unbind(vma);
                if err != 0 {
                    return err;
                }
            }
        }
        err = dma_resv_reserve_fences((*(*vma).obj).base.resv, (*eb).num_batches);
        if err != 0 {
            return err;
        }
        GEM_BUG_ON(
            drm_mm_node_allocated(&(*vma).node) && eb_vma_misplaced(entry, vma, (*ev).flags),
        );
    }
    if !list_empty(&(*eb).unbound) {
        return eb_reserve(eb);
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_get_vma()
pub unsafe fn eb_get_vma(eb: *const I915Execbuffer, handle: usize) -> *mut EbVma {
    if (*eb).lut_size < 0 {
        if handle >= (-(*eb).lut_size) as usize {
            return core::ptr::null_mut();
        }
        return (*eb).vma.add(handle);
    }
    let head = (*eb)
        .buckets
        .add(hash_32(handle as u32, (*eb).lut_size as u32) as usize);
    let mut node = (*head).first;
    while !node.is_null() {
        let ev = container_of!(node, EbVma, node);
        if (*ev).handle as usize == handle {
            return ev;
        }
        node = (*node).next;
    }
    core::ptr::null_mut()
}

// upstream: i915_gem_execbuffer.c eb_release_vmas()
pub unsafe fn eb_release_vmas(eb: *mut I915Execbuffer, final_release: bool) {
    for i in 0..(*eb).buffer_count {
        let ev = (*eb).vma.add(i as usize);
        let vma = (*ev).vma;
        if vma.is_null() {
            break;
        }
        eb_unreserve_vma(ev);
        if final_release {
            i915_vma_put(vma);
        }
    }
    eb_capture_release(eb);
    eb_unpin_engine(eb);
}

// upstream: i915_gem_execbuffer.c eb_destroy()
pub unsafe fn eb_destroy(eb: *const I915Execbuffer) {
    if (*eb).lut_size > 0 {
        kfree((*eb).buckets as *mut c_void);
    }
}

// upstream: i915_gem_execbuffer.c relocation_target()
#[inline]
pub unsafe fn relocation_target(
    reloc: *const DrmI915GemRelocationEntry,
    target: *const I915Vma,
) -> u64 {
    gen8_canonical_addr(i915_vma_offset(target).wrapping_add((*reloc).delta as i32 as i64 as u64))
}

// upstream: i915_gem_execbuffer.c reloc_cache_init()
pub unsafe fn reloc_cache_init(cache: *mut RelocCache, i915: *mut DrmI915Private) {
    (*cache).page = usize::MAX;
    (*cache).vaddr = 0;
    (*cache).graphics_ver = GRAPHICS_VER(i915);
    (*cache).has_llc = HAS_LLC(i915);
    (*cache).use_64bit_reloc = HAS_64BIT_RELOC(i915);
    (*cache).has_fence = (*cache).graphics_ver < 4;
    (*cache).needs_unfenced = (*INTEL_INFO(i915)).unfenced_needs_alignment;
    (*cache).node.flags = 0;
}

pub const KMAP: usize = 0x4;

// upstream: i915_gem_execbuffer.c unmask_page()
#[inline]
pub fn unmask_page(p: usize) -> *mut c_void {
    (p & PAGE_MASK as usize) as *mut c_void
}

// upstream: i915_gem_execbuffer.c unmask_flags()
#[inline]
pub fn unmask_flags(p: usize) -> usize {
    p & !(PAGE_MASK as usize)
}

// upstream: i915_gem_execbuffer.c cache_to_ggtt()
pub unsafe fn cache_to_ggtt(cache: *mut RelocCache) -> *mut I915Ggtt {
    let eb = container_of!(cache, I915Execbuffer, reloc_cache);
    (*to_gt((*eb).i915)).ggtt
}

// upstream: i915_gem_execbuffer.c reloc_cache_unmap()
pub unsafe fn reloc_cache_unmap(cache: *mut RelocCache) {
    if (*cache).vaddr == 0 {
        return;
    }
    let vaddr = unmask_page((*cache).vaddr);
    if (*cache).vaddr & KMAP != 0 {
        kunmap_local(vaddr);
    } else {
        io_mapping_unmap_atomic(vaddr);
    }
}

// upstream: i915_gem_execbuffer.c reloc_cache_remap()
pub unsafe fn reloc_cache_remap(cache: *mut RelocCache, obj: *mut DrmI915GemObject) {
    if (*cache).vaddr == 0 {
        return;
    }
    if (*cache).vaddr & KMAP != 0 {
        let page = i915_gem_object_get_page(obj, (*cache).page);
        let vaddr = kmap_local_page(page);
        (*cache).vaddr = unmask_flags((*cache).vaddr) | vaddr as usize;
    } else {
        let ggtt = cache_to_ggtt(cache);
        let mut offset = (*cache).node.start;
        if !drm_mm_node_allocated(&(*cache).node) {
            offset += ((*cache).page << PAGE_SHIFT) as u64;
        }
        (*cache).vaddr = io_mapping_map_atomic_wc(&mut (*ggtt).iomap, offset) as usize;
    }
}

// upstream: i915_gem_execbuffer.c reloc_cache_reset()
pub unsafe fn reloc_cache_reset(cache: *mut RelocCache, _eb: *mut I915Execbuffer) {
    if (*cache).vaddr == 0 {
        return;
    }
    let vaddr = unmask_page((*cache).vaddr);
    if (*cache).vaddr & KMAP != 0 {
        let obj = (*cache).node.mm as *mut DrmI915GemObject;
        if (*cache).vaddr & CLFLUSH_AFTER as usize != 0 {
            mb();
        }
        kunmap_local(vaddr);
        i915_gem_object_finish_access(obj);
    } else {
        let ggtt = cache_to_ggtt(cache);
        intel_gt_flush_ggtt_writes((*ggtt).vm.gt);
        io_mapping_unmap_atomic(vaddr);
        if drm_mm_node_allocated(&(*cache).node) {
            ((*(*ggtt).vm.clear_range)(&mut (*ggtt).vm, (*cache).node.start, (*cache).node.size));
            mutex_lock(&mut (*ggtt).vm.mutex);
            drm_mm_remove_node(&mut (*cache).node);
            mutex_unlock(&mut (*ggtt).vm.mutex);
        } else {
            i915_vma_unpin((*cache).node.mm as *mut I915Vma);
        }
    }
    (*cache).vaddr = 0;
    (*cache).page = usize::MAX;
}

// upstream: i915_gem_execbuffer.c reloc_kmap()
pub unsafe fn reloc_kmap(
    obj: *mut DrmI915GemObject,
    cache: *mut RelocCache,
    pageno: usize,
) -> *mut c_void {
    if (*cache).vaddr != 0 {
        kunmap_local(unmask_page((*cache).vaddr));
    } else {
        let mut flushes = 0;
        let err = i915_gem_object_prepare_write(obj, &mut flushes);
        if err != 0 {
            return ERR_PTR(err);
        }
        BUILD_BUG_ON!(KMAP & CLFLUSH_FLAGS != 0);
        BUILD_BUG_ON!((KMAP | CLFLUSH_FLAGS) & PAGE_MASK as usize != 0);
        (*cache).vaddr = flushes as usize | KMAP;
        (*cache).node.mm = obj as *mut c_void;
        if flushes != 0 {
            mb();
        }
    }
    let page = i915_gem_object_get_page(obj, pageno);
    if !(*obj).mm.dirty {
        set_page_dirty(page);
    }
    let vaddr = kmap_local_page(page);
    (*cache).vaddr = unmask_flags((*cache).vaddr) | vaddr as usize;
    (*cache).page = pageno;
    vaddr
}

// upstream: i915_gem_execbuffer.c reloc_iomap()
pub unsafe fn reloc_iomap(
    batch: *mut I915Vma,
    eb: *mut I915Execbuffer,
    page: usize,
) -> *mut c_void {
    let obj = (*batch).obj;
    let cache = &mut (*eb).reloc_cache;
    let ggtt = cache_to_ggtt(cache);
    let mut offset;
    if cache.vaddr != 0 {
        intel_gt_flush_ggtt_writes((*ggtt).vm.gt);
        io_mapping_unmap_atomic(unmask_page(cache.vaddr));
    } else {
        let mut vma = ERR_PTR(-ENODEV);
        if i915_gem_object_is_tiled(obj) {
            return ERR_PTR(-EINVAL);
        }
        if use_cpu_reloc(cache, obj) != 0 {
            return core::ptr::null_mut();
        }
        let mut err = i915_gem_object_set_to_gtt_domain(obj, true);
        if err != 0 {
            return ERR_PTR(err);
        }
        if !i915_is_ggtt((*batch).vm) || !i915_vma_misplaced(batch, 0, 0, PIN_MAPPABLE) {
            vma = i915_gem_object_ggtt_pin_ww(
                obj,
                &mut (*eb).ww,
                core::ptr::null_mut(),
                0,
                0,
                PIN_MAPPABLE | PIN_NONBLOCK | PIN_NOEVICT,
            );
        }
        if vma == ERR_PTR(-EDEADLK) {
            return vma;
        }
        if IS_ERR(vma) {
            core::ptr::write_bytes(&mut cache.node, 0, 1);
            mutex_lock(&mut (*ggtt).vm.mutex);
            err = drm_mm_insert_node_in_range(
                &mut (*ggtt).vm.mm,
                &mut cache.node,
                PAGE_SIZE as u64,
                0,
                I915_COLOR_UNEVICTABLE,
                0,
                (*ggtt).mappable_end,
                DRM_MM_INSERT_LOW,
            );
            mutex_unlock(&mut (*ggtt).vm.mutex);
            if err != 0 {
                return core::ptr::null_mut();
            }
        } else {
            cache.node.start = i915_ggtt_offset(vma);
            cache.node.mm = vma as *mut c_void;
        }
    }
    offset = cache.node.start;
    if drm_mm_node_allocated(&cache.node) {
        ((*(*ggtt).vm.insert_page)(
            &mut (*ggtt).vm,
            i915_gem_object_get_dma_address(obj, page),
            offset,
            i915_gem_get_pat_index((*ggtt).vm.i915, I915_CACHE_NONE),
            0,
        ));
    } else {
        offset += (page << PAGE_SHIFT) as u64;
    }
    let vaddr = io_mapping_map_atomic_wc(&mut (*ggtt).iomap, offset);
    cache.page = page;
    cache.vaddr = vaddr as usize;
    vaddr
}

// upstream: i915_gem_execbuffer.c reloc_vaddr()
pub unsafe fn reloc_vaddr(vma: *mut I915Vma, eb: *mut I915Execbuffer, page: usize) -> *mut c_void {
    let cache = &mut (*eb).reloc_cache;
    if cache.page == page {
        unmask_page(cache.vaddr)
    } else {
        let mut vaddr = core::ptr::null_mut();
        if cache.vaddr & KMAP == 0 {
            vaddr = reloc_iomap(vma, eb, page);
        }
        if vaddr.is_null() {
            vaddr = reloc_kmap((*vma).obj, cache, page);
        }
        vaddr
    }
}

// upstream: i915_gem_execbuffer.c clflush_write32()
pub unsafe fn clflush_write32(addr: *mut u32, value: u32, flushes: u32) {
    if flushes & (CLFLUSH_BEFORE | CLFLUSH_AFTER) != 0 {
        if flushes & CLFLUSH_BEFORE != 0 {
            drm_clflush_virt_range(addr as *const c_void, 4);
        }
        *addr = value;
        if flushes & CLFLUSH_AFTER != 0 {
            drm_clflush_virt_range(addr as *const c_void, 4);
        }
    } else {
        *addr = value;
    }
}

// upstream: i915_gem_execbuffer.c relocate_entry()
pub unsafe fn relocate_entry(
    vma: *mut I915Vma,
    reloc: *const DrmI915GemRelocationEntry,
    eb: *mut I915Execbuffer,
    target: *const I915Vma,
) -> u64 {
    let mut target_addr = relocation_target(reloc, target);
    let mut offset = (*reloc).offset;
    let mut wide = (*eb).reloc_cache.use_64bit_reloc;
    loop {
        let vaddr = reloc_vaddr(vma, eb, (offset >> PAGE_SHIFT) as usize);
        if IS_ERR(vaddr) {
            return PTR_ERR(vaddr) as u64;
        }
        GEM_BUG_ON(!IS_ALIGNED(offset, core::mem::size_of::<u32>() as u64));
        clflush_write32(
            vaddr.add(offset_in_page(offset) as usize) as *mut u32,
            lower_32_bits(target_addr),
            (*eb).reloc_cache.vaddr as u32,
        );
        if wide {
            offset += core::mem::size_of::<u32>() as u64;
            target_addr >>= 32;
            wide = false;
            continue;
        }
        return (*target).node.start | UPDATE;
    }
}

// upstream: i915_gem_execbuffer.c eb_relocate_entry()
pub unsafe fn eb_relocate_entry(
    eb: *mut I915Execbuffer,
    ev: *mut EbVma,
    reloc: *const DrmI915GemRelocationEntry,
) -> u64 {
    let i915 = (*eb).i915;
    let target = eb_get_vma(eb, (*reloc).target_handle as usize);
    if target.is_null() {
        return (-ENOENT) as u64;
    }
    if (*reloc).write_domain & ((*reloc).write_domain - 1) != 0 {
        drm_dbg(
            &mut (*i915).drm,
            "reloc with multiple write domains: target %d offset %d read %08x write %08x\n",
            (*reloc).target_handle,
            (*reloc).offset as i32,
            (*reloc).read_domains,
            (*reloc).write_domain,
        );
        return (-EINVAL) as u64;
    }
    if ((*reloc).write_domain | (*reloc).read_domains) & !I915_GEM_GPU_DOMAINS != 0 {
        drm_dbg(
            &mut (*i915).drm,
            "reloc with read/write non-GPU domains: target %d offset %d read %08x write %08x\n",
            (*reloc).target_handle,
            (*reloc).offset as i32,
            (*reloc).read_domains,
            (*reloc).write_domain,
        );
        return (-EINVAL) as u64;
    }
    if (*reloc).write_domain != 0 {
        (*target).flags |= EXEC_OBJECT_WRITE;
        if (*reloc).write_domain == I915_GEM_DOMAIN_INSTRUCTION
            && GRAPHICS_VER(i915) == 6
            && !i915_vma_is_bound((*target).vma, I915_VMA_GLOBAL_BIND)
        {
            let vma = (*target).vma;
            reloc_cache_unmap(&mut (*eb).reloc_cache);
            mutex_lock(&mut (*vma).vm.mutex);
            let err = i915_vma_bind(
                (*target).vma,
                (*(*vma).obj).pat_index,
                PIN_GLOBAL,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            );
            mutex_unlock(&mut (*vma).vm.mutex);
            reloc_cache_remap(&mut (*eb).reloc_cache, (*(*ev).vma).obj);
            if err != 0 {
                return err as u64;
            }
        }
    }
    if DBG_FORCE_RELOC == 0
        && gen8_canonical_addr(i915_vma_offset((*target).vma)) == (*reloc).presumed_offset
    {
        return 0;
    }
    if (*reloc).offset
        > (*(*ev).vma).size
            - if (*eb).reloc_cache.use_64bit_reloc {
                8
            } else {
                4
            }
    {
        drm_dbg(
            &mut (*i915).drm,
            "Relocation beyond object bounds: target %d offset %d size %d.\n",
            (*reloc).target_handle,
            (*reloc).offset as i32,
            (*(*ev).vma).size as i32,
        );
        return (-EINVAL) as u64;
    }
    if (*reloc).offset & 3 != 0 {
        drm_dbg(
            &mut (*i915).drm,
            "Relocation not 4-byte aligned: target %d offset %d.\n",
            (*reloc).target_handle,
            (*reloc).offset as i32,
        );
        return (-EINVAL) as u64;
    }
    (*ev).flags &= !EXEC_OBJECT_ASYNC;
    relocate_entry((*ev).vma, reloc, eb, (*target).vma)
}

// upstream: i915_gem_execbuffer.c eb_relocate_vma()
pub unsafe fn eb_relocate_vma(eb: *mut I915Execbuffer, ev: *mut EbVma) -> i32 {
    const RELOCS_PER_STACK: usize = 512 / core::mem::size_of::<DrmI915GemRelocationEntry>();
    let entry = (*ev).exec;
    let mut urelocs = u64_to_user_ptr((*entry).relocs_ptr) as *mut DrmI915GemRelocationEntry;
    let mut remain = (*entry).relocation_count as usize;
    let mut stack: [DrmI915GemRelocationEntry; RELOCS_PER_STACK] = core::mem::zeroed();
    if remain > (i32::MAX as usize / core::mem::size_of::<DrmI915GemRelocationEntry>()) {
        return -EINVAL;
    }
    if !access_ok(
        urelocs as *const c_void,
        remain * core::mem::size_of::<DrmI915GemRelocationEntry>(),
    ) {
        return -EFAULT;
    }
    loop {
        let count = core::cmp::min(remain, RELOCS_PER_STACK);
        pagefault_disable();
        let copied = __copy_from_user_inatomic(
            stack.as_mut_ptr() as *mut c_void,
            urelocs as *const c_void,
            count * core::mem::size_of::<DrmI915GemRelocationEntry>(),
        );
        pagefault_enable();
        if copied != 0 {
            remain = (-EFAULT) as usize;
            break;
        }
        remain -= count;
        let mut r = 0;
        let mut count_left = count;
        while count_left != 0 {
            let offset = eb_relocate_entry(eb, ev, stack.as_ptr().add(r));
            if offset == 0 {
                r += 1;
                count_left -= 1;
                continue;
            }
            if (offset as i64) < 0 {
                remain = offset as i32 as usize;
                break;
            }
            let offset = gen8_canonical_addr(offset & !UPDATE);
            __put_user(offset, &mut (*urelocs.add(r)).presumed_offset);
            r += 1;
            count_left -= 1;
        }
        if count_left != 0 {
            break;
        }
        urelocs = urelocs.add(RELOCS_PER_STACK);
        if remain == 0 {
            break;
        }
    }
    reloc_cache_reset(&mut (*eb).reloc_cache, eb);
    remain as i32
}

// upstream: i915_gem_execbuffer.c eb_relocate_vma_slow()
pub unsafe fn eb_relocate_vma_slow(eb: *mut I915Execbuffer, ev: *mut EbVma) -> i32 {
    let entry = (*ev).exec;
    let relocs = u64_to_ptr::<DrmI915GemRelocationEntry>((*entry).relocs_ptr);
    let mut err = 0;
    for i in 0..(*entry).relocation_count as usize {
        let offset = eb_relocate_entry(eb, ev, relocs.add(i));
        if (offset as i64) < 0 {
            err = offset as i32;
            break;
        }
    }
    reloc_cache_reset(&mut (*eb).reloc_cache, eb);
    err
}

// upstream: i915_gem_execbuffer.c check_relocations()
pub unsafe fn check_relocations(entry: *const DrmI915GemExecObject2) -> i32 {
    let mut size = (*entry).relocation_count as usize;
    if size == 0 {
        return 0;
    }
    if size > (i32::MAX as usize / core::mem::size_of::<DrmI915GemRelocationEntry>()) {
        return -EINVAL;
    }
    let addr = u64_to_user_ptr((*entry).relocs_ptr) as *mut u8;
    size *= core::mem::size_of::<DrmI915GemRelocationEntry>();
    if !access_ok(addr as *const c_void, size) {
        return -EFAULT;
    }
    let end = addr.add(size);
    let mut cur = addr;
    while cur < end {
        let mut c = 0u8;
        let err = __get_user(&mut c, cur);
        if err != 0 {
            return err;
        }
        cur = cur.add(PAGE_SIZE as usize);
    }
    let mut c = 0u8;
    __get_user(&mut c, end.sub(1))
}

// upstream: i915_gem_execbuffer.c eb_copy_relocations()
pub unsafe fn eb_copy_relocations(eb: *const I915Execbuffer) -> i32 {
    let mut relocs: *mut DrmI915GemRelocationEntry = core::ptr::null_mut();
    let count = (*eb).buffer_count;
    let mut i = 0;
    let mut err = 0;
    while i < count {
        let entry = (*eb).exec.add(i as usize);
        let nreloc = (*entry).relocation_count;
        if nreloc == 0 {
            i += 1;
            continue;
        }
        err = check_relocations(entry);
        if err != 0 {
            break;
        }
        let urelocs = u64_to_user_ptr((*entry).relocs_ptr) as *const u8;
        let size = nreloc as usize * core::mem::size_of::<DrmI915GemRelocationEntry>();
        relocs = kvmalloc_array(1, size, GFP_KERNEL) as *mut DrmI915GemRelocationEntry;
        if relocs.is_null() {
            err = -ENOMEM;
            break;
        }
        let mut copied = 0usize;
        while copied < size {
            let len = core::cmp::min(1usize << 31, size - copied);
            if __copy_from_user(relocs.cast::<u8>().add(copied), urelocs.add(copied), len) != 0 {
                break;
            }
            copied += len;
        }
        if copied < size {
            kvfree(relocs.cast());
            err = -EFAULT;
            break;
        }
        if !user_access_begin(urelocs.cast(), size) {
            kvfree(relocs.cast());
            err = -EFAULT;
            break;
        }
        let mut copied_reloc = 0;
        while copied_reloc < nreloc as usize {
            if unsafe_put_user(
                -1i64,
                &mut (*(urelocs as *mut DrmI915GemRelocationEntry).add(copied_reloc))
                    .presumed_offset,
            ) != 0
            {
                user_access_end();
                kvfree(relocs.cast());
                err = -EFAULT;
                break;
            }
            copied_reloc += 1;
        }
        if err != 0 {
            break;
        }
        user_access_end();
        (*(*eb).exec.add(i as usize)).relocs_ptr = relocs as usize as u64;
        i += 1;
    }
    if err == 0 {
        return 0;
    }
    // Upstream frees every earlier relocation copy, including the current
    // entry when it had already been installed into exec[].
    while i != 0 {
        i -= 1;
        let entry = (*eb).exec.add(i as usize);
        relocs = (*entry).relocs_ptr as usize as *mut DrmI915GemRelocationEntry;
        if (*entry).relocation_count != 0 {
            kvfree(relocs.cast());
        }
    }
    err
}

// upstream: i915_gem_execbuffer.c eb_prefault_relocations()
pub unsafe fn eb_prefault_relocations(eb: *const I915Execbuffer) -> i32 {
    for i in 0..(*eb).buffer_count {
        let err = check_relocations((*eb).exec.add(i as usize));
        if err != 0 {
            return err;
        }
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_reinit_userptr()
pub unsafe fn eb_reinit_userptr(eb: *mut I915Execbuffer) -> i32 {
    if (*eb).args.as_ref().unwrap().flags & EXEC_USERPTR_USED as u64 == 0 {
        return 0;
    }
    for i in 0..(*eb).buffer_count {
        let ev = (*eb).vma.add(i as usize);
        let obj = (*(*ev).vma).obj;
        if !i915_gem_object_is_userptr(obj) {
            continue;
        }
        let ret = i915_gem_object_userptr_submit_init(obj);
        if ret != 0 {
            return ret;
        }
        (*ev).flags |= EXEC_OBJECT_USERPTR_INIT;
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_relocate_parse_slow()
pub unsafe fn eb_relocate_parse_slow(eb: *mut I915Execbuffer) -> i32 {
    let mut have_copy = false;
    let mut err = 0;
    'repeat: loop {
        if signal_pending(current()) {
            err = -ERESTARTSYS;
            break;
        }
        eb_release_vmas(eb, false);
        i915_gem_ww_ctx_fini(&mut (*eb).ww);
        if err == 0 {
            err = eb_prefault_relocations(eb);
        } else if !have_copy {
            err = eb_copy_relocations(eb);
            have_copy = err == 0;
        } else {
            cond_resched();
            err = 0;
        }
        if err == 0 {
            err = eb_reinit_userptr(eb);
        }
        i915_gem_ww_ctx_init(&mut (*eb).ww, true);
        if err != 0 {
            break;
        }
        'repeat_validate: loop {
            err = eb_pin_engine(eb, false);
            if err != 0 {
                break;
            }
            err = eb_validate_vmas(eb);
            if err != 0 {
                break;
            }
            GEM_BUG_ON!((*eb).batches[0].is_null());
            let mut node = (*eb).relocs.next;
            while node != &mut (*eb).relocs as *mut ListHead {
                let ev = container_of!(node, EbVma, reloc_link);
                err = if !have_copy {
                    eb_relocate_vma(eb, ev)
                } else {
                    eb_relocate_vma_slow(eb, ev)
                };
                if err != 0 {
                    break;
                }
                node = (*node).next;
            }
            if err == -EDEADLK {
                break;
            }
            if err != 0 && !have_copy {
                continue 'repeat;
            }
            if err != 0 {
                break;
            }
            err = eb_parse(eb);
            if err == 0 {
                break;
            }
            break;
        }
        if err == -EDEADLK {
            eb_release_vmas(eb, false);
            err = i915_gem_ww_ctx_backoff(&mut (*eb).ww);
            if err == 0 {
                continue 'repeat_validate;
            }
        }
        if err == -EAGAIN {
            continue 'repeat;
        }
        break;
    }
    if have_copy {
        for i in 0..(*eb).buffer_count {
            let entry = (*eb).exec.add(i as usize);
            if (*entry).relocation_count == 0 {
                continue;
            }
            let relocs = (*entry).relocs_ptr as usize as *mut DrmI915GemRelocationEntry;
            kvfree(relocs.cast());
        }
    }
    err
}

// upstream: i915_gem_execbuffer.c eb_relocate_parse()
pub unsafe fn eb_relocate_parse(eb: *mut I915Execbuffer) -> i32 {
    let mut throttle = true;
    'retry: loop {
        let mut err = eb_pin_engine(eb, throttle);
        if err != 0 {
            if err != -EDEADLK {
                return err;
            }
            // Continue to the common ww backoff path below.
        } else {
            throttle = false;
            err = eb_validate_vmas(eb);
            if err == -EAGAIN {
                break;
            }
            if err != 0 {
                if err != -EDEADLK {
                    return err;
                }
            } else {
                if (*(*eb).args).flags & EXEC_HAS_RELOC as u64 != 0 {
                    let mut node = (*eb).relocs.next;
                    while node != &mut (*eb).relocs as *mut ListHead {
                        let ev = container_of!(node, EbVma, reloc_link);
                        err = eb_relocate_vma(eb, ev);
                        if err != 0 {
                            break;
                        }
                        node = (*node).next;
                    }
                    if err == -EDEADLK {
                        // Enter common ww backoff handling.
                    } else if err != 0 {
                        break;
                    }
                }
                if err == 0 {
                    err = eb_parse(eb);
                }
                if err != -EDEADLK {
                    return err;
                }
            }
        }
        eb_release_vmas(eb, false);
        err = i915_gem_ww_ctx_backoff(&mut (*eb).ww);
        if err != 0 {
            return err;
        }
        continue 'retry;
    }
    let err = eb_relocate_parse_slow(eb);
    if err != 0 {
        (*(*eb).args).flags &= !(EXEC_HAS_RELOC as u64);
    }
    err
}

// upstream: i915_gem_execbuffer.c eb_find_first_request_added()
pub unsafe fn eb_find_first_request_added(eb: *mut I915Execbuffer) -> *mut I915Request {
    let mut i = (*eb).num_batches as i32 - 1;
    while i >= 0 {
        if !(*eb).requests[i as usize].is_null() {
            return (*eb).requests[i as usize];
        }
        i -= 1;
    }
    GEM_BUG_ON!("Request not found");
    core::ptr::null_mut()
}

#[cfg(feature = "drm-i915-capture-error")]
// upstream: i915_gem_execbuffer.c eb_capture_stage()
pub unsafe fn eb_capture_stage(eb: *mut I915Execbuffer) -> i32 {
    let mut i = (*eb).buffer_count;
    while i != 0 {
        i -= 1;
        let ev = (*eb).vma.add(i as usize);
        let vma = (*ev).vma;
        if (*ev).flags & EXEC_OBJECT_CAPTURE == 0 {
            continue;
        }
        if i915_gem_context_is_recoverable((*eb).gem_context)
            && (IS_DGFX((*eb).i915) || GRAPHICS_VER_FULL((*eb).i915) > IP_VER(12, 0))
        {
            return -EINVAL;
        }
        for j in 0..(*eb).num_batches as usize {
            let capture = kmalloc_obj::<I915CaptureList>(GFP_KERNEL);
            if capture.is_null() {
                continue;
            }
            (*capture).next = (*eb).capture_lists[j];
            (*capture).vma_res = i915_vma_resource_get((*vma).resource);
            (*eb).capture_lists[j] = capture;
        }
    }
    0
}

#[cfg(feature = "drm-i915-capture-error")]
// upstream: i915_gem_execbuffer.c eb_capture_commit()
pub unsafe fn eb_capture_commit(eb: *mut I915Execbuffer) {
    for j in 0..(*eb).num_batches as usize {
        let rq = (*eb).requests[j];
        if rq.is_null() {
            break;
        }
        (*rq).capture_list = (*eb).capture_lists[j];
        (*eb).capture_lists[j] = core::ptr::null_mut();
    }
}

#[cfg(feature = "drm-i915-capture-error")]
// upstream: i915_gem_execbuffer.c eb_capture_release()
pub unsafe fn eb_capture_release(eb: *mut I915Execbuffer) {
    for j in 0..(*eb).num_batches as usize {
        if !(*eb).capture_lists[j].is_null() {
            i915_request_free_capture_list((*eb).capture_lists[j]);
            (*eb).capture_lists[j] = core::ptr::null_mut();
        }
    }
}

#[cfg(feature = "drm-i915-capture-error")]
// upstream: i915_gem_execbuffer.c eb_capture_list_clear()
pub unsafe fn eb_capture_list_clear(eb: *mut I915Execbuffer) {
    core::ptr::write_bytes(
        (*eb).capture_lists.as_mut_ptr(),
        0,
        (*eb).capture_lists.len(),
    );
}

#[cfg(not(feature = "drm-i915-capture-error"))]
// upstream: i915_gem_execbuffer.c eb_capture_stage()
pub unsafe fn eb_capture_stage(_eb: *mut I915Execbuffer) -> i32 {
    0
}

#[cfg(not(feature = "drm-i915-capture-error"))]
// upstream: i915_gem_execbuffer.c eb_capture_commit()
pub unsafe fn eb_capture_commit(_eb: *mut I915Execbuffer) {}

#[cfg(not(feature = "drm-i915-capture-error"))]
// upstream: i915_gem_execbuffer.c eb_capture_release()
pub unsafe fn eb_capture_release(_eb: *mut I915Execbuffer) {}

#[cfg(not(feature = "drm-i915-capture-error"))]
// upstream: i915_gem_execbuffer.c eb_capture_list_clear()
pub unsafe fn eb_capture_list_clear(_eb: *mut I915Execbuffer) {}

// upstream: i915_gem_execbuffer.c eb_move_to_gpu()
pub unsafe fn eb_move_to_gpu(eb: *mut I915Execbuffer) -> i32 {
    let count = (*eb).buffer_count;
    let mut i = count;
    let mut err = 0;
    while i != 0 {
        i -= 1;
        let ev = (*eb).vma.add(i as usize);
        let vma = (*ev).vma;
        let mut flags = (*ev).flags;
        let obj = (*vma).obj;
        assert_vma_held(vma);
        if (*obj).cache_dirty & !(*obj).cache_coherent != 0 {
            if i915_gem_clflush_object(obj, 0) {
                flags &= !EXEC_OBJECT_ASYNC;
            }
        }
        if err == 0 && flags & EXEC_OBJECT_ASYNC == 0 {
            err = i915_request_await_object(
                eb_find_first_request_added(eb),
                obj,
                flags & EXEC_OBJECT_WRITE != 0,
            );
        }
        let mut j = (*eb).num_batches as i32 - 1;
        while j >= 0 {
            if err != 0 {
                break;
            }
            if !(*eb).requests[j as usize].is_null() {
                let rq = (*eb).requests[j as usize];
                let fence = if j != 0 {
                    core::ptr::null_mut()
                } else if !(*eb).composite_fence.is_null() {
                    (*eb).composite_fence
                } else {
                    &mut (*rq).fence
                };
                err = _i915_vma_move_to_active(
                    vma,
                    rq,
                    fence,
                    flags | EXEC_OBJECT_NO_RESERVE | EXEC_OBJECT_NO_REQUEST_AWAIT,
                );
            }
            j -= 1;
        }
    }
    #[cfg(feature = "mmu-notifier")]
    if err == 0 && (*(*eb).args).flags & EXEC_USERPTR_USED as u64 != 0 {
        for i in 0..count {
            let obj = (*(*(*eb).vma.add(i as usize)).vma).obj;
            if !i915_gem_object_is_userptr(obj) {
                continue;
            }
            err = i915_gem_object_userptr_submit_done(obj);
            if err != 0 {
                break;
            }
        }
    }
    if err != 0 {
        for j in 0..(*eb).num_batches as usize {
            if (*eb).requests[j].is_null() {
                break;
            }
            i915_request_set_error_once((*eb).requests[j], err);
        }
        return err;
    }
    intel_gt_chipset_flush((*eb).gt);
    eb_capture_commit(eb);
    0
}

// upstream: i915_gem_execbuffer.c i915_gem_check_execbuffer()
pub unsafe fn i915_gem_check_execbuffer(
    i915: *mut DrmI915Private,
    exec: *mut DrmI915GemExecbuffer2,
) -> i32 {
    if (*exec).flags & I915_EXEC_ILLEGAL_FLAGS != 0 {
        return -EINVAL;
    }
    if (*exec).flags & (I915_EXEC_FENCE_ARRAY | I915_EXEC_USE_EXTENSIONS) == 0
        && ((*exec).num_cliprects != 0 || (*exec).cliprects_ptr != 0)
    {
        return -EINVAL;
    }
    if (*exec).DR4 == 0xffff_ffff {
        drm_dbg(&mut (*i915).drm, "UXA submitting garbage DR4, fixing up\n");
        (*exec).DR4 = 0;
    }
    if (*exec).DR1 != 0 || (*exec).DR4 != 0 {
        return -EINVAL;
    }
    if ((*exec).batch_start_offset | (*exec).batch_len) & 7 != 0 {
        return -EINVAL;
    }
    0
}

// upstream: i915_gem_execbuffer.c i915_reset_gen7_sol_offsets()
pub unsafe fn i915_reset_gen7_sol_offsets(rq: *mut I915Request) -> i32 {
    if GRAPHICS_VER((*rq).i915) != 7 || (*(*rq).engine).id != RCS0 {
        drm_dbg(&mut (*(*rq).i915).drm, "sol reset is gen7/rcs only\n");
        return -EINVAL;
    }
    let mut cs = intel_ring_begin(rq, 4 * 2 + 2);
    if IS_ERR(cs) {
        return PTR_ERR(cs);
    }
    *cs = MI_LOAD_REGISTER_IMM(4);
    cs = cs.add(1);
    for i in 0..4 {
        *cs = i915_mmio_reg_offset(GEN7_SO_WRITE_OFFSET(i));
        cs = cs.add(1);
        *cs = 0;
        cs = cs.add(1);
    }
    *cs = MI_NOOP;
    cs = cs.add(1);
    intel_ring_advance(rq, cs);
    0
}

// upstream: i915_gem_execbuffer.c shadow_batch_pin()
pub unsafe fn shadow_batch_pin(
    eb: *mut I915Execbuffer,
    obj: *mut DrmI915GemObject,
    vm: *mut I915AddressSpace,
    flags: u32,
) -> *mut I915Vma {
    let vma = i915_vma_instance(obj, vm, core::ptr::null_mut());
    if IS_ERR(vma) {
        return vma;
    }
    let err = i915_vma_pin_ww(vma, &mut (*eb).ww, 0, 0, flags | PIN_VALIDATE);
    if err != 0 {
        return ERR_PTR(err);
    }
    vma
}

// upstream: i915_gem_execbuffer.c eb_dispatch_secure()
pub unsafe fn eb_dispatch_secure(eb: *mut I915Execbuffer, vma: *mut I915Vma) -> *mut I915Vma {
    if (*eb).batch_flags & I915_DISPATCH_SECURE != 0 {
        i915_gem_object_ggtt_pin_ww(
            (*vma).obj,
            &mut (*eb).ww,
            core::ptr::null_mut(),
            0,
            0,
            PIN_VALIDATE,
        )
    } else {
        core::ptr::null_mut()
    }
}

// upstream: i915_gem_execbuffer.c eb_parse()
pub unsafe fn eb_parse(eb: *mut I915Execbuffer) -> i32 {
    let i915 = (*eb).i915;
    let mut pool = (*eb).batch_pool;
    let mut shadow;
    let mut trampoline;
    let mut batch;
    let mut len;
    if !eb_use_cmdparser(eb) {
        batch = eb_dispatch_secure(eb, (*(*eb).batches[0]).vma);
        if IS_ERR(batch) {
            return PTR_ERR(batch);
        }
        if !batch.is_null() {
            if intel_context_is_parallel((*eb).context) {
                return -EINVAL;
            }
            (*eb).batches[0] = (*eb).vma.add((*eb).buffer_count as usize);
            (*eb).buffer_count += 1;
            (*(*eb).batches[0]).flags = EXEC_OBJECT_HAS_PIN;
            (*(*eb).batches[0]).vma = i915_vma_get(batch);
        }
        return 0;
    }
    if intel_context_is_parallel((*eb).context) {
        return -EINVAL;
    }
    len = (*eb).batch_len[0] as usize;
    if !CMDPARSER_USES_GGTT((*eb).i915) {
        if !(*(*(*eb).context).vm).has_read_only {
            drm_dbg(
                &mut (*i915).drm,
                "Cannot prevent post-scan tampering without RO capable vm\n",
            );
            return -EINVAL;
        }
    } else {
        len += I915_CMD_PARSER_TRAMPOLINE_SIZE;
    }
    if len < (*eb).batch_len[0] as usize {
        return -EINVAL;
    }
    if pool.is_null() {
        pool = intel_gt_get_buffer_pool((*eb).gt, len, I915_MAP_WB);
        if IS_ERR(pool) {
            return PTR_ERR(pool);
        }
        (*eb).batch_pool = pool;
    }
    let mut err = i915_gem_object_lock((*pool).obj, &mut (*eb).ww);
    if err != 0 {
        return err;
    }
    shadow = shadow_batch_pin(eb, (*pool).obj, (*(*eb).context).vm, PIN_USER);
    if IS_ERR(shadow) {
        return PTR_ERR(shadow);
    }
    intel_gt_buffer_pool_mark_used(pool);
    i915_gem_object_set_readonly((*shadow).obj);
    (*shadow).private = pool as *mut c_void;
    trampoline = core::ptr::null_mut();
    if CMDPARSER_USES_GGTT((*eb).i915) {
        trampoline = shadow;
        shadow = shadow_batch_pin(eb, (*pool).obj, &mut (*(*(*eb).gt).ggtt).vm, PIN_GLOBAL);
        if IS_ERR(shadow) {
            return PTR_ERR(shadow);
        }
        (*shadow).private = pool as *mut c_void;
        (*eb).batch_flags |= I915_DISPATCH_SECURE;
    }
    batch = eb_dispatch_secure(eb, shadow);
    if IS_ERR(batch) {
        return PTR_ERR(batch);
    }
    err = dma_resv_reserve_fences((*(*shadow).obj).base.resv, 1);
    if err != 0 {
        return err;
    }
    err = intel_engine_cmd_parser(
        (*(*eb).context).engine,
        (*(*eb).batches[0]).vma,
        (*eb).batch_start_offset,
        (*eb).batch_len[0],
        shadow,
        trampoline,
    );
    if err != 0 {
        return err;
    }
    (*eb).batches[0] = (*eb).vma.add((*eb).buffer_count as usize);
    (*eb).buffer_count += 1;
    (*(*eb).batches[0]).vma = i915_vma_get(shadow);
    (*(*eb).batches[0]).flags = EXEC_OBJECT_HAS_PIN;
    (*eb).trampoline = trampoline;
    (*eb).batch_start_offset = 0;
    if !batch.is_null() {
        if intel_context_is_parallel((*eb).context) {
            return -EINVAL;
        }
        (*eb).batches[0] = (*eb).vma.add((*eb).buffer_count as usize);
        (*eb).buffer_count += 1;
        (*(*eb).batches[0]).flags = EXEC_OBJECT_HAS_PIN;
        (*(*eb).batches[0]).vma = i915_vma_get(batch);
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_request_submit()
pub unsafe fn eb_request_submit(
    eb: *mut I915Execbuffer,
    rq: *mut I915Request,
    batch: *mut I915Vma,
    batch_len: u64,
) -> i32 {
    if intel_context_nopreempt((*rq).context) {
        __set_bit(I915_FENCE_FLAG_NOPREEMPT, &mut (*rq).fence.flags);
    }
    if (*(*eb).args).flags & I915_EXEC_GEN7_SOL_RESET != 0 {
        let err = i915_reset_gen7_sol_offsets(rq);
        if err != 0 {
            return err;
        }
    }
    if let Some(emit) = (*(*(*rq).context).engine).emit_init_breadcrumb {
        let err = emit(rq);
        if err != 0 {
            return err;
        }
    }
    let err = intel_engine_emit_bb_start(
        (*(*rq).context).engine,
        rq,
        i915_vma_offset(batch) + (*eb).batch_start_offset as u64,
        batch_len,
        (*eb).batch_flags,
    );
    if err != 0 {
        return err;
    }
    if !(*eb).trampoline.is_null() {
        GEM_BUG_ON!(intel_context_is_parallel((*rq).context));
        GEM_BUG_ON!((*eb).batch_start_offset != 0);
        let err = intel_engine_emit_bb_start(
            (*(*rq).context).engine,
            rq,
            i915_vma_offset((*eb).trampoline) + batch_len,
            0,
            0,
        );
        if err != 0 {
            return err;
        }
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_submit()
pub unsafe fn eb_submit(eb: *mut I915Execbuffer) -> i32 {
    let mut err = eb_move_to_gpu(eb);
    for i in 0..(*eb).num_batches as usize {
        let rq = (*eb).requests[i];
        if rq.is_null() {
            break;
        }
        trace_i915_request_queue(rq, (*eb).batch_flags);
        if err == 0 {
            err = eb_request_submit(eb, rq, (*(*eb).batches[i]).vma, (*eb).batch_len[i]);
        }
    }
    err
}

// upstream: i915_gem_execbuffer.c gen8_dispatch_bsd_engine()
pub unsafe fn gen8_dispatch_bsd_engine(i915: *mut DrmI915Private, file: *mut DrmFile) -> u32 {
    let file_priv = (*file).driver_priv as *mut DrmI915FilePrivate;
    if (*file_priv).bsd_engine < 0 {
        (*file_priv).bsd_engine =
            get_random_u32_below((*i915).engine_uabi_class_count[I915_ENGINE_CLASS_VIDEO as usize])
                as i32;
    }
    (*file_priv).bsd_engine as u32
}

pub const USER_RING_MAP: [i32; 5] = [RCS0, RCS0, BCS0, VCS0, VECS0];

// upstream: i915_gem_execbuffer.c eb_throttle()
pub unsafe fn eb_throttle(_eb: *mut I915Execbuffer, ce: *mut IntelContext) -> *mut I915Request {
    let ring = (*ce).ring;
    let tl = (*ce).timeline;
    if intel_ring_update_space(ring) >= PAGE_SIZE as u32 {
        return core::ptr::null_mut();
    }
    let head = &mut (*tl).requests as *mut ListHead;
    let mut link = (*head).next;
    while link != head {
        let rq = container_of!(link, I915Request, link);
        if (*rq).ring == ring
            && __intel_ring_space((*rq).postfix, (*ring).emit, (*ring).size) > (*ring).size / 2
        {
            return i915_request_get(rq);
        }
        link = (*link).next;
    }
    // As in list_for_each_entry(), the sentinel is reached when no qualifying
    // request exists; the upstream empty-list check returns NULL here.
    core::ptr::null_mut()
}

// upstream: i915_gem_execbuffer.c eb_pin_timeline()
pub unsafe fn eb_pin_timeline(
    eb: *mut I915Execbuffer,
    ce: *mut IntelContext,
    throttle: bool,
) -> i32 {
    let tl = intel_context_timeline_lock(ce);
    if IS_ERR(tl) {
        return PTR_ERR(tl);
    }
    intel_context_enter(ce);
    let rq = if throttle {
        eb_throttle(eb, ce)
    } else {
        core::ptr::null_mut()
    };
    intel_context_timeline_unlock(tl);
    if !rq.is_null() {
        let nonblock = (*(*(*eb).file).filp).f_flags & O_NONBLOCK != 0;
        let timeout = if nonblock { 0 } else { MAX_SCHEDULE_TIMEOUT };
        if i915_request_wait(rq, I915_WAIT_INTERRUPTIBLE, timeout) < 0 {
            i915_request_put(rq);
            mutex_lock(&mut (*(*ce).timeline).mutex);
            intel_context_exit(ce);
            mutex_unlock(&mut (*(*ce).timeline).mutex);
            return if nonblock { -EWOULDBLOCK } else { -EINTR };
        }
        i915_request_put(rq);
    }
    0
}

// upstream: i915_gem_execbuffer.c eb_pin_engine()
pub unsafe fn eb_pin_engine(eb: *mut I915Execbuffer, throttle: bool) -> i32 {
    let ce = (*eb).context;
    GEM_BUG_ON!((*(*eb).args).flags & EXEC_ENGINE_PINNED as u64 != 0);
    if intel_context_is_banned(ce) {
        return -EIO;
    }
    let mut err = intel_context_pin_ww(ce, &mut (*eb).ww);
    if err != 0 {
        return err;
    }
    let mut child = intel_context_first_child(ce);
    while !child.is_null() {
        err = intel_context_pin_ww(child, &mut (*eb).ww);
        GEM_BUG_ON!(err != 0);
        child = intel_context_next_child(ce, child);
    }
    let mut i = 0;
    child = intel_context_first_child(ce);
    while !child.is_null() {
        err = eb_pin_timeline(eb, child, throttle);
        if err != 0 {
            break;
        }
        i += 1;
        child = intel_context_next_child(ce, child);
    }
    if err == 0 {
        err = eb_pin_timeline(eb, ce, throttle);
    }
    if err == 0 {
        (*(*eb).args).flags |= EXEC_ENGINE_PINNED as u64;
        return 0;
    }
    let mut j = 0;
    child = intel_context_first_child(ce);
    while !child.is_null() {
        if j < i {
            mutex_lock(&mut (*(*child).timeline).mutex);
            intel_context_exit(child);
            mutex_unlock(&mut (*(*child).timeline).mutex);
        }
        j += 1;
        child = intel_context_next_child(ce, child);
    }
    child = intel_context_first_child(ce);
    while !child.is_null() {
        intel_context_unpin(child);
        child = intel_context_next_child(ce, child);
    }
    intel_context_unpin(ce);
    err
}

// upstream: i915_gem_execbuffer.c eb_unpin_engine()
pub unsafe fn eb_unpin_engine(eb: *mut I915Execbuffer) {
    let ce = (*eb).context;
    if (*(*eb).args).flags & EXEC_ENGINE_PINNED as u64 == 0 {
        return;
    }
    (*(*eb).args).flags &= !(EXEC_ENGINE_PINNED as u64);
    let mut child = intel_context_first_child(ce);
    while !child.is_null() {
        mutex_lock(&mut (*(*child).timeline).mutex);
        intel_context_exit(child);
        mutex_unlock(&mut (*(*child).timeline).mutex);
        intel_context_unpin(child);
        child = intel_context_next_child(ce, child);
    }
    mutex_lock(&mut (*(*ce).timeline).mutex);
    intel_context_exit(ce);
    mutex_unlock(&mut (*(*ce).timeline).mutex);
    intel_context_unpin(ce);
}

// upstream: i915_gem_execbuffer.c eb_select_legacy_ring()
pub unsafe fn eb_select_legacy_ring(eb: *mut I915Execbuffer) -> u32 {
    let i915 = (*eb).i915;
    let args = (*eb).args;
    let user_ring_id = ((*args).flags & I915_EXEC_RING_MASK) as usize;
    if user_ring_id != I915_EXEC_BSD as usize && (*args).flags & I915_EXEC_BSD_MASK != 0 {
        drm_dbg(
            &mut (*i915).drm,
            "execbuf with non bsd ring but with invalid bsd dispatch flags: %d\n",
            (*args).flags as i32,
        );
        return u32::MAX;
    }
    if user_ring_id == I915_EXEC_BSD as usize
        && (*i915).engine_uabi_class_count[I915_ENGINE_CLASS_VIDEO as usize] > 1
    {
        let mut bsd_idx = ((*args).flags & I915_EXEC_BSD_MASK) as u32;
        if bsd_idx == I915_EXEC_BSD_DEFAULT {
            bsd_idx = gen8_dispatch_bsd_engine(i915, (*eb).file);
        } else if (I915_EXEC_BSD_RING1..=I915_EXEC_BSD_RING2).contains(&bsd_idx) {
            bsd_idx >>= I915_EXEC_BSD_SHIFT;
            bsd_idx -= 1;
        } else {
            drm_dbg(
                &mut (*i915).drm,
                "execbuf with unknown bsd ring: %u\n",
                bsd_idx,
            );
            return u32::MAX;
        }
        return _VCS(bsd_idx as i32) as u32;
    }
    if user_ring_id >= USER_RING_MAP.len() {
        drm_dbg(
            &mut (*i915).drm,
            "execbuf with unknown ring: %u\n",
            user_ring_id as u32,
        );
        return u32::MAX;
    }
    USER_RING_MAP[user_ring_id] as u32
}

// upstream: i915_gem_execbuffer.c eb_select_engine()
pub unsafe fn eb_select_engine(eb: *mut I915Execbuffer) -> i32 {
    let idx = if i915_gem_context_user_engines((*eb).gem_context) {
        ((*(*eb).args).flags & I915_EXEC_RING_MASK) as u32
    } else {
        eb_select_legacy_ring(eb)
    };
    let ce = i915_gem_context_get_engine((*eb).gem_context, idx);
    if IS_ERR(ce) {
        return PTR_ERR(ce);
    }
    let mut child = core::ptr::null_mut();
    if intel_context_is_parallel(ce) {
        if (*eb).buffer_count < (*ce).parallel.number_children + 1 {
            intel_context_put(ce);
            return -EINVAL;
        }
        if (*eb).batch_start_offset != 0 || (*(*eb).args).batch_len != 0 {
            intel_context_put(ce);
            return -EINVAL;
        }
    }
    (*eb).num_batches = (*ce).parallel.number_children + 1;
    let gt = (*(*ce).engine).gt;
    child = intel_context_first_child(ce);
    while !child.is_null() {
        intel_context_get(child);
        child = intel_context_next_child(ce, child);
    }
    (*eb).wakeref = intel_gt_pm_get((*(*ce).engine).gt);
    if (*gt).info.id != 0 {
        (*eb).wakeref_gt0 = intel_gt_pm_get(to_gt((*gt).i915));
    }
    let mut err = 0;
    if !test_bit(CONTEXT_ALLOC_BIT, &(*ce).flags) {
        err = intel_context_alloc_state(ce);
        if err != 0 {
            return eb_select_engine_err(eb, ce, gt, err);
        }
    }
    child = intel_context_first_child(ce);
    while !child.is_null() {
        if !test_bit(CONTEXT_ALLOC_BIT, &(*child).flags) {
            err = intel_context_alloc_state(child);
            if err != 0 {
                return eb_select_engine_err(eb, ce, gt, err);
            }
        }
        child = intel_context_next_child(ce, child);
    }
    err = intel_gt_terminally_wedged((*(*ce).engine).gt);
    if err != 0 {
        return eb_select_engine_err(eb, ce, gt, err);
    }
    if !i915_vm_tryget((*ce).vm) {
        return eb_select_engine_err(eb, ce, gt, -ENOENT);
    }
    (*eb).context = ce;
    (*eb).gt = (*(*ce).engine).gt;
    err
}

// Shared unwind for the error label in eb_select_engine().
unsafe fn eb_select_engine_err(
    eb: *mut I915Execbuffer,
    ce: *mut IntelContext,
    gt: *mut IntelGt,
    err: i32,
) -> i32 {
    if (*gt).info.id != 0 {
        intel_gt_pm_put(to_gt((*gt).i915), (*eb).wakeref_gt0);
    }
    intel_gt_pm_put((*(*ce).engine).gt, (*eb).wakeref);
    let mut child = intel_context_first_child(ce);
    while !child.is_null() {
        intel_context_put(child);
        child = intel_context_next_child(ce, child);
    }
    intel_context_put(ce);
    err
}

// upstream: i915_gem_execbuffer.c eb_put_engine()
pub unsafe fn eb_put_engine(eb: *mut I915Execbuffer) {
    let ce = (*eb).context;
    i915_vm_put((*ce).vm);
    if (*(*eb).gt).info.id != 0 {
        intel_gt_pm_put(to_gt((*(*eb).gt).i915), (*eb).wakeref_gt0);
    }
    intel_gt_pm_put((*(*ce).engine).gt, (*eb).wakeref);
    let mut child = intel_context_first_child(ce);
    while !child.is_null() {
        intel_context_put(child);
        child = intel_context_next_child(ce, child);
    }
    intel_context_put(ce);
}

// upstream: i915_gem_execbuffer.c __free_fence_array()
pub unsafe fn __free_fence_array(fences: *mut EbFence, mut n: u32) {
    while n != 0 {
        n -= 1;
        drm_syncobj_put(ptr_mask_bits((*fences.add(n as usize)).syncobj, 2));
        dma_fence_put((*fences.add(n as usize)).dma_fence);
        dma_fence_chain_free((*fences.add(n as usize)).chain_fence);
    }
    kvfree(fences.cast());
}

// upstream: i915_gem_execbuffer.c add_timeline_fence_array()
pub unsafe fn add_timeline_fence_array(
    eb: *mut I915Execbuffer,
    timeline_fences: *const DrmI915GemExecbufferExtTimelineFences,
) -> i32 {
    let mut nfences = (*timeline_fences).fence_count;
    if nfences == 0 {
        return 0;
    }
    let mut user_fences =
        u64_to_user_ptr((*timeline_fences).handles_ptr) as *mut DrmI915GemExecFence;
    if nfences > (ULONG_MAX as u64 / core::mem::size_of::<DrmI915GemExecFence>() as u64)
        || nfences
            > (usize::MAX as u64 / core::mem::size_of::<EbFence>() as u64) - (*eb).num_fences as u64
    {
        return -EINVAL;
    }
    if !access_ok(
        user_fences.cast(),
        nfences as usize * core::mem::size_of::<DrmI915GemExecFence>(),
    ) {
        return -EFAULT;
    }
    let mut user_values = u64_to_user_ptr((*timeline_fences).values_ptr) as *mut u64;
    if !access_ok(
        user_values.cast(),
        nfences as usize * core::mem::size_of::<u64>(),
    ) {
        return -EFAULT;
    }
    let total = (*eb).num_fences + nfences as usize;
    let mut f = krealloc(
        (*eb).fences,
        total * core::mem::size_of::<EbFence>(),
        __GFP_NOWARN | GFP_KERNEL,
    ) as *mut EbFence;
    if f.is_null() {
        return -ENOMEM;
    }
    (*eb).fences = f;
    f = f.add((*eb).num_fences);
    let mut err = 0;
    while nfences != 0 {
        nfences -= 1;
        let mut user_fence: DrmI915GemExecFence = core::mem::zeroed();
        if __copy_from_user(
            &mut user_fence as *mut _ as *mut c_void,
            user_fences as *const c_void,
            core::mem::size_of::<DrmI915GemExecFence>(),
        ) != 0
        {
            return -EFAULT;
        }
        user_fences = user_fences.add(1);
        if user_fence.flags & I915_EXEC_FENCE_UNKNOWN_FLAGS != 0 {
            return -EINVAL;
        }
        let mut point = 0u64;
        if __get_user(&mut point, user_values) != 0 {
            return -EFAULT;
        }
        user_values = user_values.add(1);
        let syncobj = drm_syncobj_find((*eb).file, user_fence.handle);
        if syncobj.is_null() {
            drm_dbg(&mut (*(*eb).i915).drm, "Invalid syncobj handle provided\n");
            return -ENOENT;
        }
        let mut fence = drm_syncobj_fence_get(syncobj);
        if fence.is_null()
            && user_fence.flags != 0
            && user_fence.flags & I915_EXEC_FENCE_SIGNAL == 0
        {
            drm_dbg(&mut (*(*eb).i915).drm, "Syncobj handle has no fence\n");
            drm_syncobj_put(syncobj);
            return -EINVAL;
        }
        if !fence.is_null() {
            err = dma_fence_chain_find_seqno(&mut fence, point);
        }
        if err != 0 && user_fence.flags & I915_EXEC_FENCE_SIGNAL == 0 {
            drm_dbg(
                &mut (*(*eb).i915).drm,
                "Syncobj handle missing requested point %llu\n",
                point,
            );
            dma_fence_put(fence);
            drm_syncobj_put(syncobj);
            return err;
        }
        if fence.is_null() && user_fence.flags & I915_EXEC_FENCE_SIGNAL == 0 {
            drm_syncobj_put(syncobj);
            continue;
        }
        let chain_fence;
        if point != 0 && user_fence.flags & I915_EXEC_FENCE_SIGNAL != 0 {
            if user_fence.flags & I915_EXEC_FENCE_WAIT != 0 {
                drm_dbg(
                    &mut (*(*eb).i915).drm,
                    "Trying to wait & signal the same timeline point.\n",
                );
                dma_fence_put(fence);
                drm_syncobj_put(syncobj);
                return -EINVAL;
            }
            chain_fence = dma_fence_chain_alloc();
            if chain_fence.is_null() {
                drm_syncobj_put(syncobj);
                dma_fence_put(fence);
                return -ENOMEM;
            }
        } else {
            chain_fence = core::ptr::null_mut();
        }
        (*f).syncobj = ptr_pack_bits(syncobj, user_fence.flags, 2);
        (*f).dma_fence = fence;
        (*f).value = point;
        (*f).chain_fence = chain_fence;
        f = f.add(1);
        (*eb).num_fences += 1;
    }
    0
}

// upstream: i915_gem_execbuffer.c add_fence_array()
pub unsafe fn add_fence_array(eb: *mut I915Execbuffer) -> i32 {
    let args = (*eb).args;
    if (*args).flags & I915_EXEC_FENCE_ARRAY == 0 {
        return 0;
    }
    let mut num_fences = (*args).num_cliprects as usize;
    if num_fences == 0 {
        return 0;
    }
    let mut user = u64_to_user_ptr((*args).cliprects_ptr) as *mut DrmI915GemExecFence;
    if num_fences > ULONG_MAX as usize / core::mem::size_of::<DrmI915GemExecFence>()
        || num_fences > usize::MAX / core::mem::size_of::<EbFence>() - (*eb).num_fences
    {
        return -EINVAL;
    }
    if !access_ok(
        user.cast(),
        num_fences * core::mem::size_of::<DrmI915GemExecFence>(),
    ) {
        return -EFAULT;
    }
    let total = (*eb).num_fences + num_fences;
    let mut f = krealloc(
        (*eb).fences,
        total * core::mem::size_of::<EbFence>(),
        __GFP_NOWARN | GFP_KERNEL,
    ) as *mut EbFence;
    if f.is_null() {
        return -ENOMEM;
    }
    (*eb).fences = f;
    f = f.add((*eb).num_fences);
    while num_fences != 0 {
        num_fences -= 1;
        let mut user_fence: DrmI915GemExecFence = core::mem::zeroed();
        if __copy_from_user(
            &mut user_fence as *mut _ as *mut c_void,
            user.cast(),
            core::mem::size_of::<DrmI915GemExecFence>(),
        ) != 0
        {
            return -EFAULT;
        }
        user = user.add(1);
        if user_fence.flags & I915_EXEC_FENCE_UNKNOWN_FLAGS != 0 {
            return -EINVAL;
        }
        let syncobj = drm_syncobj_find((*eb).file, user_fence.handle);
        if syncobj.is_null() {
            drm_dbg(&mut (*(*eb).i915).drm, "Invalid syncobj handle provided\n");
            return -ENOENT;
        }
        let mut fence = core::ptr::null_mut();
        if user_fence.flags & I915_EXEC_FENCE_WAIT != 0 {
            fence = drm_syncobj_fence_get(syncobj);
            if fence.is_null() {
                drm_dbg(&mut (*(*eb).i915).drm, "Syncobj handle has no fence\n");
                drm_syncobj_put(syncobj);
                return -EINVAL;
            }
        }
        (*f).syncobj = ptr_pack_bits(syncobj, user_fence.flags, 2);
        (*f).dma_fence = fence;
        (*f).value = 0;
        (*f).chain_fence = core::ptr::null_mut();
        f = f.add(1);
        (*eb).num_fences += 1;
    }
    0
}

// upstream: i915_gem_execbuffer.c put_fence_array()
pub unsafe fn put_fence_array(fences: *mut EbFence, num_fences: i32) {
    if !fences.is_null() {
        __free_fence_array(fences, num_fences as u32);
    }
}

// upstream: i915_gem_execbuffer.c await_fence_array()
pub unsafe fn await_fence_array(eb: *mut I915Execbuffer, rq: *mut I915Request) -> i32 {
    for n in 0..(*eb).num_fences {
        let fence = (*(*eb).fences.add(n)).dma_fence;
        if fence.is_null() {
            continue;
        }
        let err = i915_request_await_dma_fence(rq, fence);
        if err < 0 {
            return err;
        }
    }
    0
}

// upstream: i915_gem_execbuffer.c signal_fence_array()
pub unsafe fn signal_fence_array(eb: *const I915Execbuffer, fence: *mut DmaFence) {
    for n in 0..(*eb).num_fences {
        let f = (*eb).fences.add(n);
        let mut flags = 0u32;
        let syncobj = ptr_unpack_bits((*f).syncobj, &mut flags, 2);
        if flags & I915_EXEC_FENCE_SIGNAL == 0 {
            continue;
        }
        if !(*f).chain_fence.is_null() {
            drm_syncobj_add_point(syncobj, (*f).chain_fence, fence, (*f).value);
            (*f).chain_fence = core::ptr::null_mut();
        } else {
            drm_syncobj_replace_fence(syncobj, fence);
        }
    }
}

// upstream: i915_gem_execbuffer.c parse_timeline_fences()
pub unsafe fn parse_timeline_fences(ext: *mut I915UserExtension, data: *mut c_void) -> i32 {
    let eb = data as *mut I915Execbuffer;
    let mut timeline_fences: DrmI915GemExecbufferExtTimelineFences = core::mem::zeroed();
    if copy_from_user(
        &mut timeline_fences as *mut _ as *mut c_void,
        ext.cast(),
        core::mem::size_of_val(&timeline_fences),
    ) != 0
    {
        return -EFAULT;
    }
    add_timeline_fence_array(eb, &timeline_fences)
}

// upstream: i915_gem_execbuffer.c retire_requests()
pub unsafe fn retire_requests(tl: *mut IntelTimeline, end: *mut I915Request) {
    let mut rq = list_first_entry_or_null!(&mut (*tl).requests, I915Request, link);
    while !rq.is_null() {
        let next = i915_request_next(rq, tl);
        if rq == end || !i915_request_retire(rq) {
            break;
        }
        rq = next;
    }
}

// upstream: i915_gem_execbuffer.c eb_request_add()
pub unsafe fn eb_request_add(
    eb: *mut I915Execbuffer,
    rq: *mut I915Request,
    mut err: i32,
    last_parallel: bool,
) -> i32 {
    let tl = i915_request_timeline(rq);
    let mut attr = I915SchedAttr::default();
    lockdep_assert_held(&(*tl).mutex);
    lockdep_unpin_lock(&(*tl).mutex, (*rq).cookie);
    trace_i915_request_add(rq);
    let prev = __i915_request_commit(rq);
    if !intel_context_is_closed((*eb).context) {
        attr = (*(*eb).gem_context).sched;
    } else {
        i915_request_set_error_once(rq, -ENOENT);
        __i915_request_skip(rq);
        err = -ENOENT;
    }
    if intel_context_is_parallel((*eb).context) {
        if err != 0 {
            __i915_request_skip(rq);
            set_bit(I915_FENCE_FLAG_SKIP_PARALLEL, &mut (*rq).fence.flags);
        }
        if last_parallel {
            set_bit(I915_FENCE_FLAG_SUBMIT_PARALLEL, &mut (*rq).fence.flags);
        }
    }
    __i915_request_queue(rq, &attr);
    if !prev.is_null() {
        retire_requests(tl, prev);
    }
    mutex_unlock(&mut (*tl).mutex);
    err
}

// upstream: i915_gem_execbuffer.c eb_requests_add()
pub unsafe fn eb_requests_add(eb: *mut I915Execbuffer, mut err: i32) -> i32 {
    let mut i = (*eb).num_batches as i32 - 1;
    while i >= 0 {
        let rq = (*eb).requests[i as usize];
        if !rq.is_null() {
            err |= eb_request_add(eb, rq, err, i == 0);
        }
        i -= 1;
    }
    err
}

// DRM_I915_GEM_EXECBUFFER_EXT_TIMELINE_FENCES extension callback table.
pub static EXECBUF_EXTENSIONS: [Option<unsafe fn(*mut I915UserExtension, *mut c_void) -> i32>; 1] =
    [Some(parse_timeline_fences)];

// upstream: i915_gem_execbuffer.c parse_execbuf2_extensions()
pub unsafe fn parse_execbuf2_extensions(
    args: *mut DrmI915GemExecbuffer2,
    eb: *mut I915Execbuffer,
) -> i32 {
    if (*args).flags & I915_EXEC_USE_EXTENSIONS == 0 {
        return 0;
    }
    if (*eb).args.as_ref().unwrap().flags & I915_EXEC_FENCE_ARRAY != 0 {
        return -EINVAL;
    }
    if (*args).num_cliprects != 0 {
        return -EINVAL;
    }
    i915_user_extensions(
        u64_to_user_ptr((*args).cliprects_ptr) as *mut I915UserExtension,
        EXECBUF_EXTENSIONS.as_ptr(),
        EXECBUF_EXTENSIONS.len(),
        eb.cast(),
    )
}

// upstream: i915_gem_execbuffer.c eb_requests_get()
pub unsafe fn eb_requests_get(eb: *mut I915Execbuffer) {
    for i in 0..(*eb).num_batches as usize {
        let rq = (*eb).requests[i];
        if rq.is_null() {
            break;
        }
        i915_request_get(rq);
    }
}

// upstream: i915_gem_execbuffer.c eb_requests_put()
pub unsafe fn eb_requests_put(eb: *mut I915Execbuffer) {
    for i in 0..(*eb).num_batches as usize {
        let rq = (*eb).requests[i];
        if rq.is_null() {
            break;
        }
        i915_request_put(rq);
    }
}

// upstream: i915_gem_execbuffer.c eb_composite_fence_create()
pub unsafe fn eb_composite_fence_create(
    eb: *mut I915Execbuffer,
    out_fence_fd: i32,
) -> *mut SyncFile {
    GEM_BUG_ON!(!intel_context_is_parent((*eb).context));
    let fences = kmalloc_array(
        (*eb).num_batches as usize,
        core::mem::size_of::<*mut DmaFence>(),
        GFP_KERNEL,
    ) as *mut *mut DmaFence;
    if fences.is_null() {
        return ERR_PTR(-ENOMEM);
    }
    for i in 0..(*eb).num_batches as usize {
        *fences.add(i) = &mut (*(*eb).requests[i]).fence;
        __set_bit(
            I915_FENCE_FLAG_COMPOSITE,
            &mut (*(*eb).requests[i]).fence.flags,
        );
    }
    let fence_array = dma_fence_array_create(
        (*eb).num_batches,
        fences,
        (*(*eb).context).parallel.fence_context,
        (*(*eb).context).parallel.seqno,
    );
    (*(*eb).context).parallel.seqno += 1;
    if fence_array.is_null() {
        kfree(fences.cast());
        return ERR_PTR(-ENOMEM);
    }
    for i in 0..(*eb).num_batches as usize {
        dma_fence_get(*fences.add(i));
    }
    let mut out_fence = core::ptr::null_mut();
    if out_fence_fd != -1 {
        out_fence = sync_file_create(&mut (*fence_array).base);
        dma_fence_put(&mut (*fence_array).base);
        if out_fence.is_null() {
            return ERR_PTR(-ENOMEM);
        }
    }
    (*eb).composite_fence = &mut (*fence_array).base;
    out_fence
}

// upstream: i915_gem_execbuffer.c eb_fences_add()
pub unsafe fn eb_fences_add(
    eb: *mut I915Execbuffer,
    rq: *mut I915Request,
    in_fence: *mut DmaFence,
    out_fence_fd: i32,
) -> *mut SyncFile {
    if !(*(*eb).gem_context).syncobj.is_null() {
        let fence = drm_syncobj_fence_get((*(*eb).gem_context).syncobj);
        let err = i915_request_await_dma_fence(rq, fence);
        dma_fence_put(fence);
        if err != 0 {
            return ERR_PTR(err);
        }
    }
    if !in_fence.is_null() {
        let err = if (*(*eb).args).flags & I915_EXEC_FENCE_SUBMIT != 0 {
            i915_request_await_execution(rq, in_fence)
        } else {
            i915_request_await_dma_fence(rq, in_fence)
        };
        if err < 0 {
            return ERR_PTR(err);
        }
    }
    if !(*eb).fences.is_null() {
        let err = await_fence_array(eb, rq);
        if err != 0 {
            return ERR_PTR(err);
        }
    }
    if intel_context_is_parallel((*eb).context) {
        let out_fence = eb_composite_fence_create(eb, out_fence_fd);
        if IS_ERR(out_fence) {
            return ERR_PTR(-ENOMEM);
        }
        out_fence
    } else if out_fence_fd != -1 {
        let out_fence = sync_file_create(&mut (*rq).fence);
        if out_fence.is_null() {
            return ERR_PTR(-ENOMEM);
        }
        out_fence
    } else {
        core::ptr::null_mut()
    }
}

// upstream: i915_gem_execbuffer.c eb_find_context()
pub unsafe fn eb_find_context(
    eb: *mut I915Execbuffer,
    mut context_number: u32,
) -> *mut IntelContext {
    if context_number == 0 {
        return (*eb).context;
    }
    let mut child = intel_context_first_child((*eb).context);
    while !child.is_null() {
        context_number -= 1;
        if context_number == 0 {
            return child;
        }
        child = intel_context_next_child((*eb).context, child);
    }
    GEM_BUG_ON!("Context not found");
    core::ptr::null_mut()
}

// upstream: i915_gem_execbuffer.c eb_requests_create()
pub unsafe fn eb_requests_create(
    eb: *mut I915Execbuffer,
    in_fence: *mut DmaFence,
    out_fence_fd: i32,
) -> *mut SyncFile {
    let mut out_fence = core::ptr::null_mut();
    for i in 0..(*eb).num_batches as usize {
        (*eb).requests[i] = i915_request_create(eb_find_context(eb, i as u32));
        if IS_ERR((*eb).requests[i]) {
            out_fence = ERR_CAST((*eb).requests[i]);
            (*eb).requests[i] = core::ptr::null_mut();
            return out_fence;
        }
        if i + 1 == (*eb).num_batches as usize {
            out_fence = eb_fences_add(eb, (*eb).requests[i], in_fence, out_fence_fd);
            if IS_ERR(out_fence) {
                return out_fence;
            }
        }
        let batch_vma = (*(*eb).batches[i]).vma;
        if !batch_vma.is_null() {
            (*(*eb).requests[i]).batch_res = i915_vma_resource_get((*batch_vma).resource);
        }
        if !(*eb).batch_pool.is_null() {
            GEM_BUG_ON!(intel_context_is_parallel((*eb).context));
            intel_gt_buffer_pool_mark_active((*eb).batch_pool, (*eb).requests[i]);
        }
    }
    out_fence
}

// upstream: i915_gem_execbuffer.c i915_gem_do_execbuffer()
pub unsafe fn i915_gem_do_execbuffer(
    dev: *mut DrmDevice,
    file: *mut DrmFile,
    args: *mut DrmI915GemExecbuffer2,
    exec: *mut DrmI915GemExecObject2,
) -> i32 {
    let i915 = to_i915(dev);
    let mut eb: I915Execbuffer = core::mem::zeroed();
    let mut in_fence: *mut DmaFence = core::ptr::null_mut();
    let mut out_fence: *mut SyncFile = core::ptr::null_mut();
    let mut out_fence_fd = -1;
    let mut err;
    let mut bucket_created = false;
    let mut context_selected = false;
    let mut engine_selected = false;
    let mut vmas_acquired = false;
    let mut ww_initialized = false;
    let mut request_path = false;
    if EXEC_INTERNAL_FLAGS & !I915_EXEC_ILLEGAL_FLAGS != 0 {
        build_bug();
    }
    if EXEC_OBJECT_INTERNAL_FLAGS & !EXEC_OBJECT_UNKNOWN_FLAGS != 0 {
        build_bug();
    }
    eb.i915 = i915;
    eb.file = file;
    eb.args = args;
    if DBG_FORCE_RELOC != 0 || (*args).flags & I915_EXEC_NO_RELOC == 0 {
        (*args).flags |= EXEC_HAS_RELOC as u64;
    }
    eb.exec = exec;
    eb.vma = exec.add((*args).buffer_count as usize + 1).cast();
    core::ptr::write_bytes(eb.vma, 0, (*args).buffer_count as usize + 1);
    eb.batch_pool = core::ptr::null_mut();
    eb.invalid_flags = EXEC_OBJECT_UNKNOWN_FLAGS as u64;
    reloc_cache_init(&mut eb.reloc_cache, eb.i915);
    eb.buffer_count = (*args).buffer_count;
    eb.batch_start_offset = (*args).batch_start_offset;
    eb.trampoline = core::ptr::null_mut();
    eb.fences = core::ptr::null_mut();
    eb.num_fences = 0;
    eb_capture_list_clear(&mut eb);
    core::ptr::write_bytes(eb.requests.as_mut_ptr(), 0, eb.requests.len());
    eb.composite_fence = core::ptr::null_mut();
    eb.batch_flags = 0;
    if (*args).flags & I915_EXEC_SECURE != 0 {
        if GRAPHICS_VER(i915) >= 11 {
            return -ENODEV;
        }
        if !HAS_SECURE_BATCHES(i915) {
            return -EPERM;
        }
        if !drm_is_current_master(file) || !capable(CAP_SYS_ADMIN) {
            return -EPERM;
        }
        eb.batch_flags |= I915_DISPATCH_SECURE;
    }
    if (*args).flags & I915_EXEC_IS_PINNED != 0 {
        eb.batch_flags |= I915_DISPATCH_PINNED;
    }
    err = parse_execbuf2_extensions(args, &mut eb);
    if err == 0 {
        err = add_fence_array(&mut eb);
    }
    if err == 0 {
        let in_fences = (*args).flags & (I915_EXEC_FENCE_IN | I915_EXEC_FENCE_SUBMIT);
        if in_fences == (I915_EXEC_FENCE_IN | I915_EXEC_FENCE_SUBMIT) {
            return -EINVAL;
        } else if in_fences != 0 {
            in_fence = sync_file_get_fence(lower_32_bits((*args).rsvd2) as i32);
            if in_fence.is_null() {
                err = -EINVAL;
            }
        }
    }
    if err == 0 && (*args).flags & I915_EXEC_FENCE_OUT != 0 {
        out_fence_fd = get_unused_fd_flags(O_CLOEXEC);
        if out_fence_fd < 0 {
            err = out_fence_fd;
        }
    }
    if err == 0 {
        err = eb_create(&mut eb);
        bucket_created = err == 0;
    }
    if err == 0 {
        GEM_BUG_ON!(eb.lut_size == 0);
        err = eb_select_context(&mut eb);
        context_selected = err == 0;
    }
    if err == 0 {
        err = eb_select_engine(&mut eb);
        engine_selected = err == 0;
    }
    if err == 0 {
        err = eb_lookup_vmas(&mut eb);
        vmas_acquired = err == 0;
        if err != 0 {
            eb_release_vmas(&mut eb, true);
        }
    }
    if err == 0 {
        i915_gem_ww_ctx_init(&mut eb.ww, true);
        ww_initialized = true;
        err = eb_relocate_parse(&mut eb);
        if err != 0 {
            (*args).flags &= !(EXEC_HAS_RELOC as u64);
        }
    }
    if err == 0 {
        ww_acquire_done(&mut eb.ww.ctx);
        err = eb_capture_stage(&mut eb);
    }
    if err == 0 {
        out_fence = eb_requests_create(&mut eb, in_fence, out_fence_fd);
        if IS_ERR(out_fence) {
            err = PTR_ERR(out_fence);
            out_fence = core::ptr::null_mut();
            request_path = !eb.requests[0].is_null();
        } else {
            err = eb_submit(&mut eb);
            request_path = true;
        }
    }
    if request_path {
        eb_requests_get(&mut eb);
        err = eb_requests_add(&mut eb, err);
        if !eb.fences.is_null() {
            signal_fence_array(
                &eb,
                if !eb.composite_fence.is_null() {
                    eb.composite_fence
                } else {
                    &mut (*eb.requests[0]).fence
                },
            );
        }
        if !(*eb.gem_context).syncobj.is_null() {
            drm_syncobj_replace_fence(
                (*eb.gem_context).syncobj,
                if !eb.composite_fence.is_null() {
                    eb.composite_fence
                } else {
                    &mut (*eb.requests[0]).fence
                },
            );
        }
        if !out_fence.is_null() {
            if err == 0 {
                fd_install(out_fence_fd, (*out_fence).file);
                (*args).rsvd2 &= GENMASK_ULL(31, 0);
                (*args).rsvd2 |= (out_fence_fd as u64) << 32;
                out_fence_fd = -1;
            } else {
                fput((*out_fence).file);
            }
        }
        if out_fence.is_null() && !eb.composite_fence.is_null() {
            dma_fence_put(eb.composite_fence);
        }
        eb_requests_put(&mut eb);
    }
    // C labels err_vma/err_engine/err_context/err_destroy are represented by
    // these same ordered cleanup gates. Lookup failure already released VMA
    // refs before entering err_engine.
    if ww_initialized {
        eb_release_vmas(&mut eb, true);
        WARN_ON(err == -EDEADLK);
        i915_gem_ww_ctx_fini(&mut eb.ww);
    } else if vmas_acquired {
        eb_release_vmas(&mut eb, true);
    }
    if !eb.batch_pool.is_null() {
        intel_gt_buffer_pool_put(eb.batch_pool);
    }
    if engine_selected {
        eb_put_engine(&mut eb);
    }
    if context_selected {
        i915_gem_context_put(eb.gem_context);
    }
    if bucket_created {
        eb_destroy(&eb);
    }
    if out_fence_fd != -1 {
        put_unused_fd(out_fence_fd);
    }
    dma_fence_put(in_fence);
    put_fence_array(eb.fences, eb.num_fences as i32);
    err
}

// upstream: i915_gem_execbuffer.c eb_element_size()
#[inline]
pub fn eb_element_size() -> usize {
    core::mem::size_of::<DrmI915GemExecObject2>() + core::mem::size_of::<EbVma>()
}

// upstream: i915_gem_execbuffer.c check_buffer_count()
pub fn check_buffer_count(count: usize) -> bool {
    let sz = eb_element_size();
    !(count < 1 || count > i32::MAX as usize || count > usize::MAX / sz - 1)
}

// upstream: i915_gem_execbuffer.c i915_gem_execbuffer2_ioctl()
pub unsafe fn i915_gem_execbuffer2_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    let i915 = to_i915(dev);
    let args = data as *mut DrmI915GemExecbuffer2;
    let count = (*args).buffer_count as usize;
    if !check_buffer_count(count) {
        drm_dbg(&mut (*i915).drm, "execbuf2 with %zd buffers\n", count);
        return -EINVAL;
    }
    let mut err = i915_gem_check_execbuffer(i915, args);
    if err != 0 {
        return err;
    }
    let exec2_list = kvmalloc_array(count + 2, eb_element_size(), __GFP_NOWARN | GFP_KERNEL)
        as *mut DrmI915GemExecObject2;
    if exec2_list.is_null() {
        drm_dbg(
            &mut (*i915).drm,
            "Failed to allocate exec list for %zd buffers\n",
            count,
        );
        return -ENOMEM;
    }
    if copy_from_user(
        exec2_list.cast(),
        u64_to_user_ptr((*args).buffers_ptr) as *const c_void,
        core::mem::size_of::<DrmI915GemExecObject2>() * count,
    ) != 0
    {
        drm_dbg(&mut (*i915).drm, "copy %zd exec entries failed\n", count);
        kvfree(exec2_list.cast());
        return -EFAULT;
    }
    err = i915_gem_do_execbuffer(dev, file, args, exec2_list);
    if (*args).flags & EXEC_HAS_RELOC as u64 != 0 {
        let user_exec_list = u64_to_user_ptr((*args).buffers_ptr) as *mut DrmI915GemExecObject2;
        if user_write_access_begin(
            user_exec_list.cast(),
            count * core::mem::size_of::<DrmI915GemExecObject2>(),
        ) {
            let mut i = 0;
            while i < (*args).buffer_count as usize {
                if (*exec2_list.add(i)).offset & UPDATE != 0 {
                    (*exec2_list.add(i)).offset =
                        gen8_canonical_addr((*exec2_list.add(i)).offset & PIN_OFFSET_MASK);
                    if unsafe_put_user(
                        (*exec2_list.add(i)).offset,
                        &mut (*user_exec_list.add(i)).offset,
                    ) != 0
                    {
                        break;
                    }
                }
                i += 1;
            }
            user_write_access_end();
        }
    }
    (*args).flags &= !I915_EXEC_UNKNOWN_FLAGS;
    kvfree(exec2_list.cast());
    err
}
