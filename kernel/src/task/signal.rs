use alloc::sync::{Arc, Weak};
use core::{convert::Infallible, future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use axtask::{
    TaskInner, current,
    future::{block_on, interruptible},
};
use linux_raw_sys::general::{RLIMIT_SIGPENDING, SI_TIMER};
use starry_process::Pid;
use starry_signal::{
    DefaultSignalAction, PreparedSignal, SignalDisposition, SignalInfo, SignalOSAction,
    SignalQueueAccount, SignalSet, Signo, api::ThreadSignalManager,
};

use super::{
    AsThread, ContinueResult, ProcessData, Thread, acknowledge_posix_timer_signal, do_exit,
    get_process_data, get_process_group, get_task, get_visible_task,
};

fn notify_tracer_or_parent_stop_continue(proc_data: &ProcessData) {
    let notify_pid = proc_data
        .ptrace_tracer()
        .or_else(|| proc_data.proc.parent().map(|parent| parent.pid()));
    if let Some(pid) = notify_pid {
        let _ = send_signal_to_process(pid, Some(SignalInfo::new_kernel(Signo::SIGCHLD)));
        if let Ok(waiter) = get_process_data(pid) {
            waiter.child_exit_event.wake();
        }
    }
}

fn try_ptrace_signal_stop(
    proc_data: &ProcessData,
    record: PtraceSignalRecord,
) -> Result<(), PtraceSignalRecord> {
    let signo = record.info().signo();
    if matches!(signo, Signo::SIGKILL | Signo::SIGCONT) || proc_data.ptrace_tracer().is_none() {
        return Err(record);
    }
    proc_data.try_ptrace_signal_stop(record)?;
    info!(
        "Stopping traced process {} by signal {}",
        proc_data.proc.pid(),
        signo as u8
    );
    notify_tracer_or_parent_stop_continue(proc_data);
    interrupt_stop_siblings(proc_data);
    Ok(())
}

fn publish_prepared_thread(
    signal: &ThreadSignalManager,
    sig: SignalInfo,
    prepared: PreparedSignal,
) -> starry_signal::api::ThreadSignalSendOutcome {
    let mut prepared = Some(prepared);
    let outcome = signal.try_send_signal_with(sig, |_| {
        Ok::<_, Infallible>(prepared.take().expect("prepared signal consumed once"))
    });
    // Ignore/coalescing skips the closure. Release retained ownership only
    // after the signal manager has released all of its spin guards.
    drop(prepared);
    match outcome {
        Ok(outcome) => outcome,
        Err(error) => match error {},
    }
}

#[derive(Clone, Copy)]
enum SignalQueuePolicy {
    BestEffortKill,
    QueueRequired,
}

/// Exact signal ownership retained across a ptrace signal-delivery stop.
pub(crate) struct PtraceSignalRecord {
    target: PtraceSignalTarget,
    prepared: PreparedSignal,
}

enum PtraceSignalTarget {
    Process,
    Thread {
        tid: Pid,
        signal: Weak<ThreadSignalManager>,
    },
}

impl PtraceSignalRecord {
    fn process(prepared: PreparedSignal) -> Self {
        Self {
            target: PtraceSignalTarget::Process,
            prepared,
        }
    }

    fn thread(thr: &Thread, prepared: PreparedSignal) -> Self {
        Self {
            target: PtraceSignalTarget::Thread {
                tid: thr.tid(),
                signal: alloc::sync::Arc::downgrade(&thr.signal),
            },
            prepared,
        }
    }

    pub(crate) fn info(&self) -> &SignalInfo {
        self.prepared.info()
    }

    pub(crate) fn replace_info(&mut self, mut info: SignalInfo) -> Option<SignalInfo> {
        if self.info().code() == SI_TIMER {
            if info.code() != SI_TIMER {
                return None;
            }
            let current = unsafe {
                self.info()
                    .0
                    .__bindgen_anon_1
                    .__bindgen_anon_1
                    ._sifields
                    ._timer
            };
            let replacement =
                unsafe { &mut info.0.__bindgen_anon_1.__bindgen_anon_1._sifields._timer };
            // These fields identify kernel ownership and are not part of the
            // tracer-mutable payload, despite sharing the userspace layout.
            replacement._tid = current._tid;
            replacement._sys_private = current._sys_private;
        }
        self.prepared.replace_info(info)
    }
}

fn prepare_signal_with_accounts(
    info: SignalInfo,
    policy: SignalQueuePolicy,
    limit: u64,
    per_user: &alloc::sync::Arc<SignalQueueAccount>,
    global: &alloc::sync::Arc<SignalQueueAccount>,
) -> AxResult<PreparedSignal> {
    if !info.signo().is_realtime() {
        return Ok(PreparedSignal::unqueued(info));
    }

    match PreparedSignal::try_accounted(info.clone(), per_user, limit, global) {
        Ok(prepared) => Ok(prepared),
        Err(_) if matches!(policy, SignalQueuePolicy::BestEffortKill) => {
            Ok(PreparedSignal::unqueued(info))
        }
        Err(_) => Err(AxError::WouldBlock),
    }
}

fn prepare_signal_for_target(
    target: &ProcessData,
    info: SignalInfo,
    policy: SignalQueuePolicy,
) -> AxResult<PreparedSignal> {
    if !info.signo().is_realtime() {
        return Ok(PreparedSignal::unqueued(info));
    }
    let limit = target.rlim.read()[RLIMIT_SIGPENDING].current;
    let cred = target.current_cred();
    match cred.user_ns().try_signal_queue_accounts(cred.ids().ruid) {
        Ok((per_user, global)) => {
            prepare_signal_with_accounts(info, policy, limit, &per_user, &global)
        }
        Err(_) if matches!(policy, SignalQueuePolicy::BestEffortKill) => {
            Ok(PreparedSignal::unqueued(info))
        }
        Err(_) => Err(AxError::WouldBlock),
    }
}

/// Prepares one mandatory queued record for a process-owned kernel event.
///
/// Callers may retain the returned record until the event fires. All queue
/// accounting and allocation has already completed, so later publication is
/// allocation-free and cannot lose real-time siginfo under pressure.
pub(crate) fn prepare_queued_signal_for_process(
    target: &ProcessData,
    info: SignalInfo,
) -> AxResult<PreparedSignal> {
    prepare_signal_for_target(target, info, SignalQueuePolicy::QueueRequired)
}

pub fn check_signals(
    thr: &Thread,
    uctx: &mut UserContext,
    restore_blocked: Option<SignalSet>,
) -> bool {
    let Some(delivered) = thr.signal.check_signals(uctx, restore_blocked) else {
        return false;
    };
    acknowledge_posix_timer_signal(&thr.proc_data, &delivered.info);

    let signo = delivered.info.signo();
    thr.finish_signal_delivery(delivered.os_action, delivered.restartable_handler);
    match delivered.os_action {
        SignalOSAction::Terminate => {
            do_exit(signo as i32, true);
        }
        SignalOSAction::CoreDump => {
            if let Err(e) = super::coredump::generate_core_dump(thr, uctx, signo as u8) {
                warn!("Core dump failed: {e:?}");
            }
            do_exit((signo as i32) | 0x80, true);
        }
        SignalOSAction::Stop => {
            do_stop(thr, uctx, signo as u8);
        }
        SignalOSAction::Continue => {
            do_continue(&thr.proc_data);
        }
        SignalOSAction::Handler => {
            // do nothing
        }
    }
    true
}

pub(crate) fn has_pending_syscall_signal(thr: &Thread) -> bool {
    let pending = thr.signal.pending();
    if pending.is_empty() {
        return false;
    }

    let blocked = thr.signal.blocked();
    let actions = thr.proc_data.signal.actions.lock();
    for raw in 1..=64u8 {
        let Some(signo) = Signo::from_repr(raw) else {
            continue;
        };
        if !pending.has(signo) || blocked.has(signo) {
            continue;
        }

        let ignored = match actions[signo].disposition {
            SignalDisposition::Ignore => true,
            SignalDisposition::Default => {
                matches!(signo.default_action(), DefaultSignalAction::Ignore)
            }
            SignalDisposition::Handler(_) => false,
        };
        if !ignored {
            return true;
        }
    }
    false
}

pub(crate) fn has_pending_fatal_signal(thr: &Thread) -> bool {
    let pending = thr.signal.pending();
    if pending.is_empty() {
        return false;
    }

    let actions = thr.proc_data.signal.actions.lock();
    for raw in 1..=64u8 {
        let Some(signo) = Signo::from_repr(raw) else {
            continue;
        };
        if !pending.has(signo) {
            continue;
        }
        if matches!(
            actions[signo].disposition,
            SignalDisposition::Default
                if matches!(
                    signo.default_action(),
                    DefaultSignalAction::Terminate | DefaultSignalAction::CoreDump
                )
        ) {
            return true;
        }
    }
    false
}

pub fn with_blocked_signals<R>(
    blocked: Option<SignalSet>,
    f: impl FnOnce() -> AxResult<R>,
) -> AxResult<R> {
    let curr = current();
    let sig = &curr.as_thread().signal;

    let old_blocked = blocked.map(|set| sig.set_blocked(set));
    let result = f();
    if let Some(old) = old_blocked {
        sig.set_blocked(old);
    }
    result
}

fn send_signal_thread_inner_with(
    task: &TaskInner,
    thr: &Thread,
    sig: SignalInfo,
    policy: SignalQueuePolicy,
) -> AxResult<bool> {
    let signo = sig.signo();
    if signo == Signo::SIGCONT {
        do_continue(&thr.proc_data);
    }

    if thr.proc_data.ptrace_tracer().is_some() && !matches!(signo, Signo::SIGKILL | Signo::SIGCONT)
    {
        let prepared = prepare_signal_for_target(&thr.proc_data, sig, policy)?;
        match try_ptrace_signal_stop(&thr.proc_data, PtraceSignalRecord::thread(thr, prepared)) {
            Ok(()) => {
                task.interrupt();
                return Ok(true);
            }
            Err(record) => {
                let info = record.info().clone();
                let outcome = publish_prepared_thread(&thr.signal, info, record.prepared);
                if outcome.wake {
                    task.interrupt();
                }
                return Ok(outcome.published);
            }
        }
    }

    let outcome = thr.signal.try_send_signal_with(sig, |info| {
        prepare_signal_for_target(&thr.proc_data, info, policy)
    })?;
    if outcome.wake {
        task.interrupt();
    }
    Ok(outcome.published)
}

pub(crate) fn send_signal_thread_inner(task: &TaskInner, thr: &Thread, sig: SignalInfo) {
    let _ = send_signal_thread_inner_with(task, thr, sig, SignalQueuePolicy::BestEffortKill);
}

/// Sends a resolved thread-directed signal with mandatory RT admission.
pub(crate) fn send_queued_signal_thread_inner(
    task: &TaskInner,
    thr: &Thread,
    sig: SignalInfo,
) -> AxResult<bool> {
    send_signal_thread_inner_with(task, thr, sig, SignalQueuePolicy::QueueRequired)
}

/// Sends a signal to a thread.
pub fn send_signal_to_thread(tgid: Option<Pid>, tid: Pid, sig: Option<SignalInfo>) -> AxResult<()> {
    let task = get_task(tid)?;
    let thread = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
    if tgid.is_some_and(|tgid| thread.proc_data.proc.pid() != tgid) {
        return Err(AxError::NoSuchProcess);
    }

    if let Some(sig) = sig {
        info!("Send signal {:?} to thread {}", sig.signo(), tid);
        send_signal_thread_inner(&task, thread, sig);
    }

    Ok(())
}

/// Sends a signal to a thread using the user-visible TID namespace.
pub fn send_signal_to_visible_thread(
    tgid: Option<Pid>,
    tid: Pid,
    sig: Option<SignalInfo>,
) -> AxResult<()> {
    let task = get_visible_task(tid)?;
    let thread = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
    if tgid.is_some_and(|tgid| thread.proc_data.proc.pid() != tgid) {
        return Err(AxError::NoSuchProcess);
    }

    if let Some(sig) = sig {
        info!("Send signal {:?} to thread {}", sig.signo(), tid);
        send_signal_thread_inner(&task, thread, sig);
    }

    Ok(())
}

/// Sends a signal to a visible thread with mandatory RT queue admission.
pub(crate) fn send_queued_signal_to_visible_thread(
    tgid: Option<Pid>,
    tid: Pid,
    sig: Option<SignalInfo>,
) -> AxResult<bool> {
    let task = get_visible_task(tid)?;
    let thread = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
    if tgid.is_some_and(|tgid| thread.proc_data.proc.pid() != tgid) {
        return Err(AxError::NoSuchProcess);
    }

    if let Some(sig) = sig {
        info!("Queue signal {:?} to thread {}", sig.signo(), tid);
        return send_queued_signal_thread_inner(&task, thread, sig);
    }
    Ok(false)
}

/// Sends a signal to a process.
pub fn send_signal_to_process(pid: Pid, sig: Option<SignalInfo>) -> AxResult<()> {
    let proc_data = get_process_data(pid)?;
    send_signal_to_process_data(&proc_data, sig)
}

fn wake_process_signal_target(proc_data: &ProcessData, signo: Signo, should_wake: bool) {
    if !should_wake {
        return;
    }
    for tid in proc_data.proc.thread_ids() {
        let Ok(task) = get_task(tid) else {
            continue;
        };
        let Some(thread) = task.try_as_thread() else {
            continue;
        };
        if !thread.signal.signal_blocked(signo) {
            task.interrupt();
        }
    }
}

fn publish_prepared_process(
    proc_data: &ProcessData,
    sig: SignalInfo,
    prepared: PreparedSignal,
) -> bool {
    let signo = sig.signo();
    let mut prepared = Some(prepared);
    let outcome = proc_data.signal.try_send_signal_with(sig, |_| {
        Ok::<_, Infallible>(prepared.take().expect("prepared signal consumed once"))
    });
    // Ignored and coalesced standard signals skip the closure. Release their
    // retained record and queue charge outside every signal-state lock.
    drop(prepared);
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => match error {},
    };
    wake_process_signal_target(proc_data, signo, outcome.wake_tid.is_some());
    outcome.published
}

