use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axerrno::{AxError, AxResult};
use axpoll::PollSet;

use crate::readiness::block_on_poll_set_uninterruptible;

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
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
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
// Exactly one dedicated consumer owns process-timer signal publication.
static PROCESS_TIMER_WORKER_WAKE: PollSet<1> = PollSet::new();
static FILESYSTEM_FINALIZER_PUBLISHED: AtomicBool = AtomicBool::new(false);

fn policy_work_pending() -> bool {
    axfs_ng_vfs::has_deferred_dentry_cache_cleanup_work()
        || axnet::unix::has_deferred_receive_cleanup_work()
        || crate::file::has_deferred_description_cleanup_work()
        || crate::file::dnotify::has_deferred_table_cleanup_work()
        || crate::file::fanotify::has_deferred_cleanup_work()
        || crate::file::inotify::has_deferred_notification_work()
        || crate::file::io_uring::has_deferred_io_uring_work()
}

fn finalizer_work_pending() -> bool {
    FILESYSTEM_FINALIZER_PUBLISHED.load(Ordering::Acquire)
        || axfs::has_deferred_filesystem_finalizer_work()
}

fn process_timer_work_pending() -> bool {
    crate::task::has_deferred_process_itimer_work()
}

/// Makes the dedicated process-timer worker runnable after an IRQ-side
/// accounting threshold crossing. `PollSet::wake` consumes only an already
/// registered task waker; Linux signal policy remains in the worker itself.
pub(crate) fn wake_process_timer_worker() {
    PROCESS_TIMER_WORKER_WAKE.wake();
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

fn policy_worker() {
    loop {
        if let Err(error) = wait_for_worker(&POLICY_WORKER_WAKE, policy_work_pending) {
            error!("policy deferred-work worker stopped: {error}");
            return;
        }
        while policy_work_pending() {
            axfs_ng_vfs::drain_deferred_dentry_cache_cleanup();
            axnet::unix::drain_deferred_receive_cleanup_work();
            crate::file::drain_deferred_description_cleanup();
            crate::file::dnotify::drain_table_cleanup_work();
            crate::file::fanotify::drain_deferred_cleanup_work();
            crate::file::inotify::drain_close_notifications();
            crate::file::inotify::drain_filesystem_release_notifications();
            crate::file::io_uring::drain_deferred_io_uring_work();
            if policy_work_pending() {
                axtask::yield_now();
            }
        }
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

fn process_timer_worker() {
    let mut consumer = crate::task::ProcessITimerWorkConsumer::new();
    loop {
        if let Err(error) = wait_for_worker(&PROCESS_TIMER_WORKER_WAKE, || consumer.has_pending()) {
            error!("process timer deferred-work worker stopped: {error}");
            return;
        }
        while consumer.drain_batch() != 0 {
            if consumer.has_pending() {
                axtask::yield_now();
            }
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
    assert!(
        axtask::set_deferred_work_dispatcher(dispatch),
        "a different deferred-work dispatcher is already installed"
    );
    assert!(
        axfs::set_deferred_filesystem_finalizer_waker(note_filesystem_finalizer_work),
        "a different filesystem-finalizer notifier is already installed"
    );
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
    let mut process_timer_name = String::new();
    process_timer_name
        .try_reserve_exact("process-timer-worker".len())
        .expect("failed to allocate process-timer-worker name");
    process_timer_name.push_str("process-timer-worker");
    axtask::try_spawn_with_name(policy_worker, policy_name).expect("failed to start policy worker");
    axtask::try_spawn_with_name(process_timer_worker, process_timer_name)
        .expect("failed to start process-timer worker");
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
        PROCESS_TIMER_WORKER_WAKE.wake();
    }
}
