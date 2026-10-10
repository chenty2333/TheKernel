// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Asynchronous Linux `work_struct` dispatch for the i915 LinuxKPI boundary.
//!
//! This adapter provides one persistent task-context worker and an intrusive
//! FIFO. Producers only publish a pending work item under an IRQ-safe lock;
//! callbacks are always invoked later by the worker, never inline. A single
//! worker is intentionally conservative: all workqueue handles currently feed
//! the same FIFO, so this preserves non-concurrent execution of a work item but
//! does not model Linux workqueue priority or per-queue parallelism.
//!
//! The executor must be initialized from blockable task context before any
//! work can be queued. Delayed work is not implemented: this kernel has no
//! Linux timer-wheel binding, and substituting an immediate callback would
//! violate `mod_delayed_work()` semantics.

#![allow(unsafe_code)]
#![allow(non_snake_case)]

use alloc::string::String;
use core::{
    ffi::c_void,
    fmt::Write as _,
    hint::spin_loop,
    ops::{Deref, DerefMut},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering},
};

use kernel_guard::NoPreemptIrqSave;

use crate::{
    intel_engine_cs_upstream::{DelayedWork, ListHead, TimerList, WorkStruct},
    linux_timer::{del_timer, del_timer_sync, mod_timer, timer_setup},
};

const WORK_STRUCT_PENDING_BIT: u32 = 0;

/// Linux `work_pending()` tests WORK_STRUCT_PENDING_BIT with READ_ONCE.
#[inline]
pub unsafe fn work_pending(work: *const WorkStruct) -> bool {
    let data = unsafe { ptr::read_volatile(ptr::addr_of!((*work).data)) };
    data & (1u64 << WORK_STRUCT_PENDING_BIT) != 0
}

const WORKER_UNINITIALIZED: u8 = 0;
const WORKER_STARTING: u8 = 1;
const WORKER_READY: u8 = 2;

// Linux's WORK_STRUCT_PENDING_BIT is bit 0. With CONFIG_DEBUG_OBJECTS_WORK
// unset and 64-bit unsigned long, the Linux 7.2.3 WORK_DATA_INIT() off-queue
// sentinel is ((1 << 31) - 1) << 21.
const WORK_PENDING: usize = 1 << 0;
const WORK_DATA_INIT: usize = ((1usize << 31) - 1) << 21;

const _: [(); core::mem::size_of::<usize>()] = [(); core::mem::size_of::<core::ffi::c_ulong>()];
const _: [(); 32] = [(); core::mem::size_of::<WorkStruct>()];
const _: [(); 8] = [(); core::mem::align_of::<WorkStruct>()];
const _: [(); 0] = [(); core::mem::offset_of!(WorkStruct, data)];
const _: [(); 8] = [(); core::mem::offset_of!(WorkStruct, entry)];
const _: [(); 24] = [(); core::mem::offset_of!(WorkStruct, function)];
const _: [(); 40] = [(); core::mem::size_of::<TimerList>()];
const _: [(); 8] = [(); core::mem::align_of::<TimerList>()];
const _: [(); 0] = [(); core::mem::offset_of!(TimerList, entry)];
const _: [(); 16] = [(); core::mem::offset_of!(TimerList, expires)];
const _: [(); 24] = [(); core::mem::offset_of!(TimerList, function)];
const _: [(); 32] = [(); core::mem::offset_of!(TimerList, flags)];
const _: [(); 88] = [(); core::mem::size_of::<DelayedWork>()];
const _: [(); 32] = [(); core::mem::offset_of!(DelayedWork, timer)];
const _: [(); 72] = [(); core::mem::offset_of!(DelayedWork, workqueue)];
const _: [(); 80] = [(); core::mem::offset_of!(DelayedWork, cpu)];

/// Linux `workqueue_struct` is an opaque handle here. Queue identity is
/// retained at the API boundary, while the current runtime funnels work into
/// the one ordered worker described above.
#[repr(C)]
pub struct WorkqueueStruct {
    _identity: u8,
}

