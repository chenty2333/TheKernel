// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! TEMPORARY C-ABI definitions for kernel/kthread.c worker API (kthread batch). Each symbol is defined once
//! here; delete the entry when its owner lands. Replaced by kthread_upstream.rs when the kthread batch lands; work items are recorded, not run.
//!
//! Symbols: kthread_queue_work, kthread_cancel_work_sync, kthread_flush_work, kthread_flush_worker, kthread_destroy_worker, kthread_run_worker, sched_set_fifo_worker.

#![allow(non_snake_case, dead_code, unsafe_code)]
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_char, c_void};
use core::sync::atomic::Ordering;
use crate::kernel_placeholder_types::KthreadWork;
use crate::pending_stubs::test_records::{QUEUED, QUEUED_COUNT, RECORD_SLOTS};

// upstream: kernel/kthread.c (the worker token is the worker identity)
static WORKER_TOKEN: u8 = 0;

// upstream: kernel/kthread.c kthread_queue_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kthread_queue_work(_worker: *mut c_void, work: *mut KthreadWork) -> bool {
    let n = QUEUED_COUNT.fetch_add(1, Ordering::SeqCst);
    if n < RECORD_SLOTS {
        QUEUED[n].store(work as u64, Ordering::SeqCst);
    }
    true
}

// upstream: kernel/kthread.c kthread_cancel_work_sync()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kthread_cancel_work_sync(_work: *mut c_void) -> bool {
    false
}

// upstream: kernel/kthread.c kthread_flush_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kthread_flush_work(_work: *mut c_void) {}

// upstream: kernel/kthread.c kthread_flush_worker()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kthread_flush_worker(_worker: *mut c_void) {}

// upstream: kernel/kthread.c kthread_destroy_worker()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kthread_destroy_worker(_worker: *mut c_void) {}

// upstream: kernel/kthread.c kthread_run_worker(flags, namefmt, ...)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kthread_run_worker(_flags: u32, _namefmt: *const c_char, _primary_index: i32, _pipe: u32) -> *mut c_void {
    core::ptr::addr_of!(WORKER_TOKEN) as *mut c_void
}

// upstream: kernel/sched/core.c sched_set_fifo(worker->task)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sched_set_fifo_worker(_worker: *mut c_void) {}