fn send_signal_to_process_data_with_policy(
    proc_data: &ProcessData,
    sig: SignalInfo,
    policy: SignalQueuePolicy,
) -> AxResult<bool> {
    let signo = sig.signo();
    if signo == Signo::SIGCONT {
        do_continue(proc_data);
    }

    if proc_data.ptrace_tracer().is_some() && !matches!(signo, Signo::SIGKILL | Signo::SIGCONT) {
        let prepared = prepare_signal_for_target(proc_data, sig, policy)?;
        match try_ptrace_signal_stop(proc_data, PtraceSignalRecord::process(prepared)) {
            Ok(()) => return Ok(true),
            Err(record) => {
                let info = record.info().clone();
                return Ok(publish_prepared_process(proc_data, info, record.prepared));
            }
        }
    }

    let outcome = proc_data.signal.try_send_signal_with(sig, |info| {
        prepare_signal_for_target(proc_data, info, policy)
    })?;
    wake_process_signal_target(proc_data, signo, outcome.wake_tid.is_some());
    Ok(outcome.published)
}

/// Sends a signal to a process object that has already been resolved.
pub fn send_signal_to_process_data(
    proc_data: &ProcessData,
    sig: Option<SignalInfo>,
) -> AxResult<()> {
    if proc_data.proc.is_zombie() || proc_data.proc.thread_count() == 0 {
        return Err(AxError::NoSuchProcess);
    }
    if let Some(sig) = sig {
        let signo = sig.signo();
        info!("Send signal {signo:?} to process {}", proc_data.proc.pid());
        send_signal_to_process_data_with_policy(proc_data, sig, SignalQueuePolicy::BestEffortKill)?;
    }

    Ok(())
}

