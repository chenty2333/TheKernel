// SPDX-License-Identifier: MIT
// Copyright (c) 2008 Intel Corporation.
//! Linux 7.2.3 i915_gpu_error.c capture/store/refcount entry points. The debugfs
//! formatter/compression framework maps to native retained GuC capture records;
//! it does not claim a Linux display snapshot or a VMA compression stream.
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
use core::{
    ffi::c_void,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::{
    intel_engine_cs_upstream::ListHead,
    intel_gt_types_upstream::IntelGt,
    intel_guc_capture_upstream::{
        IntelGucStateCapture, ParsedOutput, guc_capture_add_node_to_cachelist,
    },
    linux::{
        list::{INIT_LIST_HEAD, list_add_tail, list_del, list_empty},
        memory::{kfree, kmalloc_obj},
    },
    linux_config::{ENOMEM, GFP_KERNEL},
    linux_i915_private::DrmI915Private,
};
/// Original native storage adapter for GuC capture data. Records retain their
/// preallocated backing lists; none is replaced by a fabricated register value.
#[repr(C)]
pub struct GpuCoredump {
    refs: AtomicUsize,
    pub i915: *mut DrmI915Private,
    pub engine_mask: u32,
    pub dump_flags: u32,
    pub simulated: bool,
    pub reset_flags: u64,
    pub timestamp_ns: i64,
    pub nodes: ListHead,
    capture: *mut IntelGucStateCapture,
}
static CAPTURE_LOCK: spin::Mutex<()> = spin::Mutex::new(());
unsafe fn first_error(i915: *mut DrmI915Private) -> &'static AtomicPtr<c_void> {
    AtomicPtr::from_ptr(&mut (*i915).gpu_error.first_error)
}
fn is_err<T>(p: *mut T) -> bool {
    (p as usize) >= usize::MAX - 4094
}
/// Native GT-only capture backend. The only caller registered by this source
/// module is the GuC capture path; copying its real records needs no MMIO guess
/// and does not install an IRQ, formatter or submission path.
unsafe fn __i915_gpu_coredump(
    gt: *mut IntelGt,
    engine_mask: u32,
    dump_flags: u32,
) -> *mut GpuCoredump {
    let i915 = (*gt).i915;
    let first = first_error(i915).load(Ordering::Acquire);
    if is_err(first) {
        return first.cast();
    }
    let error = kmalloc_obj::<GpuCoredump>(GFP_KERNEL);
    if error.is_null() {
        return (-ENOMEM as isize) as *mut _;
    }
    let capture = (*gt).uc.guc.capture;
    error.write(GpuCoredump {
        refs: AtomicUsize::new(1),
        i915,
        engine_mask,
        dump_flags,
        simulated: false,
        reset_flags: (*gt).reset.flags,
        timestamp_ns: crate::linux::primitives::ktime_get(),
        nodes: unsafe { core::mem::zeroed() },
        capture,
    });
    INIT_LIST_HEAD(&mut (*error).nodes);
    if !capture.is_null() {
        let head = &mut (*capture).outlist as *mut ListHead;
        let mut link = (*head).next;
        while link != head {
            let next = (*link).next;
            let node = link.cast::<ParsedOutput>();
            if (*node).eng_class > crate::intel_guc_fwif_types_upstream::GUC_LAST_ENGINE_CLASS {
                link = next;
                continue;
            }
            let class = crate::intel_guc_fwif_types_upstream::guc_class_to_engine_class(
                (*node).eng_class as u8,
            );
            if (class as usize) < (*gt).engine_class.len()
                && ((*node).eng_inst as usize) < (*gt).engine_class[class as usize].len()
            {
                let engine = (*gt).engine_class[class as usize][(*node).eng_inst as usize];
                if !engine.is_null() && (*engine).mask & engine_mask != 0 {
                    list_del(link);
                    list_add_tail(link, &mut (*error).nodes);
                }
            }
            link = next;
        }
    }
    error
}
// upstream: i915_gpu_error.c i915_gpu_coredump()
unsafe fn i915_gpu_coredump(
    gt: *mut IntelGt,
    engine_mask: u32,
    dump_flags: u32,
) -> *mut GpuCoredump {
    let guard = CAPTURE_LOCK.lock();
    let dump = __i915_gpu_coredump(gt, engine_mask, dump_flags);
    drop(guard);
    dump
}
/// Native kref storage for the source dump get/put ownership edges.
unsafe fn i915_gpu_coredump_get(error: *mut GpuCoredump) {
    let old = (*error).refs.fetch_add(1, Ordering::Relaxed);
    assert!(old != 0 && old < usize::MAX);
}
unsafe fn i915_gpu_coredump_put(error: *mut GpuCoredump) {
    if error.is_null() || is_err(error) {
        return;
    }
    if (*error).refs.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    while !list_empty(&(*error).nodes) {
        let node = (*error).nodes.next.cast::<ParsedOutput>();
        list_del(&mut (*node).link);
        guc_capture_add_node_to_cachelist((*error).capture, node);
    }
    kfree(error);
}
// upstream: i915_gpu_error.c i915_error_state_store()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_error_state_store(error: *mut GpuCoredump) {
    if error.is_null() || is_err(error) {
        return;
    }
    let i915 = (*error).i915;
    axlog::info!(
        "GPU error capture: engines={:x} flags={:x} timestamp={}",
        (*error).engine_mask,
        (*error).dump_flags,
        (*error).timestamp_ns
    );
    if (*error).simulated
        || first_error(i915)
            .compare_exchange(
                core::ptr::null_mut(),
                error.cast(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
    {
        return;
    }
    i915_gpu_coredump_get(error);
    axlog::info!("GPU error state retained by native GT owner");
}
// upstream: i915_gpu_error.c i915_capture_error_state()
pub unsafe fn i915_capture_error_state(gt: *mut IntelGt, engine_mask: u32, dump_flags: u32) {
    let error = i915_gpu_coredump(gt, engine_mask, dump_flags);
    if is_err(error) {
        let _ = first_error((*gt).i915).compare_exchange(
            core::ptr::null_mut(),
            error.cast(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        return;
    }
    i915_error_state_store(error);
    i915_gpu_coredump_put(error);
}
// upstream: i915_gpu_error.c i915_reset_error_state()
pub unsafe fn i915_reset_error_state(i915: *mut DrmI915Private) {
    crate::linux::locks::spin_lock(&mut (*i915).gpu_error.lock);
    let error = first_error(i915).load(Ordering::Acquire);
    if error as isize != -(crate::linux_config::ENODEV as isize) {
        first_error(i915).store(core::ptr::null_mut(), Ordering::Release);
    }
    crate::linux::locks::spin_unlock(&mut (*i915).gpu_error.lock);
    if !error.is_null() && !is_err(error) {
        i915_gpu_coredump_put(error.cast());
    }
}
// upstream: i915_gpu_error.c i915_disable_error_state()
pub unsafe fn i915_disable_error_state(i915: *mut DrmI915Private, err: i32) {
    crate::linux::locks::spin_lock(&mut (*i915).gpu_error.lock);
    if first_error(i915).load(Ordering::Acquire).is_null() {
        first_error(i915).store((err as isize) as *mut c_void, Ordering::Release);
    }
    crate::linux::locks::spin_unlock(&mut (*i915).gpu_error.lock);
}
