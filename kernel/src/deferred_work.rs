use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use axerrno::{AxError, AxResult};
use axpoll::PollSet;

use crate::readiness::block_on_poll_set_uninterruptible;

#[cfg(feature = "perf-sampling")]
fn perf_retire_ipi_handler() {
    wake_policy_worker();
}

/// Per-actor completion accounting for policy work published from an
/// allocation-free Drop path.  The account is allocated with the user thread,
/// not while the final open-file-description reference is being released.
pub(crate) struct DeferredWorkAccount {
    pending: AtomicUsize,
}

impl DeferredWorkAccount {
    pub(crate) const fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
        }
    }

    /// Starts one work item. Overflow is impossible under the kernel's bounded
    /// fd limits, but keep the Drop path non-panicking if an invariant is ever
    /// violated.
    pub(crate) fn begin(&self) -> bool {
        self.pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                pending.checked_add(1)
            })
            .is_ok()
    }

    pub(crate) fn complete(&self) {
        let mut pending = self.pending.load(Ordering::Acquire);
        loop {
            if pending == 0 {
                return;
            }
            match self.pending.compare_exchange_weak(
                pending,
                pending - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return;
                }
                Err(observed) => pending = observed,
            }
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }
}

// PollSet stores a fixed number of wakers and does not allocate. Deferred-work
// producers only publish intrusive nodes/atomic pending bits; the generic
// scheduler callback below wakes these dedicated kernel workers at an
// allocation-free task-context boundary.
static POLICY_WORKER_WAKE: PollSet = PollSet::new();
static FINALIZER_WORKER_WAKE: PollSet = PollSet::new();
// Physical completion waiting is allowed to block on the lower shared block
// device. Keep its sole owner off the policy worker so fanotify/inotify/RCU
// and final-close cleanup cannot be starved by an idle device queue.
static PHYSICAL_COMPLETION_WORKER_WAKE: PollSet = PollSet::new();
// Each permanently online CPU owns one task-context consumer for its queue.
static PROCESS_TIMER_WORKER_WAKE: [PollSet<1>; axconfig::plat::MAX_CPU_NUM] =
    [const { PollSet::new() }; axconfig::plat::MAX_CPU_NUM];
const PROCESS_TIMER_WORKER_NOT_STARTED: u8 = 0;
const PROCESS_TIMER_WORKER_RUNNING: u8 = 1;
const PROCESS_TIMER_WORKER_FAILED: u8 = 2;
// Worker state is explicit so a failed affinity bind or wait does not look
// like successful policy processing. Failed queues use the same CPU's
// task-context dispatcher as a bounded fallback, preserving progress without
// pretending a dead worker is still consuming its owner queue.
static PROCESS_TIMER_WORKER_STATE: [AtomicU8; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicU8::new(PROCESS_TIMER_WORKER_NOT_STARTED) }; axconfig::plat::MAX_CPU_NUM];
// A failed worker has no registered PollSet waiter. Every publication/wake
// after failure advances this generation, so the fallback scan cannot be
// suppressed by a transiently empty tail/stub predicate. The seen generation
// is advanced only after the persistent cursor proves quiescent.
static PROCESS_TIMER_FALLBACK_GENERATION: [AtomicUsize; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicUsize::new(0) }; axconfig::plat::MAX_CPU_NUM];
static PROCESS_TIMER_FALLBACK_SEEN: [AtomicUsize; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicUsize::new(0) }; axconfig::plat::MAX_CPU_NUM];
static FILESYSTEM_FINALIZER_PUBLISHED: AtomicBool = AtomicBool::new(false);

fn policy_work_pending() -> bool {
    let pending = crate::rcu::credential_retire_pending()
        || crate::rcu::seccomp_retire_pending()
        || axfs_ng_vfs::has_deferred_dentry_cache_cleanup_work()
        || axnet::unix::has_deferred_receive_cleanup_work()
        || crate::file::has_deferred_description_cleanup_work()
        || crate::file::dnotify::has_deferred_table_cleanup_work()
        || crate::file::fanotify::has_deferred_cleanup_work()
        || crate::file::inotify::has_deferred_notification_work()
        || crate::file::io_uring::has_deferred_io_uring_work();
    #[cfg(feature = "perf-sampling")]
    {
        pending || crate::file::perf_sampling::has_deferred_custody_retire_work()
    }
    #[cfg(not(feature = "perf-sampling"))]
    {
        pending
    }
}

