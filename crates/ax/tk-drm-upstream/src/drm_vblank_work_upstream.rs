// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_vblank_work.c` (batch B1c).
//!
//! Vblank-scheduled work: work items queued on a CRTC's kthread worker that
//! run once the vblank counter reaches a target count. Functions are named
//! and ordered as in the C file.

// The Linux ports state the invariant of each C-ABI call once in the module
// header, as the GT ports do; per-block SAFETY prose is not repeated.
#![allow(non_snake_case, dead_code, unsafe_code)]

use core::ffi::{c_char, c_void};

use tk_linux_kapi::config::IS_ERR;
use tk_linux_kapi::list::{INIT_LIST_HEAD, list_add_tail, list_del_init, list_empty};
use tk_linux_kapi::locks::{
    spin_lock_irq_raw, spin_lock_irqsave_raw, spin_lock_raw, spin_unlock_irq_raw,
    spin_unlock_irqrestore_raw, spin_unlock_raw,
};
use tk_linux_kapi::types::ListHead;
use tk_linux_kapi::wait::{init_waitqueue_head, wake_up_all};

use crate::drm_crtc_types::DrmCrtc;
use crate::drm_device_types::DrmDevice;
use crate::drm_vblank_types::DrmVblankCrtc;
use crate::drm_vblank_work_types::DrmVblankWork;
use crate::kernel_placeholder_types::KthreadWork;

unsafe extern "C" {
    // upstream: kernel/kthread.c kthread_queue_work()
    fn kthread_queue_work(worker: *mut c_void, work: *mut KthreadWork) -> bool;
    // upstream: kernel/kthread.c kthread_cancel_work_sync()
    fn kthread_cancel_work_sync(work: *mut KthreadWork) -> bool;
    // upstream: kernel/kthread.c kthread_flush_work()
    fn kthread_flush_work(work: *mut KthreadWork);
    // upstream: kernel/kthread.c kthread_flush_worker()
    fn kthread_flush_worker(worker: *mut c_void);
    // upstream: kernel/kthread.c kthread_run_worker(flags, namefmt, ...)
    fn kthread_run_worker(flags: u32, namefmt: *const c_char, primary_index: i32, pipe: u32) -> *mut c_void;
    // upstream: kernel/sched/core.c sched_set_fifo(worker->task)
    fn sched_set_fifo_worker(worker: *mut c_void);
}

use crate::drm_vblank_upstream::drm_crtc_vblank_crtc;

// upstream: kernel/kthread.c kthread_init_work(); `struct kthread_work` is
// { node (list_head), func, worker, canceling } on x86_64.
unsafe fn kthread_init_work(work: *mut KthreadWork, func: unsafe extern "C" fn(*mut KthreadWork)) {
    unsafe {
        let base = work as *mut u8;
        INIT_LIST_HEAD(base as *mut ListHead);
        *(base.add(16) as *mut Option<unsafe extern "C" fn(*mut KthreadWork)>) = Some(func);
        *(base.add(24) as *mut *mut c_void) = core::ptr::null_mut();
        *(base.add(32) as *mut i32) = 0;
    }
}

/// Drops the vblank reference and queues every pending work whose target
/// count has been reached. Called with `dev->event_lock` held.
// upstream: drm_vblank_work.c drm_handle_vblank_works()
pub unsafe fn drm_handle_vblank_works(vblank: *mut DrmVblankCrtc) {
    unsafe {
        let count = (*vblank).count.counter as u64;
        let mut wake = false;

        let dev = (*vblank).dev;
        let head: *mut ListHead = core::ptr::addr_of_mut!((*vblank).pending_work);
        let mut cur = (*head).next;
        while cur != head {
            let next = (*cur).next;
            let work = (cur as *mut u8).sub(core::mem::offset_of!(DrmVblankWork, node)) as *mut DrmVblankWork;
            if !crate::drm_vblank_upstream::drm_vblank_passed(count, (*work).count) {
                cur = next;
                continue;
            }

            list_del_init(core::ptr::addr_of_mut!((*work).node));
            crate::drm_vblank_upstream::drm_vblank_put(dev, (*vblank).pipe);
            kthread_queue_work((*vblank).worker, core::ptr::addr_of_mut!((*work).base));
            wake = true;
            cur = next;
        }

        if wake {
            wake_up_all(&mut (*vblank).work_wait_queue);
        }
    }
}

