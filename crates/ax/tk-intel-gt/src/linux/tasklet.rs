// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux 7.2 tasklet scheduling for the i915 LinuxKPI boundary.
//!
//! Linux tasklets are dispatched from softirq context. This kernel currently
//! has no softirq dispatcher, so this binding uses a bounded pool of persistent
//! `axtask` workers as an asynchronous bottom-half runtime. Scheduling from an
//! IRQ only sets the upstream state bit, links the existing tasklet node under
//! a short IRQ-safe spin lock, and wakes the workers; callbacks never run on
//! the scheduling/IRQ stack. Callbacks run with preemption disabled, matching
//! the non-sleeping tasklet contract, but in task context rather than Linux
//! softirq context. `tasklet_setup` must therefore succeed before any tasklet
//! is published to IRQ paths.
//!
//! The real kernel build must feature-unify `axtask/multitask`; enabling this
//! crate's `upstream-gt` feature alone only enables the optional dependency.

#![allow(unsafe_code)]

use alloc::string::String;
use core::{
    ffi::c_ulong,
    fmt::Write as _,
    hint::spin_loop,
    ops::{Deref, DerefMut},
    ptr,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering, fence},
};

use kernel_guard::{NoPreempt, NoPreemptIrqSave};

use crate::intel_context_upstream::TaskletStruct;

pub(crate) const TASKLET_STATE_SCHED: u32 = 0;
const TASKLET_STATE_RUN: u32 = 1;
const TASKLET_WORKER_UNINITIALIZED: u8 = 0;
const TASKLET_WORKER_STARTING: u8 = 1;
const TASKLET_WORKER_READY: u8 = 2;
const MAX_HIGH_PRIORITY_BURST: u8 = 8;

const _: [(); core::mem::size_of::<usize>()] = [(); core::mem::size_of::<c_ulong>()];
const _: [(); core::mem::size_of::<Option<TaskletCallback>>()] =
    [(); core::mem::size_of::<Option<TaskletFunc>>()];

pub type TaskletCallback = unsafe fn(*mut TaskletStruct);
pub type TaskletFunc = unsafe fn(c_ulong);

/// Setup or task-context wait failed; no tasklet work is silently discarded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskletError {
    /// No running `axtask` scheduler/worker could be bound.
    WorkerUnavailable,
    /// The operation needs to block, but the current context cannot block.
    ContextCannotBlock,
    /// The task runtime refused a non-interruptible worker wait.
    WaitFailed,
    /// A null tasklet pointer was supplied.
    NullTasklet,
}

#[derive(Clone, Copy)]
enum QueueKind {
    Normal,
    High,
}

struct TaskletList {
    head: *mut TaskletStruct,
    tail: *mut TaskletStruct,
    len: usize,
}

impl TaskletList {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            len: 0,
        }
    }

    /// The node is not linked in any queue and remains live through tasklet_kill.
    unsafe fn push(&mut self, tasklet: *mut TaskletStruct) {
        // SAFETY: upheld by the caller and the queue lock; `next` belongs to
        // the single intrusive list containing this tasklet.
        unsafe { (*tasklet).next = ptr::null_mut() };
        if self.tail.is_null() {
            self.head = tasklet;
        } else {
            // SAFETY: tail is a live node in this list while the queue lock is held.
            unsafe { (*self.tail).next = tasklet };
        }
        self.tail = tasklet;
        self.len += 1;
    }

    /// The returned node is no longer linked in this list.
    unsafe fn pop(&mut self) -> *mut TaskletStruct {
        let tasklet = self.head;
        if tasklet.is_null() {
            return tasklet;
        }

        // SAFETY: head is a live node in this list while the queue lock is held.
        self.head = unsafe { (*tasklet).next };
        if self.head.is_null() {
            self.tail = ptr::null_mut();
        }
        // SAFETY: this node is now detached and may be dispatched or requeued.
        unsafe { (*tasklet).next = ptr::null_mut() };
        self.len -= 1;
        tasklet
    }
}

