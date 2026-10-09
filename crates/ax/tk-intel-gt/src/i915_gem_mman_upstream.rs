// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation.
//
//! Linux 7.2.3 source-order translation of
//! `drivers/gpu/drm/i915/gem/i915_gem_mman.c`.
//!
//! MISSING BINDINGS (intentionally not replaced with fake records or stubs):
//! Linux `mm`/`vm_area_struct`, `vm_fault`, `file`, `inode`, and
//! `vm_operations_struct`; DRM VMA-offset manager/node helpers and anon-inode
//! file operations; MM remap/page-protection helpers; and the GEM ioctl/VMA
//! services: `vm_mmap`, Linux `current()->mm` task-layout bridge,
//! `mmap_write_lock_killable`, `find_vma`, `vm_get_page_prot`, `remap_io_sg`,
//! `remap_io_mapping`, `drm_vma_node_start/offset_addr/reset/allow_once`,
//! `drm_vma_offset_manager/add/remove/lookup_locked/lock_lookup/unlock_lookup`,
//! `drm_vma_node_unmap/is_allowed`, `i915_gem_to_ttm`, anon-inode/file
//! operations, `drm_dev_get/put`, `fput`, `vma_set_file`, `vm_flags_set/clear`,
//! `pgprot_decrypted/noncached`, and MM/file primitives. `VmAreaStruct`,
//! `VmFault`, `Inode`, `FileOperations`, `MmStruct`, `SgTable::sgl`,
//! `TtmBufferObject`, and `DrmVmaOffsetManager` need source-layout/API owners.
//! `I915GttView` also needs its
//! exact `intel_partial_info` union arm (`partial.offset`/`partial.size`); the
//! owner currently exposes only the tag and opaque bytes. The target config
//! must expose `CONFIG_DRM_I915_USERFAULT_AUTOSUSPEND`; the I915 VMA owner must
//! expose `i915_vma_unpin_fence`. The control flow and error ordering are
//! retained so these functions can be wired when those owners provide their
//! source-derived ABI.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn, non_snake_case)]

use crate::linux::mm_native::{vm_mmap, find_vma, mmap_write_lock_killable, mmap_write_unlock};
use crate::linux::i915_trace::trace_i915_gem_object_fault;
use crate::linux::gem::{drm_vma_offset_lock_lookup,drm_vma_offset_unlock_lookup};

use crate::i915_mm_upstream::{remap_io_mapping,remap_io_sg};

use core::ffi::{c_int, c_long, c_ulong, c_void};

use crate::{
    for_each_ggtt_vma,
    i915_gem_object_types_upstream::{
        DrmI915GemObject, I915MmapOffset, I915MmapType, VmOperationsStruct,
    },
    i915_vma_api_upstream::{
        __i915_vma_unpin, i915_vma_pin_fence, i915_vma_revoke_mmap, i915_vma_unpin_fence,
        i915_vma_set_ggtt_write, i915_vma_set_userfault,
    },
    intel_context_upstream::{DrmGemObjectBaseLayout, I915GttView},
    intel_context_types_upstream::File,
    intel_context_api_upstream::mutex_lock_interruptible,
    i915_gem_context_upstream::{fput, I915UserExtension},
    i915_gem_ww_upstream::{
        I915GemWwCtx, i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini,
        i915_gem_ww_ctx_init,
    },
    intel_engine_cs_upstream::{RbNode, RbRoot},
    intel_gtt_api_upstream::{i915_ggtt_has_aperture, I915AddressSpace, I915Ggtt, resource_size_t},
    intel_gt_types_upstream::IntelGt,
    i915_vma_types_upstream::I915Vma,
    intel_runtime_pm_upstream::{
        assert_rpm_wakelock_held, intel_runtime_pm_get, intel_runtime_pm_put_raw,
    },
    intel_reset_upstream::intel_gt_reset_unlock,
    intel_uncore_types_upstream::IntelRuntimePm,
    linux::{
        gem::{anon_inode_getfile, drm_dev_get, drm_dev_put, drm_vma_node_offset_addr, drm_vma_node_reset, drm_vma_node_start, drm_vma_node_unmap, drm_vma_offset_add, drm_vma_offset_remove, drm_vma_offset_lookup_locked, drm_vma_node_allow_once, drm_vma_node_is_allowed, get_file_active, i915_gem_to_ttm, ttm_device_dev_mapping, DrmDevice, DrmFile, FileOperations},
        mm::{
            VmAreaStruct, VmFault, VM_DONTDUMP, VM_DONTEXPAND, VM_FAULT_NOPAGE,
            VM_FAULT_OOM, VM_FAULT_SIGBUS, VM_IO, VM_MAYWRITE, VM_PFNMAP, VM_WRITE,
            vma_pages, vma_set_file, vm_flags_clear, vm_flags_set, vm_get_page_prot,
        },
        i915::{i915_ggtt_offset, to_gt, to_i915, GRAPHICS_VER_FULL, IP_VER, HAS_LLC, HAS_LMEM, IS_DGFX},
        bits::cmpxchg,
        list::{list_add, list_del},
        memory::{kfree, kmalloc_obj},
        // Anonymous-inode and DRM VMA-offset APIs remain an explicit lower
        // binding dependency; the MM callback records are owned in linux::mm.
        mutex::{mutex_lock, mutex_unlock},
        rcu::{rcu_read_lock, rcu_read_unlock},
        locks::{spin_lock, spin_unlock},
    },
    linux_config::{
        current, CONFIG_DRM_I915_USERFAULT_AUTOSUSPEND, EAGAIN, EBUSY, EDEADLK, EFAULT, EINTR, EINVAL, EIO, ENODEV,
        ENOENT, ENOMEM, ENOSPC, ENXIO, EOPNOTSUPP, PAGE_SIZE, PAGE_SHIFT,
        MAX_SCHEDULE_TIMEOUT, PIN_MAPPABLE, PIN_NONBLOCK, PIN_NOEVICT,
        PIN_NOSEARCH, I915_GTT_VIEW_NORMAL,
    },
    linux_i915_private::{DrmDevicePrefix, DrmI915Private},
    linux_macros::unlikely,
    linux::gem_memory::{pgprot_decrypted, pgprot_noncached, PgProt},
    i915_gem_object_header_upstream::{
        i915_gem_object_get, i915_gem_object_get_rcu, i915_gem_object_has_pinned_pages,
        i915_gem_object_is_readonly, i915_gem_object_is_tiled,
        i915_gem_object_get_tile_row_size, i915_gem_object_lock,
        i915_gem_object_lock_interruptible, i915_gem_object_lookup,
        i915_gem_object_never_mmap, i915_gem_object_pin_pages,
        i915_gem_object_put, i915_gem_object_unlock,
        i915_gem_object_unpin_map, i915_gem_object_unpin_pages,
    },
    i915_gem_object_upstream::{
        i915_gem_object_has_cache_level, i915_gem_object_has_iomem,
        i915_gem_object_has_struct_page,
    },
    i915_gem_pages_upstream::{i915_gem_object_pin_map, __i915_gem_object_flush_map},
    i915_gem_core_upstream::{
        i915_gem_drain_freed_objects, i915_gem_object_ggtt_pin_ww,
    },
    i915_gem_object_header_upstream::DrmFileObjectLookup,
    intel_wakeref_types_upstream::{IntelWakerefAuto, intel_wakeref_auto},
};