/// Removes all pending works without running them. Called with
/// `dev->event_lock` held.
// upstream: drm_vblank_work.c drm_vblank_cancel_pending_works()
pub unsafe fn drm_vblank_cancel_pending_works(vblank: *mut DrmVblankCrtc) {
    unsafe {
        let dev = (*vblank).dev;
        let head: *mut ListHead = core::ptr::addr_of_mut!((*vblank).pending_work);

        // drm_WARN_ONCE(dev, !list_empty(&vblank->pending_work), "Cancelling pending vblank works!\n")
        tk_linux_kapi::assert::warn_on(!list_empty(&*head));

        let mut cur = (*head).next;
        while cur != head {
            let next = (*cur).next;
            let work = (cur as *mut u8).sub(core::mem::offset_of!(DrmVblankWork, node)) as *mut DrmVblankWork;
            list_del_init(core::ptr::addr_of_mut!((*work).node));
            crate::drm_vblank_upstream::drm_vblank_put(dev, (*vblank).pipe);
            cur = next;
        }

        wake_up_all(&mut (*vblank).work_wait_queue);
    }
}

/// Schedules `work` to run once the vblank count reaches `count`.
/// Returns a negative errno on failure, otherwise nonzero when scheduled.
// upstream: drm_vblank_work.c drm_vblank_work_schedule()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vblank_work_schedule(work: *mut DrmVblankWork, count: u64, nextonmiss: bool) -> i32 {
    unsafe {
        let vblank = (*work).vblank;
        let dev = (*vblank).dev;
        let mut irqflags: core::ffi::c_ulong = 0;
        let mut rescheduling = false;
        let mut wake = false;
        let mut ret: i32 = 0;

        spin_lock_irqsave_raw(core::ptr::addr_of_mut!((*dev).event_lock), &mut irqflags);
        'out: {
            if (*work).cancelling != 0 {
                break 'out;
            }

            spin_lock_raw(core::ptr::addr_of_mut!((*dev).vbl_lock));
            let inmodeset = (*vblank).inmodeset;
            spin_unlock_raw(core::ptr::addr_of_mut!((*dev).vbl_lock));
            if inmodeset != 0 {
                break 'out;
            }

            if list_empty(&(*work).node) {
                ret = crate::drm_vblank_upstream::drm_vblank_get(dev, (*vblank).pipe);
                if ret < 0 {
                    break 'out;
                }
            } else if (*work).count == count {
                // Already scheduled w/ same vbl count
                break 'out;
            } else {
                rescheduling = true;
            }

            (*work).count = count;
            let cur_vbl = crate::drm_vblank_upstream::drm_vblank_count(dev, (*vblank).pipe);
            let passed = crate::drm_vblank_upstream::drm_vblank_passed(cur_vbl, count);
            // drm_dbg_core(): debug print, dropped.

            if !nextonmiss && passed {
                crate::drm_vblank_upstream::drm_vblank_put(dev, (*vblank).pipe);
                ret = kthread_queue_work((*vblank).worker, core::ptr::addr_of_mut!((*work).base)) as i32;
                if rescheduling {
                    list_del_init(core::ptr::addr_of_mut!((*work).node));
                    wake = true;
                }
            } else {
                if !rescheduling {
                    list_add_tail(
                        core::ptr::addr_of_mut!((*work).node),
                        core::ptr::addr_of_mut!((*vblank).pending_work),
                    );
                }
                ret = 1; // true
            }
        }
        // out:
        spin_unlock_irqrestore_raw(core::ptr::addr_of_mut!((*dev).event_lock), irqflags);
        if wake {
            wake_up_all(&mut (*vblank).work_wait_queue);
        }
        ret
    }
}