/// Sends a signal to a process with mandatory RT queue admission.
pub(crate) fn send_queued_signal_to_process_data(
    proc_data: &ProcessData,
    sig: Option<SignalInfo>,
) -> AxResult<bool> {
    if proc_data.proc.is_zombie() || proc_data.proc.thread_count() == 0 {
        return Err(AxError::NoSuchProcess);
    }
    if let Some(sig) = sig {
        return send_signal_to_process_data_with_policy(
            proc_data,
            sig,
            SignalQueuePolicy::QueueRequired,
        );
    }
    Ok(false)
}

/// Publishes a previously prepared process-directed kernel notification.
///
/// This path is used by one-shot facilities such as POSIX message queues,
/// where consuming the registration before allocating an RT sigqueue record
/// would otherwise lose both the notification and its siginfo.
pub(crate) fn send_prepared_signal_to_process_data(
    proc_data: &ProcessData,
    sig: SignalInfo,
    prepared: PreparedSignal,
) -> AxResult<bool> {
    if proc_data.proc.is_zombie() || proc_data.proc.thread_count() == 0 {
        return Err(AxError::NoSuchProcess);
    }

    let signo = sig.signo();
    if signo == Signo::SIGCONT {
        do_continue(proc_data);
    }
    if proc_data.ptrace_tracer().is_some() && !matches!(signo, Signo::SIGKILL | Signo::SIGCONT) {
        match try_ptrace_signal_stop(proc_data, PtraceSignalRecord::process(prepared)) {
            Ok(()) => return Ok(true),
            Err(record) => {
                let info = record.info().clone();
                return Ok(publish_prepared_process(proc_data, info, record.prepared));
            }
        }
    }
    Ok(publish_prepared_process(proc_data, sig, prepared))
}

