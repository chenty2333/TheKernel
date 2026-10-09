// SPDX-License-Identifier: MIT
// Copyright © 2008-2015 Intel Corporation.
//
// Source-faithful Rust transcription of Linux v7.2.3
// drivers/gpu/drm/i915/i915_gem.c. Keep the source function order, retry and
// unwind edges, range checks, reclaim behavior, and object/VMA lock ordering.
// DRM, MM, user-memory, GGTT, workqueue, PM, and tracing services are bindings
// to their Linux/i915 owners; this file does not substitute implementations.

use core::ffi::{c_char, c_int, c_long, c_ulong, c_void};

use crate::{
    for_each_gt,
    i915_drm_client_upstream::{i915_drm_client_alloc, i915_drm_client_put},
    i915_gem_domain_upstream::{
        i915_gem_object_flush_if_display, i915_gem_object_prepare_read,
        i915_gem_object_prepare_write, i915_gem_object_set_to_gtt_domain,
    },
    i915_gem_object_header_upstream::{
        i915_gem_object_clear_tiling_quirk, i915_gem_object_has_pages,
        i915_gem_object_has_self_managed_shrink_list, i915_gem_object_has_tiling_quirk,
        i915_gem_object_is_readonly, i915_gem_object_is_tiled, i915_gem_object_lock,
        i915_gem_object_lock_interruptible, i915_gem_object_lookup, i915_gem_object_pin_pages,
        i915_gem_object_put, i915_gem_object_set_tiling_quirk, i915_gem_object_unlock,
        i915_gem_object_unpin_pages, i915_gem_object_finish_access,
    },
    i915_gem_object_types_upstream::{DrmI915GemObject, intel_bo_to_drm_bo},
    i915_gem_context_types_upstream::{DrmI915FilePrivate, I915DrmClient},
    i915_gem_shmem_upstream::{DrmI915GemPread, DrmI915GemPwrite},
    i915_gem_object_upstream::{
        i915_gem_get_pat_index, i915_gem_object_has_struct_page, i915_gem_init__objects,
    },
    i915_gem_pages_upstream::{
        __i915_gem_object_get_dma_address, __i915_gem_object_get_page,
        i915_gem_object_truncate,
    },
    i915_gem_shrinker_upstream::{
        i915_gem_driver_register__shrinker, i915_gem_driver_unregister__shrinker,
        i915_gem_object_make_shrinkable, i915_gem_object_make_unshrinkable,
    },
    i915_gem_ww_upstream::{I915GemWwCtx, i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini, i915_gem_ww_ctx_init},
    i915_vma_api_upstream::{
        __i915_vma_unbind, i915_vma_has_userfault, i915_vma_instance, i915_vma_is_active,
        i915_vma_is_bound, i915_vma_is_map_and_fenceable, i915_vma_is_pinned, i915_vma_misplaced,
        i915_vma_pin_ww, i915_vma_revoke_fence, i915_vma_unbind, i915_vma_unbind_async,
        i915_vma_unpin, i915_vma_wait_for_bind,
    },
    i915_vma_types_upstream::I915Vma,
    intel_context_upstream::{DrmMmNode, I915GttView, RcuHead, XArray},
    intel_engine_cs_upstream::{AtomicT, ListHead, Mutex},
    intel_ggtt_fencing_types_upstream::I915FenceReg,
    intel_gt_api_upstream::{
        intel_gt_driver_release, intel_gt_driver_remove, intel_gt_init, intel_gt_is_wedged,
    },
    intel_gt_types_upstream::IntelGt,
    intel_gtt_api_upstream::{
        i915_ggtt_enable_hw, i915_ggtt_resume, i915_init_ggtt,
        setup_private_pat, i915_vm_put, i915_vm_tryget, I915Ggtt,
    },
    intel_gt_api_upstream::intel_gt_flush_ggtt_writes,
    intel_uc_types_upstream::{intel_uc_cleanup_firmwares, intel_uc_fetch_firmwares},
    intel_wopcm_types_upstream::intel_wopcm_init,
    linux::{
        i915::{to_gt, to_i915, GRAPHICS_VER, IS_PLATFORM, INTEL_TIGERLAKE},
        gem::DrmFile,
        gem_memory::drm_mm_node_allocated,
        i915_private::{DrmI915Private, I915_MAX_GT},
        memory::{atomic_read, kfree, kzalloc_obj},
        registers::PIN_GLOBAL,
    },
    linux_config::*,
    linux_list::*,
    linux_locks::{spin_lock, spin_lock_init, spin_lock_irqsave, spin_unlock, spin_unlock_irqrestore},
    linux_memory::{atomic_dec_and_test, atomic_read as linux_atomic_read},
    linux_mutex::{mutex_lock, mutex_trylock, mutex_unlock},
    linux_pm::{intel_runtime_pm_get, intel_runtime_pm_put},
    intel_runtime_pm_upstream::intel_runtime_pm_get_if_in_use,
    linux::i915::i915_ggtt_offset,
    intel_context_api_upstream::mutex_lock_interruptible,
    intel_engine_api_upstream::drm_clflush_virt_range,
    i915_gem_domain_upstream::i915_gem_cpu_write_needs_clflush,
    linux::highmem::{kmap, kunmap},
    linux::rbtree::rb_erase,
};

const I915_GEM_OBJECT_UNBIND_ASYNC: c_ulong = 1 << 4;
const I915_GEM_OBJECT_UNBIND_BARRIER: c_ulong = 1 << 1;
const I915_GEM_OBJECT_UNBIND_TEST: c_ulong = 1 << 2;
const I915_GEM_OBJECT_UNBIND_VM_TRYLOCK: c_ulong = 1 << 3;
const I915_GEM_OBJECT_UNBIND_ACTIVE: c_ulong = 1 << 0;
const I915_GEM_OBJECT_SHRINK_NO_GPU_WAIT: u32 = 1 << 1;
const PAGE_SHIFT: u32 = 12;
const PAGE_SIZE: c_ulong = 1 << PAGE_SHIFT;
const CLFLUSH_BEFORE: u32 = 1 << 0;
const CLFLUSH_AFTER: u32 = 1 << 1;
const I915_VMA_BIND_MASK: u32 = 0x3;
const I915_WAIT_INTERRUPTIBLE: u32 = 1 << 0;
const I915_WAIT_ALL: u32 = 1 << 2;
const I915_CACHE_NONE: u32 = 0;
const I915_GTT_PAGE_SIZE_4K: u32 = 1 << 12;
const I915_MADV_DONTNEED: u32 = 1;
const I915_MADV_WILLNEED: u32 = 0;
const __I915_MADV_PURGED: u32 = 2;
const GEM_QUIRK_PIN_SWIZZLED_PAGES: c_ulong = 1 << 1;
const ORIGIN_CPU: u32 = 0;
const DRM_MM_INSERT_LOW: u32 = 1;
const I915_COLOR_UNEVICTABLE: u64 = !0;