// Out-of-line Linux/i915 APIs used here. Their real implementations are
// supplied by the kernel; the missing DRM/MM object-layout bindings are
// deliberately not replaced with local records or behavior.
unsafe extern "C" {
    fn pat_enabled() -> bool;
    fn pgprot_writecombine(prot: PgProt) -> PgProt;
    fn drm_dev_enter(dev: *mut c_void, idx: *mut c_int) -> bool;
    fn drm_dev_exit(idx: c_int);
    fn intel_gt_reset_lock_interruptible(gt: *mut IntelGt, srcu_idx: *mut c_int) -> c_int;
    fn intel_gt_retire_requests_timeout(
        gt: *mut IntelGt,
        timeout: c_long,
        remaining_timeout: *mut c_long,
    ) -> c_long;
    fn i915_gem_evict_vm(
        vm: *mut I915AddressSpace,
        ww: *mut I915GemWwCtx,
        busy_bo: *mut *mut DrmI915GemObject,
    ) -> c_int;
    fn i915_user_extensions(
        extensions: *mut I915UserExtension,
        funcs: *const Option<unsafe extern "C" fn(*mut I915UserExtension, *mut c_void) -> c_int>,
        count: u32,
        data: *mut c_void,
    ) -> c_int;
}

// `drm_dev_is_unplugged()` is the source header's inline enter/exit pair.
unsafe fn drm_dev_is_unplugged(dev: *mut c_void) -> bool {
    let mut idx = 0;
    if unsafe { drm_dev_enter(dev, &mut idx) } {
        unsafe { drm_dev_exit(idx) };
        false
    } else {
        true
    }
}

const MIN_CHUNK_PAGES: u32 = (1 << 20) >> PAGE_SHIFT;
const I915_MMAP_WC: u32 = 1 << 0;
const I915_MMAP_OFFSET_GTT: u64 = 0;
const I915_MMAP_OFFSET_WC: u64 = 1;
const I915_MMAP_OFFSET_WB: u64 = 2;
const I915_MMAP_OFFSET_UC: u64 = 3;
const I915_MMAP_OFFSET_FIXED: u64 = 4;
const I915_CACHE_NONE: u32 = 0;
const I915_MAP_FORCE_WC: u32 = 1 | (1 << 31);
// `sizeof(struct intel_partial_info)`: packed `u64 offset` + `u32 size`.
const I915_GTT_VIEW_PARTIAL: u32 = 12;
const PROT_READ: u32 = 1;
const PROT_WRITE: u32 = 2;
const MAP_SHARED: u32 = 1;
const O_RDWR: c_int = 2;
const ENOBUFS: c_int = 105;
const ERESTARTSYS: c_int = 512;
const EACCES: c_int = 13;
const PAGE_SIZE_U64: u64 = PAGE_SIZE as u64;

/// UAPI `struct drm_i915_gem_mmap` from `include/uapi/drm/i915_drm.h`.
#[repr(C)]
struct DrmI915GemMmap {
    handle: u32,
    pad: u32,
    offset: u64,
    size: u64,
    addr_ptr: u64,
    flags: u64,
}

/// UAPI `struct drm_i915_gem_mmap_offset` from `include/uapi/drm/i915_drm.h`.
#[repr(C)]
struct DrmI915GemMmapOffset {
    handle: u32,
    pad: u32,
    offset: u64,
    flags: u64,
    extensions: u64,
}

// The source's `__vma_matches()` is intentionally kept local: the missing MM
// owner must expose these exact `vm_area_struct` fields.
// upstream: i915_gem_mman.c __vma_matches()
unsafe fn __vma_matches(
    vma: *mut VmAreaStruct,
    filp: *mut File,
    addr: c_ulong,
    size: c_ulong,
) -> bool {
    if (*vma).vm_file != filp {
        return false;
    }
    (*vma).vm_start == addr
        && ((*vma).vm_end - (*vma).vm_start) == page_align(size)
}

// upstream: i915_gem_mman.c i915_gem_mmap_ioctl()
pub unsafe fn i915_gem_mmap_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> c_int {
    let i915 = to_i915(dev.cast());
    let args = data.cast::<DrmI915GemMmap>();

    if IS_DGFX(i915) || GRAPHICS_VER_FULL(i915) > IP_VER(12, 0) {
        return -EOPNOTSUPP;
    }
    if (*args).flags & !(I915_MMAP_WC as u64) != 0 {
        return -EINVAL;
    }
    if (*args).flags & I915_MMAP_WC as u64 != 0 && !pat_enabled() {
        return -ENODEV;
    }

    let obj = i915_gem_object_lookup(file.cast::<DrmFileObjectLookup>(), (*args).handle);
    if obj.is_null() {
        return -ENOENT;
    }
    // Keep all mmap failure branches in source order and release the looked-up
    // object exactly once, as the common C `err:` label does.
    let result: Result<c_ulong, c_int> = (|| {
        if (*obj).base.base.filp.is_null() {
            return Err(-ENXIO);
        }
        if range_overflows((*args).offset, (*args).size, (*obj).base.base.size as u64) {
            return Err(-EINVAL);
        }

        let addr = vm_mmap(
            (*obj).base.base.filp.cast(),
            0,
            (*args).size as c_ulong,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            (*args).offset,
        );
        if is_err_value(addr) {
            return Err(addr as c_long as c_int);
        }

        if (*args).flags & I915_MMAP_WC as u64 != 0 {
            let mm = crate::linux::mm_native::current_mm();
            if mmap_write_lock_killable(mm) != 0 {
                return Err(-EINTR);
            }
            let vma = find_vma(mm, addr);
            let mut mapped_addr = addr;
            if !vma.is_null()
                && __vma_matches(
                    vma,
                    (*obj).base.base.filp.cast(),
                    addr,
                    (*args).size as c_ulong,
                )
            {
                (*vma).vm_page_prot = pgprot_writecombine(vm_get_page_prot((*vma).vm_flags));
            } else {
                mapped_addr = (-ENOMEM) as c_ulong;
            }
            mmap_write_unlock(mm);
            if is_err_value(mapped_addr) {
                return Err(mapped_addr as c_long as c_int);
            }
        }
        Ok(addr)
    })();
    i915_gem_object_put(obj);
    match result {
        Ok(addr) => {
            (*args).addr_ptr = addr as u64;
            0
        }
        Err(err) => err,
    }
}

