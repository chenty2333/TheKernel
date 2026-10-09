// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gem/i915_gem_domain.c. GEM object layout and Linux
// helper bindings are integration points; do not replace them with no-op
// compatibility shims. Keep source order and cache-domain/locking edges.

use crate::i915_gem_clflush_upstream::i915_gem_clflush_object;

use core::ffi::c_void;

use crate::{
    i915_gem_object_api_upstream::{
        i915_gem_object_has_pages, i915_gem_object_lock, i915_gem_object_lock_interruptible,
        i915_gem_object_pin_pages, i915_gem_object_put, i915_gem_object_unlock,
        i915_gem_object_unpin_pages,
    },
    i915_gem_core_upstream::{
        i915_gem_object_frontbuffer_flush, i915_gem_object_frontbuffer_invalidate,
        i915_gem_object_ggtt_pin_ww, i915_gem_object_unbind, i915_gem_object_wait,
    },
    i915_gem_object_header_upstream::{
        __start_cpu_write, assert_object_held, i915_gem_object_is_proxy,
        i915_gem_object_is_userptr, i915_gem_object_lookup, i915_gem_object_lookup_rcu,
    },
    i915_gem_object_types_upstream::DrmI915GemObject,
    i915_gem_lmem_upstream::i915_gem_object_is_lmem,
    i915_gem_object_upstream::{
        i915_gem_object_has_struct_page, i915_gem_object_set_cache_coherency,
    },
    i915_gem_userptr_upstream::i915_gem_object_userptr_validate,
    i915_vma_api_upstream::*,
    intel_context_upstream::{I915GemWwCtx, I915GttView, I915Vma},
    linux_config::*,
    linux_i915_private::DrmI915Private,
    linux_locks::*,
    linux_macros::*,
    linux_memory::*,
};

// i915_gem_domain.h, i915_gem.h, i915_gem_gtt.h, i915_request.h,
// i915_gem_object.h and i915_gem_clflush.h constants used by this file.
const I915_GEM_DOMAIN_CPU: u32 = 0x0000_0001;
const I915_GEM_DOMAIN_RENDER: u32 = 0x0000_0002;
const I915_GEM_DOMAIN_SAMPLER: u32 = 0x0000_0004;
const I915_GEM_DOMAIN_COMMAND: u32 = 0x0000_0008;
const I915_GEM_DOMAIN_INSTRUCTION: u32 = 0x0000_0010;
const I915_GEM_DOMAIN_VERTEX: u32 = 0x0000_0020;
const I915_GEM_DOMAIN_GTT: u32 = 0x0000_0040;
const I915_GEM_DOMAIN_WC: u32 = 0x0000_0080;
const I915_GEM_GPU_DOMAINS: u32 = I915_GEM_DOMAIN_RENDER
    | I915_GEM_DOMAIN_SAMPLER
    | I915_GEM_DOMAIN_COMMAND
    | I915_GEM_DOMAIN_INSTRUCTION
    | I915_GEM_DOMAIN_VERTEX;
const I915_BO_CACHE_COHERENT_FOR_READ: u32 = 1 << 0;
const I915_BO_CACHE_COHERENT_FOR_WRITE: u32 = 1 << 1;
const I915_CLFLUSH_FORCE: u32 = 1 << 0;
const I915_CLFLUSH_SYNC: u32 = 1 << 1;
const I915_WAIT_INTERRUPTIBLE: u32 = 1 << 0;
const I915_WAIT_PRIORITY: u32 = 1 << 1;
const I915_WAIT_ALL: u32 = 1 << 2;
const I915_GEM_OBJECT_UNBIND_ACTIVE: u32 = 1 << 0;
const I915_GEM_OBJECT_UNBIND_BARRIER: u32 = 1 << 1;
const I915_GTT_PAGE_SIZE: u32 = 4096;
const PIN_OFFSET_GUARD: u32 = 1 << 8;
const PIN_MAPPABLE: u32 = 1 << 3;
const PIN_NONBLOCK: u32 = 1 << 2;
const I915_VMA_GLOBAL_BIND: i32 = 1 << 10;
const CLFLUSH_BEFORE: u32 = 1 << 0;
const CLFLUSH_AFTER: u32 = 1 << 1;
const I915_CACHE_NONE: u32 = 0;
const I915_CACHE_L3_LLC: u32 = 2;
const I915_CACHE_WT: u32 = 3;
const I915_CACHING_NONE: u32 = 0;
const I915_CACHING_CACHED: u32 = 1;
const I915_CACHING_DISPLAY: u32 = 2;
const ORIGIN_CPU: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
enum I915CacheLevel {
    None  = I915_CACHE_NONE as isize,
    Llc   = I915_CACHE_LLC as isize,
    L3Llc = I915_CACHE_L3_LLC as isize,
    Wt    = I915_CACHE_WT as isize,
}