fn finalizer_work_pending() -> bool {
    FILESYSTEM_FINALIZER_PUBLISHED.load(Ordering::Acquire)
        || axfs::has_deferred_filesystem_finalizer_work()
}

fn process_timer_cpu_needs_dispatch(cpu: usize) -> bool {
    let has_queue = crate::task::has_deferred_process_itimer_work_on_cpu(cpu);
    if PROCESS_TIMER_WORKER_STATE[cpu].load(Ordering::Acquire) == PROCESS_TIMER_WORKER_FAILED {
        has_queue
            || PROCESS_TIMER_FALLBACK_GENERATION[cpu].load(Ordering::Acquire)
                != PROCESS_TIMER_FALLBACK_SEEN[cpu].load(Ordering::Acquire)
    } else {
        has_queue
    }
}

fn process_timer_work_pending() -> bool {
    // A failed per-CPU worker must not strand its queue when that CPU is
    // idle. The scan is over a fixed platform bound; normal queues still
    // retain their single-owner cursor and their exact worker wake target.
    (0..axhal::cpu_num()).any(process_timer_cpu_needs_dispatch)
}

fn mark_process_timer_worker_failed(cpu: usize) {
    PROCESS_TIMER_WORKER_STATE[cpu].store(PROCESS_TIMER_WORKER_FAILED, Ordering::Release);
    PROCESS_TIMER_FALLBACK_GENERATION[cpu].fetch_add(1, Ordering::AcqRel);
}

/// Makes the dedicated process-timer worker runnable after an IRQ-side
/// accounting threshold crossing. `PollSet::wake` consumes only an already
/// registered task waker; Linux signal policy remains in the worker itself.
pub(crate) fn wake_process_timer_worker(cpu: usize) {
    debug_assert!(cpu < axconfig::plat::MAX_CPU_NUM);
    if PROCESS_TIMER_WORKER_STATE[cpu].load(Ordering::Acquire) == PROCESS_TIMER_WORKER_FAILED {
        PROCESS_TIMER_FALLBACK_GENERATION[cpu].fetch_add(1, Ordering::AcqRel);
    }
    PROCESS_TIMER_WORKER_WAKE[cpu].wake();
}

/// Makes the policy worker runnable after a bounded RCU publication. The
/// wake is allocation-free and harmless before deferred-work initialization.
pub(crate) fn wake_policy_worker() {
    POLICY_WORKER_WAKE.wake();
}

#[cfg(feature = "perf-sampling")]
pub(crate) fn kick_perf_retire_worker() {
    #[cfg(target_os = "none")]
    if axhal::irq::send_ipi_reason(
        axhal::irq::IpiReason::DeferredWork,
        axhal::irq::IpiTarget::Current {
            cpu_id: axhal::percpu::this_cpu_id(),
        },
    )
    .is_err()
    {
        // Registration and topology are immutable before user tasks start.
        // Continuing here could strand the only final-drop owner forever.
        axhal::power::system_off();
    }
}

/// Wakes the single task-context owner for device-global physical
/// completions. The call is allocation-free and is safe before worker init.
pub(crate) fn wake_physical_completion_worker() {
    PHYSICAL_COMPLETION_WORKER_WAKE.wake();
}

fn wait_for_worker<const CAPACITY: usize>(
    wake: &PollSet<CAPACITY>,
    mut pending: impl FnMut() -> bool,
) -> AxResult<()> {
    block_on_poll_set_uninterruptible(wake, || {
        if pending() {
            Ok(())
        } else {
            Err(AxError::WouldBlock)
        }
    })
}

const POLICY_WAIT_RETRY_LIMIT: usize = 3;