// upstream: i915_gem_mman.c tile_row_pages()
unsafe fn tile_row_pages(obj: *const DrmI915GemObject) -> u32 {
    i915_gem_object_get_tile_row_size(obj) >> PAGE_SHIFT
}

// upstream: i915_gem_mman.c i915_gem_mmap_gtt_version()
pub fn i915_gem_mmap_gtt_version() -> c_int {
    5
}

// upstream: i915_gem_mman.c compute_partial_view()
unsafe fn compute_partial_view(
    obj: *const DrmI915GemObject,
    page_offset: u64,
    mut chunk: u32,
) -> I915GttView {
    let mut view: I915GttView = core::mem::zeroed();
    if i915_gem_object_is_tiled(obj) {
        let row_pages = tile_row_pages(obj);
        chunk = round_up(chunk, if row_pages == 0 { 1 } else { row_pages });
    }
    view.r#type = I915_GTT_VIEW_PARTIAL;
    // MISSING BINDING: exact `intel_partial_info` union arm in I915GttView.
    // These source-member accesses deliberately remain unresolved until its
    // canonical owner exposes `partial.offset` and `partial.size`.
    view.info.partial.offset = round_down(page_offset, chunk as u64);
    view.info.partial.size = core::cmp::min(
        chunk,
        (((*obj).base.base.size >> PAGE_SHIFT) - view.info.partial.offset) as u32,
    );
    if chunk as u64 >= ((*obj).base.base.size >> PAGE_SHIFT) {
        view.r#type = I915_GTT_VIEW_NORMAL as u32;
    }
    view
}

// upstream: i915_gem_mman.c i915_error_to_vmf_fault()
fn i915_error_to_vmf_fault(err: c_int) -> u32 {
    if err == -EIO || err == -EFAULT || err == -ENODEV || err == -ENXIO {
        VM_FAULT_SIGBUS
    } else if err == -ENOMEM {
        VM_FAULT_OOM
    } else if err == 0 || err == -EAGAIN || err == -ENOSPC || err == -ENOBUFS
        || err == -ERESTARTSYS || err == -EINTR || err == -EBUSY
    {
        VM_FAULT_NOPAGE
    } else {
        WARN_ONCE!(err != 0, "unhandled error in i915_error_to_vmf_fault: {}", err);
        VM_FAULT_SIGBUS
    }
}

// upstream: i915_gem_mman.c vm_fault_cpu()
unsafe extern "C" fn vm_fault_cpu(vmf: *mut VmFault) -> u32 {
    let area = (*vmf).vma;
    let mmo = (*area).vm_private_data.cast::<I915MmapOffset>();
    let obj = (*mmo).obj;
    let mut err: c_int;

    if unlikely(i915_gem_object_is_readonly(obj) && (*area).vm_flags & VM_WRITE != 0) {
        return VM_FAULT_SIGBUS;
    }
    err = i915_gem_object_lock_interruptible(obj, core::ptr::null_mut());
    if err != 0 {
        return VM_FAULT_NOPAGE;
    }
    err = i915_gem_object_pin_pages(obj);
    if err != 0 {
        i915_gem_object_unlock(obj);
        return i915_error_to_vmf_fault(err);
    }

    let mut iomap: resource_size_t = !0;
    if !i915_gem_object_has_struct_page(obj) {
        iomap = (*(*obj).mm.region).iomap.base;
        iomap -= (*(*obj).mm.region).region.start;
    }
    let obj_offset = (*area).vm_pgoff - drm_vma_node_start(&mut (*mmo).vma_node);
    // PTEs are revoked in obj->ops->put_pages().
    err = remap_io_sg(
        area,
        (*area).vm_start,
        (*area).vm_end - (*area).vm_start,
        // MISSING BINDING: Linux `struct sg_table::sgl` layout.
        (*(*obj).mm.pages).sgl,
        obj_offset,
        iomap,
    );
    if (*area).vm_flags & VM_WRITE != 0 {
        GEM_BUG_ON!(!i915_gem_object_has_pinned_pages(obj));
        // `dirty` is bit 2 of the source `madv:2, dirty:1` pair.
        (*obj).mm.madv_dirty_bits |= 1 << 2;
    }
    i915_gem_object_unpin_pages(obj);
    i915_gem_object_unlock(obj);
    i915_error_to_vmf_fault(err)
}

// upstream: i915_gem_mman.c set_address_limits()
unsafe fn set_address_limits(
    area: *mut VmAreaStruct,
    vma: *mut I915Vma,
    obj_offset: c_ulong,
    gmadr_start: resource_size_t,
    start_vaddr: *mut c_ulong,
    end_vaddr: *mut c_ulong,
    pfn: *mut c_ulong,
) {
    let vm_start = (*area).vm_start >> PAGE_SHIFT;
    let vm_end = (*area).vm_end >> PAGE_SHIFT;
    let vma_size = (*vma).size >> PAGE_SHIFT;
    let mut start = vm_start as c_long;
    start -= obj_offset as c_long;
    // MISSING BINDING: exact `intel_partial_info` arm in `I915GttView`.
    start += (*vma).gtt_view.info.partial.offset as c_long;
    let mut end = start + vma_size as c_long;
    start = core::cmp::max(start, vm_start as c_long);
    end = core::cmp::min(end, vm_end as c_long);
    *start_vaddr = (start as c_ulong) << PAGE_SHIFT;
    *end_vaddr = (end as c_ulong) << PAGE_SHIFT;
    *pfn = (gmadr_start + i915_ggtt_offset(vma) as resource_size_t) >> PAGE_SHIFT;
    *pfn += (*start_vaddr - (*area).vm_start) >> PAGE_SHIFT;
    *pfn += obj_offset - (*vma).gtt_view.info.partial.offset;
}