static SYSTEM_DEFAULT_WORKQUEUE: WorkqueueStruct = WorkqueueStruct { _identity: 0 };
static SYSTEM_HIGHPRI_WORKQUEUE: WorkqueueStruct = WorkqueueStruct { _identity: 1 };

#[allow(non_upper_case_globals)]
pub const system_dfl_wq: *mut WorkqueueStruct =
    core::ptr::addr_of!(SYSTEM_DEFAULT_WORKQUEUE).cast_mut();
#[allow(non_upper_case_globals)]
pub const system_highpri_wq: *mut WorkqueueStruct =
    core::ptr::addr_of!(SYSTEM_HIGHPRI_WORKQUEUE).cast_mut();

/// Failure from an operation which needs the asynchronous work runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkqueueError {
    /// No task-context worker could be published.
    WorkerUnavailable,
    /// A blocking operation was attempted from a context that cannot block.
    ContextCannotBlock,
    /// The task runtime refused to register or retain a worker wait.
    WaitFailed,
    /// The caller supplied a null work item or workqueue handle.
    NullPointer,
    /// The work item was queued before `INIT_WORK` installed its callback.
    WorkNotInitialized,
}

/// Conversion for translated C workqueue-pointer arguments.
pub trait WorkqueuePtr {
    fn workqueue_ptr(self) -> *const c_void;
}

impl<T> WorkqueuePtr for *mut T {
    fn workqueue_ptr(self) -> *const c_void {
        self.cast()
    }
}

impl<T> WorkqueuePtr for *const T {
    fn workqueue_ptr(self) -> *const c_void {
        self.cast()
    }
}

/// Conversion for the reference and pointer forms used by source-order i915
/// workqueue helpers. Queueing retains the pointer asynchronously, so callers
/// must keep the backing work item alive until it is flushed or cancelled.
pub trait WorkStructPtr {
    fn work_struct_ptr(self) -> *mut WorkStruct;
}

impl WorkStructPtr for *mut WorkStruct {
    fn work_struct_ptr(self) -> *mut WorkStruct {
        self
    }
}

impl WorkStructPtr for *const WorkStruct {
    fn work_struct_ptr(self) -> *mut WorkStruct {
        self.cast_mut()
    }
}

impl WorkStructPtr for &mut WorkStruct {
    fn work_struct_ptr(self) -> *mut WorkStruct {
        self
    }
}

impl WorkStructPtr for &WorkStruct {
    fn work_struct_ptr(self) -> *mut WorkStruct {
        (self as *const WorkStruct).cast_mut()
    }
}

pub trait DelayedWorkPtr {
    fn delayed_work_ptr(self) -> *mut DelayedWork;
}
impl DelayedWorkPtr for *mut DelayedWork {
    fn delayed_work_ptr(self) -> *mut DelayedWork {
        self
    }
}
impl DelayedWorkPtr for *const DelayedWork {
    fn delayed_work_ptr(self) -> *mut DelayedWork {
        self.cast_mut()
    }
}
impl DelayedWorkPtr for &mut DelayedWork {
    fn delayed_work_ptr(self) -> *mut DelayedWork {
        self
    }
}
impl DelayedWorkPtr for &DelayedWork {
    fn delayed_work_ptr(self) -> *mut DelayedWork {
        self as *const DelayedWork as *mut DelayedWork
    }
}

unsafe fn delayed_workqueue_atomic<'a>(work: *mut DelayedWork) -> &'a AtomicPtr<c_void> {
    assert!(!work.is_null());
    let slot = unsafe { ptr::addr_of_mut!((*work).workqueue) };
    // SAFETY: `DelayedWork.workqueue` is naturally aligned pointer storage;
    // all accesses by this compatibility layer use this atomic view.
    unsafe { AtomicPtr::from_ptr(slot) }
}

struct WorkList {
    head: *mut ListHead,
    tail: *mut ListHead,
}