#[repr(C)]
struct DrmI915GemCaching {
    handle: u32,
    caching: u32,
}

#[repr(C)]
struct DrmI915GemSetDomain {
    handle: u32,
    read_domains: u32,
    write_domain: u32,
}

#[repr(C)]
struct DrmDevice {
    _opaque: [u8; 0],
}

#[repr(C)]
struct DrmFile {
    _opaque: [u8; 0],
}

// upstream: i915_gem_domain.c gpu_write_needs_clflush()
unsafe fn gpu_write_needs_clflush(obj: *mut DrmI915GemObject) -> bool {
    let i915 = to_i915((&(*obj).base.base).dev);

    if IS_DGFX(i915) {
        return false;
    }

    // For objects created by userspace through GEM_CREATE with pat_index
    // set by set_pat extension, i915_gem_object_has_cache_level() will
    // always return true, because the coherency of such object is managed
    // by userspace. Othereise the call here would fall back to checking
    // whether the object is un-cached or write-through.
    !(i915_gem_object_has_cache_level(obj, I915_CACHE_NONE)
        || i915_gem_object_has_cache_level(obj, I915_CACHE_WT))
}

// upstream: i915_gem_domain.c i915_gem_cpu_write_needs_clflush()
pub unsafe fn i915_gem_cpu_write_needs_clflush(obj: *mut DrmI915GemObject) -> bool {
    let i915 = to_i915((&(*obj).base.base).dev);

    if unsafe { i915_gem_object_cache_dirty(obj) } {
        return false;
    }

    if IS_DGFX(i915) {
        return false;
    }

    if (unsafe { i915_gem_object_cache_coherent(obj) } & I915_BO_CACHE_COHERENT_FOR_WRITE) == 0 {
        return true;
    }

    // Currently in use by HW (display engine)? Keep flushed.
    i915_gem_object_is_framebuffer(obj)
}

// upstream: i915_gem_domain.c flush_write_domain()
unsafe fn flush_write_domain(obj: *mut DrmI915GemObject, flush_domains: u32) {
    assert_object_held(obj);

    if ((*obj).write_domain as u32 & flush_domains) == 0 {
        return;
    }

    match (*obj).write_domain as u32 {
        I915_GEM_DOMAIN_GTT => {
            spin_lock(&mut (*obj).vma.lock);
            crate::for_each_ggtt_vma!(vma, obj, {
                i915_vma_flush_writes(vma);
            });
            spin_unlock(&mut (*obj).vma.lock);

            i915_gem_object_frontbuffer_flush(obj, ORIGIN_CPU);
        }

        I915_GEM_DOMAIN_WC => {
            wmb();
        }

        I915_GEM_DOMAIN_CPU => {
            i915_gem_clflush_object(obj, I915_CLFLUSH_SYNC);
        }

        I915_GEM_DOMAIN_RENDER => {
            if gpu_write_needs_clflush(obj) {
                i915_gem_object_set_cache_dirty(obj, true);
            }
        }

        _ => {}
    }

    (*obj).write_domain = 0;
}