// upstream: i915_gem_mman.c vm_fault_gtt()
unsafe extern "C" fn vm_fault_gtt(vmf: *mut VmFault) -> u32 {
    let area = (*vmf).vma;
    let mmo = (*area).vm_private_data.cast::<I915MmapOffset>();
    let obj = (*mmo).obj;
    let i915 = to_i915((*obj).base.base.dev);
    let rpm = core::ptr::addr_of_mut!((*i915).runtime_pm).cast::<IntelRuntimePm>();
    let ggtt = (*to_gt(i915)).ggtt;
    let write = (*area).vm_flags & VM_WRITE != 0;
    let obj_offset = (*area).vm_pgoff - drm_vma_node_start(&mut (*mmo).vma_node);
    let mut page_offset = ((*vmf).address - (*area).vm_start) >> PAGE_SHIFT;
    page_offset += obj_offset;

    trace_i915_gem_object_fault(obj, page_offset, true, write);
    let wakeref = intel_runtime_pm_get(rpm);
    let mut ww: I915GemWwCtx = core::mem::zeroed();
    i915_gem_ww_ctx_init(&mut ww, true);

    'retry: loop {
        let mut ret = i915_gem_object_lock(obj, &mut ww);
        let mut pages_pinned = false;
        let mut reset_locked = false;
        let mut fence_pinned = false;
        let mut vma = core::ptr::null_mut::<I915Vma>();
        let mut srcu = 0;

        if ret == 0 && i915_gem_object_is_readonly(obj) && write {
            ret = -EFAULT;
        }
        if ret == 0 {
            ret = i915_gem_object_pin_pages(obj);
            pages_pinned = ret == 0;
        }
        if ret == 0 {
            ret = intel_gt_reset_lock_interruptible((*ggtt).vm.gt, &mut srcu);
            reset_locked = ret == 0;
        }
        if ret == 0 {
            vma = i915_gem_object_ggtt_pin_ww(
                obj,
                &mut ww,
                core::ptr::null(),
                0,
                0,
                PIN_MAPPABLE | PIN_NONBLOCK | PIN_NOEVICT,
            );
        }

        if ret == 0 && is_err(vma) && ptr_err(vma) != -EDEADLK {
            let mut view = compute_partial_view(obj, page_offset, MIN_CHUNK_PAGES);
            let mut flags = PIN_MAPPABLE | PIN_NOSEARCH;
            if view.r#type == I915_GTT_VIEW_NORMAL as u32 {
                flags |= PIN_NONBLOCK;
            }

            // Userspace now writes through an untracked VMA; keep the source
            // retry order and abandon hardware write tracking.
            vma = i915_gem_object_ggtt_pin_ww(obj, &mut ww, &view, 0, 0, flags);
            if is_err(vma) && ptr_err(vma) != -EDEADLK {
                flags = PIN_MAPPABLE;
                view.r#type = I915_GTT_VIEW_PARTIAL;
                vma = i915_gem_object_ggtt_pin_ww(obj, &mut ww, &view, 0, 0, flags);
            }
            if is_err(vma) && ptr_err(vma) == -ENOSPC {
                ret = mutex_lock_interruptible(&mut (*ggtt).vm.mutex);
                if ret == 0 {
                    ret = i915_gem_evict_vm(&mut (*ggtt).vm, &mut ww, core::ptr::null_mut());
                    mutex_unlock(&mut (*ggtt).vm.mutex);
                }
                if ret == 0 {
                    vma = i915_gem_object_ggtt_pin_ww(obj, &mut ww, &view, 0, 0, flags);
                }
            }
        }

        if ret == 0 && is_err(vma) {
            ret = ptr_err(vma);
        }
        if ret == 0 && !(i915_gem_object_has_cache_level(obj, I915_CACHE_NONE) || HAS_LLC(i915)) {
            ret = -EFAULT;
        }
        if ret == 0 {
            ret = i915_vma_pin_fence(vma);
            fence_pinned = ret == 0;
        }
        if ret == 0 {
            let mut start = 0;
            let mut end = 0;
            let mut pfn = 0;
            set_address_limits(
                area,
                vma,
                obj_offset as c_ulong,
                (*ggtt).gmadr.start,
                &mut start,
                &mut end,
                &mut pfn,
            );
            ret = remap_io_mapping(area, start, pfn, end - start, &mut (*ggtt).iomap);

            if ret == 0 {
                assert_rpm_wakelock_held(rpm);
                mutex_lock(&mut (*(*to_gt(i915)).ggtt).vm.mutex);
                if !i915_vma_set_userfault(vma) {
                    let old_userfault_count = (*obj).userfault_count;
                    (*obj).userfault_count += 1;
                    if old_userfault_count == 0 {
                        list_add(
                            &mut (*obj).userfault_link,
                            &mut (*(*to_gt(i915)).ggtt).userfault_list,
                        );
                    }
                }
                mutex_unlock(&mut (*(*to_gt(i915)).ggtt).vm.mutex);
                // `i915_vma::mmo` is declared by the i915_vma_types owner;
                // its pointer-only I915MmapOffset overlay is ABI-identical.
                (*vma).mmo = mmo.cast();
                // MISSING CONFIG BINDING: take this integer directly from the
                // target kernel config; do not substitute upstream's default.
                if CONFIG_DRM_I915_USERFAULT_AUTOSUSPEND != 0 {
                    intel_wakeref_auto(
                        &mut (*i915).runtime_pm.userfault_wakeref,
                        msecs_to_jiffies_timeout(CONFIG_DRM_I915_USERFAULT_AUTOSUSPEND as u32),
                    );
                }
                if write {
                    GEM_BUG_ON!(!i915_gem_object_has_pinned_pages(obj));
                    i915_vma_set_ggtt_write(vma);
                    // `dirty` is bit 2 of the source `madv:2, dirty:1` pair.
                    (*obj).mm.madv_dirty_bits |= 1 << 2;
                }
            }
        }

        // Match C cleanup labels: fence, VMA, reset exclusion, object pages,
        // then wound/wait backoff and runtime-PM.
        if fence_pinned {
            // MISSING BINDING: i915_vma_unpin_fence depends on the full
            // I915FenceReg lock/pin-count owner, currently pointer-only.
            i915_vma_unpin_fence(vma);
        }
        if !vma.is_null() && !is_err(vma) {
            __i915_vma_unpin(vma);
        }
        if reset_locked {
            intel_gt_reset_unlock((*ggtt).vm.gt, srcu);
        }
        if pages_pinned {
            i915_gem_object_unpin_pages(obj);
        }
        if ret == -EDEADLK {
            ret = i915_gem_ww_ctx_backoff(&mut ww);
            if ret == 0 {
                continue 'retry;
            }
        }
        i915_gem_ww_ctx_fini(&mut ww);
        intel_runtime_pm_put_raw(rpm, wakeref);
        return i915_error_to_vmf_fault(ret);
    }
}