// The intrusive pointers are accessed only while TASKLET_QUEUE is locked. The
// owning driver must keep each tasklet alive until tasklet_kill has completed.
unsafe impl Send for TaskletList {}

struct TaskletQueue {
    high: TaskletList,
    normal: TaskletList,
    high_streak: u8,
}

impl TaskletQueue {
    const fn new() -> Self {
        Self {
            high: TaskletList::new(),
            normal: TaskletList::new(),
            high_streak: 0,
        }
    }

    fn len(&self) -> usize {
        self.high.len.saturating_add(self.normal.len)
    }

    unsafe fn push(&mut self, tasklet: *mut TaskletStruct, kind: QueueKind) {
        match kind {
            QueueKind::High => {
                // SAFETY: the node is detached and the caller holds the queue lock.
                unsafe { self.high.push(tasklet) };
            }
            QueueKind::Normal => {
                // SAFETY: the node is detached and the caller holds the queue lock.
                unsafe { self.normal.push(tasklet) };
            }
        }
    }

    unsafe fn pop(&mut self) -> Option<(*mut TaskletStruct, QueueKind)> {
        let take_high = !self.high.head.is_null()
            && (self.normal.head.is_null() || self.high_streak < MAX_HIGH_PRIORITY_BURST);
        if take_high {
            self.high_streak = self.high_streak.saturating_add(1);
            // SAFETY: queue lock is held and high is non-empty.
            Some((unsafe { self.high.pop() }, QueueKind::High))
        } else if !self.normal.head.is_null() {
            self.high_streak = 0;
            // SAFETY: queue lock is held and normal is non-empty.
            Some((unsafe { self.normal.pop() }, QueueKind::Normal))
        } else if !self.high.head.is_null() {
            self.high_streak = 1;
            // SAFETY: queue lock is held and high is non-empty.
            Some((unsafe { self.high.pop() }, QueueKind::High))
        } else {
            None
        }
    }
}

unsafe impl Send for TaskletQueue {}

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
        // Disable local IRQs before contending: otherwise an IRQ on this CPU
        // could interrupt its owner and spin forever on the same lock.
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
        // SAFETY: this guard is the unique owner of the lock.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqSpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: this guard is the unique owner of the lock.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqSpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
        // `_irq_guard` restores IRQ/preemption state after this lock is released.
    }
}

static TASKLET_QUEUE: IrqSpinLock<TaskletQueue> = IrqSpinLock::new(TaskletQueue::new());
static WORKER_STATE: AtomicU8 = AtomicU8::new(TASKLET_WORKER_UNINITIALIZED);
static WORKER_COUNT: AtomicUsize = AtomicUsize::new(0);
static WORK_GENERATION: AtomicUsize = AtomicUsize::new(0);
static WORK_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();
static STATE_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();

fn wake_workers() {
    WORK_GENERATION.fetch_add(1, Ordering::Release);
    WORK_WAKE.notify_all(false);
}

fn wake_state_waiters() {
    STATE_WAKE.notify_all(false);
}

fn make_worker_name(index: usize) -> Result<String, TaskletError> {
    let mut name = String::new();
    name.try_reserve_exact(24)
        .map_err(|_| TaskletError::WorkerUnavailable)?;
    name.push_str("i915-tasklet-");
    name.write_fmt(format_args!("{index}"))
        .map_err(|_| TaskletError::WorkerUnavailable)?;
    Ok(name)
}