// upstream: i915_gem_domain.c __i915_gem_object_flush_for_display()
unsafe fn __i915_gem_object_flush_for_display(obj: *mut DrmI915GemObject) {
    // We manually flush the CPU domain so that we can override and
    // force the flush for the display, and perform it asyncrhonously.
    flush_write_domain(obj, !I915_GEM_DOMAIN_CPU);
    if unsafe { i915_gem_object_cache_dirty(obj) } {
        i915_gem_clflush_object(obj, I915_CLFLUSH_FORCE);
    }
    (*obj).write_domain = 0;
}

// upstream: i915_gem_domain.c i915_gem_object_flush_if_display()
pub unsafe fn i915_gem_object_flush_if_display(obj: *mut DrmI915GemObject) {
    if !i915_gem_object_is_framebuffer(obj) {
        return;
    }

    i915_gem_object_lock(obj, core::ptr::null_mut());
    __i915_gem_object_flush_for_display(obj);
    i915_gem_object_unlock(obj);
}

// upstream: i915_gem_domain.c i915_gem_object_flush_if_display_locked()
pub unsafe fn i915_gem_object_flush_if_display_locked(obj: *mut DrmI915GemObject) {
    if i915_gem_object_is_framebuffer(obj) {
        __i915_gem_object_flush_for_display(obj);
    }
}

/// i915_gem_object_set_to_wc_domain - Moves a single object to the WC read, and
///                                    possibly write domain.
/// @obj: object to act on
/// @write: ask for write access or read only
///
/// This function returns when the move is complete, including waiting on
/// flushes to occur.
// upstream: i915_gem_domain.c i915_gem_object_set_to_wc_domain()
pub unsafe fn i915_gem_object_set_to_wc_domain(obj: *mut DrmI915GemObject, write: bool) -> i32 {
    assert_object_held(obj);

    let mut ret = i915_gem_object_wait(
        obj,
        I915_WAIT_INTERRUPTIBLE | if write { I915_WAIT_ALL } else { 0 },
        MAX_SCHEDULE_TIMEOUT as _,
    );
    if ret != 0 {
        return ret;
    }

    if (*obj).write_domain as u32 == I915_GEM_DOMAIN_WC {
        return 0;
    }

    // Flush and acquire obj->pages so that we are coherent through
    // direct access in memory with previous cached writes through
    // shmemfs and that our cache domain tracking remains valid.
    // For example, if the obj->filp was moved to swap without us
    // being notified and releasing the pages, we would mistakenly
    // continue to assume that the obj remained out of the CPU cached
    // domain.
    ret = i915_gem_object_pin_pages(obj);
    if ret != 0 {
        return ret;
    }

    flush_write_domain(obj, !I915_GEM_DOMAIN_WC);

    // Serialise direct access to this object with the barriers for
    // coherent writes from the GPU, by effectively invalidating the
    // WC domain upon first access.
    if ((*obj).read_domains as u32 & I915_GEM_DOMAIN_WC) == 0 {
        mb();
    }

    // It should now be out of any other write domains, and we can update
    // the domain values for our changes.
    GEM_BUG_ON!((*obj).write_domain as u32 & !I915_GEM_DOMAIN_WC != 0);
    (*obj).read_domains = ((*obj).read_domains as u32 | I915_GEM_DOMAIN_WC) as u16;
    if write {
        (*obj).read_domains = I915_GEM_DOMAIN_WC as u16;
        (*obj).write_domain = I915_GEM_DOMAIN_WC as u16;
        (*obj).mm.set_dirty(true);
    }

    i915_gem_object_unpin_pages(obj);
    0
}