fn publish_ptrace_target(
    proc_data: &ProcessData,
    target: PtraceSignalTarget,
    prepared: PreparedSignal,
) -> AxResult<bool> {
    let info = prepared.info().clone();
    let signo = info.signo();
    if signo == Signo::SIGCONT {
        do_continue(proc_data);
    }
    match target {
        PtraceSignalTarget::Process => Ok(publish_prepared_process(proc_data, info, prepared)),
        PtraceSignalTarget::Thread { tid, signal } => {
            let signal = signal.upgrade().ok_or(AxError::NoSuchProcess)?;
            let outcome = publish_prepared_thread(&signal, info, prepared);
            if outcome.wake
                && let Ok(task) = get_visible_task(tid)
                && let Some(thread) = task.try_as_thread()
                && Arc::ptr_eq(&thread.signal, &signal)
            {
                task.interrupt();
            }
            Ok(outcome.published)
        }
    }
}

/// Completes one ptrace signal-delivery stop without re-entering ptrace.
///
/// A zero signal discards the exact queued record. Reinjecting the same signal
/// publishes that record with its original accounting and siginfo; choosing a
/// different signal first acknowledges the original timer ownership and then
/// prepares a fresh no-info signal for the same target.
pub(crate) fn reinject_ptrace_signal(
    proc_data: &ProcessData,
    record: Option<PtraceSignalRecord>,
    requested: Option<Signo>,
) -> AxResult<()> {
    let Some(record) = record else {
        if let Some(signo) = requested {
            let info = SignalInfo::new_kernel(signo);
            let prepared =
                prepare_signal_for_target(proc_data, info, SignalQueuePolicy::BestEffortKill)?;
            publish_ptrace_target(proc_data, PtraceSignalTarget::Process, prepared)?;
        }
        return Ok(());
    };

    let original_info = record.info().clone();
    let original_signo = original_info.signo();
    let PtraceSignalRecord { target, prepared } = record;

    match requested {
        None => {
            acknowledge_posix_timer_signal(proc_data, &original_info);
            drop(prepared);
            Ok(())
        }
        Some(signo) if signo == original_signo => {
            match publish_ptrace_target(proc_data, target, prepared) {
                Ok(true) => Ok(()),
                Ok(false) => {
                    acknowledge_posix_timer_signal(proc_data, &original_info);
                    Ok(())
                }
                Err(err) => {
                    acknowledge_posix_timer_signal(proc_data, &original_info);
                    Err(err)
                }
            }
        }
        Some(signo) => {
            acknowledge_posix_timer_signal(proc_data, &original_info);
            drop(prepared);
            let info = SignalInfo::new_kernel(signo);
            let prepared =
                prepare_signal_for_target(proc_data, info, SignalQueuePolicy::BestEffortKill)?;
            publish_ptrace_target(proc_data, target, prepared)?;
            Ok(())
        }
    }
}