/// Start the persistent bottom-half worker pool in task context.
///
/// The function is idempotent. At least one worker must be published before
/// tasklets are initialized; partial pool startup is valid because tasklet
/// state/locking preserves correctness with fewer workers, while distinct
/// workers may execute distinct tasklets concurrently.
pub fn init_tasklet_executor() -> Result<usize, TaskletError> {
    if !axtask::can_block_current() {
        return Err(TaskletError::ContextCannotBlock);
    }

    loop {
        match WORKER_STATE.load(Ordering::Acquire) {
            TASKLET_WORKER_READY => return Ok(WORKER_COUNT.load(Ordering::Acquire)),
            TASKLET_WORKER_STARTING => {
                WORK_WAKE
                    .wait_until(|| WORKER_STATE.load(Ordering::Acquire) != TASKLET_WORKER_STARTING)
                    .map_err(|_| TaskletError::WaitFailed)?;
            }
            TASKLET_WORKER_UNINITIALIZED => {
                if WORKER_STATE
                    .compare_exchange(
                        TASKLET_WORKER_UNINITIALIZED,
                        TASKLET_WORKER_STARTING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }

                let requested = axhal::cpu_num().max(1);
                let mut started = 0;
                for index in 0..requested {
                    let Ok(name) = make_worker_name(index) else {
                        continue;
                    };
                    if let Ok(worker) = axtask::try_spawn_with_name(tasklet_worker, name) {
                        // Workers have kernel-lifetime ownership; retaining
                        // these bounded references keeps them alive until
                        // subsystem shutdown, for which the kernel has no hook.
                        core::mem::forget(worker);
                        started += 1;
                    }
                }

                if started == 0 {
                    WORKER_STATE.store(TASKLET_WORKER_UNINITIALIZED, Ordering::Release);
                    WORK_WAKE.notify_all(false);
                    return Err(TaskletError::WorkerUnavailable);
                }

                WORKER_COUNT.store(started, Ordering::Release);
                WORKER_STATE.store(TASKLET_WORKER_READY, Ordering::Release);
                WORK_WAKE.notify_all(false);
                return Ok(started);
            }
            _ => unreachable!("invalid tasklet worker state"),
        }
    }
}

/// Initialize the callback-style `tasklet_struct` used by the i915 sources.
/// Must run in task context before the tasklet is visible to IRQ paths.
pub unsafe fn tasklet_setup(tasklet: *mut TaskletStruct, callback: TaskletCallback) {
    unsafe { try_tasklet_setup(tasklet, callback) }
        .expect("failed to bind the LinuxKPI tasklet executor");
}

/// Fallible form of [`tasklet_setup`] for driver initialization paths which
/// can propagate scheduler or allocation failures.
pub unsafe fn try_tasklet_setup(
    tasklet: *mut TaskletStruct,
    callback: TaskletCallback,
) -> Result<(), TaskletError> {
    if tasklet.is_null() {
        return Err(TaskletError::NullTasklet);
    }
    init_tasklet_executor()?;

    // SAFETY: caller provides exclusive, not-yet-published tasklet storage.
    unsafe {
        (*tasklet).next = ptr::null_mut();
        (*tasklet).state = 0;
        (*tasklet).count.counter = 0;
        (*tasklet).use_callback = true;
        (*tasklet).callback = Some(callback);
        (*tasklet).data = 0;
    }
    Ok(())
}

/// Initialize the legacy `func(data)` tasklet arm using the same C union slot.
pub unsafe fn tasklet_init(tasklet: *mut TaskletStruct, function: TaskletFunc, data: c_ulong) {
    unsafe { try_tasklet_init(tasklet, function, data) }
        .expect("failed to bind the LinuxKPI tasklet executor");
}

/// Fallible form of [`tasklet_init`].
pub unsafe fn try_tasklet_init(
    tasklet: *mut TaskletStruct,
    function: TaskletFunc,
    data: c_ulong,
) -> Result<(), TaskletError> {
    if tasklet.is_null() {
        return Err(TaskletError::NullTasklet);
    }
    init_tasklet_executor()?;

    let function_slot: Option<TaskletFunc> = Some(function);
    // SAFETY: the C tasklet union arms are same-size function-pointer slots;
    // `use_callback = false` selects this exact `func(data)` arm at dispatch.
    let callback_slot: Option<TaskletCallback> = unsafe { core::mem::transmute(function_slot) };
    // SAFETY: caller provides exclusive, not-yet-published tasklet storage.
    unsafe {
        (*tasklet).next = ptr::null_mut();
        (*tasklet).state = 0;
        (*tasklet).count.counter = 0;
        (*tasklet).use_callback = false;
        (*tasklet).callback = callback_slot;
        (*tasklet).data = data;
    }
    Ok(())
}

