// SPDX-License-Identifier: MIT
// Copyright © 2008-2015 Intel Corporation.
//
// Source-faithful Rust transcription of Linux v7.2.3
// drivers/gpu/drm/i915/gem/i915_gem_shrinker.c. Keep the source order,
// shrinker phases, lock/refcount ordering, flags, and reclaim error semantics.
// GEM/MM/shrinker/notifier/runtime-PM helpers are integration bindings and must
// resolve to the source Linux/i915 behavior; this file does not emulate them.

use core::ffi::{c_char, c_int, c_long, c_ulong, c_void};

use crate::{
    for_each_gt,
    i915_gem_object_header_upstream::{
        i915_gem_object_has_pages, i915_gem_object_is_framebuffer, i915_gem_object_is_shrinkable,
        i915_gem_object_lock, i915_gem_object_put, i915_gem_object_trylock, i915_gem_object_unlock,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, intel_bo_to_drm_bo, intel_bo_to_i915},
    i915_gem_pages_upstream::__i915_gem_object_put_pages,
    i915_gem_ww_upstream::I915GemWwCtx,
    i915_vma_api_upstream::{__i915_vma_unbind, i915_vma_is_active},
    i915_vma_types_upstream::I915Vma,
    intel_context_upstream::IntelWakerefHandle,
    intel_engine_cs_upstream::{ListHead, Mutex},
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::{I915Ggtt, intel_vm_no_concurrent_access_wa},
    intel_runtime_pm_upstream::intel_runtime_pm_get_if_in_use,
    linux::{
        i915_trace::trace_i915_gem_shrink,
        gem_memory::{NotifierBlock, Shrinker},
        i915_private::DrmI915Private,
    },
    linux_config::{CONFIG_LOCKDEP, GFP_KERNEL},
    linux_list::*,
    linux_locks::{spin_lock_irqsave, spin_unlock_irqrestore},
    linux_memory::{
        atomic_add_unless, atomic_dec_and_test, atomic_fetch_inc, kref_get_unless_zero, kref_read,
    },
    linux_mutex::{mutex_lock, mutex_unlock},
    linux_pm::intel_runtime_pm_put,
};

// Values from i915_gem_shrinker.h and i915_gem_object_types.h.
const I915_SHRINK_UNBOUND: u32 = 1 << 0;
const I915_SHRINK_BOUND: u32 = 1 << 1;
const I915_SHRINK_ACTIVE: u32 = 1 << 2;
const I915_SHRINK_VMAPS: u32 = 1 << 3;
const I915_SHRINK_WRITEBACK: u32 = 1 << 4;
const I915_GEM_OBJECT_SHRINK_WRITEBACK: u32 = 1 << 0;
const I915_GEM_OBJECT_SHRINK_NO_GPU_WAIT: u32 = 1 << 1;
const I915_GEM_OBJECT_UNBIND_ACTIVE: c_ulong = 1 << 0;
const I915_GEM_OBJECT_UNBIND_TEST: c_ulong = 1 << 1;
const I915_GEM_OBJECT_UNBIND_VM_TRYLOCK: c_ulong = 1 << 2;
const I915_MADV_DONTNEED: u32 = 1;
const PAGE_SHIFT: u32 = 12;
const SHRINK_STOP: c_ulong = c_ulong::MAX - 1;
const SHRINK_BATCH_DEFAULT: c_ulong = 128;
const SHRINKER_BATCH_INITIAL: c_ulong = 4096;
const NOTIFY_DONE: c_int = 0;

/// Linux v7.2.3 `struct shrink_control` prefix/fields used by the callbacks.
/// This is the source ABI (gfp_mask, nid, nr_to_scan, nr_scanned, memcg), not a
/// reclaim implementation; allocation/call sequencing remains Linux-owned.
#[repr(C)]
struct ShrinkControl {
    _gfp_mask: u32,
    _nid: c_int,
    nr_to_scan: c_ulong,
    nr_scanned: c_ulong,
    _memcg: *mut c_void,
}