#[repr(C)]
struct GemGetApertureArgs {
    aper_size: u64,
    aper_available_size: u64,
}
#[repr(C)]
struct GemPreadArgs {
    handle: u32,
    _pad: u32,
    offset: u64,
    size: u64,
    data_ptr: u64,
}
#[repr(C)]
struct GemPwriteArgs {
    handle: u32,
    _pad: u32,
    offset: u64,
    size: u64,
    data_ptr: u64,
}
#[repr(C)]
struct GemMadviseArgs {
    handle: u32,
    madv: u32,
    retained: u32,
}
#[repr(C)]
struct GemSwFinishArgs {
    handle: u32,
}

// Source-backed lower Linux/i915 dependencies. These declarations bind actual
// kernel APIs; they are not fallback bodies or emulated substitutes.
unsafe extern "C" {
    fn drm_mm_insert_node_in_range(
        mm: *mut c_void, node: *mut DrmMmNode, size: u64, range_start: u64,
        color: u64, start: u64, end: u64, flags: u32,
    ) -> c_int;
    fn drm_mm_remove_node(node: *mut DrmMmNode);
    fn boot_cpu_data_clflush_size() -> usize;
    fn __i915_gem_object_frontbuffer_flush(obj: *mut DrmI915GemObject, origin: u32);
    fn __i915_gem_object_frontbuffer_invalidate(obj: *mut DrmI915GemObject, origin: u32);
    pub(crate) fn i915_gem_object_wait(obj: *mut DrmI915GemObject, flags: u32, timeout: c_long) -> c_int;
    fn __i915_gem_object_release_mmap_gtt(obj: *mut DrmI915GemObject);
    fn i915_gem_object_runtime_pm_release_mmap_offset(obj: *mut DrmI915GemObject);
    fn __copy_to_user(to: *mut c_void, from: *const c_void, n: usize) -> usize;
    fn __copy_from_user(to: *mut c_void, from: *const c_void, n: usize) -> usize;
    fn copy_to_user(to: *mut c_void, from: *const c_void, n: usize) -> usize;
    fn __copy_to_user_inatomic(to: *mut c_void, from: *const c_void, n: usize) -> usize;
    fn copy_from_user_inatomic_nontemporal(to: *mut c_void, from: *const c_void, n: usize) -> usize;
    fn copy_from_user(to: *mut c_void, from: *const c_void, n: usize) -> usize;
    fn io_mapping_map_atomic_wc(mapping: *mut c_void, offset: c_long) -> *mut c_void;
    fn io_mapping_unmap_atomic(addr: *mut c_void);
    pub(crate) fn io_mapping_map_wc(mapping: *mut c_void, offset: c_long, size: usize) -> *mut c_void;
    pub(crate) fn io_mapping_unmap(addr: *mut c_void);
    pub(crate) fn access_ok(addr: *const c_void, size: u64) -> bool;
    fn trace_i915_gem_object_pread(obj: *mut DrmI915GemObject, offset: u64, size: u64);
    fn trace_i915_gem_object_pwrite(obj: *mut DrmI915GemObject, offset: u64, size: u64);
    fn i915_gem_suspend_late(i915: *mut DrmI915Private);
    fn i915_gem_context_open(i915: *mut DrmI915Private, file: *mut c_void) -> c_int;
    fn i915_gem_context_init(i915: *mut DrmI915Private);
    fn intel_engines_driver_register(i915: *mut DrmI915Private);
    fn i915_probe_error(i915: *mut DrmI915Private, fmt: *const c_char, ...);
    fn intel_clock_gating_init(dev: *mut c_void);
    fn intel_gt_set_wedged(gt: *mut IntelGt);
    fn intel_vgpu_active(i915: *mut DrmI915Private) -> bool;
    fn intel_vgpu_has_huge_gtt(i915: *mut DrmI915Private) -> bool;
    fn flush_work(work: *mut c_void);
    fn flush_workqueue(wq: *mut c_void);
    fn drain_workqueue(wq: *mut c_void);
    static jiffies: c_ulong;
}

// Kernel constants/macros whose selected values are fixed by Linux 7.2.3.
const EFAULT: c_int = 14;
const EINTR: c_int = 4;
const ENOENT: c_int = 2;
const ENODEV: c_int = 19;
const ENOSPC: c_int = 28;
const E2BIG: c_int = 7;
const EINVAL: c_int = 22;
const EBUSY: c_int = 16;
const EAGAIN: c_int = 11;
const EDEADLK: c_int = 35;
const EIO: c_int = 5;
const EOPNOTSUPP: c_int = 95;
const ENOMEM: c_int = 12;
const I915_CACHE_WT: u32 = 3;
const I915_CACHE_L3_LLC: u32 = 2;
const I915_CACHE_LLC: u32 = 1;
const MAX_SCHEDULE_TIMEOUT: c_long = c_long::MAX;
const GRAPHICS_VER_MIN_TGL: u32 = 12;

#[repr(C)]
struct DrmI915FilePrivateView {
    i915: *mut DrmI915Private,
    file_or_rcu: I915FileOrRcuView,
    proto_context_lock: Mutex,
    proto_context_xa: XArray,
    context_xa: XArray,
    vm_xa: XArray,
    bsd_engine: u32,
    ban_score: AtomicT,
    hang_timestamp: c_ulong,
    client: *mut I915DrmClient,
}
#[repr(C)]
union I915FileOrRcuView {
    file: *mut c_void,
    rcu: core::mem::ManuallyDrop<RcuHead>,
}
const _: [(); 96] = [(); core::mem::offset_of!(DrmI915FilePrivateView, bsd_engine)];
const _: [(); 112] = [(); core::mem::offset_of!(DrmI915FilePrivateView, client)];

#[repr(C)]
struct I915GemContextsView {
    lock: crate::intel_engine_cs_upstream::Spinlock,
    list: ListHead,
}
#[repr(C)]
struct DrmI915PrivateTailView {
    contexts: I915GemContextsView,
    mmap_singleton: *mut c_void,
    frontbuffer_lock: crate::intel_engine_cs_upstream::Spinlock,
}
const _: [(); 32] = [(); core::mem::offset_of!(DrmI915PrivateTailView, frontbuffer_lock)];

#[repr(C)]
struct IntelRuntimePmUserfaultListView {
    _header: [u8; 24],
    lmem_userfault_list: ListHead,
}
const _: [(); 24] = [(); core::mem::offset_of!(IntelRuntimePmUserfaultListView, lmem_userfault_list)];

#[inline]
unsafe fn private_tail(i915: *mut DrmI915Private) -> *mut DrmI915PrivateTailView {
    unsafe {
        (i915.cast::<u8>())
            .add(core::mem::offset_of!(DrmI915Private, media_gt) + core::mem::size_of::<*mut IntelGt>())
            .cast()
    }
}

#[inline]
pub(crate) fn u64_to_user_ptr(value: u64) -> *mut c_void {
    value as usize as *mut c_void
}

#[inline]
fn offset_in_page(offset: u64) -> usize {
    offset as usize & (PAGE_SIZE as usize - 1)
}