/// i915_gem_object_set_to_gtt_domain - Moves a single object to the GTT read,
///                                     and possibly write domain.
/// @obj: object to act on
/// @write: ask for write access or read only
///
/// This function returns when the move is complete, including waiting on
/// flushes to occur.
// upstream: i915_gem_domain.c i915_gem_object_set_to_gtt_domain()
pub unsafe fn i915_gem_object_set_to_gtt_domain(obj: *mut DrmI915GemObject, write: bool) -> i32 {
    assert_object_held(obj);

    let mut ret = i915_gem_object_wait(
        obj,
        I915_WAIT_INTERRUPTIBLE | if write { I915_WAIT_ALL } else { 0 },
        MAX_SCHEDULE_TIMEOUT as _,
    );
    if ret != 0 {
        return ret;
    }

    if (*obj).write_domain as u32 == I915_GEM_DOMAIN_GTT {
        return 0;
    }

    // Flush and acquire obj->pages so that we are coherent through
    // direct access in memory with previous cached writes through
    // shmemfs and that our cache domain tracking remains valid.
    // For example, if the obj->filp was moved to swap without us
    // being notified and releasing the pages, we would mistakenly
    // continue to assume that the obj remained out of the CPU cached
    // domain.
    ret = i915_gem_object_pin_pages(obj);
    if ret != 0 {
        return ret;
    }

    flush_write_domain(obj, !I915_GEM_DOMAIN_GTT);

    // Serialise direct access to this object with the barriers for
    // coherent writes from the GPU, by effectively invalidating the
    // GTT domain upon first access.
    if ((*obj).read_domains as u32 & I915_GEM_DOMAIN_GTT) == 0 {
        mb();
    }

    // It should now be out of any other write domains, and we can update
    // the domain values for our changes.
    GEM_BUG_ON!((*obj).write_domain as u32 & !I915_GEM_DOMAIN_GTT != 0);
    (*obj).read_domains = ((*obj).read_domains as u32 | I915_GEM_DOMAIN_GTT) as u16;
    if write {
        (*obj).read_domains = I915_GEM_DOMAIN_GTT as u16;
        (*obj).write_domain = I915_GEM_DOMAIN_GTT as u16;
        (*obj).mm.set_dirty(true);

        spin_lock(&mut (*obj).vma.lock);
        crate::for_each_ggtt_vma!(vma, obj, {
            if i915_vma_is_bound(vma, I915_VMA_GLOBAL_BIND as u32) {
                i915_vma_set_ggtt_write(vma);
            }
        });
        spin_unlock(&mut (*obj).vma.lock);
    }

    i915_gem_object_unpin_pages(obj);
    0
}

/// i915_gem_object_set_cache_level - Changes the cache-level of an object across all VMA.
/// @obj: object to act on
/// @cache_level: new cache level to set for the object
///
/// After this function returns, the object will be in the new cache-level
/// across all GTT and the contents of the backing storage will be coherent,
/// with respect to the new cache-level. In order to keep the backing storage
/// coherent for all users, we only allow a single cache level to be set
/// globally on the object and prevent it from being changed whilst the
/// hardware is reading from the object. That is if the object is currently
/// on the scanout it will be set to uncached (or equivalent display
/// cache coherency) and all non-MOCS GPU access will also be uncached so
/// that all direct access to the scanout remains coherent.
// upstream: i915_gem_domain.c i915_gem_object_set_cache_level()
pub unsafe fn i915_gem_object_set_cache_level(
    obj: *mut DrmI915GemObject,
    cache_level: I915CacheLevel,
) -> i32 {
    // For objects created by userspace through GEM_CREATE with pat_index
    // set by set_pat extension, simply return 0 here without touching
    // the cache setting, because such objects should have an immutable
    // cache setting by design and always managed by userspace.
    if i915_gem_object_has_cache_level(obj, cache_level as u32) {
        return 0;
    }

    let ret = i915_gem_object_wait(
        obj,
        I915_WAIT_INTERRUPTIBLE | I915_WAIT_ALL,
        MAX_SCHEDULE_TIMEOUT as i64,
    );
    if ret != 0 {
        return ret;
    }

    // Always invalidate stale cachelines
    i915_gem_object_set_cache_coherency(obj, cache_level as u32);
    i915_gem_object_set_cache_dirty(obj, true);

    // The cache-level will be applied when each vma is rebound.
    i915_gem_object_unbind(
        obj,
        (I915_GEM_OBJECT_UNBIND_ACTIVE | I915_GEM_OBJECT_UNBIND_BARRIER) as u64,
    )
}

