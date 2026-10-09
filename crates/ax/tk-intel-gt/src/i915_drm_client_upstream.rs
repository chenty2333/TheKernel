// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
//
// Source-order translation of Linux 7.2.3 drivers/gpu/drm/i915/i915_drm_client.c
// for the supplied CONFIG_PROC_FS=y target. DRM/client/file layout views are
// limited to fields accessed by this file; RCU, locking, allocation, refcount,
// object and context operations use their canonical LinuxKPI/upstream owners.

#![allow(non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use core::{
    ffi::{c_char, c_int, c_void},
    mem::offset_of,
    sync::atomic::{AtomicPtr, AtomicU64, Ordering},
};

use crate::{
    i915_gem_context_types_upstream::{
        DrmI915FilePrivate, I915DrmClient, I915GemContext, I915GemEngines,
    },
    i915_gem_object_types_upstream::DrmI915GemObject,
    intel_context_types_upstream::IntelContext,
    intel_context_upstream::{DrmGemObjectBaseLayout, Kref, RadixTreeRoot},
    intel_engine_cs_upstream::{ListHead, Spinlock},
    linux::{
        gem,
        gem_memory::{INTEL_REGION_SMEM, INTEL_REGION_UNKNOWN, IntelMemoryRegion},
        locks, memory,
        print::DrmPrinter,
        rcu,
    },
    linux_i915_private::DrmI915Private,
};

#[repr(C)]
struct Idr {
    _radix_tree_root: crate::intel_context_upstream::RadixTreeRoot,
    _base: u32,
    _next: u32,
}

/// `drm_file` prefix to the members used by `show_meminfo()` (`drm_file.h`).
#[repr(C)]
struct DrmFileView {
    _prefix: [u8; 80],
    object_idr: Idr,
    table_lock: Spinlock,
    _xa_alignment: [u8; 4],
    _syncobj_xa: [u8; 16],
    _filp: *mut c_void,
    driver_priv: *mut DrmI915FilePrivate,
}

/// `drm_i915_file_private` fields accessed by this file (`i915_file_private.h`).
#[repr(C)]
struct DrmI915FilePrivateView {
    i915: *mut DrmI915Private,
    _prefix: [u8; 104],
    client: *mut I915DrmClient,
}