/// The subset of Linux `struct shrinker` fields accessed here. On the target
/// x86_64 config, `private_data` follows the two callbacks, `batch`, and the
/// source-owned refcount/completion/RCU fields at byte offset 80.
#[repr(C)]
struct ShrinkerView {
    count_objects: Option<unsafe extern "C" fn(*mut Shrinker, *mut ShrinkControl) -> c_ulong>,
    scan_objects: Option<unsafe extern "C" fn(*mut Shrinker, *mut ShrinkControl) -> c_ulong>,
    batch: c_long,
    _source_fields_before_private: [u8; 56],
    private_data: *mut c_void,
}

const _: [(); 80] = [(); core::mem::offset_of!(ShrinkerView, private_data)];

/// `list_splice_tail()` from Linux list.h, kept local because the shared list
/// owner intentionally exposes only primitive link operations.
unsafe fn list_splice_tail(list: *mut ListHead, head: *mut ListHead) {
    if list_empty(unsafe { &*list }) {
        return;
    }
    let first = unsafe { (*list).next };
    let last = unsafe { (*list).prev };
    let at = unsafe { (*head).prev };
    unsafe {
        (*first).prev = at;
        (*at).next = first;
        (*last).next = head;
        (*head).prev = last;
    }
}

// Linux kernel APIs not represented by a shared Rust owner yet. These are
// declarations only: the symbols/inline bindings remain provided by Linux.
unsafe extern "C" {
    fn i915_gem_object_unbind(obj: *mut DrmI915GemObject, flags: c_ulong) -> c_int;
    fn is_vmalloc_addr(ptr: *const c_void) -> bool;
    // Linux MM/shrinker APIs declared by include/linux/{shrinker,oom,vmalloc}.h.
    fn get_nr_swap_pages() -> c_long;
    fn shrinker_alloc(flags: u32, fmt: *const c_char, ...) -> *mut Shrinker;
    fn shrinker_register(shrinker: *mut Shrinker);
    fn shrinker_free(shrinker: *mut Shrinker);
    fn register_oom_notifier(nb: *mut NotifierBlock) -> c_int;
    fn unregister_oom_notifier(nb: *mut NotifierBlock) -> c_int;
    fn register_vmap_purge_notifier(nb: *mut NotifierBlock) -> c_int;
    fn unregister_vmap_purge_notifier(nb: *mut NotifierBlock) -> c_int;
    fn intel_gt_retire_requests_timeout(
        gt: *mut IntelGt,
        timeout: c_long,
        remaining_timeout: *mut c_long,
    ) -> c_long;
}

/// Source inline `intel_gt_retire_requests()` delegates to the exported
/// timeout API with a zero timeout and no remaining-time output.
unsafe fn intel_gt_retire_requests(gt: *mut IntelGt) {
    let _ = intel_gt_retire_requests_timeout(gt, 0, core::ptr::null_mut());
}

// The shrinker translation requires the source-ordered i915/MM and notifier
// APIs named below. Their C-layout records and operations belong to the shared
// LinuxKPI/i915 integration layer, not to this source translation.

// upstream: i915_gem_shrinker.c swap_available()
unsafe fn swap_available() -> bool {
    get_nr_swap_pages() > 0
}

// upstream: i915_gem_shrinker.c can_release_pages()
unsafe fn can_release_pages(obj: *mut DrmI915GemObject) -> bool {
    // Consider only shrinkable objects.
    if !i915_gem_object_is_shrinkable(obj) {
        return false;
    }

    // We can only return physical pages to the system if we can either
    // discard the contents (because the user has marked them as being
    // purgeable) or if we can move their contents out to swap.
    swap_available() || (*obj).mm.madv() == I915_MADV_DONTNEED
}

// upstream: i915_gem_shrinker.c drop_pages()
unsafe fn drop_pages(obj: *mut DrmI915GemObject, shrink: c_ulong, trylock_vm: bool) -> bool {
    let mut flags: c_ulong = 0;
    if shrink & I915_SHRINK_ACTIVE as c_ulong != 0 {
        flags |= I915_GEM_OBJECT_UNBIND_ACTIVE;
    }
    if shrink & I915_SHRINK_BOUND as c_ulong == 0 {
        flags |= I915_GEM_OBJECT_UNBIND_TEST;
    }
    if trylock_vm {
        flags |= I915_GEM_OBJECT_UNBIND_VM_TRYLOCK;
    }

    if i915_gem_object_unbind(obj, flags) == 0 {
        return true;
    }

    false
}