unsafe fn state_fetch_or(tasklet: *mut TaskletStruct, mask: usize) -> usize {
    // SAFETY: tasklet is valid/aligned and all concurrent state accesses use
    // this atomic view; the x86_64 Linux unsigned-long storage is usize-sized.
    let state = unsafe { ptr::addr_of_mut!((*tasklet).state).cast::<usize>() };
    unsafe { AtomicUsize::from_ptr(state) }.fetch_or(mask, Ordering::AcqRel)
}

unsafe fn state_fetch_and(tasklet: *mut TaskletStruct, mask: usize) -> usize {
    // SAFETY: same initialized atomic storage contract as state_fetch_or.
    let state = unsafe { ptr::addr_of_mut!((*tasklet).state).cast::<usize>() };
    unsafe { AtomicUsize::from_ptr(state) }.fetch_and(mask, Ordering::AcqRel)
}

unsafe fn state_load(tasklet: *mut TaskletStruct) -> usize {
    // SAFETY: same initialized atomic storage contract as state_fetch_or.
    let state = unsafe { ptr::addr_of_mut!((*tasklet).state).cast::<usize>() };
    unsafe { AtomicUsize::from_ptr(state) }.load(Ordering::Acquire)
}

unsafe fn count_fetch_add(tasklet: *mut TaskletStruct, value: i32) -> i32 {
    // SAFETY: tasklet is valid/aligned; count is accessed atomically after setup.
    let count = unsafe { ptr::addr_of_mut!((*tasklet).count.counter) };
    unsafe { AtomicI32::from_ptr(count) }.fetch_add(value, Ordering::SeqCst)
}

unsafe fn count_load(tasklet: *mut TaskletStruct) -> i32 {
    // SAFETY: same initialized atomic storage contract as count_fetch_add.
    let count = unsafe { ptr::addr_of_mut!((*tasklet).count.counter) };
    unsafe { AtomicI32::from_ptr(count) }.load(Ordering::Relaxed)
}

/// Attempt to acquire Linux's `TASKLET_STATE_RUN` bit.
pub unsafe fn tasklet_trylock(tasklet: *mut TaskletStruct) -> bool {
    if tasklet.is_null() {
        return false;
    }
    // SAFETY: caller provides a live initialized tasklet.
    (unsafe { state_fetch_or(tasklet, 1usize << TASKLET_STATE_RUN) }
        & (1usize << TASKLET_STATE_RUN))
        == 0
}

pub unsafe fn tasklet_lock(tasklet: *mut TaskletStruct) {
    while !unsafe { tasklet_trylock(tasklet) } {
        spin_loop();
    }
}

pub unsafe fn tasklet_is_locked(tasklet: *const TaskletStruct) -> bool {
    if tasklet.is_null() {
        return false;
    }
    // SAFETY: caller provides a live initialized tasklet.
    (unsafe { state_load(tasklet.cast_mut()) } & (1usize << TASKLET_STATE_RUN)) != 0
}

pub unsafe fn __tasklet_is_scheduled(tasklet: *mut TaskletStruct) -> bool {
    if tasklet.is_null() {
        return false;
    }
    // SAFETY: caller provides a live initialized tasklet.
    (unsafe { state_load(tasklet) } & (1usize << TASKLET_STATE_SCHED)) != 0
}

pub unsafe fn __tasklet_is_enabled(tasklet: *const TaskletStruct) -> bool {
    if tasklet.is_null() {
        return false;
    }
    // SAFETY: caller provides a live initialized tasklet.
    unsafe { count_load(tasklet.cast_mut()) == 0 }
}

/// Clear RUN and wake blocked task-context synchronizers/workers.
pub unsafe fn tasklet_unlock(tasklet: *mut TaskletStruct) {
    if tasklet.is_null() {
        return;
    }
    // SAFETY: caller provides a live initialized tasklet.
    unsafe { state_fetch_and(tasklet, !(1usize << TASKLET_STATE_RUN)) };
    wake_state_waiters();
    wake_workers();
}