/// Cancels a vblank work item, waiting for a running instance to finish.
/// Returns true if the work was pending or running.
// upstream: drm_vblank_work.c drm_vblank_work_cancel_sync()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vblank_work_cancel_sync(work: *mut DrmVblankWork) -> bool {
    unsafe {
        let vblank = (*work).vblank;
        let dev = (*vblank).dev;
        let mut ret = false;

        spin_lock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));
        if !list_empty(&(*work).node) {
            list_del_init(core::ptr::addr_of_mut!((*work).node));
            crate::drm_vblank_upstream::drm_vblank_put(vblank_dev(vblank), (*vblank).pipe);
            ret = true;
        }
        (*work).cancelling += 1;
        spin_unlock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));

        wake_up_all(&mut (*vblank).work_wait_queue);

        if kthread_cancel_work_sync(core::ptr::addr_of_mut!((*work).base)) {
            ret = true;
        }

        spin_lock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));
        (*work).cancelling -= 1;
        spin_unlock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));

        ret
    }
}

#[inline]
unsafe fn vblank_dev(vblank: *mut DrmVblankCrtc) -> *mut DrmDevice {
    unsafe { (*vblank).dev }
}

/// Waits until the work is no longer pending, then flushes the kthread work.
// upstream: drm_vblank_work.c drm_vblank_work_flush()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vblank_work_flush(work: *mut DrmVblankWork) {
    unsafe {
        let vblank = (*work).vblank;
        let dev = (*vblank).dev;

        spin_lock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));
        // wait_event_lock_irq(vblank->work_wait_queue, list_empty(&work->node), dev->event_lock)
        while !list_empty(&(*work).node) {
            spin_unlock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));
            tk_linux_kapi::wait::cond_resched();
            spin_lock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));
        }
        spin_unlock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));

        kthread_flush_work(core::ptr::addr_of_mut!((*work).base));
    }
}

/// Waits until no vblank work is pending on the CRTC, then flushes its worker.
// upstream: drm_vblank_work.c drm_vblank_work_flush_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vblank_work_flush_all(crtc: *mut DrmCrtc) {
    unsafe {
        let dev = (*crtc).dev;
        let vblank = drm_crtc_vblank_crtc(crtc);

        spin_lock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));
        // wait_event_lock_irq(vblank->work_wait_queue, list_empty(&vblank->pending_work), dev->event_lock)
        while !list_empty(&(*vblank).pending_work) {
            spin_unlock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));
            tk_linux_kapi::wait::cond_resched();
            spin_lock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));
        }
        spin_unlock_irq_raw(core::ptr::addr_of_mut!((*dev).event_lock));

        kthread_flush_worker((*vblank).worker);
    }
}

/// Initializes a vblank work item for `crtc`, running `func` on the worker.
// upstream: drm_vblank_work.c drm_vblank_work_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_vblank_work_init(
    work: *mut DrmVblankWork,
    crtc: *mut DrmCrtc,
    func: unsafe extern "C" fn(*mut KthreadWork),
) {
    unsafe {
        kthread_init_work(core::ptr::addr_of_mut!((*work).base), func);
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*work).node));
        (*work).vblank = drm_crtc_vblank_crtc(crtc);
    }
}

/// Creates the per-CRTC vblank worker thread (SCHED_FIFO).
// upstream: drm_vblank_work.c drm_vblank_worker_init()
pub unsafe fn drm_vblank_worker_init(vblank: *mut DrmVblankCrtc) -> i32 {
    unsafe {
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*vblank).pending_work));
        init_waitqueue_head(&mut (*vblank).work_wait_queue);

        // kthread_run_worker(0, "card%d-crtc%d", dev->primary->index, pipe)
        let primary_index = (*(*(*vblank).dev).primary).index as i32;
        let worker = kthread_run_worker(0, c"card%d-crtc%d".as_ptr(), primary_index, (*vblank).pipe);
        if IS_ERR(worker as *const c_void) {
            return tk_linux_kapi::config::PTR_ERR(worker as *const c_void);
        }
        (*vblank).worker = worker;
        sched_set_fifo_worker(worker);
        0
    }
}