// upstream: i915_gem_shrinker.c try_to_writeback()
unsafe fn try_to_writeback(obj: *mut DrmI915GemObject, flags: u32) -> c_int {
    if let Some(shrink) = (*(*obj).ops).shrink {
        let mut shrink_flags = 0;

        if flags & I915_SHRINK_ACTIVE == 0 {
            shrink_flags |= I915_GEM_OBJECT_SHRINK_NO_GPU_WAIT;
        }

        if flags & I915_SHRINK_WRITEBACK != 0 {
            shrink_flags |= I915_GEM_OBJECT_SHRINK_WRITEBACK;
        }

        return shrink(obj, shrink_flags);
    }

    0
}

/// i915_gem_shrink - Shrink buffer object caches
/// @ww: i915 gem ww acquire ctx, or NULL
/// @i915: i915 device
/// @target: amount of memory to make available, in pages
/// @nr_scanned: optional output for number of pages scanned (incremental)
/// @shrink: control flags for selecting cache types
///
/// This function is the main interface to the shrinker. It will try to release
/// up to @target pages of main memory backing storage from buffer objects.
/// Selection of the specific caches can be done with @flags. This is e.g. useful
/// when purgeable objects should be removed from caches preferentially.
///
/// Note that it's not guaranteed that released amount is actually available as
/// free system memory - the pages might still be in-used to due to other reasons
/// (like cpu mmaps) or the mm core has reused them before we could grab them.
/// Therefore code that needs to explicitly shrink buffer objects caches (e.g. to
/// avoid deadlocks in memory reclaim) must fall back to i915_gem_shrink_all().
///
/// Also note that any kind of pinning (both per-vma address space pins and
/// backing storage pins at the buffer object level) result in the shrinker code
/// having to skip the object.
///
/// Returns:
/// The number of pages of backing storage actually released.
// upstream: i915_gem_shrinker.c i915_gem_shrink()
pub unsafe fn i915_gem_shrink(
    ww: *mut I915GemWwCtx,
    i915: *mut DrmI915Private,
    target: c_ulong,
    nr_scanned: *mut c_ulong,
    mut shrink: u32,
) -> c_ulong {
    let phases = [
        ShrinkPhase {
            list: core::ptr::addr_of_mut!((*i915).mm.purge_list),
            bit: !0u32,
        },
        ShrinkPhase {
            list: core::ptr::addr_of_mut!((*i915).mm.shrink_list),
            bit: I915_SHRINK_BOUND | I915_SHRINK_UNBOUND,
        },
        ShrinkPhase {
            list: core::ptr::null_mut(),
            bit: 0,
        },
    ];
    let mut wakeref: IntelWakerefHandle = core::ptr::null_mut();
    let mut count: c_ulong = 0;
    let mut scanned: c_ulong = 0;
    let mut err: c_int = 0;
    let mut i: c_int = 0;
    let mut gt: *mut IntelGt;

    // CHV + VTD workaround use stop_machine(); need to trylock vm->mutex.
    let trylock_vm = ww.is_null() && intel_vm_no_concurrent_access_wa(i915);

    trace_i915_gem_shrink(i915, target, shrink);

    // Unbinding of objects will require HW access; Let us not wake the
    // device just to recover a little memory. If absolutely necessary,
    // we will force the wake during oom-notifier.
    if shrink & I915_SHRINK_BOUND != 0 {
        wakeref = unsafe {
            intel_runtime_pm_get_if_in_use(
                core::ptr::addr_of_mut!((*i915).runtime_pm).cast(),
            )
        };
        if wakeref.is_null() {
            shrink &= !I915_SHRINK_BOUND;
        }
    }

    // When shrinking the active list, we should also consider active
    // contexts. Active contexts are pinned until they are retired, and
    // so can not be simply unbound to retire and unpin their pages. To
    // shrink the contexts, we must wait until the gpu is idle and
    // completed its switch to the kernel context. In short, we do
    // not have a good mechanism for idling a specific context, but
    // what we can do is give them a kick so that we do not keep idle
    // contexts around longer than is necessary.
    if shrink & I915_SHRINK_ACTIVE != 0 {
        for_each_gt!(gt, i915, i, {
            // Retire requests to unpin all idle contexts.
            intel_gt_retire_requests(gt);
        });
    }

    // As we may completely rewrite the (un)bound list whilst unbinding
    // (due to retiring requests) we have to strictly process only
    // one element of the list at the time, and recheck the list
    // on every iteration.
    //
    // In particular, we must hold a reference whilst removing the
    // object as we may end up waiting for and/or retiring the objects.
    // This might release the final reference (held by the active list)
    // and result in the object being freed from under us. This is
    // similar to the precautions the eviction code must take whilst
    // removing objects.
    //
    // Also note that although these lists do not hold a reference to
    // the object we can safely grab one here: The final object
    // unreferencing and the bound_list are both protected by the
    // i915->mm.obj_lock and so we won't ever be able to observe an
    // object on the bound_list with a reference count equals 0.
    let mut phase_index = 0;
    while !phases[phase_index].list.is_null() {
        let phase = &phases[phase_index];
        let mut still_in_list = ListHead {
            next: core::ptr::null_mut(),
            prev: core::ptr::null_mut(),
        };
        let mut flags: c_ulong = 0;

        if shrink & phase.bit == 0 {
            phase_index += 1;
            continue;
        }

        INIT_LIST_HEAD!(&mut still_in_list);

        // We serialize our access to unreferenced objects through
        // the use of the obj_lock. While the objects are not
        // yet freed (due to RCU then a workqueue) we still want
        // to be able to shrink their pages, so they remain on the
        // unbound/bound list until actually freed.
        spin_lock_irqsave(&mut (*i915).mm.obj_lock, &mut flags);
        while count < target {
            let obj = list_first_entry_or_null!(phase.list, DrmI915GemObject, mm.link);
            if obj.is_null() {
                break;
            }
            list_move_tail(core::ptr::addr_of_mut!((*obj).mm.link), &mut still_in_list);

            if shrink & I915_SHRINK_VMAPS != 0 && !is_vmalloc_addr((*obj).mm.mapping) {
                continue;
            }

            if shrink & I915_SHRINK_ACTIVE == 0 && i915_gem_object_is_framebuffer(obj) {
                continue;
            }

            if !can_release_pages(obj) {
                continue;
            }

            if !kref_get_unless_zero(&mut (*intel_bo_to_drm_bo(obj)).refcount) {
                continue;
            }

            spin_unlock_irqrestore(&mut (*i915).mm.obj_lock, flags);

            // May arrive from get_pages on another BO.
            let object_locked;
            if ww.is_null() {
                object_locked = i915_gem_object_trylock(obj, core::ptr::null_mut());
            } else {
                err = i915_gem_object_lock(obj, ww);
                object_locked = err == 0;
            }
            if !object_locked {
                // C's `goto skip` drops the reference, reacquires obj_lock,
                // and only then tests the object-lock error.
                i915_gem_object_put(obj);
                spin_lock_irqsave(&mut (*i915).mm.obj_lock, &mut flags);
                if err != 0 {
                    break;
                }
                continue;
            }

            if drop_pages(obj, shrink as c_ulong, trylock_vm)
                && __i915_gem_object_put_pages(obj) == 0
                && try_to_writeback(obj, shrink) == 0
            {
                count = count.wrapping_add((*intel_bo_to_drm_bo(obj)).size >> PAGE_SHIFT);
            }

            if ww.is_null() {
                i915_gem_object_unlock(obj);
            }

            scanned = scanned.wrapping_add((*intel_bo_to_drm_bo(obj)).size >> PAGE_SHIFT);
            i915_gem_object_put(obj);

            spin_lock_irqsave(&mut (*i915).mm.obj_lock, &mut flags);
            if err != 0 {
                break;
            }
        }
        list_splice_tail(&mut still_in_list, phase.list);
        spin_unlock_irqrestore(&mut (*i915).mm.obj_lock, flags);
        if err != 0 {
            break;
        }
        phase_index += 1;
    }

    if shrink & I915_SHRINK_BOUND != 0 {
        intel_runtime_pm_put(core::ptr::addr_of_mut!((*i915).runtime_pm), wakeref);
    }

    if err != 0 {
        // The C function returns this signed error through its unsigned result.
        return err as c_ulong;
    }

    if !nr_scanned.is_null() {
        *nr_scanned = (*nr_scanned).wrapping_add(scanned);
    }
    count
}