// upstream: i915_gem_mman.c vm_access()
unsafe extern "C" fn vm_access(area: *mut VmAreaStruct, addr: c_ulong, buf: *mut c_void, len: c_int, write: c_int) -> c_int {
    let mmo = (*area).vm_private_data.cast::<I915MmapOffset>();
    let obj = (*mmo).obj;
    if i915_gem_object_is_readonly(obj) && write != 0 { return -EACCES; }
    let obj_addr = addr - (*area).vm_start;
    if range_overflows_t(obj_addr as u64, len as u64, (*obj).base.base.size) { return -EINVAL; }
    let mut ww: I915GemWwCtx = core::mem::zeroed();
    i915_gem_ww_ctx_init(&mut ww, true);
    'retry: loop {
        let mut err = i915_gem_object_lock(obj, &mut ww);
        if err == 0 {
            let vaddr = i915_gem_object_pin_map(obj, I915_MAP_FORCE_WC);
            if is_err(vaddr) {
                err = ptr_err(vaddr);
            } else {
                if write != 0 {
                    core::ptr::copy_nonoverlapping(buf.cast::<u8>(), vaddr.cast::<u8>().add(obj_addr as usize), len as usize);
                    __i915_gem_object_flush_map(obj, obj_addr as u64, len as u64);
                } else {
                    core::ptr::copy_nonoverlapping(vaddr.cast::<u8>().add(obj_addr as usize), buf.cast::<u8>(), len as usize);
                }
                i915_gem_object_unpin_map(obj);
            }
        }
        if err == -EDEADLK {
            err = i915_gem_ww_ctx_backoff(&mut ww);
            if err == 0 { continue 'retry; }
        }
        i915_gem_ww_ctx_fini(&mut ww);
        if err != 0 { return err; }
        return len;
    }
}

// upstream: i915_gem_mman.c __i915_gem_object_release_mmap_gtt()
pub unsafe fn __i915_gem_object_release_mmap_gtt(obj: *mut DrmI915GemObject) {
    GEM_BUG_ON!((*obj).userfault_count == 0);
    for_each_ggtt_vma!(vma, obj, { i915_vma_revoke_mmap(vma); });
    GEM_BUG_ON!((*obj).userfault_count != 0);
}

// upstream: i915_gem_mman.c i915_gem_object_release_mmap_gtt()
pub unsafe fn i915_gem_object_release_mmap_gtt(obj: *mut DrmI915GemObject) {
    let i915 = to_i915((*obj).base.base.dev);
    let rpm = core::ptr::addr_of_mut!((*i915).runtime_pm).cast::<IntelRuntimePm>();
    let wakeref = intel_runtime_pm_get(rpm);
    let ggtt = (*to_gt(i915)).ggtt;
    mutex_lock(&mut (*ggtt).vm.mutex);
    if (*obj).userfault_count != 0 {
        __i915_gem_object_release_mmap_gtt(obj);
        wmb!();
    }
    mutex_unlock(&mut (*ggtt).vm.mutex);
    intel_runtime_pm_put_raw(rpm, wakeref);
}

// upstream: i915_gem_mman.c i915_gem_object_runtime_pm_release_mmap_offset()
pub unsafe fn i915_gem_object_runtime_pm_release_mmap_offset(obj: *mut DrmI915GemObject) {
    // MISSING TTM LAYOUT BINDING: `i915_gem_to_ttm()` needs the source-derived
    // TtmBufferObject/DrmGemObject prefix and device mapping owner.
    let bo = i915_gem_to_ttm(obj);
    let bdev = (*bo).bdev;
    drm_vma_node_unmap(&mut (*bo).base.vma_node, ttm_device_dev_mapping(bdev));
    GEM_BUG_ON!((*obj).userfault_count == 0);
    list_del(&mut (*obj).userfault_link);
    (*obj).userfault_count = 0;
}

// upstream: i915_gem_mman.c i915_gem_object_release_mmap_offset()
pub unsafe fn i915_gem_object_release_mmap_offset(obj: *mut DrmI915GemObject) {
    if let Some(unmap_virtual) = (*(*obj).ops).unmap_virtual { unmap_virtual(obj); }
    spin_lock(&mut (*obj).mmo.lock);
    // Linux's `rbtree_postorder_for_each_entry_safe()` computes the next node
    // before each body, since the body temporarily drops the tree lock.
    let mut rb = rb_first_postorder(&mut (*obj).mmo.offsets);
    while !rb.is_null() {
        let next = rb_next_postorder(rb);
        let mmo = rb_entry!(rb, I915MmapOffset, offset);
        if (*mmo).mmap_type == I915MmapType::I915_MMAP_TYPE_GTT {
            rb = next;
            continue;
        }
        spin_unlock(&mut (*obj).mmo.lock);
        drm_vma_node_unmap(&mut (*mmo).vma_node, (*(*(*obj).base.base.dev.cast::<DrmDevicePrefix>()).anon_inode).i_mapping);
        spin_lock(&mut (*obj).mmo.lock);
        rb = next;
    }
    spin_unlock(&mut (*obj).mmo.lock);
}