/// Sends a signal to a process group.
pub fn send_signal_to_process_group(pgid: Pid, sig: Option<SignalInfo>) -> AxResult<()> {
    let pg = get_process_group(pgid)?;

    if let Some(sig) = sig {
        info!("Send signal {:?} to process group {}", sig.signo(), pgid);
        for proc in pg.try_processes().map_err(|_| AxError::NoMemory)? {
            if proc.is_zombie() {
                continue;
            }
            send_signal_to_process(proc.pid(), Some(sig.clone()))?;
        }
    }

    Ok(())
}

/// Sends a fatal signal to the current process.
pub fn raise_signal_fatal(sig: SignalInfo) -> AxResult<()> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;

    let signo = sig.signo();
    info!("Send fatal signal {signo:?} to the current process");
    if let Some(tid) = proc_data.signal.send_unqueued_signal(sig)
        && let Ok(task) = get_task(tid)
    {
        task.interrupt();
    } else {
        // No task wants to handle the signal, abort the task
        do_exit(signo as i32, true);
    }

    Ok(())
}

/// Forces a synchronous signal onto the current thread.
///
/// Linux forced signals cannot be suppressed by an ignored disposition or a
/// blocked mask. A user handler is retained when it is already unblocked;
/// otherwise the disposition is reset to default before enqueueing.
pub(crate) fn force_signal_current_thread(sig: SignalInfo) {
    let curr = current();
    let thr = curr.as_thread();
    let signo = sig.signo();
    let was_blocked = thr.signal.signal_blocked(signo);

    {
        let mut actions = thr.proc_data.signal.actions.lock();
        if was_blocked || matches!(&actions[signo].disposition, SignalDisposition::Ignore) {
            actions[signo] = Default::default();
        }
    }

    if was_blocked {
        let mut blocked = thr.signal.blocked();
        blocked.remove(signo);
        thr.signal.set_blocked(blocked);
    }

    send_signal_thread_inner(&curr, thr, sig);
}