#[repr(C)]
struct ShrinkPhase {
    list: *mut ListHead,
    bit: u32,
}

/// i915_gem_shrink_all - Shrink buffer object caches completely
/// @i915: i915 device
///
/// This is a simple wrapper around i915_gem_shrink() to aggressively shrink all
/// caches completely. It also first waits for and retires all outstanding
/// requests to also be able to release backing storage for active objects.
///
/// This should only be used in code to intentionally quiescent the gpu or as a
/// last-ditch effort when memory seems to have run out.
///
/// Returns:
/// The number of pages of backing storage actually released.
// upstream: i915_gem_shrinker.c i915_gem_shrink_all()
pub unsafe fn i915_gem_shrink_all(i915: *mut DrmI915Private) -> c_ulong {
    let mut wakeref: IntelWakerefHandle;
    let mut freed: c_ulong = 0;

    with_intel_runtime_pm!(core::ptr::addr_of_mut!((*i915).runtime_pm), wakeref, {
        freed = i915_gem_shrink(
            core::ptr::null_mut(),
            i915,
            c_ulong::MAX,
            core::ptr::null_mut(),
            I915_SHRINK_BOUND | I915_SHRINK_UNBOUND,
        );
    });

    freed
}

// upstream: i915_gem_shrinker.c i915_gem_shrinker_count()
unsafe extern "C" fn i915_gem_shrinker_count(
    shrinker: *mut Shrinker,
    sc: *mut ShrinkControl,
) -> c_ulong {
    let view = shrinker.cast::<ShrinkerView>();
    let i915 = (*view).private_data.cast::<DrmI915Private>();
    let mut num_objects: c_ulong;
    let mut count: c_ulong;
    let _ = sc;

    count = READ_ONCE!((*i915).mm.shrink_memory) >> PAGE_SHIFT;
    num_objects = READ_ONCE!((*i915).mm.shrink_count) as c_ulong;

    // Update our preferred vmscan batch size for the next pass.
    // Our rough guess for an effective batch size is roughly 2
    // available GEM objects worth of pages. That is we don't want
    // the shrinker to fire, until it is worth the cost of freeing an
    // entire GEM object.
    if num_objects != 0 {
        let avg = count.wrapping_mul(2) / num_objects;

        let view = (*i915).mm.shrinker.cast::<ShrinkerView>();
        (*view).batch = core::cmp::max(
            ((*view).batch.wrapping_add(avg as c_long)) >> 1,
            SHRINK_BATCH_DEFAULT as c_long,
        );
    }

    count
}