// upstream: i915_gem_mman.c lookup_mmo()
unsafe fn lookup_mmo(obj: *mut DrmI915GemObject, mmap_type: I915MmapType) -> *mut I915MmapOffset {
    spin_lock(&mut (*obj).mmo.lock);
    let mut rb = (*obj).mmo.offsets.node;
    while !rb.is_null() {
        let mmo = rb_entry!(rb, I915MmapOffset, offset);
        if (*mmo).mmap_type == mmap_type { spin_unlock(&mut (*obj).mmo.lock); return mmo; }
        rb = if ((*mmo).mmap_type as i32) < mmap_type as i32 { (*rb).right } else { (*rb).left };
    }
    spin_unlock(&mut (*obj).mmo.lock);
    core::ptr::null_mut()
}

// upstream: i915_gem_mman.c insert_mmo()
unsafe fn insert_mmo(obj: *mut DrmI915GemObject, mmo: *mut I915MmapOffset) -> *mut I915MmapOffset {
    spin_lock(&mut (*obj).mmo.lock);
    let mut rb: *mut RbNode = core::ptr::null_mut();
    let mut p = &mut (*obj).mmo.offsets.node as *mut *mut RbNode;
    while !(*p).is_null() {
        rb = *p;
        let pos = rb_entry!(rb, I915MmapOffset, offset);
        if (*pos).mmap_type == (*mmo).mmap_type {
            spin_unlock(&mut (*obj).mmo.lock);
            drm_vma_offset_remove((*(*obj).base.base.dev.cast::<DrmDevicePrefix>()).vma_offset_manager, &mut (*mmo).vma_node);
            kfree(mmo.cast::<c_void>());
            return pos;
        }
        p = if ((*pos).mmap_type as i32) < (*mmo).mmap_type as i32 { &mut (*rb).right } else { &mut (*rb).left };
    }
    rb_link_node(&mut (*mmo).offset, rb, p);
    rb_insert_color(&mut (*mmo).offset, &mut (*obj).mmo.offsets);
    spin_unlock(&mut (*obj).mmo.lock);
    mmo
}

// upstream: i915_gem_mman.c mmap_offset_attach()
unsafe fn mmap_offset_attach(obj: *mut DrmI915GemObject, mmap_type: I915MmapType, file: *mut DrmFile) -> *mut I915MmapOffset {
    let i915 = to_i915((*obj).base.base.dev);
    GEM_BUG_ON!((*(*obj).ops).mmap_offset.is_some() || !(*(*obj).ops).mmap_ops.is_null());
    let mut mmo = lookup_mmo(obj, mmap_type);
    if mmo.is_null() {
        mmo = kmalloc_obj::<I915MmapOffset>(crate::linux_config::GFP_KERNEL);
        if mmo.is_null() { return ERR_PTR!(-ENOMEM); }
        (*mmo).obj = obj;
        (*mmo).mmap_type = mmap_type;
        drm_vma_node_reset(&mut (*mmo).vma_node);

        let manager = (*(*obj).base.base.dev.cast::<DrmDevicePrefix>()).vma_offset_manager;
        let mut err = drm_vma_offset_add(manager, &mut (*mmo).vma_node,
            (*obj).base.base.size / PAGE_SIZE_U64);
        if unlikely(err != 0) {
            // Reap dead objects once, then retry the vma-space allocation.
            err = intel_gt_retire_requests_timeout(
                to_gt(i915),
                MAX_SCHEDULE_TIMEOUT as c_long,
                core::ptr::null_mut(),
            ) as c_int;
            if err != 0 {
                kfree(mmo.cast::<c_void>());
                return ERR_PTR!(err);
            }
            i915_gem_drain_freed_objects(i915);
            err = drm_vma_offset_add(manager, &mut (*mmo).vma_node,
                (*obj).base.base.size / PAGE_SIZE_U64);
            if err != 0 {
                kfree(mmo.cast::<c_void>());
                return ERR_PTR!(err);
            }
        }

        mmo = insert_mmo(obj, mmo);
        GEM_BUG_ON!(lookup_mmo(obj, mmap_type) != mmo);
    }
    if !file.is_null() { drm_vma_node_allow_once(&mut (*mmo).vma_node, file); }
    mmo
}

// upstream: i915_gem_mman.c __assign_mmap_offset()
unsafe fn __assign_mmap_offset(obj: *mut DrmI915GemObject, mmap_type: I915MmapType, offset: *mut u64, file: *mut DrmFile) -> c_int {
    if i915_gem_object_never_mmap(obj) { return -ENODEV; }
    if let Some(mmap_offset) = (*(*obj).ops).mmap_offset {
        if mmap_type != I915MmapType::I915_MMAP_TYPE_FIXED { return -ENODEV; }
        *offset = mmap_offset(obj);
        return 0;
    }
    if mmap_type == I915MmapType::I915_MMAP_TYPE_FIXED { return -ENODEV; }
    if mmap_type != I915MmapType::I915_MMAP_TYPE_GTT
        && !i915_gem_object_has_struct_page(obj)
        && !i915_gem_object_has_iomem(obj) { return -ENODEV; }
    let mmo = mmap_offset_attach(obj, mmap_type, file);
    if is_err(mmo) { return ptr_err(mmo); }
    *offset = drm_vma_node_offset_addr(&mut (*mmo).vma_node);
    0
}

// upstream: i915_gem_mman.c __assign_mmap_offset_handle()
unsafe fn __assign_mmap_offset_handle(file: *mut DrmFile, handle: u32, mmap_type: I915MmapType, offset: *mut u64) -> c_int {
        let obj = i915_gem_object_lookup(file.cast::<DrmFileObjectLookup>(), handle);
    if obj.is_null() { return -ENOENT; }
    let mut err = i915_gem_object_lock_interruptible(obj, core::ptr::null_mut());
    if err == 0 {
        err = __assign_mmap_offset(obj, mmap_type, offset, file);
        i915_gem_object_unlock(obj);
    }
    i915_gem_object_put(obj);
    err
}

// upstream: i915_gem_mman.c i915_gem_dumb_mmap_offset()
pub unsafe fn i915_gem_dumb_mmap_offset(file: *mut DrmFile, dev: *mut DrmDevice, handle: u32, offset: *mut u64) -> c_int {
    let i915 = to_i915(dev.cast());
    let mmap_type = if HAS_LMEM(i915) {
        I915MmapType::I915_MMAP_TYPE_FIXED
    } else if pat_enabled() {
        I915MmapType::I915_MMAP_TYPE_WC
    } else if !i915_ggtt_has_aperture((*to_gt(i915)).ggtt) {
        return -ENODEV;
    } else {
        I915MmapType::I915_MMAP_TYPE_GTT
    };
    __assign_mmap_offset_handle(file, handle, mmap_type, offset)
}