unsafe fn tasklet_unlock_without_work_wake(tasklet: *mut TaskletStruct) {
    // SAFETY: forwarded from the worker, which owns RUN for this tasklet.
    unsafe { state_fetch_and(tasklet, !(1usize << TASKLET_STATE_RUN)) };
    wake_state_waiters();
}

/// Wait until RUN is clear; this operation requires a blockable task context.
pub unsafe fn tasklet_unlock_wait(tasklet: *mut TaskletStruct) {
    assert!(!tasklet.is_null(), "tasklet_unlock_wait received null");
    assert!(
        axtask::can_block_current(),
        "tasklet_unlock_wait requires a blockable task context"
    );
    // SAFETY: caller provides a live initialized tasklet.
    STATE_WAKE
        .wait_until(|| unsafe { !tasklet_is_locked(tasklet) })
        .expect("tasklet_unlock_wait lost its task wait context");
}

/// Atomic-context variant of tasklet_unlock_wait.
pub unsafe fn tasklet_unlock_spin_wait(tasklet: *mut TaskletStruct) {
    while unsafe { tasklet_is_locked(tasklet) } {
        spin_loop();
    }
}

/// Increment the disable count without waiting for an already running callback.
pub unsafe fn tasklet_disable_nosync(tasklet: *mut TaskletStruct) {
    assert!(!tasklet.is_null(), "tasklet_disable_nosync received null");
    // Linux uses atomic_inc followed by smp_mb__after_atomic().
    // SAFETY: caller provides a live initialized tasklet.
    unsafe { count_fetch_add(tasklet, 1) };
    fence(Ordering::SeqCst);
}

/// Disable from atomic context and wait for an already running callback.
pub unsafe fn tasklet_disable_in_atomic(tasklet: *mut TaskletStruct) {
    unsafe { tasklet_disable_nosync(tasklet) };
    unsafe { tasklet_unlock_spin_wait(tasklet) };
    fence(Ordering::SeqCst);
}

/// Disable and wait until a running callback releases RUN.
pub unsafe fn tasklet_disable(tasklet: *mut TaskletStruct) {
    assert!(!tasklet.is_null(), "tasklet_disable received null");
    assert!(
        axtask::can_block_current(),
        "tasklet_disable requires a blockable task context"
    );
    unsafe { tasklet_disable_nosync(tasklet) };
    unsafe { tasklet_unlock_wait(tasklet) };
    fence(Ordering::SeqCst);
}

/// Decrement count and wake workers when a pending tasklet becomes runnable.
pub unsafe fn tasklet_enable(tasklet: *mut TaskletStruct) {
    assert!(!tasklet.is_null(), "tasklet_enable received null");
    // `smp_mb__before_atomic()` from include/linux/interrupt.h.
    fence(Ordering::SeqCst);
    // SAFETY: caller provides a live initialized tasklet.
    unsafe { count_fetch_add(tasklet, -1) };
    wake_workers();
}

/// i915's `__tasklet_disable_sync_once()` helper.
pub unsafe fn __tasklet_disable_sync_once(tasklet: *mut TaskletStruct) {
    // SAFETY: caller provides a live initialized tasklet.
    if unsafe { count_fetch_add(tasklet, 1) } == 0 {
        unsafe { tasklet_unlock_spin_wait(tasklet) };
    }
    fence(Ordering::SeqCst);
}

/// i915's `__tasklet_enable()` helper; true means the disable count reached 0.
pub unsafe fn __tasklet_enable(tasklet: *mut TaskletStruct) -> bool {
    fence(Ordering::SeqCst);
    // SAFETY: caller provides a live initialized tasklet.
    let old = unsafe { count_fetch_add(tasklet, -1) };
    let enabled = old == 1;
    if enabled {
        wake_workers();
    }
    enabled
}

fn enqueue(tasklet: *mut TaskletStruct, kind: QueueKind) {
    {
        let mut queue = TASKLET_QUEUE.lock();
        // SAFETY: the SCHED bit guarantees this tasklet is not already queued;
        // tasklet_setup/tasklet_kill own its lifetime until it is detached.
        unsafe { queue.push(tasklet, kind) };
    }
    wake_workers();
}