// upstream: i915_gem_domain.c i915_gem_get_caching_ioctl()
pub unsafe fn i915_gem_get_caching_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    let i915 = to_i915(dev.cast());
    let args = data.cast::<DrmI915GemCaching>();
    let mut obj: *mut DrmI915GemObject;
    let mut err = 0;

    if IS_DGFX(i915) {
        return -ENODEV;
    }

    rcu_read_lock();
    obj = i915_gem_object_lookup_rcu(file.cast(), (*args).handle);
    if obj.is_null() {
        err = -ENOENT;
        rcu_read_unlock();
        return err;
    }

    // This ioctl should be disabled for the objects with pat_index
    // set by user space.
    if i915_gem_object_pat_set_by_user(obj) {
        err = -EOPNOTSUPP;
        rcu_read_unlock();
        return err;
    }

    if i915_gem_object_has_cache_level(obj, I915_CACHE_LLC)
        || i915_gem_object_has_cache_level(obj, I915_CACHE_L3_LLC)
    {
        (*args).caching = I915_CACHING_CACHED;
    } else if i915_gem_object_has_cache_level(obj, I915_CACHE_WT) {
        (*args).caching = I915_CACHING_DISPLAY;
    } else {
        (*args).caching = I915_CACHING_NONE;
    }

    rcu_read_unlock();
    err
}

// upstream: i915_gem_domain.c i915_gem_set_caching_ioctl()
pub unsafe fn i915_gem_set_caching_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    let i915 = to_i915(dev.cast());
    let args = data.cast::<DrmI915GemCaching>();
    let mut obj: *mut DrmI915GemObject;
    let level: I915CacheLevel;
    let mut ret = 0;

    if IS_DGFX(i915) {
        return -ENODEV;
    }

    if graphics_ver_full(i915) >= IP_VER(12, 70) {
        return -EOPNOTSUPP;
    }

    level = match (*args).caching {
        I915_CACHING_NONE => I915CacheLevel::None,
        I915_CACHING_CACHED => {
            // Due to a HW issue on BXT A stepping, GPU stores via a
            // snooped mapping may leave stale data in a corresponding CPU
            // cacheline, whereas normally such cachelines would get
            // invalidated.
            if !HAS_LLC(i915) && !HAS_SNOOP(i915) {
                return -ENODEV;
            }

            I915CacheLevel::Llc
        }
        I915_CACHING_DISPLAY => {
            if HAS_WT(i915) {
                I915CacheLevel::Wt
            } else {
                I915CacheLevel::None
            }
        }
        _ => return -EINVAL,
    };

    obj = i915_gem_object_lookup(file.cast(), (*args).handle);
    if obj.is_null() {
        return -ENOENT;
    }

    // This ioctl should be disabled for the objects with pat_index
    // set by user space.
    if i915_gem_object_pat_set_by_user(obj) {
        ret = -EOPNOTSUPP;
    } else if i915_gem_object_is_proxy(obj) {
        // The caching mode of proxy object is handled by its generator, and
        // not allowed to be changed by userspace.
        // Silently allow cached for userptr; the vulkan driver
        // sets all objects to cached
        if !i915_gem_object_is_userptr(obj) || (*args).caching != I915_CACHING_CACHED {
            ret = -ENXIO;
        }
    } else {
        ret = i915_gem_object_lock_interruptible(obj, core::ptr::null_mut());
        if ret == 0 {
            ret = i915_gem_object_set_cache_level(obj, level);
            i915_gem_object_unlock(obj);
        }
    }

    i915_gem_object_put(obj);
    ret
}