#[inline]
fn range_overflows_t_u64(start: u64, size: u64, limit: u64) -> bool {
    start.checked_add(size).is_none_or(|end| end > limit)
}

#[inline]
fn overflows_type_u64(value: u64) -> bool {
    c_ulong::try_from(value).is_err()
}

#[inline]
unsafe fn assert_object_held(_obj: *const DrmI915GemObject) {
    // The selected Linux configuration has CONFIG_LOCKDEP=n, so the source
    // dma_resv_assert_held() inline compiles away.
}

#[inline]
unsafe fn gem_base(
    obj: *mut DrmI915GemObject,
) -> *mut crate::intel_context_upstream::DrmGemObjectBaseLayout {
    unsafe { intel_bo_to_drm_bo(obj) }
}

// upstream: i915_gem.c insert_mappable_node()
unsafe fn insert_mappable_node(ggtt: *mut I915Ggtt, node: *mut DrmMmNode, size: u32) -> c_int {
    let vm = core::ptr::addr_of_mut!((*ggtt).vm);
    let err = mutex_lock_interruptible(&mut (*vm).mutex);
    if err != 0 {
        return err;
    }
    core::ptr::write_bytes(node, 0, 1);
    let err = drm_mm_insert_node_in_range(
        core::ptr::addr_of_mut!((*vm).mm).cast(),
        node,
        size as u64,
        0,
        I915_COLOR_UNEVICTABLE,
        0,
        (*ggtt).mappable_end,
        DRM_MM_INSERT_LOW,
    );
    mutex_unlock(&mut (*vm).mutex);
    err
}

// upstream: i915_gem.c remove_mappable_node()
unsafe fn remove_mappable_node(ggtt: *mut I915Ggtt, node: *mut DrmMmNode) {
    mutex_lock(&mut (*ggtt).vm.mutex);
    drm_mm_remove_node(node);
    mutex_unlock(&mut (*ggtt).vm.mutex);
}

// upstream: i915_gem.c i915_gem_get_aperture_ioctl()
pub unsafe fn i915_gem_get_aperture_ioctl(
    dev: *mut c_void,
    data: *mut c_void,
    _file: *mut c_void,
) -> c_int {
    let i915 = to_i915(dev);
    let ggtt = (*to_gt(i915)).ggtt.cast::<I915Ggtt>();
    let args = data.cast::<GemGetApertureArgs>();
    let err = mutex_lock_interruptible(&mut (*ggtt).vm.mutex);
    if err != 0 {
        return -EINTR;
    }
    let mut pinned = (*ggtt).vm.reserved;
    let mut vma: *mut I915Vma;
    list_for_each_entry!(vma, &(*ggtt).vm.bound_list, vm_link, {
        if i915_vma_is_pinned(vma) {
            pinned = pinned.wrapping_add((*vma).node.size);
        }
    });
    mutex_unlock(&mut (*ggtt).vm.mutex);
    (*args).aper_size = (*ggtt).vm.total;
    (*args).aper_available_size = (*args).aper_size.wrapping_sub(pinned);
    0
}

// upstream: i915_gem.c i915_gem_object_unbind()
pub unsafe fn i915_gem_object_unbind(obj: *mut DrmI915GemObject, flags: c_ulong) -> c_int {
    let i915 = to_i915((*gem_base(obj)).dev);
    let rpm = core::ptr::addr_of_mut!((*i915).runtime_pm);
    let vm_trylock = flags & I915_GEM_OBJECT_UNBIND_VM_TRYLOCK != 0;
    let mut still_in_list = ListHead {
        next: core::ptr::null_mut(),
        prev: core::ptr::null_mut(),
    };
    let wakeref;
    let mut vma: *mut I915Vma;
    let mut ret: c_int;
    assert_object_held(obj);
    if list_empty(&(*obj).vma.list) {
        return 0;
    }
    wakeref = intel_runtime_pm_get(rpm);
    unsafe {
        INIT_LIST_HEAD!(&mut still_in_list);
        'try_again: loop {
            ret = 0;
            spin_lock(&mut (*obj).vma.lock);
            while ret == 0 {
                vma = list_first_entry_or_null!(&mut (*obj).vma.list, I915Vma, obj_link);
                if vma.is_null() {
                    break;
                }
                list_move_tail(core::ptr::addr_of_mut!((*vma).obj_link), &mut still_in_list);
                if !i915_vma_is_bound(vma, I915_VMA_BIND_MASK) {
                    continue;
                }
                if flags & I915_GEM_OBJECT_UNBIND_TEST != 0 {
                    ret = -EBUSY;
                    break;
                }
                ret = -EAGAIN;
                if i915_vm_tryget((*vma).vm).is_null() {
                    break;
                }
                spin_unlock(&mut (*obj).vma.lock);
                ret = -EBUSY;
                if flags & I915_GEM_OBJECT_UNBIND_ASYNC != 0 {
                    assert_object_held((*vma).obj);
                    ret = i915_vma_unbind_async(vma, vm_trylock);
                }
                if ret == -EBUSY
                    && (flags & I915_GEM_OBJECT_UNBIND_ACTIVE != 0 || !i915_vma_is_active(vma))
                {
                    if vm_trylock {
                        if mutex_trylock(&mut (*(*vma).vm).mutex) {
                            ret = __i915_vma_unbind(vma);
                            mutex_unlock(&mut (*(*vma).vm).mutex);
                        }
                    } else {
                        ret = i915_vma_unbind(vma);
                    }
                }
                i915_vm_put((*vma).vm);
                spin_lock(&mut (*obj).vma.lock);
            }
            list_splice_init(&mut still_in_list, &mut (*obj).vma.list);
            spin_unlock(&mut (*obj).vma.lock);
            if ret == -EAGAIN && flags & I915_GEM_OBJECT_UNBIND_BARRIER != 0 {
                rcu_barrier();
                continue 'try_again;
            }
            break;
        }
    }
    intel_runtime_pm_put(rpm, wakeref);
    ret
}

// upstream: i915_gem.c shmem_pread()
unsafe fn shmem_pread(
    page: *mut crate::i915_gem_object_types_upstream::Page,
    offset: c_int,
    len: c_int,
    user: *mut c_char,
    needs_clflush: bool,
) -> c_int {
    let vaddr = kmap(page);
    if needs_clflush {
        drm_clflush_virt_range(vaddr.add(offset as usize), len as u64);
    }
    let ret = __copy_to_user(user.cast(), vaddr.add(offset as usize), len as usize);
    kunmap(page);
    if ret != 0 { -EFAULT } else { 0 }
}