// upstream: i915_gem_shrinker.c i915_gem_shrinker_scan()
unsafe extern "C" fn i915_gem_shrinker_scan(
    shrinker: *mut Shrinker,
    sc: *mut ShrinkControl,
) -> c_ulong {
    let i915 = (*shrinker.cast::<ShrinkerView>())
        .private_data
        .cast::<DrmI915Private>();
    let mut freed: c_ulong;

    (*sc).nr_scanned = 0;

    freed = i915_gem_shrink(
        core::ptr::null_mut(),
        i915,
        (*sc).nr_to_scan,
        core::ptr::addr_of_mut!((*sc).nr_scanned),
        I915_SHRINK_BOUND | I915_SHRINK_UNBOUND,
    );
    if (*sc).nr_scanned < (*sc).nr_to_scan && current_is_kswapd() {
        let mut wakeref: IntelWakerefHandle;

        with_intel_runtime_pm!(core::ptr::addr_of_mut!((*i915).runtime_pm), wakeref, {
            freed = freed.wrapping_add(i915_gem_shrink(
                core::ptr::null_mut(),
                i915,
                (*sc).nr_to_scan - (*sc).nr_scanned,
                core::ptr::addr_of_mut!((*sc).nr_scanned),
                I915_SHRINK_ACTIVE
                    | I915_SHRINK_BOUND
                    | I915_SHRINK_UNBOUND
                    | I915_SHRINK_WRITEBACK,
            ));
        });
    }

    if (*sc).nr_scanned != 0 {
        freed
    } else {
        SHRINK_STOP
    }
}