/// Stops the current process (all threads) due to a stop signal.
fn do_stop(thr: &Thread, uctx: &mut UserContext, signo: u8) {
    let proc_data = &thr.proc_data;

    // Ignore duplicate stop requests while a stop is already in progress.
    if !proc_data.begin_stop(signo) {
        return;
    }

    info!(
        "Stopping process {} by signal {}",
        proc_data.proc.pid(),
        signo
    );

    if proc_data.finish_stop() {
        notify_tracer_or_parent_stop_continue(proc_data);
        interrupt_stop_siblings(proc_data);
    }

    // Block this thread until the process is continued.
    wait_if_stopped(thr, uctx);
}

/// Continues a stopped process.
fn do_continue(proc_data: &ProcessData) {
    match proc_data.continue_job() {
        ContinueResult::None => {}
        ContinueResult::CanceledStopping => {
            info!(
                "Canceling in-flight stop for process {}",
                proc_data.proc.pid()
            );
        }
        ContinueResult::ResumedStopped => {
            info!("Continuing process {}", proc_data.proc.pid());
            notify_tracer_or_parent_stop_continue(proc_data);
        }
    }
}

/// Blocks the current thread while the process is in the stopped state.
///
/// Called from `check_signals` (the thread that received the stop signal)
/// and from the user task main loop (sibling threads).
pub fn wait_if_stopped(thr: &Thread, uctx: &mut UserContext) {
    let proc_data = &thr.proc_data;
    let tid = current().id().as_u64() as Pid;
    while !thr.pending_exit()
        && !proc_data.should_exit_for_exec(tid)
        && proc_data.should_wait_for_stop()
    {
        match block_on(interruptible(poll_fn(|cx| {
            if !proc_data.should_wait_for_stop()
                || thr.pending_exit()
                || proc_data.should_exit_for_exec(tid)
            {
                Poll::Ready(())
            } else {
                proc_data.stop_event.register(cx.waker());
                // Re-check after registration to avoid missed wake-ups.
                if !proc_data.should_wait_for_stop()
                    || thr.pending_exit()
                    || proc_data.should_exit_for_exec(tid)
                {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }))) {
            Ok(()) => {}
            Err(_) => handle_stopped_interrupt(thr, uctx),
        }
    }
}

fn interrupt_stop_siblings(proc_data: &ProcessData) {
    let curr_tid = current().id().as_u64() as Pid;
    for tid in proc_data.proc.thread_ids() {
        if tid != curr_tid {
            if let Ok(task) = get_task(tid) {
                task.interrupt();
            }
        }
    }
}

fn handle_stopped_interrupt(thr: &Thread, uctx: &mut UserContext) {
    let tid = current().id().as_u64() as Pid;
    if thr.proc_data.should_exit_for_exec(tid) {
        if has_pending_fatal_signal(thr) {
            while check_signals(thr, uctx, None) {}
        }
        return;
    }

    if thr.signal.pending().is_empty() {
        return;
    }

    while check_signals(thr, uctx, None) {}
}

pub fn notify_ptrace_attach_stop(proc_data: &ProcessData) {
    notify_tracer_or_parent_stop_continue(proc_data);
}

#[cfg(test)]
mod tests {
    use linux_raw_sys::general::{SI_MESGQ, SI_TIMER, sigval_t};
    use starry_signal::{PreparedSignal, SignalInfo, SignalQueueAccount, Signo};

    use super::{PtraceSignalRecord, SignalQueuePolicy, prepare_signal_with_accounts};

    #[test]
    fn strict_and_kill_realtime_paths_diverge_at_zero_limit() {
        let per_user = SignalQueueAccount::try_new(4).unwrap();
        let global = SignalQueueAccount::try_new(4).unwrap();

        let strict = prepare_signal_with_accounts(
            SignalInfo::new_user(Signo::SIGRTMIN, -6, 1),
            SignalQueuePolicy::QueueRequired,
            0,
            &per_user,
            &global,
        );
        assert!(matches!(strict, Err(axerrno::AxError::WouldBlock)));

        let fallback = prepare_signal_with_accounts(
            SignalInfo::new_user(Signo::SIGRTMIN, 0, 1),
            SignalQueuePolicy::BestEffortKill,
            0,
            &per_user,
            &global,
        );
        assert!(fallback.is_ok());
        assert_eq!(per_user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }

    #[test]
    fn ptrace_setsiginfo_preserves_private_timer_identity() {
        let mut original = SignalInfo::new_kernel(Signo::SIGRTMIN);
        original.set_code(SI_TIMER);
        let timer = unsafe {
            &mut original
                .0
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._timer
        };
        timer._tid = 7;
        timer._sys_private = 123;

        let mut record = PtraceSignalRecord::process(PreparedSignal::unqueued(original));
        let mut replacement = record.info().clone();
        let timer = unsafe {
            &mut replacement
                .0
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._timer
        };
        timer._tid = 99;
        timer._sys_private = 456;
        record.replace_info(replacement).unwrap();

        let timer = unsafe {
            record
                .info()
                .0
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._timer
        };
        assert_eq!(timer._tid, 7);
        assert_eq!(timer._sys_private, 123);
    }

    #[test]
    fn ptrace_record_owns_one_rt_charge_and_retains_complete_mqueue_info() {
        let per_user = SignalQueueAccount::try_new(4).unwrap();
        let global = SignalQueueAccount::try_new(4).unwrap();
        let mut info = SignalInfo::new_kernel(Signo::SIGRTMIN);
        info.set_code(SI_MESGQ);
        let value = 0x1234_5678_abcd_ef01usize;
        let rt = unsafe { &mut info.0.__bindgen_anon_1.__bindgen_anon_1._sifields._rt };
        rt._pid = 42;
        rt._uid = 1000;
        rt._sigval = sigval_t {
            sival_ptr: value as *mut linux_raw_sys::ctypes::c_void,
        };

        let prepared = PreparedSignal::try_accounted(info, &per_user, 4, &global).unwrap();
        let record = PtraceSignalRecord::process(prepared);
        assert_eq!(per_user.queued(), 1);
        assert_eq!(global.queued(), 1);
        assert_eq!(record.info().code(), SI_MESGQ);
        let retained = unsafe {
            record
                .info()
                .0
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._rt
        };
        assert_eq!(retained._pid, 42);
        assert_eq!(retained._uid, 1000);
        assert_eq!(unsafe { retained._sigval.sival_ptr } as usize, value);

        drop(record);
        assert_eq!(per_user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }
}