// Prepare buffer for display plane (scanout, cursors, etc). Can be called from
// an uninterruptible phase (modesetting) and allows any flushes to be pipelined
// (for pageflips). We only flush the caches while preparing the buffer for
// display, the callers are responsible for frontbuffer flush.
// upstream: i915_gem_domain.c i915_gem_object_pin_to_display_plane()
pub unsafe fn i915_gem_object_pin_to_display_plane(
    obj: *mut DrmI915GemObject,
    ww: *mut I915GemWwCtx,
    alignment: u32,
    guard: u32,
    view: *const I915GttView,
    mut flags: u32,
) -> *mut I915Vma {
    let i915 = to_i915((&(*obj).base.base).dev);
    let mut vma: *mut I915Vma;
    let ret: i32;

    // Frame buffer must be in LMEM
    if HAS_LMEM(i915) && !unsafe { i915_gem_object_is_lmem(obj) } {
        return ERR_PTR(-EINVAL);
    }

    // The display engine is not coherent with the LLC cache on gen6.  As
    // a result, we make sure that the pinning that is about to occur is
    // done with uncached PTEs. This is lowest common denominator for all
    // chipsets.
    //
    // However for gen6+, we could do better by using the GFDT bit instead
    // of uncaching, which would allow us to flush all the LLC-cached data
    // with that bit in the PTE to main memory with just one PIPE_CONTROL.
    ret = i915_gem_object_set_cache_level(
        obj,
        if HAS_WT(i915) {
            I915CacheLevel::Wt
        } else {
            I915CacheLevel::None
        },
    );
    if ret != 0 {
        return ERR_PTR(ret);
    }

    // VT-d may overfetch before/after the vma, so pad with scratch
    if guard != 0 {
        flags |= PIN_OFFSET_GUARD | (guard * I915_GTT_PAGE_SIZE);
    }

    // As the user may map the buffer once pinned in the display plane
    // (e.g. libkms for the bootup splash), we have to ensure that we
    // always use map_and_fenceable for all scanout buffers. However,
    // it may simply be too big to fit into mappable, in which case
    // put it anyway and hope that userspace can cope (but always first
    // try to preserve the existing ABI).
    vma = ERR_PTR(-ENOSPC);
    if flags & PIN_MAPPABLE as u32 == 0
        && (view.is_null() || (*view).r#type == I915_GTT_VIEW_NORMAL as u32)
    {
        vma = i915_gem_object_ggtt_pin_ww(
            obj,
            ww,
            view,
            0,
            alignment as u64,
            (flags | PIN_MAPPABLE as u32 | PIN_NONBLOCK as u32) as u64,
        );
    }
    if IS_ERR(vma) && vma != ERR_PTR(-EDEADLK) {
        vma = i915_gem_object_ggtt_pin_ww(obj, ww, view, 0, alignment as u64, flags as u64);
    }
    if IS_ERR(vma) {
        return vma;
    }

    (*vma).display_alignment = core::cmp::max((*vma).display_alignment, alignment);
    i915_vma_mark_scanout(vma);

    i915_gem_object_flush_if_display_locked(obj);

    vma
}

/// i915_gem_object_set_to_cpu_domain - Moves a single object to the CPU read,
///                                     and possibly write domain.
/// @obj: object to act on
/// @write: requesting write or read-only access
///
/// This function returns when the move is complete, including waiting on
/// flushes to occur.
// upstream: i915_gem_domain.c i915_gem_object_set_to_cpu_domain()
pub unsafe fn i915_gem_object_set_to_cpu_domain(obj: *mut DrmI915GemObject, write: bool) -> i32 {
    assert_object_held(obj);

    let ret = i915_gem_object_wait(
        obj,
        I915_WAIT_INTERRUPTIBLE | if write { I915_WAIT_ALL } else { 0 },
        MAX_SCHEDULE_TIMEOUT as i64,
    );
    if ret != 0 {
        return ret;
    }

    flush_write_domain(obj, !I915_GEM_DOMAIN_CPU);

    // Flush the CPU cache if it's still invalid.
    if (*obj).read_domains as u32 & I915_GEM_DOMAIN_CPU == 0 {
        i915_gem_clflush_object(obj, I915_CLFLUSH_SYNC);
        (*obj).read_domains = ((*obj).read_domains as u32 | I915_GEM_DOMAIN_CPU) as u16;
    }

    // It should now be out of any other write domains, and we can update
    // the domain values for our changes.
    GEM_BUG_ON!((*obj).write_domain as u32 & !I915_GEM_DOMAIN_CPU != 0);

    // If we're writing through the CPU, then the GPU read domains will
    // need to be invalidated at next use.
    if write {
        __start_cpu_write(obj);
    }

    0
}

/// i915_gem_set_domain_ioctl - Called when user space prepares to use an
///                             object with the CPU, either
/// through the mmap ioctl's mapping or a GTT mapping.
/// @dev: drm device
/// @data: ioctl data blob
/// @file: drm file
// upstream: i915_gem_domain.c i915_gem_set_domain_ioctl()
pub unsafe fn i915_gem_set_domain_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    let args = data.cast::<DrmI915GemSetDomain>();
    let mut obj: *mut DrmI915GemObject;
    let read_domains = (*args).read_domains;
    let write_domain = (*args).write_domain;
    let mut err: i32;

    if IS_DGFX(to_i915(dev.cast())) {
        return -ENODEV;
    }

    // Only handle setting domains to types used by the CPU.
    if (write_domain | read_domains) & I915_GEM_GPU_DOMAINS != 0 {
        return -EINVAL;
    }

    // Having something in the write domain implies it's in the read
    // domain, and only that read domain.  Enforce that in the request.
    if write_domain != 0 && read_domains != write_domain {
        return -EINVAL;
    }

    if read_domains == 0 {
        return 0;
    }

    obj = i915_gem_object_lookup(file.cast(), (*args).handle);
    if obj.is_null() {
        return -ENOENT;
    }

    // Try to flush the object off the GPU without holding the lock.
    // We will repeat the flush holding the lock in the normal manner
    // to catch cases where we are gazumped.
    err = i915_gem_object_wait(
        obj,
        I915_WAIT_INTERRUPTIBLE
            | I915_WAIT_PRIORITY
            | if write_domain != 0 { I915_WAIT_ALL } else { 0 },
        MAX_SCHEDULE_TIMEOUT as i64,
    );
    if err != 0 {
        i915_gem_object_put(obj);
        return err;
    }

    if i915_gem_object_is_userptr(obj) {
        // Try to grab userptr pages, iris uses set_domain to check
        // userptr validity
        err = i915_gem_object_userptr_validate(obj);
        if err == 0 {
            err = i915_gem_object_wait(
                obj,
                I915_WAIT_INTERRUPTIBLE
                    | I915_WAIT_PRIORITY
                    | if write_domain != 0 { I915_WAIT_ALL } else { 0 },
                MAX_SCHEDULE_TIMEOUT as i64,
            );
        }
        i915_gem_object_put(obj);
        return err;
    }

    // Proxy objects do not control access to the backing storage, ergo
    // they cannot be used as a means to manipulate the cache domain
    // tracking for that backing storage. The proxy object is always
    // considered to be outside of any cache domain.
    if i915_gem_object_is_proxy(obj) {
        i915_gem_object_put(obj);
        return -ENXIO;
    }

    err = i915_gem_object_lock_interruptible(obj, core::ptr::null_mut());
    if err != 0 {
        i915_gem_object_put(obj);
        return err;
    }

    // Flush and acquire obj->pages so that we are coherent through
    // direct access in memory with previous cached writes through
    // shmemfs and that our cache domain tracking remains valid.
    // For example, if the obj->filp was moved to swap without us
    // being notified and releasing the pages, we would mistakenly
    // continue to assume that the obj remained out of the CPU cached
    // domain.
    err = i915_gem_object_pin_pages(obj);
    if err == 0 {
        // Already in the desired write domain? Nothing for us to do!
        //
        // We apply a little bit of cunning here to catch a broader set of
        // no-ops. If obj->write_domain is set, we must be in the same
        // obj->read_domains, and only that domain. Therefore, if that
        // obj->write_domain matches the request read_domains, we are
        // already in the same read/write domain and can skip the operation,
        // without having to further check the requested write_domain.
        if READ_ONCE!((*obj).write_domain) as u32 != read_domains {
            if read_domains & I915_GEM_DOMAIN_WC != 0 {
                err = i915_gem_object_set_to_wc_domain(obj, write_domain != 0);
            } else if read_domains & I915_GEM_DOMAIN_GTT != 0 {
                err = i915_gem_object_set_to_gtt_domain(obj, write_domain != 0);
            } else {
                err = i915_gem_object_set_to_cpu_domain(obj, write_domain != 0);
            }
        }
        i915_gem_object_unpin_pages(obj);
    }

    i915_gem_object_unlock(obj);

    if err == 0 && write_domain != 0 {
        i915_gem_object_frontbuffer_invalidate(obj, ORIGIN_CPU);
    }

    i915_gem_object_put(obj);
    err
}