/// `i915_drm_client`, with the CONFIG_PROC_FS member sequence (`i915_drm_client.h`).
#[repr(C)]
struct I915DrmClientView {
    kref: Kref,
    ctx_lock: Spinlock,
    ctx_list: ListHead,
    objects_lock: Spinlock,
    objects_list: ListHead,
    past_runtime: [AtomicU64; 5],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmMemoryStats {
    shared: u64,
    private: u64,
    resident: u64,
    purgeable: u64,
    active: u64,
}

/// Source-derived position of `drm_i915_private.engine_uabi_class_count`.
/// The array ends 160 bytes before `wq`; the registered private owner asserts
/// `wq` at 2536 for this target, placing the array at 2376.
#[repr(C)]
struct I915EngineUabiClassCountView {
    _prefix: [u8; 2376],
    count: [u32; 5],
}

const _: [(); 80] = [(); offset_of!(DrmFileView, object_idr)];
const _: [(); 104] = [(); offset_of!(DrmFileView, table_lock)];
const _: [(); 112] = [(); offset_of!(DrmFileView, _syncobj_xa)];
const _: [(); 136] = [(); offset_of!(DrmFileView, driver_priv)];
const _: [(); 0] = [(); offset_of!(DrmI915FilePrivateView, i915)];
const _: [(); 112] = [(); offset_of!(DrmI915FilePrivateView, client)];
const _: [(); 24] = [(); offset_of!(I915DrmClientView, objects_lock)];
const _: [(); 32] = [(); offset_of!(I915DrmClientView, objects_list)];
const _: [(); 48] = [(); offset_of!(I915DrmClientView, past_runtime)];
const _: [(); 88] = [(); core::mem::size_of::<I915DrmClientView>()];
const _: [(); 2376] = [(); offset_of!(I915EngineUabiClassCountView, count)];
const _: [(); 20] = [(); core::mem::size_of::<[u32; 5]>()];
const _: [(); 4] = [(); core::mem::size_of::<Kref>()];
const _: [(); 2536] = [(); offset_of!(DrmI915Private, wq)];

const DRM_GEM_OBJECT_RESIDENT: c_int = 1 << 0;
const DRM_GEM_OBJECT_PURGEABLE: c_int = 1 << 1;
const DRM_GEM_OBJECT_ACTIVE: c_int = 1 << 2;
const DMA_RESV_USAGE_BOOKKEEP: c_int = 3;
const I915_MADV_DONTNEED: u32 = 1;
const UABI_CLASS_NAMES: [&str; 5] = ["render", "copy", "video", "video-enhance", "compute"];

unsafe extern "C" {
    fn idr_get_next(idr: *mut Idr, nextid: *mut c_int) -> *mut c_void;
    fn dma_resv_test_signaled(resv: *mut c_void, usage: c_int) -> bool;
    fn drm_print_memory_stats(
        printer: *mut DrmPrinter,
        stats: *const DrmMemoryStats,
        supported_status: c_int,
        region: *const c_char,
    );
}

#[inline]
unsafe fn client_view(client: *mut I915DrmClient) -> *mut I915DrmClientView {
    client.cast()
}

#[inline]
unsafe fn file_private_view(file: *mut DrmFileView) -> *mut DrmI915FilePrivateView {
    unsafe { (*file).driver_priv.cast() }
}

#[inline]
unsafe fn object_gem_base(obj: *mut DrmI915GemObject) -> *mut gem::DrmGemObject {
    unsafe { crate::i915_gem_object_types_upstream::intel_bo_to_drm_bo(obj) }
}

#[inline]
unsafe fn i915_engine_class_count(i915: *mut DrmI915Private, class: usize) -> u32 {
    unsafe {
        core::ptr::read_volatile(core::ptr::addr_of!(
            (*i915.cast::<I915EngineUabiClassCountView>()).count[class]
        ))
    }
}

/// The `list_add_tail_rcu()` store order used by Linux's list.h.
unsafe fn list_add_tail_rcu(new: *mut ListHead, head: *mut ListHead) {
    unsafe {
        let prev = (*head).prev;
        (*new).next = head;
        (*new).prev = prev;
        core::sync::atomic::fence(Ordering::Release);
        (*prev).next = new;
        (*head).prev = new;
    }
}

// upstream: i915_drm_client.h i915_drm_client_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_drm_client_get(client: *mut I915DrmClient) -> *mut I915DrmClient {
    unsafe { memory::kref_get(core::ptr::addr_of_mut!((*client_view(client)).kref)) };
    client
}

// upstream: i915_drm_client.h i915_drm_client_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_drm_client_put(client: *mut I915DrmClient) {
    unsafe {
        memory::kref_put(
            core::ptr::addr_of_mut!((*client_view(client)).kref),
            __i915_drm_client_free,
        );
    }
}

// upstream: i915_drm_client.c i915_drm_client_alloc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_drm_client_alloc() -> *mut I915DrmClient {
    unsafe {
        let client = memory::kzalloc_obj::<I915DrmClientView>();
        if client.is_null() {
            return core::ptr::null_mut();
        }
        memory::kref_init(core::ptr::addr_of_mut!((*client).kref));
        locks::spin_lock_init(&mut (*client).ctx_lock);
        crate::linux_list::INIT_LIST_HEAD(core::ptr::addr_of_mut!((*client).ctx_list));
        locks::spin_lock_init(&mut (*client).objects_lock);
        crate::linux_list::INIT_LIST_HEAD(core::ptr::addr_of_mut!((*client).objects_list));
        client.cast()
    }
}

// upstream: i915_drm_client.c __i915_drm_client_free()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __i915_drm_client_free(kref: *mut Kref) {
    unsafe { memory::kfree(kref.cast::<I915DrmClientView>()) }
}