// upstream: i915_gem_shrinker.c i915_gem_shrinker_oom()
unsafe extern "C" fn i915_gem_shrinker_oom(
    nb: *mut NotifierBlock,
    event: c_ulong,
    ptr: *mut c_void,
) -> c_int {
    let i915 = container_of!(nb, DrmI915Private, mm.oom_notifier);
    let mut obj: *mut DrmI915GemObject = core::ptr::null_mut();
    let mut unevictable: c_ulong;
    let mut available: c_ulong;
    let mut freed_pages: c_ulong;
    let mut wakeref: IntelWakerefHandle;
    let mut flags: c_ulong = 0;
    let _ = event;

    freed_pages = 0;
    with_intel_runtime_pm!(core::ptr::addr_of_mut!((*i915).runtime_pm), wakeref, {
        freed_pages = freed_pages.wrapping_add(i915_gem_shrink(
            core::ptr::null_mut(),
            i915,
            c_ulong::MAX,
            core::ptr::null_mut(),
            I915_SHRINK_BOUND | I915_SHRINK_UNBOUND | I915_SHRINK_WRITEBACK,
        ));
    });

    // Because we may be allocating inside our own driver, we cannot
    // assert that there are no objects with pinned pages that are not
    // being pointed to by hardware.
    available = 0;
    unevictable = 0;
    spin_lock_irqsave(&mut (*i915).mm.obj_lock, &mut flags);
    list_for_each_entry!(obj, &(*i915).mm.shrink_list, mm.link, {
        if !can_release_pages(obj) {
            unevictable = unevictable.wrapping_add((*intel_bo_to_drm_bo(obj)).size >> PAGE_SHIFT);
        } else {
            available = available.wrapping_add((*intel_bo_to_drm_bo(obj)).size >> PAGE_SHIFT);
        }
    });
    spin_unlock_irqrestore(&mut (*i915).mm.obj_lock, flags);

    if freed_pages != 0 || available != 0 {
        pr_info!(
            "Purging GPU memory, %lu pages freed, %lu pages still pinned, %lu pages left \
             available.\n",
            freed_pages,
            unevictable,
            available
        );
    }

    *(ptr.cast::<c_ulong>()) = (*ptr.cast::<c_ulong>()).wrapping_add(freed_pages);
    NOTIFY_DONE
}

// upstream: i915_gem_shrinker.c i915_gem_shrinker_vmap()
unsafe extern "C" fn i915_gem_shrinker_vmap(
    nb: *mut NotifierBlock,
    event: c_ulong,
    ptr: *mut c_void,
) -> c_int {
    let i915 = container_of!(nb, DrmI915Private, mm.vmap_notifier);
    let mut vma: *mut I915Vma = core::ptr::null_mut();
    let mut next: *mut I915Vma;
    let mut freed_pages: c_ulong = 0;
    let mut wakeref: IntelWakerefHandle;
    let mut gt: *mut IntelGt;
    let mut i: c_int;
    let _ = event;

    with_intel_runtime_pm!(core::ptr::addr_of_mut!((*i915).runtime_pm), wakeref, {
        freed_pages = freed_pages.wrapping_add(i915_gem_shrink(
            core::ptr::null_mut(),
            i915,
            c_ulong::MAX,
            core::ptr::null_mut(),
            I915_SHRINK_BOUND | I915_SHRINK_UNBOUND | I915_SHRINK_VMAPS,
        ));
    });

    // We also want to clear any cached iomaps as they wrap vmap.
    for_each_gt!(gt, i915, i, {
        let ggtt = (*gt).ggtt.cast::<I915Ggtt>();
        mutex_lock(&mut (*ggtt).vm.mutex);
        list_for_each_entry_safe!(vma, next, &(*ggtt).vm.bound_list, vm_link, {
            let count = (*vma).size >> PAGE_SHIFT;
            let obj = (*vma).obj;

            if (*vma).iomap.is_null() || i915_vma_is_active(vma) {
                continue;
            }

            if !i915_gem_object_trylock(obj, core::ptr::null_mut()) {
                continue;
            }

            if __i915_vma_unbind(vma) == 0 {
                freed_pages = freed_pages.wrapping_add(count);
            }

            i915_gem_object_unlock(obj);
        });
        mutex_unlock(&mut (*ggtt).vm.mutex);
    });

    *(ptr.cast::<c_ulong>()) = (*ptr.cast::<c_ulong>()).wrapping_add(freed_pages);
    NOTIFY_DONE
}