impl WorkList {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
        }
    }

    /// The caller holds `WORK_QUEUE` and the node is self-linked.
    unsafe fn push(&mut self, work: *mut WorkStruct) {
        let node = unsafe { ptr::addr_of_mut!((*work).entry) };
        unsafe {
            assert!(core::ptr::eq((*node).next, node));
            assert!(core::ptr::eq((*node).prev, node));
        }
        if self.tail.is_null() {
            self.head = node;
            self.tail = node;
            unsafe {
                (*node).next = node;
                (*node).prev = node;
            }
        } else {
            unsafe {
                (*node).next = self.head;
                (*node).prev = self.tail;
                (*self.tail).next = node;
                (*self.head).prev = node;
            }
            self.tail = node;
        }
    }

    /// Remove and return the FIFO head. The caller holds `WORK_QUEUE`.
    unsafe fn pop(&mut self) -> *mut WorkStruct {
        let node = self.head;
        if node.is_null() {
            return ptr::null_mut();
        }

        let next = unsafe { (*node).next };
        if next == node {
            self.head = ptr::null_mut();
            self.tail = ptr::null_mut();
        } else {
            let previous = unsafe { (*node).prev };
            self.head = next;
            unsafe {
                (*next).prev = previous;
                (*previous).next = next;
            }
        }
        unsafe {
            (*node).next = node;
            (*node).prev = node;
        }
        unsafe { work_from_entry(node) }
    }

    /// Remove a pending item. The caller holds `WORK_QUEUE`.
    unsafe fn remove(&mut self, work: *mut WorkStruct) -> bool {
        let node = unsafe { ptr::addr_of_mut!((*work).entry) };
        let next = unsafe { (*node).next };
        if self.head.is_null() || (next == node && self.head != node) {
            return false;
        }
        let previous = unsafe { (*node).prev };
        if next == node {
            self.head = ptr::null_mut();
            self.tail = ptr::null_mut();
        } else {
            unsafe {
                (*previous).next = next;
                (*next).prev = previous;
            }
            if self.head == node {
                self.head = next;
            }
            if self.tail == node {
                self.tail = previous;
            }
        }
        unsafe {
            (*node).next = node;
            (*node).prev = node;
        }
        true
    }
}

unsafe fn work_from_entry(node: *mut ListHead) -> *mut WorkStruct {
    unsafe {
        node.cast::<u8>()
            .sub(core::mem::offset_of!(WorkStruct, entry))
            .cast()
    }
}

// Queue pointers are accessed only while WORK_QUEUE is locked, and the caller
// owns each work item through its flush/cancel lifetime.
unsafe impl Send for WorkList {}

struct IrqSpinLock<T> {
    held: AtomicBool,
    data: core::cell::UnsafeCell<T>,
}

unsafe impl<T: Send> Send for IrqSpinLock<T> {}
unsafe impl<T: Send> Sync for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    const fn new(data: T) -> Self {
        Self {
            held: AtomicBool::new(false),
            data: core::cell::UnsafeCell::new(data),
        }
    }

    fn lock(&self) -> IrqSpinGuard<'_, T> {
        let irq_guard = NoPreemptIrqSave::new();
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.held.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
        IrqSpinGuard {
            lock: self,
            _irq_guard: irq_guard,
        }
    }
}

struct IrqSpinGuard<'a, T> {
    lock: &'a IrqSpinLock<T>,
    _irq_guard: NoPreemptIrqSave,
}

impl<T> Deref for IrqSpinGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqSpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqSpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}

static WORK_QUEUE: IrqSpinLock<WorkList> = IrqSpinLock::new(WorkList::new());
static WORKER_STATE: AtomicU8 = AtomicU8::new(WORKER_UNINITIALIZED);
static WORK_GENERATION: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_WORK: AtomicPtr<WorkStruct> = AtomicPtr::new(ptr::null_mut());
static WORK_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();
static WORK_STATE_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();

fn notify_workers() {
    WORK_GENERATION.fetch_add(1, Ordering::Release);
    WORK_WAKE.notify_all(false);
}