// Pins the specified object's pages and synchronizes the object with
// GPU accesses. Sets needs_clflush to non-zero if the caller should
// flush the object from the CPU cache.
// upstream: i915_gem_domain.c i915_gem_object_prepare_read()
pub unsafe fn i915_gem_object_prepare_read(
    obj: *mut DrmI915GemObject,
    needs_clflush: *mut u32,
) -> i32 {
    *needs_clflush = 0;
    if !i915_gem_object_has_struct_page(obj) {
        return -ENODEV;
    }

    assert_object_held(obj);

    let mut ret = i915_gem_object_wait(obj, I915_WAIT_INTERRUPTIBLE, MAX_SCHEDULE_TIMEOUT as i64);
    if ret != 0 {
        return ret;
    }

    ret = i915_gem_object_pin_pages(obj);
    if ret != 0 {
        return ret;
    }

    if unsafe { i915_gem_object_cache_coherent(obj) } & I915_BO_CACHE_COHERENT_FOR_READ != 0
        || !static_cpu_has(X86_FEATURE_CLFLUSH)
    {
        ret = i915_gem_object_set_to_cpu_domain(obj, false);
        if ret != 0 {
            i915_gem_object_unpin_pages(obj);
            return ret;
        }
        return 0;
    }

    flush_write_domain(obj, !I915_GEM_DOMAIN_CPU);

    // If we're not in the cpu read domain, set ourself into the gtt
    // read domain and manually flush cachelines (if required). This
    // optimizes for the case when the gpu will dirty the data
    // anyway again before the next pread happens.
    if !unsafe { i915_gem_object_cache_dirty(obj) }
        && (*obj).read_domains as u32 & I915_GEM_DOMAIN_CPU == 0
    {
        *needs_clflush = CLFLUSH_BEFORE;
    }

    // return with the pages pinned
    0
}