// upstream: i915_gem.c i915_gem_shmem_pread()
unsafe fn i915_gem_shmem_pread(obj: *mut DrmI915GemObject, args: *const GemPreadArgs) -> c_int {
    let mut needs_clflush = 0u32;
    let mut user_data = u64_to_user_ptr((*args).data_ptr).cast::<c_char>();
    let mut offset = offset_in_page((*args).offset);
    let mut idx = ((*args).offset >> PAGE_SHIFT) as u64;
    let mut remain = (*args).size;
    let mut ret = i915_gem_object_lock_interruptible(obj, core::ptr::null_mut());
    if ret != 0 {
        return ret;
    }
    ret = i915_gem_object_pin_pages(obj);
    if ret != 0 {
        i915_gem_object_unlock(obj);
        return ret;
    }
    ret = i915_gem_object_prepare_read(obj, &mut needs_clflush);
    if ret != 0 {
        i915_gem_object_unpin_pages(obj);
        i915_gem_object_unlock(obj);
        return ret;
    }
    i915_gem_object_finish_access(obj);
    i915_gem_object_unlock(obj);
    while remain != 0 {
        let page = __i915_gem_object_get_page(obj, idx);
        let length = core::cmp::min(remain, PAGE_SIZE - offset as c_ulong);
        ret = shmem_pread(
            page,
            offset as c_int,
            length as c_int,
            user_data,
            needs_clflush != 0,
        );
        if ret != 0 {
            break;
        }
        remain -= length;
        user_data = user_data.add(length as usize);
        offset = 0;
        idx += 1;
    }
    i915_gem_object_unpin_pages(obj);
    ret
}

// upstream: i915_gem.c gtt_user_read()
unsafe fn gtt_user_read(
    mapping: *mut c_void,
    base: c_long,
    offset: c_int,
    user_data: *mut c_char,
    length: c_int,
) -> bool {
    let mut vaddr = io_mapping_map_atomic_wc(mapping, base);
    let mut unwritten = __copy_to_user_inatomic(
        user_data.cast(),
        vaddr.add(offset as usize),
        length as usize,
    );
    io_mapping_unmap_atomic(vaddr);
    if unwritten != 0 {
        vaddr = io_mapping_map_wc(mapping, base, PAGE_SIZE as usize);
        unwritten = copy_to_user(
            user_data.cast(),
            vaddr.add(offset as usize),
            length as usize,
        );
        io_mapping_unmap(vaddr);
    }
    unwritten != 0
}

// upstream: i915_gem.c i915_gem_gtt_prepare()
unsafe fn i915_gem_gtt_prepare(
    obj: *mut DrmI915GemObject,
    node: *mut DrmMmNode,
    write: bool,
) -> *mut I915Vma {
    let i915 = to_i915((*gem_base(obj)).dev);
    let ggtt = (*to_gt(i915)).ggtt.cast::<I915Ggtt>();
    let mut vma: *mut I915Vma;
    let mut ww = core::mem::MaybeUninit::<I915GemWwCtx>::uninit();
    let mut ret: c_int;
    i915_gem_ww_ctx_init(ww.as_mut_ptr(), true);
    'retry: loop {
      loop {
        vma = ERR_PTR::<I915Vma>(-ENODEV);
        ret = i915_gem_object_lock(obj, ww.as_mut_ptr());
        if ret == 0 {
            ret = i915_gem_object_set_to_gtt_domain(obj, write);
        }
        if ret != 0 {
            break;
        }
        if !i915_gem_object_is_tiled(obj) {
            vma = i915_gem_object_ggtt_pin_ww(
                obj,
                ww.as_mut_ptr(),
                core::ptr::null(),
                0,
                0,
                PIN_MAPPABLE | PIN_NONBLOCK | PIN_NOEVICT,
            );
        }
    if vma == ERR_PTR::<I915Vma>(-EDEADLK) {
            ret = -EDEADLK;
            break;
        }
        if !IS_ERR(vma) {
            (*node).start = i915_ggtt_offset(vma) as u64;
            (*node).flags = 0;
        } else {
            ret = insert_mappable_node(ggtt, node, PAGE_SIZE as u32);
            if ret != 0 {
                break;
            }
            GEM_BUG_ON!(!drm_mm_node_allocated(&*node));
            vma = core::ptr::null_mut();
        }
        ret = i915_gem_object_pin_pages(obj);
        if ret != 0 {
            if drm_mm_node_allocated(&*node) {
                ((*ggtt).vm.clear_range.unwrap())(&mut (*ggtt).vm, (*node).start, (*node).size);
                remove_mappable_node(ggtt, node);
            } else {
                i915_vma_unpin(vma);
            }
        }
        break;
      }
      if ret == -EDEADLK {
          ret = i915_gem_ww_ctx_backoff(ww.as_mut_ptr());
          if ret == 0 {
              continue 'retry;
          }
        }
      break;
    }
    i915_gem_ww_ctx_fini(ww.as_mut_ptr());
    if ret != 0 { ERR_PTR::<I915Vma>(ret) } else { vma }
}

// upstream: i915_gem.c i915_gem_gtt_cleanup()
unsafe fn i915_gem_gtt_cleanup(
    obj: *mut DrmI915GemObject,
    node: *mut DrmMmNode,
    vma: *mut I915Vma,
) {
    let i915 = to_i915((*gem_base(obj)).dev);
    let ggtt = (*to_gt(i915)).ggtt.cast::<I915Ggtt>();
    i915_gem_object_unpin_pages(obj);
    if drm_mm_node_allocated(&*node) {
        ((*ggtt).vm.clear_range.unwrap())(&mut (*ggtt).vm, (*node).start, (*node).size);
        remove_mappable_node(ggtt, node);
    } else {
        i915_vma_unpin(vma);
    }
}

// upstream: i915_gem.c i915_gem_gtt_pread()
unsafe fn i915_gem_gtt_pread(obj: *mut DrmI915GemObject, args: *const GemPreadArgs) -> c_int {
    let i915 = to_i915((*gem_base(obj)).dev);
    let ggtt = (*to_gt(i915)).ggtt.cast::<I915Ggtt>();
    let mut remain: c_ulong = 0;
    let mut offset: c_ulong = 0;
    let mut wakeref;
    let mut node = core::mem::MaybeUninit::<DrmMmNode>::uninit();
    let mut user_data: *mut c_char;
    let mut vma;
    let mut ret = 0;
    if overflows_type_u64((*args).size) || overflows_type_u64((*args).offset) {
        return -EINVAL;
    }
    wakeref = intel_runtime_pm_get(&mut (*i915).runtime_pm);
    vma = i915_gem_gtt_prepare(obj, node.as_mut_ptr(), false);
    if IS_ERR(vma) {
        ret = PTR_ERR(vma);
    } else {
        user_data = u64_to_user_ptr((*args).data_ptr).cast();
        remain = (*args).size as c_ulong;
        offset = (*args).offset as c_ulong;
        while remain > 0 {
            let page_base = (*node.as_ptr()).start as u32;
            let page_offset = (offset as usize) & (PAGE_SIZE as usize - 1);
            let mut page_length = PAGE_SIZE as usize - page_offset;
            page_length = core::cmp::min(remain as usize, page_length);
            let mut map_base = page_base;
            if drm_mm_node_allocated(&*node.as_ptr()) {
                ((*ggtt).vm.insert_page.unwrap())(
                    &mut (*ggtt).vm,
                    __i915_gem_object_get_dma_address(obj, (offset >> PAGE_SHIFT) as u64),
                    (*node.as_ptr()).start,
                    i915_gem_get_pat_index(i915, I915_CACHE_NONE),
                    0,
                );
            } else {
                map_base = map_base.wrapping_add((offset & !(PAGE_SIZE as c_ulong - 1)) as u32);
            }
            if gtt_user_read(
                core::ptr::addr_of_mut!((*ggtt).iomap).cast(),
                map_base as c_long,
                page_offset as c_int,
                user_data,
                page_length as c_int,
            ) {
                ret = -EFAULT;
                break;
            }
            remain -= page_length as c_ulong;
            user_data = user_data.add(page_length);
            offset += page_length as c_ulong;
        }
        i915_gem_gtt_cleanup(obj, node.as_mut_ptr(), vma);
    }
    intel_runtime_pm_put(&mut (*i915).runtime_pm, wakeref);
    ret
}