// upstream: i915_gem_shrinker.c i915_gem_driver_register__shrinker()
pub unsafe fn i915_gem_driver_register__shrinker(i915: *mut DrmI915Private) {
    (*i915).mm.shrinker = shrinker_alloc(0, c"drm-i915_gem".as_ptr());
    if (*i915).mm.shrinker.is_null() {
        drm_WARN_ON!(&mut (*i915).drm, true);
    } else {
        let view = (*i915).mm.shrinker.cast::<ShrinkerView>();
        (*view).scan_objects = Some(i915_gem_shrinker_scan);
        (*view).count_objects = Some(i915_gem_shrinker_count);
        (*view).batch = SHRINKER_BATCH_INITIAL as c_long;
        (*view).private_data = i915.cast::<c_void>();

        shrinker_register((*i915).mm.shrinker);
    }

    (*i915).mm.oom_notifier.notifier_call = Some(i915_gem_shrinker_oom);
    drm_WARN_ON!(
        &mut (*i915).drm,
        register_oom_notifier(&mut (*i915).mm.oom_notifier) != 0
    );

    (*i915).mm.vmap_notifier.notifier_call = Some(i915_gem_shrinker_vmap);
    drm_WARN_ON!(
        &mut (*i915).drm,
        register_vmap_purge_notifier(&mut (*i915).mm.vmap_notifier) != 0
    );
}

// upstream: i915_gem_shrinker.c i915_gem_driver_unregister__shrinker()
pub unsafe fn i915_gem_driver_unregister__shrinker(i915: *mut DrmI915Private) {
    drm_WARN_ON!(
        &mut (*i915).drm,
        unregister_vmap_purge_notifier(&mut (*i915).mm.vmap_notifier) != 0
    );
    drm_WARN_ON!(
        &mut (*i915).drm,
        unregister_oom_notifier(&mut (*i915).mm.oom_notifier) != 0
    );
    shrinker_free((*i915).mm.shrinker);
}

// upstream: i915_gem_shrinker.c i915_gem_shrinker_taints_mutex()
pub unsafe fn i915_gem_shrinker_taints_mutex(i915: *mut DrmI915Private, mutex: *mut Mutex) {
    let _ = i915;
    if !IS_ENABLED!(CONFIG_LOCKDEP) {
        return;
    }
    // The active Linux configuration disables CONFIG_LOCKDEP, so source
    // `fs_reclaim_*` and mutex lockdep annotations compile away. Keep the
    // runtime integration boundary explicit rather than fabricating a map.
    let _ = (mutex, GFP_KERNEL);
}

/// i915_gem_object_make_unshrinkable - Hide the object from the shrinker. By
/// default all object types that support shrinking(see IS_SHRINKABLE), will also
/// make the object visible to the shrinker after allocating the system memory
/// pages.
/// @obj: The GEM object.
///
/// This is typically used for special kernel internal objects that can't be
/// easily processed by the shrinker, like if they are perma-pinned.
// upstream: i915_gem_shrinker.c i915_gem_object_make_unshrinkable()
pub unsafe fn i915_gem_object_make_unshrinkable(obj: *mut DrmI915GemObject) {
    let i915 = intel_bo_to_i915(obj);
    let mut flags: c_ulong = 0;

    // We can only be called while the pages are pinned or when
    // the pages are released. If pinned, we should only be called
    // from a single caller under controlled conditions; and on release
    // only one caller may release us. Neither the two may cross.
    if atomic_add_unless(&mut (*obj).mm.shrink_pin, 1, 0) {
        return;
    }

    spin_lock_irqsave(&mut (*i915).mm.obj_lock, &mut flags);
    if atomic_fetch_inc(&mut (*obj).mm.shrink_pin) == 0 && !list_empty(&(*obj).mm.link) {
        list_del_init(core::ptr::addr_of_mut!((*obj).mm.link));
        (*i915).mm.shrink_count = (*i915).mm.shrink_count.wrapping_sub(1);
        (*i915).mm.shrink_memory = (*i915)
            .mm
            .shrink_memory
            .wrapping_sub((*intel_bo_to_drm_bo(obj)).size);
    }
    spin_unlock_irqrestore(&mut (*i915).mm.obj_lock, flags);
}