fn make_worker_name() -> Result<String, WorkqueueError> {
    let mut name = String::new();
    name.try_reserve_exact(16)
        .map_err(|_| WorkqueueError::WorkerUnavailable)?;
    name.push_str("i915-workqueue");
    Ok(name)
}

/// Start the persistent asynchronous work worker from blockable task context.
/// This is idempotent and must complete before the first `queue_work()` call.
pub fn init_workqueue_executor() -> Result<(), WorkqueueError> {
    if !axtask::can_block_current() {
        return Err(WorkqueueError::ContextCannotBlock);
    }

    loop {
        match WORKER_STATE.load(Ordering::Acquire) {
            WORKER_READY => return Ok(()),
            WORKER_STARTING => {
                WORK_WAKE
                    .wait_until(|| WORKER_STATE.load(Ordering::Acquire) != WORKER_STARTING)
                    .map_err(|_| WorkqueueError::WaitFailed)?;
            }
            WORKER_UNINITIALIZED => {
                if WORKER_STATE
                    .compare_exchange(
                        WORKER_UNINITIALIZED,
                        WORKER_STARTING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }

                let worker = match make_worker_name().and_then(|name| {
                    axtask::try_spawn_with_name(workqueue_worker, name)
                        .map_err(|_| WorkqueueError::WorkerUnavailable)
                }) {
                    Ok(worker) => worker,
                    Err(error) => {
                        WORKER_STATE.store(WORKER_UNINITIALIZED, Ordering::Release);
                        WORK_WAKE.notify_all(false);
                        return Err(error);
                    }
                };
                // The executor has kernel lifetime; no workqueue shutdown hook
                // exists in this driver boundary.
                core::mem::forget(worker);
                WORKER_STATE.store(WORKER_READY, Ordering::Release);
                WORK_WAKE.notify_all(false);
                return Ok(());
            }
            _ => unreachable!("invalid workqueue worker state"),
        }
    }
}

unsafe fn work_data<'a>(work: *mut WorkStruct) -> &'a AtomicUsize {
    assert!(!work.is_null());
    // Linux declares work_struct.data as atomic_long_t. This x86_64 binding
    // keeps the same storage size and alignment in its C-layout WorkStruct.
    unsafe { &*ptr::addr_of_mut!((*work).data).cast::<AtomicUsize>() }
}

/// Initialize `work_struct` for a non-capturing safe Rust callback taking
/// `&mut WorkStruct`. Function items and non-capturing closures are zero-sized;
/// capturing closures are rejected rather than retaining borrowed state.
pub fn INIT_WORK<F>(work: &mut WorkStruct, function: F)
where
    F: for<'a> Fn(&'a mut WorkStruct) + Copy + Send + 'static,
{
    init_workqueue_executor().expect("failed to bind the LinuxKPI workqueue executor");
    assert_eq!(
        core::mem::size_of::<F>(),
        0,
        "INIT_WORK requires a non-capturing callback"
    );
    let _ = function;
    work.data = WORK_DATA_INIT as core::ffi::c_ulong;
    let entry = ptr::addr_of_mut!(work.entry);
    work.entry.next = entry;
    work.entry.prev = entry;
    work.function = Some(rust_ref_work_trampoline::<F>);
}

unsafe extern "C" fn rust_ref_work_trampoline<F>(work: *mut WorkStruct)
where
    F: for<'a> Fn(&'a mut WorkStruct) + Copy + Send + 'static,
{
    assert!(!work.is_null());
    // The only zero-sized Fn values accepted by INIT_WORK are function items
    // and non-capturing closures, whose value has no runtime storage.
    let function = unsafe { core::mem::MaybeUninit::<F>::zeroed().assume_init() };
    function(unsafe { &mut *work });
}