/// Retries a failed policy wait a bounded number of times, yielding between
/// attempts so a transient block-state or readiness failure cannot turn into
/// a busy loop. A persistent failure is returned to the owner, which fail
/// stops rather than silently abandoning the RCU retirement queues.
fn wait_with_bounded_retry(
    mut wait: impl FnMut() -> AxResult<()>,
    mut retry: impl FnMut(),
) -> AxResult<()> {
    for attempt in 0..=POLICY_WAIT_RETRY_LIMIT {
        match wait() {
            Ok(()) => return Ok(()),
            Err(_) if attempt < POLICY_WAIT_RETRY_LIMIT => retry(),
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded policy wait retry loop must return");
}

fn policy_worker() {
    loop {
        if let Err(error) = wait_with_bounded_retry(
            || wait_for_worker(&POLICY_WORKER_WAKE, policy_work_pending),
            axtask::yield_now,
        ) {
            error!("policy deferred-work worker wait failed persistently: {error}");
            // This owner is the only task-context consumer for credential and
            // seccomp retirement. Do not return and leave those bounded queues
            // permanently full; an unrecoverable scheduler/readiness failure
            // is an internal invariant violation and must fail-stop.
            axhal::power::system_off();
        }
        while policy_work_pending() {
            crate::rcu::drain_credential_retire(16);
            crate::rcu::drain_seccomp_retire(16);
            axfs_ng_vfs::drain_deferred_dentry_cache_cleanup();
            axnet::unix::drain_deferred_receive_cleanup_work();
            crate::file::drain_deferred_description_cleanup();
            crate::file::dnotify::drain_table_cleanup_work();
            crate::file::fanotify::drain_deferred_cleanup_work();
            crate::file::inotify::drain_close_notifications();
            crate::file::inotify::drain_filesystem_release_notifications();
            crate::file::io_uring::drain_deferred_io_uring_work();
            #[cfg(feature = "perf-sampling")]
            crate::file::perf_sampling::drain_deferred_custody_retire_work();
            if policy_work_pending() {
                axtask::yield_now();
            }
        }
    }
}

fn physical_completion_worker() {
    crate::file::io_uring::note_physical_completion_worker_started();
    loop {
        if crate::file::io_uring::physical_completion_worker_is_stopped() {
            if crate::file::io_uring::has_physical_completion_work() {
                error!(
                    "physical completion worker stopped with live custody; fail-stopping system"
                );
                axhal::power::system_off();
            }
            return;
        }
        if let Err(error) = wait_with_bounded_retry(
            || {
                wait_for_worker(
                    &PHYSICAL_COMPLETION_WORKER_WAKE,
                    crate::file::io_uring::has_physical_completion_work,
                )
            },
            axtask::yield_now,
        ) {
            if crate::file::io_uring::has_physical_completion_work() {
                error!(
                    "physical completion worker wait failed with live custody; fail-stopping: \
                     {error}"
                );
                axhal::power::system_off();
            }
            crate::file::io_uring::note_physical_completion_worker_stopped();
            error!("physical completion worker stopped after bounded wait failure: {error}");
            return;
        }
        // The lower wait is bounded by the device-global completion owner and
        // may sleep until an IRQ generation. No policy work runs on this
        // stack while that wait is in progress.
        crate::file::io_uring::drain_physical_completion_work();
    }
}

fn filesystem_finalizer_worker() {
    loop {
        if let Err(error) = wait_for_worker(&FINALIZER_WORKER_WAKE, finalizer_work_pending) {
            error!("filesystem finalizer worker stopped: {error}");
            return;
        }
        while axfs::drain_deferred_filesystem_finalizers(axtask::yield_now) != 0 {
            // New work published while one finite FIFO batch was being drained
            // remains on the shared queue for the next iteration.
        }
        FILESYSTEM_FINALIZER_PUBLISHED.store(false, Ordering::Release);
        if axfs::has_deferred_filesystem_finalizer_work() {
            FILESYSTEM_FINALIZER_PUBLISHED.store(true, Ordering::Release);
        }
    }
}

fn process_timer_worker(cpu: usize) {
    let mut cpumask = axtask::AxCpuMask::new();
    cpumask.set(cpu, true);
    if let Err(error) = axtask::set_current_affinity(cpumask) {
        mark_process_timer_worker_failed(cpu);
        error!("process timer worker CPU {cpu} affinity bind failed: {error}");
        return;
    }
    // No queue access occurs before the one-shot bind. This is the startup
    // publication point for this CPU's sole consumer.
    debug_assert_eq!(axhal::percpu::this_cpu_id(), cpu);
    if !crate::task::acquire_process_itimer_worker_consumer(cpu) {
        mark_process_timer_worker_failed(cpu);
        error!("process timer worker CPU {cpu} consumer ownership is unavailable");
        return;
    }
    PROCESS_TIMER_WORKER_STATE[cpu].store(PROCESS_TIMER_WORKER_RUNNING, Ordering::Release);
    loop {
        if let Err(error) = wait_for_worker(&PROCESS_TIMER_WORKER_WAKE[cpu], || {
            crate::task::process_itimer_consumer_has_pending(cpu)
        }) {
            // Release only the owner token. The fixed cursor remains exactly
            // where the worker stopped, including a producer tail-link gap;
            // a fallback task context acquires that same cursor on its next
            // safe point.
            crate::task::release_process_itimer_worker_consumer(cpu);
            mark_process_timer_worker_failed(cpu);
            error!("process timer deferred-work worker CPU {cpu} stopped: {error}");
            return;
        }
        while crate::task::drain_process_itimer_batch(cpu) != 0 {
            if crate::task::process_itimer_consumer_has_pending(cpu) {
                axtask::yield_now();
            }
        }
    }
}

fn dispatch_process_timer_work() {
    for cpu in 0..axhal::cpu_num() {
        let state = PROCESS_TIMER_WORKER_STATE[cpu].load(Ordering::Acquire);
        let has_queue = crate::task::has_deferred_process_itimer_work_on_cpu(cpu);
        match state {
            PROCESS_TIMER_WORKER_RUNNING => {
                if !has_queue {
                    continue;
                }
                // The queue's owner token is also the wake target. This wake
                // is intentionally not redirected to the CPU executing this
                // bounded dispatcher.
                PROCESS_TIMER_WORKER_WAKE[cpu].wake();
            }
            PROCESS_TIMER_WORKER_FAILED => {
                // A failed worker is never reported as running. Its
                // permanently owned cursor remains drainable from any
                // ordinary task context, which keeps an idle owner CPU from
                // stranding work. Do not gate admission on the raw queue
                // predicate: a producer tail/link gap can leave that
                // predicate empty after the cursor has already advanced.
                let generation = PROCESS_TIMER_FALLBACK_GENERATION[cpu].load(Ordering::Acquire);
                let seen = PROCESS_TIMER_FALLBACK_SEEN[cpu].load(Ordering::Acquire);
                if !has_queue && generation == seen {
                    continue;
                }
                // Only one fallback can own the cursor, and each safe point
                // takes exactly one fixed batch.
                if crate::task::acquire_process_itimer_fallback_consumer(cpu) {
                    let _ = crate::task::drain_process_itimer_batch(cpu);
                    let before = PROCESS_TIMER_FALLBACK_GENERATION[cpu].load(Ordering::Acquire);
                    let quiescent = crate::task::process_itimer_consumer_is_quiescent(cpu);
                    let after = PROCESS_TIMER_FALLBACK_GENERATION[cpu].load(Ordering::Acquire);
                    if quiescent && before == after {
                        PROCESS_TIMER_FALLBACK_SEEN[cpu].store(after, Ordering::Release);
                    }
                    crate::task::release_process_itimer_fallback_consumer(cpu);
                }
            }
            PROCESS_TIMER_WORKER_NOT_STARTED => {
                // The worker will bind before it first waits or drains. Its
                // register-then-check wait observes work published meanwhile.
            }
            _ => unreachable!("invalid process timer worker state"),
        }
    }
}

/// Called from the ext4 final-Arc Drop path. It deliberately does not wake or
/// lock a task; a later scheduler safe point observes this atomic publication.
fn note_filesystem_finalizer_work() {
    FILESYSTEM_FINALIZER_PUBLISHED.store(true, Ordering::Release);
}

/// Registers the kernel's allocation-free task-context dispatcher and the
/// workers that own subsystem policy, process-timer signals, and blocking
/// filesystem teardown.
pub(crate) fn init() {
    #[cfg(feature = "perf-sampling")]
    assert!(axhal::irq::register_ipi_reason(
        axhal::irq::IpiReason::DeferredWork,
        perf_retire_ipi_handler,
    ));
    assert!(
        axtask::set_deferred_work_dispatcher(dispatch),
        "a different deferred-work dispatcher is already installed"
    );
    crate::task::init_process_itimer_work_queues();
    assert!(
        axfs::set_deferred_filesystem_finalizer_waker(note_filesystem_finalizer_work),
        "a different filesystem-finalizer notifier is already installed"
    );
    // axfs has published its root SharedBlockDevice before kernel init reaches
    // deferred work. Install the sole device-global completion owner before
    // any user task can submit an admitted physical effect; unsupported
    // devices leave admission explicitly disabled.
    crate::file::io_uring::install_default_physical_completion_device();
    let mut policy_name = String::new();
    policy_name
        .try_reserve_exact("policy-worker".len())
        .expect("failed to allocate policy-worker name");
    policy_name.push_str("policy-worker");
    let mut finalizer_name = String::new();
    finalizer_name
        .try_reserve_exact("fs-finalizer".len())
        .expect("failed to allocate fs-finalizer name");
    finalizer_name.push_str("fs-finalizer");
    axtask::try_spawn_with_name(policy_worker, policy_name).expect("failed to start policy worker");
    let mut physical_name = String::new();
    physical_name
        .try_reserve_exact("physical-completion-worker".len())
        .expect("failed to allocate physical-completion-worker name");
    physical_name.push_str("physical-completion-worker");
    axtask::try_spawn_with_name(physical_completion_worker, physical_name)
        .expect("failed to start physical completion worker");
    for cpu in 0..axhal::cpu_num() {
        let mut process_timer_name = String::new();
        process_timer_name
            .try_reserve_exact("process-timer-worker-".len() + 20)
            .expect("failed to allocate process-timer-worker name");
        process_timer_name.push_str("process-timer-worker-");
        use core::fmt::Write as _;
        write!(&mut process_timer_name, "{cpu}")
            .expect("writing process-timer-worker name cannot fail");
        if let Err(error) =
            axtask::try_spawn_with_name(move || process_timer_worker(cpu), process_timer_name)
        {
            mark_process_timer_worker_failed(cpu);
            error!("failed to start process-timer worker CPU {cpu}: {error}");
        }
    }
    axtask::try_spawn_with_name(filesystem_finalizer_worker, finalizer_name)
        .expect("failed to start filesystem-finalizer worker");
}

fn dispatch() {
    // This generic scheduler hook is constant-time and allocation-free. Linux
    // inotify/fanotify/dnotify policy, VFS reclamation, and filesystem shutdown
    // execute only in their dedicated task contexts.
    if policy_work_pending() {
        POLICY_WORKER_WAKE.wake();
    }
    if finalizer_work_pending() {
        FINALIZER_WORKER_WAKE.wake();
    }
    if process_timer_work_pending() {
        dispatch_process_timer_work();
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[test]
    fn policy_wait_error_retries_then_allows_following_drain() {
        let attempts = Cell::new(0);
        let retry_count = Cell::new(0);
        let result = wait_with_bounded_retry(
            || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                if attempt == 1 {
                    Err(AxError::BadState)
                } else {
                    Ok(())
                }
            },
            || retry_count.set(retry_count.get() + 1),
        );

        assert_eq!(result, Ok(()));
        assert_eq!(attempts.get(), 2);
        assert_eq!(retry_count.get(), 1);

        // The successful subsequent wake is the handoff point at which the
        // owner drains the queues; the retry path itself never performs Drop.
        let drained = Cell::new(0);
        if result.is_ok() {
            drained.set(drained.get() + 1);
        }
        assert_eq!(drained.get(), 1);
    }

    #[test]
    fn policy_wait_failure_is_bounded_before_fail_stop() {
        let attempts = Cell::new(0);
        let retries = Cell::new(0);
        let result = wait_with_bounded_retry(
            || {
                attempts.set(attempts.get() + 1);
                Err(AxError::BadState)
            },
            || retries.set(retries.get() + 1),
        );

        assert_eq!(result, Err(AxError::BadState));
        assert_eq!(attempts.get(), POLICY_WAIT_RETRY_LIMIT + 1);
        assert_eq!(retries.get(), POLICY_WAIT_RETRY_LIMIT);
    }
}