// upstream: i915_gem_shrinker.c ___i915_gem_object_make_shrinkable()
unsafe fn ___i915_gem_object_make_shrinkable(obj: *mut DrmI915GemObject, head: *mut ListHead) {
    let i915 = intel_bo_to_i915(obj);
    let mut flags: c_ulong = 0;

    if !i915_gem_object_is_shrinkable(obj) {
        return;
    }

    if atomic_add_unless(&mut (*obj).mm.shrink_pin, -1, 1) {
        return;
    }

    spin_lock_irqsave(&mut (*i915).mm.obj_lock, &mut flags);
    GEM_BUG_ON!(kref_read(&(*intel_bo_to_drm_bo(obj)).refcount) == 0);
    if atomic_dec_and_test(&mut (*obj).mm.shrink_pin) {
        GEM_BUG_ON!(!list_empty(&(*obj).mm.link));

        list_add_tail(core::ptr::addr_of_mut!((*obj).mm.link), head);
        (*i915).mm.shrink_count = (*i915).mm.shrink_count.wrapping_add(1);
        (*i915).mm.shrink_memory = (*i915)
            .mm
            .shrink_memory
            .wrapping_add((*intel_bo_to_drm_bo(obj)).size);
    }
    spin_unlock_irqrestore(&mut (*i915).mm.obj_lock, flags);
}

/// __i915_gem_object_make_shrinkable - Move the object to the tail of the
/// shrinkable list. Objects on this list might be swapped out. Used with
/// WILLNEED objects.
/// @obj: The GEM object.
///
/// DO NOT USE. This is intended to be called on very special objects that don't
/// yet have mm.pages, but are guaranteed to have potentially reclaimable pages
/// underneath.
// upstream: i915_gem_shrinker.c __i915_gem_object_make_shrinkable()
pub unsafe fn __i915_gem_object_make_shrinkable(obj: *mut DrmI915GemObject) {
    ___i915_gem_object_make_shrinkable(
        obj,
        core::ptr::addr_of_mut!((*intel_bo_to_i915(obj)).mm.shrink_list),
    );
}

/// __i915_gem_object_make_purgeable - Move the object to the tail of the
/// purgeable list. Objects on this list might be swapped out. Used with
/// DONTNEED objects.
/// @obj: The GEM object.
///
/// DO NOT USE. This is intended to be called on very special objects that don't
/// yet have mm.pages, but are guaranteed to have potentially reclaimable pages
/// underneath.
// upstream: i915_gem_shrinker.c __i915_gem_object_make_purgeable()
pub unsafe fn __i915_gem_object_make_purgeable(obj: *mut DrmI915GemObject) {
    ___i915_gem_object_make_shrinkable(
        obj,
        core::ptr::addr_of_mut!((*intel_bo_to_i915(obj)).mm.purge_list),
    );
}

/// i915_gem_object_make_shrinkable - Move the object to the tail of the
/// shrinkable list. Objects on this list might be swapped out. Used with
/// WILLNEED objects.
/// @obj: The GEM object.
///
/// MUST only be called on objects which have backing pages.
///
/// MUST be balanced with previous call to i915_gem_object_make_unshrinkable().
// upstream: i915_gem_shrinker.c i915_gem_object_make_shrinkable()
pub unsafe fn i915_gem_object_make_shrinkable(obj: *mut DrmI915GemObject) {
    GEM_BUG_ON!(!i915_gem_object_has_pages(obj));
    __i915_gem_object_make_shrinkable(obj);
}

/// i915_gem_object_make_purgeable - Move the object to the tail of the purgeable
/// list. Used with DONTNEED objects. Unlike with shrinkable objects, the
/// shrinker will attempt to discard the backing pages, instead of trying to
/// swap them out.
/// @obj: The GEM object.
///
/// MUST only be called on objects which have backing pages.
///
/// MUST be balanced with previous call to i915_gem_object_make_unshrinkable().
// upstream: i915_gem_shrinker.c i915_gem_object_make_purgeable()
pub unsafe fn i915_gem_object_make_purgeable(obj: *mut DrmI915GemObject) {
    GEM_BUG_ON!(!i915_gem_object_has_pages(obj));
    __i915_gem_object_make_purgeable(obj);
}