/// Initialize a work item with an actual C-ABI callback.
pub fn INIT_WORK_C(work: &mut WorkStruct, function: unsafe extern "C" fn(*mut WorkStruct)) {
    init_workqueue_executor().expect("failed to bind the LinuxKPI workqueue executor");
    work.data = WORK_DATA_INIT as core::ffi::c_ulong;
    let entry = ptr::addr_of_mut!(work.entry);
    work.entry.next = entry;
    work.entry.prev = entry;
    work.function = Some(function);
}

unsafe extern "C" fn delayed_work_timer_fn(timer: *mut TimerList) {
    assert!(!timer.is_null());
    let delayed = unsafe {
        timer
            .cast::<u8>()
            .sub(core::mem::offset_of!(DelayedWork, timer))
            .cast::<DelayedWork>()
    };
    let workqueue = unsafe { delayed_workqueue_atomic(delayed) }.load(Ordering::Acquire);
    assert!(!workqueue.is_null(), "delayed work timer has no workqueue");
    unsafe { queue_work(workqueue, ptr::addr_of_mut!((*delayed).work)) };
}

/// Initialize Linux `delayed_work`: work callback plus a timer-list callback.
/// The timer backend is monotonic and asynchronous; delay units are jiffies.
pub fn INIT_DELAYED_WORK<F>(delayed: &mut DelayedWork, function: F)
where
    F: for<'a> Fn(&'a mut WorkStruct) + Copy + Send + 'static,
{
    INIT_WORK(&mut delayed.work, function);
    unsafe { delayed_workqueue_atomic(delayed) }.store(ptr::null_mut(), Ordering::Release);
    delayed.cpu = -1;
    unsafe { timer_setup(&mut delayed.timer, delayed_work_timer_fn, 0) };
}

/// Queue a delayed work item at an absolute jiffies delay.
pub unsafe fn queue_delayed_work<Q: WorkqueuePtr, D: DelayedWorkPtr>(
    workqueue: Q,
    delayed: D,
    delay_jiffies: u64,
) -> bool {
    let workqueue = workqueue.workqueue_ptr();
    let delayed = delayed.delayed_work_ptr();
    assert!(!workqueue.is_null() && !delayed.is_null());
    if work_busy(unsafe { ptr::addr_of_mut!((*delayed).work) }) {
        return false;
    }
    unsafe { delayed_workqueue_atomic(delayed) }.store(workqueue.cast_mut(), Ordering::Release);
    if delay_jiffies == 0 {
        return unsafe { queue_work(workqueue, ptr::addr_of_mut!((*delayed).work)) };
    }
    let expires = crate::linux::primitives::jiffies().wrapping_add(delay_jiffies);
    !unsafe {
        mod_timer(
            ptr::addr_of_mut!((*delayed).timer),
            expires as core::ffi::c_ulong,
        )
    }
}

/// Modify a delayed work deadline. Existing timer or work state is preserved
/// according to Linux's `mod_delayed_work()` contract.
pub unsafe fn mod_delayed_work<Q: WorkqueuePtr, D: DelayedWorkPtr>(
    workqueue: Q,
    delayed: D,
    delay_jiffies: u64,
) -> bool {
    let workqueue = workqueue.workqueue_ptr();
    let delayed = delayed.delayed_work_ptr();
    assert!(!workqueue.is_null() && !delayed.is_null());
    unsafe { delayed_workqueue_atomic(delayed) }.store(workqueue.cast_mut(), Ordering::Release);
    if delay_jiffies == 0 {
        let was_pending = unsafe { del_timer(ptr::addr_of_mut!((*delayed).timer)) };
        return unsafe { queue_work(workqueue, ptr::addr_of_mut!((*delayed).work)) } || was_pending;
    }
    unsafe {
        mod_timer(
            ptr::addr_of_mut!((*delayed).timer),
            crate::linux::primitives::jiffies().wrapping_add(delay_jiffies) as core::ffi::c_ulong,
        )
    }
}