// upstream: i915_gem.c i915_gem_pread_ioctl()
pub unsafe fn i915_gem_pread_ioctl(
    dev: *mut c_void,
    data: *mut c_void,
    file: *mut c_void,
) -> c_int {
    let i915 = to_i915(dev);
    let args = data.cast::<GemPreadArgs>();
    if GRAPHICS_VER(i915) >= GRAPHICS_VER_MIN_TGL as u8 && !IS_PLATFORM(i915, INTEL_TIGERLAKE) {
        return -EOPNOTSUPP;
    }
    if (*args).size == 0 {
        return 0;
    }
    if !access_ok(u64_to_user_ptr((*args).data_ptr), (*args).size) {
        return -EFAULT;
    }
    let obj = i915_gem_object_lookup(file.cast(), (*args).handle);
    if obj.is_null() {
        return -ENOENT;
    }
    let size = (*gem_base(obj)).size;
    if range_overflows_t_u64((*args).offset, (*args).size, size) {
        i915_gem_object_put(obj);
        return -EINVAL;
    }
    trace_i915_gem_object_pread(obj, (*args).offset, (*args).size);
    let mut ret = -ENODEV;
    if let Some(pread) = (*(*obj).ops).pread {
        ret = pread(obj, args.cast::<DrmI915GemPread>());
    }
    if ret == -ENODEV {
        ret = i915_gem_object_wait(obj, I915_WAIT_INTERRUPTIBLE, MAX_SCHEDULE_TIMEOUT);
        if ret == 0 {
            ret = i915_gem_shmem_pread(obj, args);
            if ret == -EFAULT || ret == -ENODEV {
                ret = i915_gem_gtt_pread(obj, args);
            }
        }
    }
    i915_gem_object_put(obj);
    ret
}

// upstream: i915_gem.c ggtt_write()
unsafe fn ggtt_write(
    mapping: *mut c_void,
    base: c_long,
    offset: c_int,
    user_data: *mut c_char,
    length: c_int,
) -> bool {
    let mut vaddr = io_mapping_map_atomic_wc(mapping, base);
    let mut unwritten = copy_from_user_inatomic_nontemporal(
        vaddr.add(offset as usize),
        user_data.cast(),
        length as usize,
    );
    io_mapping_unmap_atomic(vaddr);
    if unwritten != 0 {
        vaddr = io_mapping_map_wc(mapping, base, PAGE_SIZE as usize);
        unwritten = copy_from_user(
            vaddr.add(offset as usize),
            user_data.cast(),
            length as usize,
        );
        io_mapping_unmap(vaddr);
    }
    unwritten != 0
}

// upstream: i915_gem.c i915_gem_gtt_pwrite_fast()
unsafe fn i915_gem_gtt_pwrite_fast(
    obj: *mut DrmI915GemObject,
    args: *const GemPwriteArgs,
) -> c_int {
    let i915 = to_i915((*gem_base(obj)).dev);
    let ggtt = (*to_gt(i915)).ggtt.cast::<I915Ggtt>();
    let rpm = core::ptr::addr_of_mut!((*i915).runtime_pm).cast();
    let mut remain: c_ulong = 0;
    let mut offset: c_ulong = 0;
    let wakeref;
    let mut node = core::mem::MaybeUninit::<DrmMmNode>::uninit();
    let mut vma;
    let mut user_data: *mut c_char;
    let mut ret = 0;
    if overflows_type_u64((*args).size) || overflows_type_u64((*args).offset) {
        return -EINVAL;
    }
    if i915_gem_object_has_struct_page(obj) {
        wakeref = intel_runtime_pm_get_if_in_use(rpm);
        if wakeref.is_null() {
            return -EFAULT;
        }
    } else {
        wakeref = intel_runtime_pm_get(rpm);
    }
    vma = i915_gem_gtt_prepare(obj, node.as_mut_ptr(), true);
    if IS_ERR(vma) {
        ret = PTR_ERR(vma);
    } else {
        __i915_gem_object_frontbuffer_invalidate(obj, ORIGIN_CPU);
        user_data = u64_to_user_ptr((*args).data_ptr).cast();
        offset = (*args).offset as c_ulong;
        remain = (*args).size as c_ulong;
        while remain != 0 {
            let mut page_base = (*node.as_ptr()).start as u32;
            let page_offset = (offset as usize) & (PAGE_SIZE as usize - 1);
            let page_length = core::cmp::min(remain as usize, PAGE_SIZE as usize - page_offset);
            if drm_mm_node_allocated(&*node.as_ptr()) {
                intel_gt_flush_ggtt_writes((*ggtt).vm.gt);
                ((*ggtt).vm.insert_page.unwrap())(
                    &mut (*ggtt).vm,
                    __i915_gem_object_get_dma_address(obj, (offset >> PAGE_SHIFT) as u64),
                    (*node.as_ptr()).start,
                    i915_gem_get_pat_index(i915, I915_CACHE_NONE),
                    0,
                );
                wmb!();
            } else {
                page_base = page_base.wrapping_add((offset & !(PAGE_SIZE as c_ulong - 1)) as u32);
            }
            if ggtt_write(
            core::ptr::addr_of_mut!((*ggtt).iomap).cast(),
                page_base as c_long,
                page_offset as c_int,
                user_data,
                page_length as c_int,
            ) {
                ret = -EFAULT;
                break;
            }
            remain -= page_length as c_ulong;
            user_data = user_data.add(page_length);
            offset += page_length as c_ulong;
        }
        intel_gt_flush_ggtt_writes((*ggtt).vm.gt);
        __i915_gem_object_frontbuffer_flush(obj, ORIGIN_CPU);
        i915_gem_gtt_cleanup(obj, node.as_mut_ptr(), vma);
    }
    intel_runtime_pm_put(rpm, wakeref);
    ret
}

// upstream: i915_gem.c shmem_pwrite()
unsafe fn shmem_pwrite(
    page: *mut crate::i915_gem_object_types_upstream::Page,
    offset: c_int,
    len: c_int,
    user: *mut c_char,
    before: bool,
    after: bool,
) -> c_int {
    let vaddr = kmap(page);
    if before {
        drm_clflush_virt_range(vaddr.add(offset as usize), len as u64);
    }
    let ret = __copy_from_user(vaddr.add(offset as usize), user.cast(), len as usize);
    if ret == 0 && after {
        drm_clflush_virt_range(vaddr.add(offset as usize), len as u64);
    }
    kunmap(page);
    if ret != 0 { -EFAULT } else { 0 }
}