// upstream: i915_gem_domain.c i915_gem_object_prepare_write()
pub unsafe fn i915_gem_object_prepare_write(
    obj: *mut DrmI915GemObject,
    needs_clflush: *mut u32,
) -> i32 {
    *needs_clflush = 0;
    if !i915_gem_object_has_struct_page(obj) {
        return -ENODEV;
    }

    assert_object_held(obj);

    let mut ret = i915_gem_object_wait(
        obj,
        I915_WAIT_INTERRUPTIBLE | I915_WAIT_ALL,
        MAX_SCHEDULE_TIMEOUT as i64,
    );
    if ret != 0 {
        return ret;
    }

    ret = i915_gem_object_pin_pages(obj);
    if ret != 0 {
        return ret;
    }

    if unsafe { i915_gem_object_cache_coherent(obj) } & I915_BO_CACHE_COHERENT_FOR_WRITE != 0
        || !static_cpu_has(X86_FEATURE_CLFLUSH)
    {
        ret = i915_gem_object_set_to_cpu_domain(obj, true);
        if ret != 0 {
            i915_gem_object_unpin_pages(obj);
            return ret;
        }
    } else {
        flush_write_domain(obj, !I915_GEM_DOMAIN_CPU);

        // If we're not in the cpu write domain, set ourself into the
        // gtt write domain and manually flush cachelines (as required).
        // This optimizes for the case when the gpu will use the data
        // right away and we therefore have to clflush anyway.
        if !unsafe { i915_gem_object_cache_dirty(obj) } {
            *needs_clflush |= CLFLUSH_AFTER;

            // Same trick applies to invalidate partially written
            // cachelines read before writing.
            if (*obj).read_domains as u32 & I915_GEM_DOMAIN_CPU == 0 {
                *needs_clflush |= CLFLUSH_BEFORE;
            }
        }
    }

    i915_gem_object_frontbuffer_invalidate(obj, ORIGIN_CPU);
    (*obj).mm.set_dirty(true);
    // return with the pages pinned
    0
}