// upstream: i915_gem_mman.c i915_gem_mmap_offset_ioctl()
pub unsafe fn i915_gem_mmap_offset_ioctl(dev: *mut DrmDevice, data: *mut c_void, file: *mut DrmFile) -> c_int {
    let i915 = to_i915(dev.cast());
    let args = data.cast::<DrmI915GemMmapOffset>();
    // Historical ABI: pad and offset are intentionally not validated.
    let err = i915_user_extensions(
        (*args).extensions as usize as *mut I915UserExtension,
        core::ptr::null_mut(),
        0,
        core::ptr::null_mut(),
    );
    if err != 0 { return err; }
    let mmap_type = match (*args).flags {
        I915_MMAP_OFFSET_GTT => { if !i915_ggtt_has_aperture((*to_gt(i915)).ggtt) { return -ENODEV; } I915MmapType::I915_MMAP_TYPE_GTT },
        I915_MMAP_OFFSET_WC => { if !pat_enabled() { return -ENODEV; } I915MmapType::I915_MMAP_TYPE_WC },
        I915_MMAP_OFFSET_WB => I915MmapType::I915_MMAP_TYPE_WB,
        I915_MMAP_OFFSET_UC => { if !pat_enabled() { return -ENODEV; } I915MmapType::I915_MMAP_TYPE_UC },
        I915_MMAP_OFFSET_FIXED => I915MmapType::I915_MMAP_TYPE_FIXED,
        _ => return -EINVAL,
    };
    __assign_mmap_offset_handle(file, (*args).handle, mmap_type, &mut (*args).offset)
}

// upstream: i915_gem_mman.c vm_open()
unsafe extern "C" fn vm_open(vma: *mut VmAreaStruct) {
    let mmo = (*vma).vm_private_data.cast::<I915MmapOffset>();
    let obj = (*mmo).obj;
    GEM_BUG_ON!(obj.is_null());
    i915_gem_object_get(obj);
}

// upstream: i915_gem_mman.c vm_close()
unsafe extern "C" fn vm_close(vma: *mut VmAreaStruct) {
    let mmo = (*vma).vm_private_data.cast::<I915MmapOffset>();
    let obj = (*mmo).obj;
    GEM_BUG_ON!(obj.is_null());
    i915_gem_object_put(obj);
}

static vm_ops_gtt: VmOperationsStruct = VmOperationsStruct {
    fault: Some(vm_fault_gtt), access: Some(vm_access), open: Some(vm_open), close: Some(vm_close),
    ..VmOperationsStruct::EMPTY
};
static vm_ops_cpu: VmOperationsStruct = VmOperationsStruct {
    fault: Some(vm_fault_cpu), access: Some(vm_access), open: Some(vm_open), close: Some(vm_close),
    ..VmOperationsStruct::EMPTY
};

// upstream: i915_gem_mman.c singleton_release()
unsafe extern "C" fn singleton_release(_inode: *mut crate::linux_i915_private::Inode, file: *mut File) -> c_int {
    let i915 = (*file).private_data.cast::<DrmI915Private>();
    cmpxchg(&mut (*i915).gem.mmap_singleton, file, core::ptr::null_mut());
    drm_dev_put(core::ptr::addr_of_mut!((*i915).drm).cast());
    0
}

static singleton_fops: FileOperations = FileOperations {
    owner: core::ptr::null_mut(),
    fop_flags: 0,
    _flags_pad: 0,
    _before_release: [core::ptr::null(); 13],
    release: Some(singleton_release),
    _after_release: [core::ptr::null(); 18],
};

// upstream: i915_gem_mman.c mmap_singleton()
unsafe fn mmap_singleton(i915: *mut DrmI915Private) -> *mut File {
    let mut file = get_file_active(&mut (*i915).gem.mmap_singleton);
    if !file.is_null() { return file; }
    file = anon_inode_getfile(c"i915.gem".as_ptr(), &singleton_fops, i915.cast(), O_RDWR);
    if is_err(file) { return file; }
    (*file).f_mapping = (*(*i915).drm.anon_inode).i_mapping;
    crate::linux::primitives::smp_store_mb(&mut (*i915).gem.mmap_singleton, file);
    drm_dev_get(core::ptr::addr_of_mut!((*i915).drm).cast());
    file
}

// upstream: i915_gem_mman.c i915_gem_object_mmap()
unsafe fn i915_gem_object_mmap(obj: *mut DrmI915GemObject, mmo: *mut I915MmapOffset, vma: *mut VmAreaStruct) -> c_int {
    let i915 = to_i915((*obj).base.base.dev);
    let dev = &mut (*i915).drm;
    if i915_gem_object_is_readonly(obj) {
        if (*vma).vm_flags & VM_WRITE != 0 { i915_gem_object_put(obj); return -EINVAL; }
        vm_flags_clear(vma, VM_MAYWRITE);
    }
    let anon = mmap_singleton(i915);
    if is_err(anon) { i915_gem_object_put(obj); return ptr_err(anon); }
    vm_flags_set(vma, VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP | VM_IO);
    vma_set_file(vma, anon);
    fput(anon.cast());
    if !(*(*obj).ops).mmap_ops.is_null() {
        (*vma).vm_page_prot = pgprot_decrypted(vm_get_page_prot((*vma).vm_flags));
        (*vma).vm_ops = (*(*obj).ops).mmap_ops;
        (*vma).vm_private_data = (*obj).base.base.vma_node.driver_private;
        return 0;
    }
    (*vma).vm_private_data = mmo.cast();
    match (*mmo).mmap_type {
        I915MmapType::I915_MMAP_TYPE_WC => { (*vma).vm_page_prot = pgprot_writecombine(vm_get_page_prot((*vma).vm_flags)); (*vma).vm_ops = &vm_ops_cpu; },
        I915MmapType::I915_MMAP_TYPE_FIXED => { GEM_WARN_ON!(true); (*vma).vm_page_prot = vm_get_page_prot((*vma).vm_flags); (*vma).vm_ops = &vm_ops_cpu; },
        I915MmapType::I915_MMAP_TYPE_WB => { (*vma).vm_page_prot = vm_get_page_prot((*vma).vm_flags); (*vma).vm_ops = &vm_ops_cpu; },
        I915MmapType::I915_MMAP_TYPE_UC => { (*vma).vm_page_prot = pgprot_noncached(vm_get_page_prot((*vma).vm_flags)); (*vma).vm_ops = &vm_ops_cpu; },
        I915MmapType::I915_MMAP_TYPE_GTT => { (*vma).vm_page_prot = pgprot_writecombine(vm_get_page_prot((*vma).vm_flags)); (*vma).vm_ops = &vm_ops_gtt; },
    }
    (*vma).vm_page_prot = pgprot_decrypted((*vma).vm_page_prot);
    let _ = dev;
    0
}