// upstream: i915_gem.c i915_gem_shmem_pwrite()
unsafe fn i915_gem_shmem_pwrite(obj: *mut DrmI915GemObject, args: *const GemPwriteArgs) -> c_int {
    let mut partial_cacheline_write = 0usize;
    let mut needs_clflush = 0u32;
    let mut user_data = u64_to_user_ptr((*args).data_ptr).cast::<c_char>();
    let mut offset = offset_in_page((*args).offset);
    let mut idx = ((*args).offset >> PAGE_SHIFT) as u64;
    let mut remain = (*args).size;
    let mut ret = i915_gem_object_lock_interruptible(obj, core::ptr::null_mut());
    if ret != 0 {
        return ret;
    }
    ret = i915_gem_object_pin_pages(obj);
    if ret != 0 {
        i915_gem_object_unlock(obj);
        return ret;
    }
    ret = i915_gem_object_prepare_write(obj, &mut needs_clflush);
    if ret != 0 {
        i915_gem_object_unpin_pages(obj);
        i915_gem_object_unlock(obj);
        return ret;
    }
    i915_gem_object_finish_access(obj);
    i915_gem_object_unlock(obj);
    if needs_clflush & CLFLUSH_BEFORE != 0 {
        partial_cacheline_write = boot_cpu_data_clflush_size().wrapping_sub(1);
    }
    while remain != 0 {
        let page = __i915_gem_object_get_page(obj, idx);
        let length = core::cmp::min(remain, PAGE_SIZE - offset as c_ulong);
        ret = shmem_pwrite(
            page,
            offset as c_int,
            length as c_int,
            user_data,
            ((offset | length as usize) & partial_cacheline_write) != 0,
            needs_clflush & CLFLUSH_AFTER != 0,
        );
        if ret != 0 {
            break;
        }
        remain -= length;
        user_data = user_data.add(length as usize);
        offset = 0;
        idx += 1;
    }
    __i915_gem_object_frontbuffer_flush(obj, ORIGIN_CPU);
    i915_gem_object_unpin_pages(obj);
    ret
}

// upstream: i915_gem.c i915_gem_pwrite_ioctl()
pub unsafe fn i915_gem_pwrite_ioctl(
    dev: *mut c_void,
    data: *mut c_void,
    file: *mut c_void,
) -> c_int {
    let i915 = to_i915(dev);
    let args = data.cast::<GemPwriteArgs>();
    if GRAPHICS_VER(i915) >= GRAPHICS_VER_MIN_TGL as u8 && !IS_PLATFORM(i915, INTEL_TIGERLAKE) {
        return -EOPNOTSUPP;
    }
    if (*args).size == 0 {
        return 0;
    }
    if !access_ok(u64_to_user_ptr((*args).data_ptr), (*args).size) {
        return -EFAULT;
    }
    let obj = i915_gem_object_lookup(file.cast(), (*args).handle);
    if obj.is_null() {
        return -ENOENT;
    }
    let size = (*gem_base(obj)).size;
    if range_overflows_t_u64((*args).offset, (*args).size, size) {
        i915_gem_object_put(obj);
        return -EINVAL;
    }
    if i915_gem_object_is_readonly(obj) {
        i915_gem_object_put(obj);
        return -EINVAL;
    }
    trace_i915_gem_object_pwrite(obj, (*args).offset, (*args).size);
    let mut ret = -ENODEV;
    if let Some(pwrite) = (*(*obj).ops).pwrite {
        ret = pwrite(obj, args.cast::<DrmI915GemPwrite>());
    }
    if ret == -ENODEV {
        ret = i915_gem_object_wait(
            obj,
            I915_WAIT_INTERRUPTIBLE | I915_WAIT_ALL,
            MAX_SCHEDULE_TIMEOUT,
        );
        if ret == 0 {
            ret = -EFAULT;
            if !i915_gem_object_has_struct_page(obj) || i915_gem_cpu_write_needs_clflush(obj)
            {
                ret = i915_gem_gtt_pwrite_fast(obj, args);
            }
            if ret == -EFAULT || ret == -ENOSPC {
                if i915_gem_object_has_struct_page(obj) {
                    ret = i915_gem_shmem_pwrite(obj, args);
                }
            }
        }
    }
    i915_gem_object_put(obj);
    ret
}

// upstream: i915_gem.c i915_gem_sw_finish_ioctl()
pub unsafe fn i915_gem_sw_finish_ioctl(
    _dev: *mut c_void,
    data: *mut c_void,
    file: *mut c_void,
) -> c_int {
    let args = data.cast::<GemSwFinishArgs>();
    let obj = i915_gem_object_lookup(file.cast(), (*args).handle);
    if obj.is_null() {
        return -ENOENT;
    }
    i915_gem_object_flush_if_display(obj);
    i915_gem_object_put(obj);
    0
}

// upstream: i915_gem.c i915_gem_runtime_suspend()
pub unsafe fn i915_gem_runtime_suspend(i915: *mut DrmI915Private) {
    let mut obj: *mut DrmI915GemObject;
    let mut on: *mut DrmI915GemObject;
    let gt = to_gt(i915);
    let ggtt = (*gt).ggtt.cast::<I915Ggtt>();

    list_for_each_entry_safe!(obj, on, &(*ggtt).userfault_list, userfault_link, {
        __i915_gem_object_release_mmap_gtt(obj);
    });

    let rpm_list = core::ptr::addr_of_mut!((*i915).runtime_pm)
        .cast::<IntelRuntimePmUserfaultListView>();
    list_for_each_entry_safe!(obj, on, &(*rpm_list).lmem_userfault_list, userfault_link, {
        i915_gem_object_runtime_pm_release_mmap_offset(obj);
    });

    // The fence register is lost during power-down. All non-live fences are
    // marked dirty and are reacquired by users on the next device wake.
    for index in 0..(*ggtt).num_fences as usize {
        let reg = (*ggtt).fence_regs.add(index);
        let vma = (*reg).vma;
        if vma.is_null() {
            continue;
        }
        GEM_BUG_ON!(i915_vma_has_userfault(vma));
        (*reg).dirty = true;
    }
}

// upstream: i915_gem.c discard_ggtt_vma()
unsafe fn discard_ggtt_vma(vma: *mut I915Vma) {
    let obj = (*vma).obj;
    spin_lock(&mut (*obj).vma.lock);
    if !RB_EMPTY_NODE!(&(*vma).obj_node) {
        rb_erase(&mut (*vma).obj_node, &mut (*obj).vma.tree);
        RB_CLEAR_NODE!(&mut (*vma).obj_node);
    }
    spin_unlock(&mut (*obj).vma.lock);
}