/// Schedule a tasklet for normal-priority deferred execution.
pub unsafe fn tasklet_schedule(tasklet: *mut TaskletStruct) {
    unsafe { try_tasklet_schedule(tasklet) }
        .expect("LinuxKPI tasklet executor is unavailable for tasklet_schedule");
}

/// Fallible form of [`tasklet_schedule`] for callers that can propagate a
/// startup/runtime error rather than fail-stop on a missing worker.
pub unsafe fn try_tasklet_schedule(tasklet: *mut TaskletStruct) -> Result<(), TaskletError> {
    if tasklet.is_null() {
        return Err(TaskletError::NullTasklet);
    }
    if WORKER_STATE.load(Ordering::Acquire) != TASKLET_WORKER_READY {
        return Err(TaskletError::WorkerUnavailable);
    }
    // SAFETY: caller provides a live initialized tasklet.
    let old = unsafe { state_fetch_or(tasklet, 1usize << TASKLET_STATE_SCHED) };
    if old & (1usize << TASKLET_STATE_SCHED) == 0 {
        enqueue(tasklet, QueueKind::Normal);
    }
    Ok(())
}

/// Schedule a tasklet for high-priority deferred execution.
pub unsafe fn tasklet_hi_schedule(tasklet: *mut TaskletStruct) {
    unsafe { try_tasklet_hi_schedule(tasklet) }
        .expect("LinuxKPI tasklet executor is unavailable for tasklet_hi_schedule");
}

/// Fallible form of [`tasklet_hi_schedule`].
pub unsafe fn try_tasklet_hi_schedule(tasklet: *mut TaskletStruct) -> Result<(), TaskletError> {
    if tasklet.is_null() {
        return Err(TaskletError::NullTasklet);
    }
    if WORKER_STATE.load(Ordering::Acquire) != TASKLET_WORKER_READY {
        return Err(TaskletError::WorkerUnavailable);
    }
    // SAFETY: caller provides a live initialized tasklet.
    let old = unsafe { state_fetch_or(tasklet, 1usize << TASKLET_STATE_SCHED) };
    if old & (1usize << TASKLET_STATE_SCHED) == 0 {
        enqueue(tasklet, QueueKind::High);
    }
    Ok(())
}

fn pop_budget() -> usize {
    TASKLET_QUEUE.lock().len()
}

fn pop_tasklet() -> Option<(*mut TaskletStruct, QueueKind)> {
    // SAFETY: the queue lock serializes all intrusive link reads/writes.
    unsafe { TASKLET_QUEUE.lock().pop() }
}

fn requeue(tasklet: *mut TaskletStruct, kind: QueueKind) {
    let mut queue = TASKLET_QUEUE.lock();
    // SCHED remains set while a disabled or already-running tasklet is put back.
    // SAFETY: this tasklet was just detached by a worker and retains its lifetime.
    unsafe { queue.push(tasklet, kind) };
}

fn worker_wait_for_generation(generation: usize) {
    WORK_WAKE
        .wait_until(|| WORK_GENERATION.load(Ordering::Acquire) != generation)
        .expect("tasklet worker lost its waitable task context");
}

fn worker_wait_until_ready() {
    if WORKER_STATE.load(Ordering::Acquire) == TASKLET_WORKER_READY {
        return;
    }
    WORK_WAKE
        .wait_until(|| WORKER_STATE.load(Ordering::Acquire) == TASKLET_WORKER_READY)
        .expect("tasklet worker could not wait for executor publication");
}