/// Cancel a queued timer and/or work item without waiting for a callback.
pub unsafe fn cancel_delayed_work<D: DelayedWorkPtr>(delayed: D) -> bool {
    let delayed = delayed.delayed_work_ptr();
    assert!(!delayed.is_null());
    let timer_removed = unsafe { del_timer(ptr::addr_of_mut!((*delayed).timer)) };
    let work_removed = unsafe { cancel_work(ptr::addr_of_mut!((*delayed).work)) };
    timer_removed || work_removed
}

/// Synchronously cancel both the timer and any currently executing work.
pub unsafe fn cancel_delayed_work_sync<D: DelayedWorkPtr>(delayed: D) -> bool {
    let delayed = delayed.delayed_work_ptr();
    assert!(!delayed.is_null());
    let timer_removed = unsafe { del_timer_sync(ptr::addr_of_mut!((*delayed).timer)) };
    let work_removed = unsafe { cancel_work_sync(ptr::addr_of_mut!((*delayed).work)) };
    timer_removed || work_removed
}

/// Force delayed work to become immediate, then wait for its callback.
pub unsafe fn flush_delayed_work<D: DelayedWorkPtr>(delayed: D) -> bool {
    let delayed = delayed.delayed_work_ptr();
    assert!(!delayed.is_null());
    let timer_removed = unsafe { del_timer_sync(ptr::addr_of_mut!((*delayed).timer)) };
    if timer_removed {
        let workqueue = unsafe { delayed_workqueue_atomic(delayed) }.load(Ordering::Acquire);
        assert!(!workqueue.is_null());
        let _ = unsafe { queue_work(workqueue, ptr::addr_of_mut!((*delayed).work)) };
    }
    let flushed = unsafe { flush_work(ptr::addr_of_mut!((*delayed).work)) };
    timer_removed || flushed
}

/// Queue one work instance. The callback never executes on this call's stack.
///
/// # Safety
/// The work item must remain allocated and exclusively owned by this workqueue
/// until `flush_work` or `cancel_work_sync` observes it idle. The caller must
/// initialize it first and must not modify its list node or callback while
/// queued/running.
pub unsafe fn queue_work<Q: WorkqueuePtr, W: WorkStructPtr>(workqueue: Q, work: W) -> bool {
    unsafe { try_queue_work(workqueue, work) }
        .unwrap_or_else(|error| panic!("queue_work failed: {error:?}"))
}

/// Fallible implementation of [`queue_work`]. The workqueue handle is opaque;
/// this runtime currently routes all handles to one FIFO.
pub unsafe fn try_queue_work<Q: WorkqueuePtr, W: WorkStructPtr>(
    workqueue: Q,
    work: W,
) -> Result<bool, WorkqueueError> {
    let workqueue = workqueue.workqueue_ptr();
    let work = work.work_struct_ptr();
    if workqueue.is_null() || work.is_null() {
        return Err(WorkqueueError::NullPointer);
    }
    if WORKER_STATE.load(Ordering::Acquire) != WORKER_READY {
        return Err(WorkqueueError::WorkerUnavailable);
    }
    if unsafe { (*work).function }.is_none() {
        return Err(WorkqueueError::WorkNotInitialized);
    }

    {
        let mut queue = WORK_QUEUE.lock();
        let state = unsafe { work_data(work) };
        let old = state.fetch_or(WORK_PENDING, Ordering::AcqRel);
        if old & WORK_PENDING != 0 {
            return Ok(false);
        }
        unsafe { queue.push(work) };
    }
    notify_workers();
    Ok(true)
}

fn pop_work() -> *mut WorkStruct {
    let mut queue = WORK_QUEUE.lock();
    // SAFETY: intrusive links are protected by WORK_QUEUE.
    let work = unsafe { queue.pop() };
    if work.is_null() {
        return work;
    }

    // The pending-to-running transition is made under the same lock used by
    // queue/cancel, so no producer can observe a detached-but-pending item.
    let state = unsafe { work_data(work) };
    let old = state.fetch_and(!WORK_PENDING, Ordering::AcqRel);
    assert!(
        old & WORK_PENDING != 0,
        "workqueue node without pending bit"
    );
    assert!(ACTIVE_WORK.load(Ordering::Relaxed).is_null());
    ACTIVE_WORK.store(work, Ordering::Release);
    work
}