// upstream: i915_gem.c i915_gem_object_ggtt_pin_ww()
pub unsafe fn i915_gem_object_ggtt_pin_ww(
    obj: *mut DrmI915GemObject,
    ww: *mut I915GemWwCtx,
    view: *const I915GttView,
    size: u64,
    alignment: u64,
    flags: u64,
) -> *mut I915Vma {
    let i915 = to_i915((*gem_base(obj)).dev);
    let ggtt = (*to_gt(i915)).ggtt.cast::<I915Ggtt>();
    GEM_WARN_ON!(ww.is_null());
    if flags & PIN_MAPPABLE != 0 && (view.is_null() || (*view).r#type == 0) {
        if (*gem_base(obj)).size > (*ggtt).mappable_end {
            return ERR_PTR::<I915Vma>(-E2BIG);
        }
        if flags & PIN_NONBLOCK != 0 && (*gem_base(obj)).size > (*ggtt).mappable_end / 2 {
            return ERR_PTR::<I915Vma>(-ENOSPC);
        }
    }
    'new_vma: loop {
        let vma = i915_vma_instance(obj, &mut (*ggtt).vm, view);
        if IS_ERR(vma) {
            return vma;
        }
        if i915_vma_misplaced(vma, size, alignment, flags) {
            if flags & PIN_NONBLOCK != 0 {
                if i915_vma_is_pinned(vma) || i915_vma_is_active(vma) {
                    return ERR_PTR::<I915Vma>(-ENOSPC);
                }
                if flags & PIN_MAPPABLE != 0
                    && (((*vma).fence_size as u64) > (*ggtt).mappable_end / 2
                        || !i915_vma_is_map_and_fenceable(vma))
                {
                    return ERR_PTR::<I915Vma>(-ENOSPC);
                }
            }
            if i915_vma_is_pinned(vma) || i915_vma_is_active(vma) {
                discard_ggtt_vma(vma);
                continue 'new_vma;
            }
            let ret = i915_vma_unbind(vma);
            if ret != 0 {
                return ERR_PTR::<I915Vma>(ret);
            }
        }
        let ret = i915_vma_pin_ww(vma, ww, size, alignment, flags | PIN_GLOBAL);
        if ret != 0 {
            return ERR_PTR::<I915Vma>(ret);
        }
        if !(*vma).fence.is_null() && !i915_gem_object_is_tiled(obj) {
            mutex_lock(&mut (*ggtt).vm.mutex);
            i915_vma_revoke_fence(vma);
            mutex_unlock(&mut (*ggtt).vm.mutex);
        }
        let ret = i915_vma_wait_for_bind(vma);
        if ret != 0 {
            i915_vma_unpin(vma);
            return ERR_PTR::<I915Vma>(ret);
        }
        return vma;
    }
}

// upstream: i915_gem.c i915_gem_object_ggtt_pin()
pub unsafe fn i915_gem_object_ggtt_pin(
    obj: *mut DrmI915GemObject,
    view: *const I915GttView,
    size: u64,
    alignment: u64,
    flags: u64,
) -> *mut I915Vma {
    let mut ww = core::mem::MaybeUninit::<I915GemWwCtx>::uninit();
    let mut ret = 0;
    let mut vma = core::ptr::null_mut();
    i915_gem_ww_ctx_init(ww.as_mut_ptr(), true);
    loop {
        ret = i915_gem_object_lock(obj, ww.as_mut_ptr());
        if ret == 0 {
            vma = i915_gem_object_ggtt_pin_ww(obj, ww.as_mut_ptr(), view, size, alignment, flags);
            if IS_ERR(vma) {
                ret = PTR_ERR(vma);
            }
        }
        if ret == -EDEADLK {
            ret = i915_gem_ww_ctx_backoff(ww.as_mut_ptr());
            if ret == 0 {
                continue;
            }
        }
        break;
    }
    i915_gem_ww_ctx_fini(ww.as_mut_ptr());
    if ret != 0 { ERR_PTR::<I915Vma>(ret) } else { vma }
}

// upstream: i915_gem.c i915_gem_madvise_ioctl()
pub unsafe fn i915_gem_madvise_ioctl(
    dev: *mut c_void,
    data: *mut c_void,
    file: *mut c_void,
) -> c_int {
    let i915 = to_i915(dev);
    let args = data.cast::<GemMadviseArgs>();
    match (*args).madv {
        I915_MADV_DONTNEED | I915_MADV_WILLNEED => {}
        _ => return -EINVAL,
    }
    let obj = i915_gem_object_lookup(file.cast(), (*args).handle);
    if obj.is_null() {
        return -ENOENT;
    }
    let mut err = i915_gem_object_lock_interruptible(obj, core::ptr::null_mut());
    if err != 0 {
        i915_gem_object_put(obj);
        return err;
    }
    if i915_gem_object_has_pages(obj)
        && i915_gem_object_is_tiled(obj)
        && (*i915).gem_quirks & GEM_QUIRK_PIN_SWIZZLED_PAGES != 0
    {
        if (*obj).mm.madv() == I915_MADV_WILLNEED {
            GEM_BUG_ON!(!i915_gem_object_has_tiling_quirk(obj));
            i915_gem_object_clear_tiling_quirk(obj);
            i915_gem_object_make_shrinkable(obj);
        }
        if (*args).madv == I915_MADV_WILLNEED {
            GEM_BUG_ON!(i915_gem_object_has_tiling_quirk(obj));
            i915_gem_object_make_unshrinkable(obj);
            i915_gem_object_set_tiling_quirk(obj);
        }
    }
    if (*obj).mm.madv() != __I915_MADV_PURGED {
        (*obj).mm.set_madv((*args).madv);
        if let Some(adjust_lru) = (*(*obj).ops).adjust_lru {
            adjust_lru(obj);
        }
    }
    if i915_gem_object_has_pages(obj) || i915_gem_object_has_self_managed_shrink_list(obj) {
        let mut flags = 0;
        spin_lock_irqsave(&mut (*i915).mm.obj_lock, &mut flags);
        if !list_empty(&(*obj).mm.link) {
            let list = if (*obj).mm.madv() != I915_MADV_WILLNEED {
                &mut (*i915).mm.purge_list
            } else {
                &mut (*i915).mm.shrink_list
            };
            list_move_tail(core::ptr::addr_of_mut!((*obj).mm.link), list);
        }
        spin_unlock_irqrestore(&mut (*i915).mm.obj_lock, flags);
    }
    if (*obj).mm.madv() == I915_MADV_DONTNEED && !i915_gem_object_has_pages(obj) {
        i915_gem_object_truncate(obj);
    }
    (*args).retained = ((*obj).mm.madv() != __I915_MADV_PURGED) as u32;
    i915_gem_object_unlock(obj);
    i915_gem_object_put(obj);
    err
}

// upstream: i915_gem.c i915_gem_drain_freed_objects()
pub unsafe fn i915_gem_drain_freed_objects(i915: *mut DrmI915Private) {
    while atomic_read(&(*i915).mm.free_count) != 0 {
        flush_work(core::ptr::addr_of_mut!((*i915).mm.free_work).cast());
        drain_workqueue((*i915).wq);
        rcu_barrier();
    }
}