// upstream: i915_gem_mman.c i915_gem_mmap()
pub unsafe fn i915_gem_mmap(filp: *mut File, vma: *mut VmAreaStruct) -> c_int {
    let priv_ = (*filp).private_data.cast::<DrmFile>();
    let dev = (*(*priv_).minor).dev.cast::<DrmDevicePrefix>();
    let mut obj: *mut DrmI915GemObject = core::ptr::null_mut();
    let mut mmo: *mut I915MmapOffset = core::ptr::null_mut();
    if drm_dev_is_unplugged(dev.cast()) { return -ENODEV; }
    rcu_read_lock();
    drm_vma_offset_lock_lookup((*dev).vma_offset_manager);
    let node = drm_vma_offset_lookup_locked((*dev).vma_offset_manager, (*vma).vm_pgoff, vma_pages(vma));
    if !node.is_null() && drm_vma_node_is_allowed(node, priv_) {
        if (*node).driver_private.is_null() {
            mmo = container_of!(node, I915MmapOffset, vma_node);
            obj = i915_gem_object_get_rcu((*mmo).obj);
            GEM_BUG_ON!(!obj.is_null() && !(*(*obj).ops).mmap_ops.is_null());
        } else {
            obj = i915_gem_object_get_rcu(container_of!(node, DrmGemObjectBaseLayout, vma_node).cast::<DrmI915GemObject>());
            GEM_BUG_ON!(!obj.is_null() && (*(*obj).ops).mmap_ops.is_null());
        }
    }
    drm_vma_offset_unlock_lookup((*dev).vma_offset_manager);
    rcu_read_unlock();
    if obj.is_null() { return if node.is_null() { -EINVAL } else { -EACCES }; }
    i915_gem_object_mmap(obj, mmo, vma)
}

// upstream: i915_gem_mman.c i915_gem_fb_mmap()
pub unsafe fn i915_gem_fb_mmap(obj: *mut DrmI915GemObject, vma: *mut VmAreaStruct) -> c_int {
    let i915 = to_i915((*obj).base.base.dev);
    let dev = &mut (*i915).drm;
    let mut mmo: *mut I915MmapOffset = core::ptr::null_mut();
    let ggtt = (*to_gt(i915)).ggtt;
    if drm_dev_is_unplugged(core::ptr::addr_of_mut!((*i915).drm).cast()) { return -ENODEV; }
    if !(*(*obj).ops).mmap_ops.is_null() {
        let drm_gem = core::ptr::addr_of_mut!((*obj).base.base)
            .cast::<DrmGemObjectBaseLayout>();
        (*vma).vm_pgoff += drm_vma_node_start(core::ptr::addr_of!((*drm_gem).vma_node));
    } else {
        let mmap_type = if i915_ggtt_has_aperture(ggtt) { I915MmapType::I915_MMAP_TYPE_GTT } else { I915MmapType::I915_MMAP_TYPE_WC };
        mmo = mmap_offset_attach(obj, mmap_type, core::ptr::null_mut());
        if is_err(mmo) { return ptr_err(mmo); }
        (*vma).vm_pgoff += drm_vma_node_start(&mut (*mmo).vma_node);
    }
    i915_gem_object_mmap(i915_gem_object_get(obj), mmo, vma)
}

// Linux lib/rbtree.c's postorder traversal helpers, used to preserve the
// semantics of `rbtree_postorder_for_each_entry_safe()` while the DRM unmap
// callback temporarily drops `mmo.lock`.
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

// `rb_link_node()` is the inline Linux helper from include/linux/rbtree.h.
unsafe fn rb_link_node(node: *mut RbNode, parent: *mut RbNode, link: *mut *mut RbNode) {
    (*node).parent_color = parent as usize;
    (*node).left = core::ptr::null_mut();
    (*node).right = core::ptr::null_mut();
    *link = node;
}

// The balancing routine is the real out-of-line Linux `lib/rbtree.c` API.
unsafe extern "C" {
    fn rb_insert_color(node: *mut RbNode, root: *mut RbRoot);
}

// Helpers used by translated source expressions; these are arithmetic only.
#[inline]
fn is_err<T>(pointer: *mut T) -> bool { crate::linux_config::IS_ERR(pointer) }
#[inline]
fn ptr_err<T>(pointer: *mut T) -> c_int { crate::linux_config::PTR_ERR(pointer) }
#[inline]
fn is_err_value(value: c_ulong) -> bool { value >= c_ulong::MAX - 4094 }
#[inline]
fn range_overflows(start: u64, size: u64, max: u64) -> bool {
    start >= max || size > max - start
}
#[inline]
fn range_overflows_t(start: u64, size: u64, max: u64) -> bool {
    range_overflows(start, size, max)
}
#[inline]
fn page_align(value: c_ulong) -> c_ulong { (value + ((1 << PAGE_SHIFT) - 1)) & !((1 << PAGE_SHIFT) - 1) }
#[inline]
fn round_up(value: u32, alignment: u32) -> u32 { ((value + alignment - 1) / alignment) * alignment }
#[inline]
fn round_down(value: u64, alignment: u64) -> u64 { (value / alignment) * alignment }
#[inline]
fn msecs_to_jiffies_timeout(milliseconds: u32) -> c_ulong {
    let jiffies = crate::linux::primitives::msecs_to_jiffies(milliseconds);
    let max_jiffy_offset = ((c_long::MAX as u64) >> 1) - 1;
    jiffies.saturating_add(1).min(max_jiffy_offset) as c_ulong
}