fn tasklet_worker() {
    worker_wait_until_ready();
    loop {
        // Capture generation before observing the queue. The wait below uses
        // register-check-register semantics, so an IRQ publication between the
        // final check and listener installation cannot be lost.
        let generation = WORK_GENERATION.load(Ordering::Acquire);
        let budget = pop_budget();
        if budget == 0 {
            if WORK_GENERATION.load(Ordering::Acquire) == generation {
                worker_wait_for_generation(generation);
            }
            continue;
        }

        for _ in 0..budget {
            let Some((tasklet, kind)) = pop_tasklet() else {
                break;
            };

            // tasklet_action_common first obtains RUN, then checks count.
            if !unsafe { tasklet_trylock(tasklet) } {
                requeue(tasklet, kind);
                continue;
            }

            // SAFETY: worker owns RUN and tasklet remains alive until kill.
            if unsafe { count_load(tasklet) } != 0 {
                unsafe { tasklet_unlock_without_work_wake(tasklet) };
                requeue(tasklet, kind);
                continue;
            }

            // Clear SCHED before calling the handler so it may queue itself
            // again. This mirrors tasklet_clear_sched() in Linux softirq.c.
            // SAFETY: worker owns RUN for this tasklet.
            let old = unsafe { state_fetch_and(tasklet, !(1usize << TASKLET_STATE_SCHED)) };
            assert!(
                old & (1usize << TASKLET_STATE_SCHED) != 0,
                "tasklet queued without TASKLET_STATE_SCHED"
            );
            wake_state_waiters();

            let (use_callback, callback, data) = unsafe {
                (
                    (*tasklet).use_callback,
                    ptr::read_volatile(ptr::addr_of!((*tasklet).callback)),
                    (*tasklet).data,
                )
            };
            let callback = callback.expect("enabled tasklet has no installed handler");
            {
                // Tasklet callbacks are atomic/non-sleeping. Keep hard IRQs
                // enabled, but prevent task preemption or blocking while the
                // callback runs on this deferred worker.
                let _bottom_half = NoPreempt::new();
                if use_callback {
                    // SAFETY: callback was installed by tasklet_setup or
                    // directly published by the driver under its disable
                    // and tasklet-lock protocol.
                    unsafe { callback(tasklet) };
                } else {
                    // The function and callback arms share the C anonymous
                    // union slot; tasklet_init stored the former there.
                    // SAFETY: `use_callback == false` selects TaskletFunc.
                    let function: TaskletFunc = unsafe { core::mem::transmute(callback) };
                    unsafe { function(data) };
                }
            }
            unsafe { tasklet_unlock(tasklet) };
        }

        if WORK_GENERATION.load(Ordering::Acquire) == generation {
            worker_wait_for_generation(generation);
        }
    }
}

/// Wait until RUN is clear, then reserve SCHED while destroying the tasklet.
/// Callers must prevent future scheduling before invoking this function.
pub unsafe fn tasklet_kill(tasklet: *mut TaskletStruct) {
    unsafe { try_tasklet_kill(tasklet) }.expect("tasklet_kill failed outside task context");
}

/// Fallible form of [`tasklet_kill`].
pub unsafe fn try_tasklet_kill(tasklet: *mut TaskletStruct) -> Result<(), TaskletError> {
    if tasklet.is_null() {
        return Err(TaskletError::NullTasklet);
    }
    if !axtask::can_block_current() {
        return Err(TaskletError::ContextCannotBlock);
    }

    loop {
        // wait_on_bit_lock(TASKLET_STATE_SCHED): claim the bit only after any
        // queued invocation has cleared it.
        // SAFETY: caller provides a live initialized tasklet.
        let old = unsafe { state_fetch_or(tasklet, 1usize << TASKLET_STATE_SCHED) };
        if old & (1usize << TASKLET_STATE_SCHED) == 0 {
            break;
        }
        if STATE_WAKE
            .wait_until(|| {
                // SAFETY: tasklet remains live until this kill operation returns.
                unsafe { !__tasklet_is_scheduled(tasklet) }
            })
            .is_err()
        {
            return Err(TaskletError::WaitFailed);
        }
    }

    unsafe { tasklet_unlock_wait(tasklet) };

    // Release the reservation; the caller's lifecycle protocol prevents new
    // scheduling after this point.
    // SAFETY: kill owns the SCHED reservation.
    unsafe { state_fetch_and(tasklet, !(1usize << TASKLET_STATE_SCHED)) };
    wake_state_waiters();
    Ok(())
}
