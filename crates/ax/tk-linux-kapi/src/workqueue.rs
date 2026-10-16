// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Asynchronous Linux `work_struct` dispatch for the i915 LinuxKPI boundary.
//!
//! This adapter provides a pool of persistent task-context workers fed by one
//! intrusive FIFO. Producers only publish a pending work item under an
//! IRQ-safe lock; callbacks are always invoked later by a worker, never inline.
//!
//! The pool keeps the Linux execution guarantees the i915 and DRM ports rely
//! on:
//! - a work item never runs on two workers at once (non-reentrancy); an item
//!   requeued while running waits for that instance to finish;
//! - each `workqueue_struct` limits its running items to `max_active`, so an
//!   ordered queue (`__WQ_ORDERED`, `max_active == 1`) runs its items one at a
//!   time in queue order;
//! - a work item blocking on another item does not starve it: a manager task
//!   adds workers (up to [`MAX_WORKERS`]) when runnable work waits while no
//!   worker is idle, standing in for Linux's concurrency-managed pools.
//!
//! Workqueue priority (`WQ_HIGHPRI`) and CPU affinity are not modelled.
//! The executor must be initialized from blockable task context before work
//! can run; items queued earlier wait on the FIFO.

#![allow(unsafe_code)]
#![allow(non_snake_case)]

use alloc::string::String;
use core::{
    ffi::c_void,
    fmt::Write as _,
    hint::spin_loop,
    ops::{Deref, DerefMut},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU8, AtomicUsize, Ordering},
};

use kernel_guard::NoPreemptIrqSave;