fn work_busy(work: *mut WorkStruct) -> bool {
    let _queue = WORK_QUEUE.lock();
    unsafe { work_data(work) }.load(Ordering::Acquire) & WORK_PENDING != 0
        || ACTIVE_WORK.load(Ordering::Acquire) == work
}

fn worker_wait_for_generation(generation: usize) {
    WORK_WAKE
        .wait_until(|| WORK_GENERATION.load(Ordering::Acquire) != generation)
        .expect("workqueue worker lost its waitable task context");
}

fn workqueue_worker() {
    WORK_WAKE
        .wait_until(|| WORKER_STATE.load(Ordering::Acquire) == WORKER_READY)
        .expect("workqueue worker could not wait for executor publication");

    loop {
        let generation = WORK_GENERATION.load(Ordering::Acquire);
        let work = pop_work();
        if work.is_null() {
            if WORK_GENERATION.load(Ordering::Acquire) == generation {
                worker_wait_for_generation(generation);
            }
            continue;
        }

        let callback =
            unsafe { (*work).function }.expect("queued work item has no initialized callback");
        unsafe { callback(work) };

        {
            let _queue = WORK_QUEUE.lock();
            let active = ACTIVE_WORK.swap(ptr::null_mut(), Ordering::AcqRel);
            assert_eq!(active, work, "work callback completed without ownership");
        }
        WORK_STATE_WAKE.notify_all(false);
    }
}

/// Non-synchronously cancel a queued work instance.
///
/// # Safety
/// `work` must be a live initialized work item. The caller must retain it
/// until any executing instance has completed.
pub unsafe fn cancel_work<W: WorkStructPtr>(work: W) -> bool {
    let work = work.work_struct_ptr();
    assert!(!work.is_null(), "cancel_work called with null work");

    let removed = {
        let mut queue = WORK_QUEUE.lock();
        let state = unsafe { work_data(work) };
        let old = state.fetch_and(!WORK_PENDING, Ordering::AcqRel);
        if old & WORK_PENDING == 0 {
            false
        } else {
            // SAFETY: queue lock serializes cancellation with enqueue/dequeue.
            assert!(
                unsafe { queue.remove(work) },
                "pending work missing from queue"
            );
            true
        }
    };
    if removed {
        WORK_STATE_WAKE.notify_all(false);
        notify_workers();
    }
    removed
}

/// Wait for all work queued before or during the active callback to finish.
///
/// # Safety
/// `work` must remain live and initialized through the wait. The caller must
/// be in task context and must not wait for itself from its own callback.
pub unsafe fn flush_work<W: WorkStructPtr>(work: W) -> bool {
    unsafe { try_flush_work(work) }.unwrap_or_else(|error| panic!("flush_work failed: {error:?}"))
}

/// Fallible form of [`flush_work`].
pub unsafe fn try_flush_work<W: WorkStructPtr>(work: W) -> Result<bool, WorkqueueError> {
    let work = work.work_struct_ptr();
    if work.is_null() {
        return Err(WorkqueueError::NullPointer);
    }
    if !axtask::can_block_current() {
        return Err(WorkqueueError::ContextCannotBlock);
    }
    if !work_busy(work) {
        return Ok(false);
    }
    WORK_STATE_WAKE
        .wait_until(|| !work_busy(work))
        .map_err(|_| WorkqueueError::WaitFailed)?;
    Ok(true)
}

/// Cancel pending work and wait for an already executing callback to finish.
///
/// # Safety
/// `work` must remain live and initialized through the cancellation. The
/// caller must be in task context and must not cancel itself synchronously.
pub unsafe fn cancel_work_sync<W: WorkStructPtr>(work: W) -> bool {
    let work = work.work_struct_ptr();
    assert!(!work.is_null(), "cancel_work_sync called with null work");
    if !axtask::can_block_current() {
        panic!("cancel_work_sync requires blockable task context");
    }

    let cancelled = unsafe { cancel_work(work) };
    let was_active = work_busy(work);
    if was_active {
        WORK_STATE_WAKE
            .wait_until(|| !work_busy(work))
            .expect("cancel_work_sync could not wait for the worker");
    }
    cancelled || was_active
}