// upstream: i915_drm_client.c obj_meminfo()
unsafe fn obj_meminfo(obj: *mut DrmI915GemObject, stats: *mut DrmMemoryStats) {
    unsafe {
        let region = if (*obj).mm.region.is_null() {
            INTEL_REGION_SMEM
        } else {
            (*(*obj).mm.region).id
        } as usize;
        let size = (*object_gem_base(obj)).size;
        let base = object_gem_base(obj);
        // drm_gem_object_is_shared_for_memory_stats(): handle_count > 1 || dma_buf.
        let handle_count = core::ptr::read_volatile(base.cast::<u8>().add(4).cast::<u32>());
        if handle_count > 1 || !(*base).dma_buf.is_null() {
            (*stats.add(region)).shared = (*stats.add(region)).shared.wrapping_add(size);
        } else {
            (*stats.add(region)).private = (*stats.add(region)).private.wrapping_add(size);
        }
        if crate::i915_gem_object_header_upstream::i915_gem_object_has_pages(obj) {
            (*stats.add(region)).resident = (*stats.add(region)).resident.wrapping_add(size);
            if !dma_resv_test_signaled((*base).resv, DMA_RESV_USAGE_BOOKKEEP) {
                (*stats.add(region)).active = (*stats.add(region)).active.wrapping_add(size);
            } else if crate::i915_gem_object_header_upstream::i915_gem_object_is_shrinkable(obj)
                && (*obj).mm.madv() == I915_MADV_DONTNEED
            {
                (*stats.add(region)).purgeable = (*stats.add(region)).purgeable.wrapping_add(size);
            }
        }
    }
}

// upstream: i915_drm_client.c show_meminfo()
unsafe fn show_meminfo(printer: *mut DrmPrinter, file: *mut DrmFileView) {
    unsafe {
        let mut stats = [DrmMemoryStats::default(); INTEL_REGION_UNKNOWN as usize];
        let file_priv = file_private_view(file);
        let client = (*file_priv).client;
        let i915 = (*file_priv).i915;

        // Public objects, protected by drm_file.table_lock.
        locks::spin_lock(&mut (*file).table_lock);
        let mut id = 0;
        loop {
            let obj = idr_get_next(core::ptr::addr_of_mut!((*file).object_idr), &mut id)
                .cast::<DrmI915GemObject>();
            if obj.is_null() {
                break;
            }
            obj_meminfo(obj, stats.as_mut_ptr());
            id = id.wrapping_add(1);
        }
        locks::spin_unlock(&mut (*file).table_lock);

        // Internal objects; the RCU list and temporary GEM reference protect
        // each object while its per-region counters are updated.
        rcu::rcu_read_lock();
        let client_view = client_view(client);
        let head = core::ptr::addr_of_mut!((*client_view).objects_list);
        let mut node = core::ptr::read_volatile(core::ptr::addr_of!((*head).next));
        while node != head {
            let obj = node
                .cast::<u8>()
                .wrapping_sub(offset_of!(DrmI915GemObject, client_link))
                .cast::<DrmI915GemObject>();
            let obj = crate::i915_gem_object_header_upstream::i915_gem_object_get_rcu(obj);
            if !obj.is_null() {
                obj_meminfo(obj, stats.as_mut_ptr());
                crate::i915_gem_object_header_upstream::i915_gem_object_put(obj);
            }
            node = core::ptr::read_volatile(core::ptr::addr_of!((*node).next));
        }
        rcu::rcu_read_unlock();

        let mut id = 0usize;
        while id < INTEL_REGION_UNKNOWN as usize {
            let mr = (*i915).mm.regions[id];
            if !mr.is_null() {
                drm_print_memory_stats(
                    printer,
                    stats.as_ptr().add(id),
                    DRM_GEM_OBJECT_ACTIVE | DRM_GEM_OBJECT_RESIDENT | DRM_GEM_OBJECT_PURGEABLE,
                    (*mr).uabi_name.as_ptr(),
                );
            }
            id += 1;
        }
    }
}

// upstream: i915_drm_client.c busy_add()
unsafe fn busy_add(ctx: *mut I915GemContext, class: u32) -> u64 {
    unsafe {
        let engines = core::ptr::read_volatile(core::ptr::addr_of!((*ctx).engines));
        if engines.is_null() {
            return 0;
        }
        let mut total = 0u64;
        let mut idx = 0;
        while idx < (*engines).num_engines as usize {
            let ce = core::ptr::read_volatile(core::ptr::addr_of!(
                *(*engines).engines.as_ptr().add(idx)
            ));
            if !ce.is_null() && (*(*ce).engine).uabi_class as u32 == class {
                total = total.wrapping_add(
                    crate::intel_context_upstream::intel_context_get_total_runtime_ns(ce),
                );
            }
            idx += 1;
        }
        total
    }
}