use crate::{
    types::{DelayedWork, ListHead, TimerList, WorkStruct},
    timer::{del_timer, del_timer_sync, mod_timer, timer_setup},
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

/// Linux `WQ_MAX_ACTIVE` and `WQ_DFL_ACTIVE` (include/linux/workqueue.h).
const WQ_MAX_ACTIVE: u32 = 2048;
const WQ_DFL_ACTIVE: u32 = WQ_MAX_ACTIVE / 2;
/// Linux `__WQ_ORDERED` (include/linux/workqueue.h).
const __WQ_ORDERED: u32 = 1 << 17;
const WORKQUEUE_MAGIC: u32 = 0x5751_5545;

/// Linux `workqueue_struct`. Every queue feeds the shared worker pool; the
/// handle carries its concurrency limit and the in-flight count that
/// `flush_workqueue()` waits on. The counters change only under `WORK_QUEUE`.
#[repr(C)]
pub struct WorkqueueStruct {
    magic: u32,
    max_active: u32,
    /// Items of this queue currently executing.
    active: AtomicU32,
    /// Items of this queue queued or executing.
    inflight: AtomicUsize,
}

impl WorkqueueStruct {
    const fn new(max_active: u32) -> Self {
        Self {
            magic: WORKQUEUE_MAGIC,
            max_active,
            active: AtomicU32::new(0),
            inflight: AtomicUsize::new(0),
        }
    }
}

// Queued items keep their workqueue pointer in `work_struct.data` next to the
// PENDING bit, as Linux keeps its pool_workqueue there.
const _: () = assert!(core::mem::align_of::<WorkqueueStruct>() > WORK_PENDING);

static SYSTEM_DEFAULT_WORKQUEUE: WorkqueueStruct = WorkqueueStruct::new(WQ_DFL_ACTIVE);
static SYSTEM_HIGHPRI_WORKQUEUE: WorkqueueStruct = WorkqueueStruct::new(WQ_DFL_ACTIVE);

#[allow(non_upper_case_globals)]
pub const system_dfl_wq: *mut WorkqueueStruct =
    core::ptr::addr_of!(SYSTEM_DEFAULT_WORKQUEUE).cast_mut();
#[allow(non_upper_case_globals)]
pub const system_highpri_wq: *mut WorkqueueStruct =
    core::ptr::addr_of!(SYSTEM_HIGHPRI_WORKQUEUE).cast_mut();

/// A workqueue pointer published as a C data symbol.
#[repr(transparent)]
pub struct ExportedWorkqueue(*mut WorkqueueStruct);
// SAFETY: the pointer names an immutable static handle.
unsafe impl Sync for ExportedWorkqueue {}

/// Linux `struct workqueue_struct *system_dfl_wq` (kernel/workqueue.c), read
/// by C-ABI callers such as `i915_active.c`.
#[unsafe(export_name = "system_dfl_wq")]
pub static SYSTEM_DFL_WQ_SYMBOL: ExportedWorkqueue = ExportedWorkqueue(system_dfl_wq);

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
/// Items on the FIFO (pending, not yet running). Changed under `WORK_QUEUE`.
static QUEUED: AtomicUsize = AtomicUsize::new(0);
static WORK_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();
static WORK_STATE_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();
static MANAGER_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();

/// Upper bound on pool workers. Each one is a kernel task with its own stack.
pub const MAX_WORKERS: usize = 32;
const INITIAL_WORKERS: usize = 2;
/// How long runnable work may wait with no idle worker before the manager
/// adds one: long enough that ordinary busy workers pick it up first.
const STARVATION_GRACE: core::time::Duration = core::time::Duration::from_millis(10);

struct WorkerSlot {
    task: AtomicUsize,
    current: AtomicPtr<WorkStruct>,
    current_wq: AtomicPtr<WorkqueueStruct>,
}

impl WorkerSlot {
    const fn new() -> Self {
        Self {
            task: AtomicUsize::new(0),
            current: AtomicPtr::new(ptr::null_mut()),
            current_wq: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

static WORKERS: [WorkerSlot; MAX_WORKERS] = [const { WorkerSlot::new() }; MAX_WORKERS];
/// Claimed prefix of `WORKERS`.
static NR_WORKERS: AtomicUsize = AtomicUsize::new(0);
static IDLE_WORKERS: AtomicUsize = AtomicUsize::new(0);

fn workers() -> &'static [WorkerSlot] {
    &WORKERS[..NR_WORKERS.load(Ordering::Acquire).min(MAX_WORKERS)]
}

fn notify_workers() {
    WORK_GENERATION.fetch_add(1, Ordering::Release);
    WORK_WAKE.notify_all(false);
}

/// Wake the manager when work is queued and every worker is occupied.
fn kick_manager_if_starved() {
    if QUEUED.load(Ordering::Acquire) != 0 && IDLE_WORKERS.load(Ordering::Acquire) == 0 {
        MANAGER_WAKE.notify_one(false);
    }
}

fn make_task_name(prefix: &str, index: Option<usize>) -> Result<String, WorkqueueError> {
    let mut name = String::new();
    name.try_reserve_exact(24)
        .map_err(|_| WorkqueueError::WorkerUnavailable)?;
    name.push_str(prefix);
    if let Some(index) = index {
        let _ = write!(name, "/{index}");
    }
    Ok(name)
}

/// Claim the next worker slot and start its task. Called only by the
/// executor initializer and by the single manager task.
fn spawn_worker() -> Result<(), WorkqueueError> {
    let index = NR_WORKERS.load(Ordering::Acquire);
    if index >= MAX_WORKERS {
        return Err(WorkqueueError::WorkerUnavailable);
    }
    let name = make_task_name("kworker", Some(index))?;
    let worker = axtask::try_spawn_with_name(move || workqueue_worker(index), name)
        .map_err(|_| WorkqueueError::WorkerUnavailable)?;
    // Workers have kernel lifetime; no workqueue shutdown hook exists.
    core::mem::forget(worker);
    NR_WORKERS.store(index + 1, Ordering::Release);
    Ok(())
}

/// Start the persistent worker pool from blockable task context.
/// This is idempotent and must complete before queued work can run.
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

                let started = (|| {
                    while NR_WORKERS.load(Ordering::Acquire) < INITIAL_WORKERS {
                        spawn_worker()?;
                    }
                    let manager = make_task_name("kworker-manager", None).and_then(|name| {
                        axtask::try_spawn_with_name(workqueue_manager, name)
                            .map_err(|_| WorkqueueError::WorkerUnavailable)
                    })?;
                    core::mem::forget(manager);
                    Ok(())
                })();
                if let Err(error) = started {
                    // Workers already spawned stay parked until a later
                    // initializer publishes READY.
                    WORKER_STATE.store(WORKER_UNINITIALIZED, Ordering::Release);
                    WORK_WAKE.notify_all(false);
                    return Err(error);
                }
                WORKER_STATE.store(WORKER_READY, Ordering::Release);
                notify_workers();
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
    let _ = init_workqueue_executor();
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
    let _ = init_workqueue_executor();
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
    let expires = crate::primitives::jiffies().wrapping_add(delay_jiffies);
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
            crate::primitives::jiffies().wrapping_add(delay_jiffies) as core::ffi::c_ulong,
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

/// Fallible implementation of [`queue_work`].
pub unsafe fn try_queue_work<Q: WorkqueuePtr, W: WorkStructPtr>(
    workqueue: Q,
    work: W,
) -> Result<bool, WorkqueueError> {
    let workqueue = workqueue.workqueue_ptr().cast_mut().cast::<WorkqueueStruct>();
    let work = work.work_struct_ptr();
    if workqueue.is_null() || work.is_null() {
        return Err(WorkqueueError::NullPointer);
    }
    let _ = init_workqueue_executor();
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
        unsafe { enqueue_locked(&mut queue, workqueue, work) };
    }
    notify_workers();
    kick_manager_if_starved();
    Ok(true)
}

/// Publish a work item whose PENDING bit is already set. The caller holds
/// `WORK_QUEUE`.
unsafe fn enqueue_locked(queue: &mut WorkList, workqueue: *mut WorkqueueStruct, work: *mut WorkStruct) {
    assert_eq!(
        unsafe { (*workqueue).magic },
        WORKQUEUE_MAGIC,
        "queue_work on a pointer that is not a workqueue_struct"
    );
    unsafe { work_data(work) }.store(workqueue as usize | WORK_PENDING, Ordering::Release);
    unsafe { (*workqueue).inflight.fetch_add(1, Ordering::AcqRel) };
    QUEUED.fetch_add(1, Ordering::AcqRel);
    unsafe { queue.push(work) };
}

/// The workqueue recorded for a queued item, or null when the PENDING bit was
/// set without publishing the item (an RCU work item inside its grace period).
unsafe fn queued_workqueue(work: *mut WorkStruct) -> *mut WorkqueueStruct {
    let data = unsafe { work_data(work) }.load(Ordering::Acquire);
    if data & WORK_PENDING == 0 || data & !WORK_PENDING == WORK_DATA_INIT {
        return ptr::null_mut();
    }
    (data & !WORK_PENDING) as *mut WorkqueueStruct
}

/// Whether some worker is executing `work`. The caller holds `WORK_QUEUE`.
fn running_anywhere(work: *mut WorkStruct) -> bool {
    workers()
        .iter()
        .any(|slot| slot.current.load(Ordering::Acquire) == work)
}

/// Pop the oldest item that may run now: its queue is below `max_active` and
/// no other worker runs the same item. Skipped items keep their FIFO position,
/// so an ordered queue still runs in queue order. The caller holds
/// `WORK_QUEUE`.
unsafe fn take_runnable_locked(queue: &mut WorkList) -> Option<(*mut WorkStruct, *mut WorkqueueStruct)> {
    let head = queue.head;
    if head.is_null() {
        return None;
    }
    let mut node = head;
    loop {
        let work = unsafe { work_from_entry(node) };
        let workqueue = unsafe { queued_workqueue(work) };
        assert!(!workqueue.is_null(), "queued work item has no workqueue");
        let limit = unsafe { (*workqueue).max_active };
        if unsafe { (*workqueue).active.load(Ordering::Acquire) } < limit
            && !running_anywhere(work)
        {
            assert!(unsafe { queue.remove(work) }, "runnable work missing from queue");
            return Some((work, workqueue));
        }
        node = unsafe { (*node).next };
        if node == head {
            return None;
        }
    }
}

/// Whether any queued item could start now (manager heuristic).
fn has_runnable_work() -> bool {
    let queue = WORK_QUEUE.lock();
    let head = queue.head;
    if head.is_null() {
        return false;
    }
    let mut node = head;
    loop {
        let work = unsafe { work_from_entry(node) };
        let workqueue = unsafe { queued_workqueue(work) };
        if !workqueue.is_null()
            && unsafe { (*workqueue).active.load(Ordering::Acquire) < (*workqueue).max_active }
            && !running_anywhere(work)
        {
            return true;
        }
        node = unsafe { (*node).next };
        if node == head {
            return false;
        }
    }
}

/// Move one runnable item from the FIFO onto worker `index`.
fn take_work(index: usize) -> *mut WorkStruct {
    let taken = {
        let mut queue = WORK_QUEUE.lock();
        let Some((work, workqueue)) = (unsafe { take_runnable_locked(&mut queue) }) else {
            return ptr::null_mut();
        };
        // The pending-to-running transition is made under the same lock used
        // by queue/cancel, so no producer can observe a detached-but-pending
        // item, and the item can be requeued as soon as it runs.
        unsafe { work_data(work) }.store(WORK_DATA_INIT, Ordering::Release);
        QUEUED.fetch_sub(1, Ordering::AcqRel);
        unsafe { (*workqueue).active.fetch_add(1, Ordering::AcqRel) };
        let slot = &WORKERS[index];
        slot.current_wq.store(workqueue, Ordering::Release);
        slot.current.store(work, Ordering::Release);
        work
    };
    kick_manager_if_starved();
    taken
}

fn finish_work(index: usize, work: *mut WorkStruct) {
    {
        let _queue = WORK_QUEUE.lock();
        let slot = &WORKERS[index];
        let active = slot.current.swap(ptr::null_mut(), Ordering::AcqRel);
        assert_eq!(active, work, "work callback completed without ownership");
        let workqueue = slot.current_wq.swap(ptr::null_mut(), Ordering::AcqRel);
        unsafe {
            (*workqueue).active.fetch_sub(1, Ordering::AcqRel);
            (*workqueue).inflight.fetch_sub(1, Ordering::AcqRel);
        }
    }
    WORK_STATE_WAKE.notify_all(false);
    // A requeued instance of this item, or the next item of a saturated
    // queue, may be runnable now.
    if QUEUED.load(Ordering::Acquire) != 0 {
        notify_workers();
    }
}

fn work_busy(work: *mut WorkStruct) -> bool {
    let _queue = WORK_QUEUE.lock();
    unsafe { work_data(work) }.load(Ordering::Acquire) & WORK_PENDING != 0
        || running_anywhere(work)
}

fn workqueue_worker(index: usize) {
    let task = axhal::percpu::current_task_ptr::<()>();
    WORKERS[index].task.store(task as usize, Ordering::Release);
    WORK_WAKE
        .wait_until(|| WORKER_STATE.load(Ordering::Acquire) == WORKER_READY)
        .expect("workqueue worker could not wait for executor publication");

    loop {
        let generation = WORK_GENERATION.load(Ordering::Acquire);
        let work = take_work(index);
        if work.is_null() {
            IDLE_WORKERS.fetch_add(1, Ordering::AcqRel);
            WORK_WAKE
                .wait_until(|| WORK_GENERATION.load(Ordering::Acquire) != generation)
                .expect("workqueue worker lost its waitable task context");
            IDLE_WORKERS.fetch_sub(1, Ordering::AcqRel);
            continue;
        }

        let callback =
            unsafe { (*work).function }.expect("queued work item has no initialized callback");
        unsafe { callback(work) };
        finish_work(index, work);
    }
}

/// Grow the pool when runnable work has waited `STARVATION_GRACE` with no
/// idle worker: every worker is busy or blocked, possibly on that very work.
fn workqueue_manager() {
    WORK_WAKE
        .wait_until(|| WORKER_STATE.load(Ordering::Acquire) == WORKER_READY)
        .expect("workqueue manager could not wait for executor publication");
    let starved = || {
        QUEUED.load(Ordering::Acquire) != 0
            && IDLE_WORKERS.load(Ordering::Acquire) == 0
            && NR_WORKERS.load(Ordering::Acquire) < MAX_WORKERS
    };
    loop {
        MANAGER_WAKE
            .wait_until(starved)
            .expect("workqueue manager lost its waitable task context");
        let _ = axtask::sleep(STARVATION_GRACE);
        if !starved() || !has_runnable_work() {
            continue;
        }
        match spawn_worker() {
            Ok(()) => axlog::debug!(
                "workqueue: added worker {} for starved work",
                NR_WORKERS.load(Ordering::Acquire)
            ),
            Err(error) => {
                axlog::warn!("workqueue: cannot add worker: {error:?}");
                let _ = axtask::sleep(core::time::Duration::from_secs(1));
            }
        }
    }
}

/// Return the work item the calling worker task is executing, or null when
/// the caller is not a pool worker.
pub fn current_work() -> *mut WorkStruct {
    let task = axhal::percpu::current_task_ptr::<()>();
    if task.is_null() {
        return ptr::null_mut();
    }
    workers()
        .iter()
        .find(|slot| slot.task.load(Ordering::Acquire) == task as usize)
        .map_or(ptr::null_mut(), |slot| slot.current.load(Ordering::Acquire))
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
        let workqueue = unsafe { queued_workqueue(work) };
        if workqueue.is_null() {
            // Idle, or an RCU work item still inside its grace period, which
            // Linux also cannot steal.
            false
        } else {
            // SAFETY: queue lock serializes cancellation with enqueue/dequeue.
            assert!(
                unsafe { queue.remove(work) },
                "pending work missing from queue"
            );
            unsafe { work_data(work) }.store(WORK_DATA_INIT, Ordering::Release);
            QUEUED.fetch_sub(1, Ordering::AcqRel);
            unsafe { (*workqueue).inflight.fetch_sub(1, Ordering::AcqRel) };
            true
        }
    };
    if removed {
        WORK_STATE_WAKE.notify_all(false);
    }
    removed
}

/// Wait for the queued and executing instances of `work` to finish.
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
    let _ = init_workqueue_executor();
    if !work_busy(work) {
        return Ok(false);
    }
    let mut waited_s = 0u64;
    loop {
        let timed_out = WORK_STATE_WAKE
            .wait_timeout_until(core::time::Duration::from_secs(10), || !work_busy(work))
            .map_err(|_| WorkqueueError::WaitFailed)?;
        if !timed_out || !work_busy(work) {
            break;
        }
        waited_s += 10;
        report_flush_stall(work, waited_s);
    }
    Ok(true)
}