/// Set the PENDING bit for a caller that queues the item later (RCU-deferred
/// queueing). Returns false when the item was already pending.
///
/// # Safety
/// `work` must be a live, initialized work item.
pub(crate) unsafe fn mark_work_pending(work: *mut WorkStruct) -> bool {
    let old = unsafe { work_data(work) }.fetch_or(WORK_PENDING, Ordering::AcqRel);
    old & WORK_PENDING == 0
}

/// Enqueue a work item whose PENDING bit the caller already set. The pending
/// bit stays set until the worker dequeues the item.
///
/// # Safety
/// `work` must be live, initialized, marked pending and not on the queue.
pub(crate) unsafe fn enqueue_marked_work(work: *mut WorkStruct) {
    {
        let mut queue = WORK_QUEUE.lock();
        unsafe { queue.push(work) };
    }
    notify_workers();
}

/// Block until the work runtime has no queued or running item.
///
/// The runtime is one FIFO served by one worker. Waiting for it to drain
/// covers every item queued before the call, which is the Linux guarantee of
/// `flush_workqueue()` / `drain_workqueue()`; it may additionally wait for items
/// queued after the call.
pub fn flush_all_work() {
    if !axtask::can_block_current() {
        panic!("flush_workqueue requires blockable task context");
    }
    WORK_STATE_WAKE
        .wait_until(|| {
            WORK_QUEUE.lock().head.is_null() && ACTIVE_WORK.load(Ordering::Acquire).is_null()
        })
        .expect("flush_workqueue could not wait for the worker");
}

// ---------------------------------------------------------------------------
// C ABI entry points.
// ---------------------------------------------------------------------------

/// Linux `queue_work()`. Returns false when the item is already pending.
#[unsafe(export_name = "queue_work")]
pub unsafe extern "C" fn c_queue_work(wq: *mut c_void, work: *mut WorkStruct) -> bool {
    unsafe { queue_work(wq, work) }
}

/// Linux `flush_work()`.
#[unsafe(export_name = "flush_work")]
pub unsafe extern "C" fn c_flush_work(work: *mut WorkStruct) -> bool {
    unsafe { flush_work(work) }
}

/// Linux `flush_workqueue()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn flush_workqueue(_wq: *mut c_void) {
    flush_all_work();
}

/// Linux `drain_workqueue()`: flushes until no work remains. Re-queueing
/// after the drain is not rejected, which is the one difference from Linux's
/// `__WQ_DRAINING` warning path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drain_workqueue(_wq: *mut c_void) {
    flush_all_work();
}

/// Linux `alloc_workqueue()`. The format arguments name the queue for debug
/// output; the one ordered worker serves every queue, so the name is not kept.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn alloc_workqueue(
    _fmt: *const core::ffi::c_char,
    _flags: u32,
    _max_active: core::ffi::c_int,
    _args: ...
) -> *mut c_void {
    let queue = alloc::boxed::Box::new(WorkqueueStruct { _identity: 2 });
    alloc::boxed::Box::into_raw(queue).cast()
}

/// Linux `destroy_workqueue()`: drain queued work, then release the handle.
/// The shared system queues are never destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn destroy_workqueue(wq: *mut c_void) {
    assert!(!wq.is_null(), "destroy_workqueue(NULL)");
    assert!(
        wq.cast::<WorkqueueStruct>() != system_dfl_wq && wq.cast::<WorkqueueStruct>() != system_highpri_wq,
        "destroy_workqueue on a system workqueue"
    );
    flush_all_work();
    drop(unsafe { alloc::boxed::Box::from_raw(wq.cast::<WorkqueueStruct>()) });
}