// upstream: i915_gem.c i915_gem_drain_workqueue()
pub unsafe fn i915_gem_drain_workqueue(i915: *mut DrmI915Private) {
    for _ in 0..3 {
        flush_workqueue((*i915).wq);
        rcu_barrier();
        i915_gem_drain_freed_objects(i915);
    }
    drain_workqueue((*i915).wq);
}

// upstream: i915_gem.c i915_gem_init()
pub unsafe fn i915_gem_init(dev_priv: *mut DrmI915Private) -> c_int {
    let mut gt: *mut IntelGt;
    let mut i = 0usize;
    let mut ret = 0;
    BUILD_BUG_ON!(
        I915_CACHE_NONE != 0
            || I915_CACHE_LLC != 1
            || I915_CACHE_L3_LLC != 2
            || I915_CACHE_WT != 3
            || crate::i915_gem_object_types_upstream::I915_MAX_CACHE_LEVEL != 4
    );
    if intel_vgpu_active(dev_priv) && !intel_vgpu_has_huge_gtt(dev_priv) {
        (*dev_priv).runtime.page_sizes = I915_GTT_PAGE_SIZE_4K;
    }
    for_each_gt!(gt, dev_priv, i, {
        intel_uc_fetch_firmwares(core::ptr::addr_of_mut!((*gt).uc).cast());
        intel_wopcm_init(core::ptr::addr_of_mut!((*gt).wopcm).cast());
        if GRAPHICS_VER(dev_priv) >= 8 {
            setup_private_pat(gt);
        }
    });
    ret = i915_init_ggtt(dev_priv);
    if ret != 0 {
        GEM_BUG_ON!(ret == -EIO);
    }
    if ret == 0 {
        intel_clock_gating_init(core::ptr::addr_of_mut!((*dev_priv).drm).cast());
        for_each_gt!(gt, dev_priv, i, {
            ret = intel_gt_init(gt);
            if ret != 0 {
                break;
            }
        });
        if ret == 0 {
            intel_engines_driver_register(dev_priv);
            return 0;
        }
    }
    i915_gem_drain_workqueue(dev_priv);
    if ret != -EIO {
        for_each_gt!(gt, dev_priv, i, {
            intel_gt_driver_remove(gt);
            intel_gt_driver_release(gt);
            intel_uc_cleanup_firmwares(core::ptr::addr_of_mut!((*gt).uc).cast());
        });
    }
    if ret == -EIO {
        for_each_gt!(gt, dev_priv, i, {
            if !intel_gt_is_wedged(gt) {
                i915_probe_error(
                    dev_priv,
                    c"Failed to initialize GPU, declaring it wedged!\n".as_ptr(),
                );
                intel_gt_set_wedged(gt);
            }
        });
        ret = i915_ggtt_enable_hw(dev_priv);
        i915_ggtt_resume((*to_gt(dev_priv)).ggtt.cast());
        intel_clock_gating_init(core::ptr::addr_of_mut!((*dev_priv).drm).cast());
    }
    i915_gem_drain_freed_objects(dev_priv);
    ret
}

// upstream: i915_gem.c i915_gem_driver_register()
pub unsafe fn i915_gem_driver_register(i915: *mut DrmI915Private) {
    i915_gem_driver_register__shrinker(i915);
}
// upstream: i915_gem.c i915_gem_driver_unregister()
pub unsafe fn i915_gem_driver_unregister(i915: *mut DrmI915Private) {
    i915_gem_driver_unregister__shrinker(i915);
}

// upstream: i915_gem.c i915_gem_driver_remove()
pub unsafe fn i915_gem_driver_remove(dev_priv: *mut DrmI915Private) {
    i915_gem_suspend_late(dev_priv);
    let mut gt: *mut IntelGt;
    let mut i = 0usize;
    for_each_gt!(gt, dev_priv, i, {
        intel_gt_driver_remove(gt);
    });
    (*dev_priv).uabi_engines = RB_ROOT!();
    i915_gem_drain_workqueue(dev_priv);
}

// upstream: i915_gem.c i915_gem_driver_release()
pub unsafe fn i915_gem_driver_release(dev_priv: *mut DrmI915Private) {
    let mut gt: *mut IntelGt;
    let mut i = 0usize;
    for_each_gt!(gt, dev_priv, i, {
        intel_gt_driver_release(gt);
        intel_uc_cleanup_firmwares(core::ptr::addr_of_mut!((*gt).uc).cast());
    });
    i915_gem_drain_workqueue(dev_priv);
    drm_WARN_ON!(
        core::ptr::addr_of_mut!((*dev_priv).drm),
        !list_empty(&(*private_tail(dev_priv)).contexts.list)
    );
}

// upstream: i915_gem.c i915_gem_init__mm()
unsafe fn i915_gem_init__mm(i915: *mut DrmI915Private) {
    spin_lock_init(&mut (*i915).mm.obj_lock);
    init_llist_head(&mut (*i915).mm.free_list);
    INIT_LIST_HEAD!(&mut (*i915).mm.purge_list);
    INIT_LIST_HEAD!(&mut (*i915).mm.shrink_list);
    i915_gem_init__objects(i915);
}

// upstream: i915_gem.c i915_gem_init_early()
pub unsafe fn i915_gem_init_early(dev_priv: *mut DrmI915Private) {
    i915_gem_init__mm(dev_priv);
    i915_gem_context_init(dev_priv);
    spin_lock_init(&mut (*private_tail(dev_priv)).frontbuffer_lock);
}

// upstream: i915_gem.c i915_gem_cleanup_early()
pub unsafe fn i915_gem_cleanup_early(dev_priv: *mut DrmI915Private) {
    i915_gem_drain_workqueue(dev_priv);
    GEM_BUG_ON!(!llist_empty(&(*dev_priv).mm.free_list));
    GEM_BUG_ON!(atomic_read(&(*dev_priv).mm.free_count) != 0);
    drm_WARN_ON!(
        core::ptr::addr_of_mut!((*dev_priv).drm),
        (*dev_priv).mm.shrink_count != 0
    );
}

// upstream: i915_gem.c i915_gem_open()
pub unsafe fn i915_gem_open(i915: *mut DrmI915Private, file: *mut c_void) -> c_int {
    let file_priv = kzalloc_obj::<DrmI915FilePrivateView>();
    if file_priv.is_null() {
        return -ENOMEM;
    }
    let client = i915_drm_client_alloc();
    if client.is_null() {
        kfree(file_priv.cast::<c_void>());
        return -ENOMEM;
    }
    (*file.cast::<DrmFile>()).driver_priv = file_priv.cast::<c_void>();
    (*file_priv).i915 = i915;
    (*file_priv).file_or_rcu.file = file;
    (*file_priv).client = client;
    (*file_priv).bsd_engine = u32::MAX;
    (*file_priv).hang_timestamp = jiffies;
    let ret = i915_gem_context_open(i915, file);
    if ret != 0 {
        i915_drm_client_put(client);
        kfree(file_priv.cast::<c_void>());
        return ret;
    }
    0
}