/// Name the waited item and what every worker is running.
fn report_flush_stall(work: *mut WorkStruct, waited_s: u64) {
    let waited_fn = unsafe { (*work).function }.map_or(0, |f| f as usize);
    axlog::warn!(
        "flush_work stalled {waited_s}s: waiting work={work:p} fn={waited_fn:#x} data={:#x} \
         queued={} idle={}/{}",
        unsafe { work_data(work) }.load(Ordering::Acquire),
        QUEUED.load(Ordering::Acquire),
        IDLE_WORKERS.load(Ordering::Acquire),
        NR_WORKERS.load(Ordering::Acquire),
    );
    for (index, slot) in workers().iter().enumerate() {
        let active = slot.current.load(Ordering::Acquire);
        if !active.is_null() {
            let active_fn = unsafe { (*active).function }.map_or(0, |f| f as usize);
            axlog::warn!("  kworker/{index} runs work={active:p} fn={active_fn:#x}");
        }
    }
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
    let _ = init_workqueue_executor();

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
pub unsafe fn mark_work_pending(work: *mut WorkStruct) -> bool {
    let old = unsafe { work_data(work) }.fetch_or(WORK_PENDING, Ordering::AcqRel);
    old & WORK_PENDING == 0
}

/// Enqueue on `workqueue` a work item whose PENDING bit the caller already
/// set. The pending bit stays set until a worker dequeues the item.
///
/// # Safety
/// `work` must be live, initialized, marked pending and not on the queue;
/// `workqueue` must be a live workqueue handle.
pub unsafe fn enqueue_marked_work(workqueue: *mut c_void, work: *mut WorkStruct) {
    assert!(!workqueue.is_null() && !work.is_null());
    let _ = init_workqueue_executor();
    {
        let mut queue = WORK_QUEUE.lock();
        unsafe { enqueue_locked(&mut queue, workqueue.cast(), work) };
    }
    notify_workers();
    kick_manager_if_starved();
}

/// Block until `workqueue` has no queued or running item.
///
/// This covers every item queued before the call, which is the Linux
/// guarantee of `flush_workqueue()` / `drain_workqueue()`; it may additionally
/// wait for items queued on the same queue after the call.
fn flush_workqueue_items(workqueue: *mut WorkqueueStruct) {
    assert!(!workqueue.is_null(), "flush_workqueue(NULL)");
    if unsafe { (*workqueue).inflight.load(Ordering::Acquire) } == 0 {
        return;
    }
    if !axtask::can_block_current() {
        panic!("flush_workqueue requires blockable task context");
    }
    let _ = init_workqueue_executor();
    WORK_STATE_WAKE
        .wait_until(|| unsafe { (*workqueue).inflight.load(Ordering::Acquire) } == 0)
        .expect("flush_workqueue could not wait for the workers");
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
pub unsafe extern "C" fn flush_workqueue(wq: *mut c_void) {
    flush_workqueue_items(wq.cast());
}

/// Linux `drain_workqueue()`: flushes until no work remains. Re-queueing
/// after the drain is not rejected, which is the one difference from Linux's
/// `__WQ_DRAINING` warning path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drain_workqueue(wq: *mut c_void) {
    flush_workqueue_items(wq.cast());
}

/// Linux `alloc_workqueue()`. The format arguments name the queue for debug
/// output and are not kept; `flags` and `max_active` set the concurrency
/// limit as in `__alloc_workqueue()` / `wq_clamp_max_active()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn alloc_workqueue(
    _fmt: *const core::ffi::c_char,
    flags: u32,
    max_active: core::ffi::c_int,
    _args: ...
) -> *mut c_void {
    let max_active = if flags & __WQ_ORDERED != 0 {
        1
    } else if max_active <= 0 {
        WQ_DFL_ACTIVE
    } else {
        (max_active as u32).min(WQ_MAX_ACTIVE)
    };
    let queue = alloc::boxed::Box::new(WorkqueueStruct::new(max_active));
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
    flush_workqueue_items(wq.cast());
    drop(unsafe { alloc::boxed::Box::from_raw(wq.cast::<WorkqueueStruct>()) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU32;

    static TEST_CALLBACK_CALLED: AtomicU32 = AtomicU32::new(0);
    /// The tests share the global FIFO; serialize them.
    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Host tests have no blockable task, so no worker task runs; the tests
    /// drive two worker slots by hand.
    fn claim_test_slots() {
        NR_WORKERS.fetch_max(2, Ordering::AcqRel);
    }

    fn new_work() -> alloc::boxed::Box<WorkStruct> {
        let mut work = alloc::boxed::Box::new(unsafe { core::mem::zeroed::<WorkStruct>() });
        INIT_WORK_C(&mut work, test_callback);
        work
    }

    unsafe extern "C" fn test_callback(_work: *mut WorkStruct) {
        TEST_CALLBACK_CALLED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn test_early_queue_work_and_cancel() {
        let _serial = serial();
        let mut work = core::mem::MaybeUninit::<WorkStruct>::zeroed();
        let work_ptr = work.as_mut_ptr();
        unsafe {
            INIT_WORK_C(&mut *work_ptr, test_callback);
            assert!(!work_pending(work_ptr));
            // Queuing succeeds even when worker is unstarted / cannot block
            assert_eq!(try_queue_work(system_dfl_wq, work_ptr), Ok(true));
            assert!(work_pending(work_ptr));
            // Re-queuing returns Ok(false) because it's already pending
            assert_eq!(try_queue_work(system_dfl_wq, work_ptr), Ok(false));
            // Cancellation successfully removes it from queue and clears pending
            assert!(cancel_work(work_ptr));
            assert!(!work_pending(work_ptr));
            assert!(!cancel_work(work_ptr));
        }
    }

    #[test]
    fn test_cancel_work_before_execution() {
        let _serial = serial();
        let mut work = core::mem::MaybeUninit::<WorkStruct>::zeroed();
        let work_ptr = work.as_mut_ptr();
        unsafe {
            INIT_WORK_C(&mut *work_ptr, test_callback);
            assert!(try_queue_work(system_dfl_wq, work_ptr).is_ok());
            let _ = cancel_work(work_ptr);
            assert!(!work_pending(work_ptr));
        }
    }

    #[test]
    fn ordered_queue_runs_one_item_at_a_time_in_order() {
        let _serial = serial();
        claim_test_slots();
        let wq = unsafe { alloc_workqueue(c"ordered".as_ptr(), __WQ_ORDERED, 1) };
        let mut first = new_work();
        let mut second = new_work();
        let (first, second) = (&mut *first as *mut WorkStruct, &mut *second as *mut WorkStruct);
        unsafe {
            assert!(queue_work(wq, first));
            assert!(queue_work(wq, second));
        }
        assert_eq!(take_work(0), first);
        assert!(take_work(1).is_null(), "ordered queue started a second item");
        finish_work(0, first);
        assert_eq!(take_work(1), second);
        finish_work(1, second);
        assert_eq!(unsafe { (*wq.cast::<WorkqueueStruct>()).inflight.load(Ordering::Acquire) }, 0);
        unsafe { destroy_workqueue(wq) };
    }

    #[test]
    fn unordered_queue_runs_items_concurrently() {
        let _serial = serial();
        claim_test_slots();
        let mut first = new_work();
        let mut second = new_work();
        let (first, second) = (&mut *first as *mut WorkStruct, &mut *second as *mut WorkStruct);
        unsafe {
            assert!(queue_work(system_dfl_wq, first));
            assert!(queue_work(system_dfl_wq, second));
        }
        assert_eq!(take_work(0), first);
        assert_eq!(take_work(1), second);
        finish_work(0, first);
        finish_work(1, second);
    }

    #[test]
    fn requeued_running_item_waits_for_its_running_instance() {
        let _serial = serial();
        claim_test_slots();
        let mut work = new_work();
        let work = &mut *work as *mut WorkStruct;
        unsafe { assert!(queue_work(system_dfl_wq, work)) };
        assert_eq!(take_work(0), work);
        assert!(!unsafe { work_pending(work) });
        unsafe { assert!(queue_work(system_dfl_wq, work)) };
        assert!(take_work(1).is_null(), "work item ran on two workers at once");
        finish_work(0, work);
        assert_eq!(take_work(1), work);
        finish_work(1, work);
        assert!(!work_busy(work));
    }
}