// upstream: i915_drm_client.c show_client_class()
unsafe fn show_client_class(
    printer: *mut DrmPrinter,
    i915: *mut DrmI915Private,
    client: *mut I915DrmClient,
    class: u32,
) {
    unsafe {
        let capacity = i915_engine_class_count(i915, class as usize);
        let client_view = client_view(client);
        let mut total = (*client_view).past_runtime[class as usize].load(Ordering::Relaxed);
        rcu::rcu_read_lock();
        let head = core::ptr::addr_of_mut!((*client_view).ctx_list);
        let mut node = core::ptr::read_volatile(core::ptr::addr_of!((*head).next));
        while node != head {
            let ctx = node
                .cast::<u8>()
                .wrapping_sub(offset_of!(I915GemContext, client_link))
                .cast::<I915GemContext>();
            total = total.wrapping_add(busy_add(ctx, class));
            node = core::ptr::read_volatile(core::ptr::addr_of!((*node).next));
        }
        rcu::rcu_read_unlock();

        let name = UABI_CLASS_NAMES[class as usize];
        if capacity != 0 {
            drm_printf!(printer, "drm-engine-%s:\t%llu ns\n", name, total);
        }
        if capacity > 1 {
            drm_printf!(printer, "drm-engine-capacity-%s:\t%u\n", name, capacity);
        }
    }
}

// upstream: i915_drm_client.c i915_drm_client_fdinfo()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_drm_client_fdinfo(printer: *mut DrmPrinter, file: *mut DrmFileView) {
    unsafe {
        let file_priv = file_private_view(file);
        let i915 = (*file_priv).i915;
        show_meminfo(printer, file);
        if crate::linux::i915::GRAPHICS_VER(i915) < 8 {
            return;
        }
        let mut class = 0;
        while class < UABI_CLASS_NAMES.len() as u32 {
            show_client_class(printer, i915, (*file_priv).client, class);
            class += 1;
        }
    }
}

// upstream: i915_drm_client.c i915_drm_client_add_object()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_drm_client_add_object(
    client: *mut I915DrmClient,
    obj: *mut DrmI915GemObject,
) {
    unsafe {
        GEM_WARN_ON!(!(*obj).client.is_null());
        GEM_WARN_ON!(!crate::linux::list::list_empty(&(*obj).client_link));
        let mut flags = 0;
        locks::spin_lock_irqsave_raw(
            core::ptr::addr_of_mut!((*client_view(client)).objects_lock),
            &mut flags,
        );
        (*obj).client = i915_drm_client_get(client);
        list_add_tail_rcu(
            core::ptr::addr_of_mut!((*obj).client_link),
            core::ptr::addr_of_mut!((*client_view(client)).objects_list),
        );
        locks::spin_unlock_irqrestore_raw(
            core::ptr::addr_of_mut!((*client_view(client)).objects_lock),
            flags,
        );
    }
}

// upstream: i915_drm_client.c i915_drm_client_remove_object()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_drm_client_remove_object(obj: *mut DrmI915GemObject) {
    unsafe {
        let client = AtomicPtr::from_ptr(core::ptr::addr_of_mut!((*obj).client))
            .swap(core::ptr::null_mut(), Ordering::AcqRel);
        if client.is_null() {
            return;
        }
        let mut flags = 0;
        locks::spin_lock_irqsave_raw(
            core::ptr::addr_of_mut!((*client_view(client)).objects_lock),
            &mut flags,
        );
        crate::linux::list::list_del_rcu(core::ptr::addr_of_mut!((*obj).client_link));
        locks::spin_unlock_irqrestore_raw(
            core::ptr::addr_of_mut!((*client_view(client)).objects_lock),
            flags,
        );
        i915_drm_client_put(client);
    }
}

// upstream: i915_drm_client.c i915_drm_client_add_context_objects()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_drm_client_add_context_objects(
    client: *mut I915DrmClient,
    ce: *mut IntelContext,
) {
    unsafe {
        if !(*ce).state.is_null() {
            i915_drm_client_add_object(client, (*(*ce).state).obj);
        }
        if (*ce).ring != (*(*ce).engine).legacy.ring && !(*(*ce).ring).vma.is_null() {
            i915_drm_client_add_object(client, (*(*(*ce).ring).vma).obj);
        }
    }
}
