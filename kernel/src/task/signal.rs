use alloc::sync::{Arc, Weak};
use core::convert::Infallible;

use axerrno::{AxError, AxResult};
use axhal::uspace::{UserContext, UserReturnHookAction};
use axtask::{TaskInner, current};
use linux_raw_sys::general::{RLIMIT_SIGPENDING, SI_TIMER};
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::{
    DefaultSignalAction, PreparedSignal, SignalAction, SignalDisposition, SignalInfo,
    SignalOSAction, SignalQueueAccount, SignalRecordGeneration, SignalSet, Signo,
    api::{
        DeliveredSignal, SignalDeliveryPreflight, SignalDeliveryResult, ThreadSignalManager,
        ThreadSignalSendOutcome,
    },
};
use thekernel_linux_usercopy::UserMemoryContext;

use super::{
    AsThread, ContinueResult, Cred, ProcessData, Thread, acknowledge_posix_timer_signal, do_exit,
    fail_closed_exit, get_process_data, get_process_group, get_task, get_visible_task,
    linux_pid_from_task_id, process_domain, process_error,
};
use crate::{mm::AddressSpaceUserMemory, readiness::block_on_poll_set};

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

// The error variant deliberately carries the untouched `PtraceSignalRecord`
// back to the caller so it can publish the signal normally. Boxing it to
// shrink the variant would put an allocation on the signal-delivery path.
#[allow(clippy::result_large_err)]
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
) -> ThreadSignalSendOutcome {
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

fn ptrace_signal_target_cred(
    proc_data: &ProcessData,
    target: &PtraceSignalTarget,
) -> AxResult<Arc<Cred>> {
    match target {
        PtraceSignalTarget::Process => Ok(proc_data.group_leader_cred()),
        PtraceSignalTarget::Thread { tid, signal } => {
            let task = get_visible_task(*tid)?;
            let thread = task.as_thread();
            let live_signal = signal.upgrade().ok_or(AxError::NoSuchProcess)?;
            if !Arc::ptr_eq(&thread.signal, &live_signal) {
                return Err(AxError::NoSuchProcess);
            }
            Ok(thread.current_cred())
        }
    }
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
            let current = self.info().timer_payload();
            let mut replacement = info.timer_payload();
            // These fields identify kernel ownership and are not part of the
            // tracer-mutable payload, despite sharing the userspace layout.
            replacement.tid = current.tid;
            replacement.sys_private = current.sys_private;
            info.set_timer_payload(replacement);
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
    target_cred: &Cred,
    info: SignalInfo,
    policy: SignalQueuePolicy,
) -> AxResult<PreparedSignal> {
    if !info.signo().is_realtime() {
        return Ok(PreparedSignal::unqueued(info));
    }
    let limit = target.rlim.read()[RLIMIT_SIGPENDING].current;
    let real_uid = target_cred.ids().ruid;
    match target_cred.user_ns().try_signal_queue_accounts(real_uid) {
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
    let target_cred = target.group_leader_cred();
    prepare_signal_for_target(target, &target_cred, info, SignalQueuePolicy::QueueRequired)
}

pub fn check_signals(
    thr: &Thread,
    uctx: &mut UserContext,
    restore_blocked: Option<SignalSet>,
) -> bool {
    if thr.pending_exit() {
        return false;
    }
    // The signal manager invokes this callback only after selecting a
    // non-ignored signal and before it can write a frame or publish a handler
    // IP/SP. Resolve one explicit address-space handle immediately before
    // each pre-delivery operation so an image replacement cannot redirect the
    // rseq gate to an unrelated address space.
    let saved_uctx = *uctx;
    let aspace = thr.proc_data.aspace();
    let mut provider = AddressSpaceUserMemory::new(aspace.clone());
    let mut memory = UserMemoryContext::new(&mut provider);
    let result = thr.signal.check_signals_with_pre_delivery(
        &mut memory,
        uctx,
        restore_blocked,
        |uctx, sig, _| {
            // A forced SIGSEGV generated by a failed final rseq gate gets one
            // signal-bound bypass. This lets an already-installed handler
            // observe the fault without re-entering the same failing gate;
            // default-action signals never reach this callback.
            if thr.signal.take_signal_delivery_bypass(sig.signo()) {
                return SignalDeliveryPreflight::Proceed;
            }
            match thr.pre_signal_rseq_delivery(uctx, &aspace) {
                UserReturnHookAction::EnterUser => SignalDeliveryPreflight::Proceed,
                UserReturnHookAction::Retry => SignalDeliveryPreflight::Retry,
                UserReturnHookAction::Fault => {
                    // The selected record is not recoverable by retrying its
                    // handler frame. Replace it with the exact forced
                    // SIGSEGV record when one was published; otherwise the
                    // manager consumes it and the caller performs the fatal
                    // SIGSEGV action.
                    if force_rseq_fault_signal_current_thread() {
                        SignalDeliveryPreflight::Replaced
                    } else {
                        SignalDeliveryPreflight::Fatal
                    }
                }
            }
        },
    );
    match result {
        SignalDeliveryResult::Delivered(delivered) => {
            complete_signal_delivery(thr, uctx, delivered);
            true
        }
        SignalDeliveryResult::None => false,
        SignalDeliveryResult::Replaced => {
            // The selected handler record was consumed and replaced by an
            // origin-bound forced SIGSEGV. Keep scanning immediately so its
            // exact-generation bypass is the next delivery candidate.
            true
        }
        SignalDeliveryResult::Fatal => {
            *uctx = saved_uctx;
            terminate_rseq_fault_current_thread();
            false
        }
        SignalDeliveryResult::Retry => {
            // The final rseq hook may have updated the saved context before a
            // later nofault publication failed. Signal retry/fault must not
            // retain that partial context, and the manager has already
            // returned the selected signal to its original queue.
            *uctx = saved_uctx;
            false
        }
        SignalDeliveryResult::Fault => {
            // Preserve the old generic-fault boundary for callers which still
            // report Fault directly. A successful replacement must be
            // consumed by the next scan; failure is fatal rather than a
            // requeue loop.
            *uctx = saved_uctx;
            if force_rseq_fault_signal_current_thread() {
                true
            } else {
                terminate_rseq_fault_current_thread();
                false
            }
        }
    }
}

/// Completes the kernel-owned effects of one signal-manager delivery.
///
/// The signal manager owns queue selection, disposition claiming, and handler
/// frame publication. The embedding kernel remains the sole owner of process,
/// job-control, timer, and restart state transitions.
pub(crate) fn complete_signal_delivery(
    thr: &Thread,
    uctx: &mut UserContext,
    delivered: DeliveredSignal,
) {
    acknowledge_posix_timer_signal(&thr.proc_data, &delivered.info);

    let signo = delivered.info.signo();
    thr.finish_signal_delivery(delivered.os_action, delivered.restartable_handler);
    match delivered.os_action {
        SignalOSAction::Terminate => {
            if let Err(error) = do_exit(signo as i32, true) {
                fail_closed_exit(error);
            }
        }
        SignalOSAction::CoreDump => {
            let dumped = match super::coredump::generate_core_dump(thr, uctx, signo as u8) {
                Ok(dumped) => dumped,
                Err(error) => {
                    warn!("Core dump failed: {error:?}");
                    false
                }
            };
            let exit_code = (signo as i32) | if dumped { 0x80 } else { 0 };
            if let Err(error) = do_exit(exit_code, true) {
                fail_closed_exit(error);
            }
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
}

pub(crate) fn has_pending_syscall_signal(thr: &Thread) -> bool {
    let pending = thr.signal.pending();
    if pending.is_empty() {
        return false;
    }

    let blocked = thr.signal.blocked();
    for raw in 1..=64u8 {
        let Some(signo) = Signo::from_repr(raw) else {
            continue;
        };
        if !pending.has(signo) || blocked.has(signo) {
            continue;
        }

        let ignored = match thr.proc_data.signal.action(signo).disposition {
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

    for raw in 1..=64u8 {
        let Some(signo) = Signo::from_repr(raw) else {
            continue;
        };
        if !pending.has(signo) {
            continue;
        }
        if matches!(
            thr.proc_data.signal.action(signo).disposition,
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

/// Matches Linux's `fatal_signal_pending()` boundary used by `mfill_atomic`.
///
/// Linux's predicate is narrower than this kernel's exec-oriented
/// [`has_pending_fatal_signal`]: it asks specifically whether SIGKILL is
/// pending, independent of the blocked mask. Keep the distinction explicit so
/// a blocked default-terminate signal cannot truncate a UFFD COPY/ZEROPAGE.
pub(crate) fn has_pending_sigkill(thr: &Thread) -> bool {
    thr.signal.pending().has(Signo::SIGKILL)
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
    target_cred: &Cred,
    sig: SignalInfo,
    policy: SignalQueuePolicy,
) -> AxResult<(bool, Option<SignalRecordGeneration>)> {
    let signo = sig.signo();
    if signo == Signo::SIGCONT {
        do_continue(&thr.proc_data);
    }

    if thr.proc_data.ptrace_tracer().is_some() && !matches!(signo, Signo::SIGKILL | Signo::SIGCONT)
    {
        let prepared = prepare_signal_for_target(&thr.proc_data, target_cred, sig, policy)?;
        match try_ptrace_signal_stop(&thr.proc_data, PtraceSignalRecord::thread(thr, prepared)) {
            Ok(()) => {
                task.interrupt();
                return Ok((true, None));
            }
            Err(record) => {
                let info = record.info().clone();
                let outcome = publish_prepared_thread(&thr.signal, info, record.prepared);
                if outcome.wake {
                    task.interrupt();
                }
                return Ok((outcome.published, outcome.generation));
            }
        }
    }

    let outcome = thr.signal.try_send_signal_with(sig, |info| {
        prepare_signal_for_target(&thr.proc_data, target_cred, info, policy)
    })?;
    if outcome.wake {
        task.interrupt();
    }
    Ok((outcome.published, outcome.generation))
}

pub(crate) fn send_signal_thread_inner(task: &TaskInner, thr: &Thread, sig: SignalInfo) {
    let target_cred = thr.current_cred();
    let _ = send_signal_thread_inner_with(
        task,
        thr,
        &target_cred,
        sig,
        SignalQueuePolicy::BestEffortKill,
    );
}

/// Sends a resolved thread-directed signal with mandatory RT admission.
pub(crate) fn send_queued_signal_thread_inner(
    task: &TaskInner,
    thr: &Thread,
    sig: SignalInfo,
) -> AxResult<bool> {
    let target_cred = thr.current_cred();
    send_signal_thread_inner_with(
        task,
        thr,
        &target_cred,
        sig,
        SignalQueuePolicy::QueueRequired,
    )
    .map(|(published, _)| published)
}

/// Publishes a userspace-authorized thread-directed request using the exact
/// immutable credential snapshot which passed the security hook.
pub(crate) fn send_authorized_signal_thread_inner(
    task: &TaskInner,
    thr: &Thread,
    target_cred: &Cred,
    sig: SignalInfo,
    queue_required: bool,
) -> AxResult<bool> {
    let policy = if queue_required {
        SignalQueuePolicy::QueueRequired
    } else {
        SignalQueuePolicy::BestEffortKill
    };
    send_signal_thread_inner_with(task, thr, target_cred, sig, policy)
        .map(|(published, _)| published)
}

/// Completes generation of a thread-directed signal for a retained exited
/// group leader while sibling threads keep the process alive.
///
/// Linux still applies generation-time group effects (notably SIGCONT) and RT
/// queue admission. The persistent group-leader signal manager retains that
/// private pending record until exec replaces the leader identity or final
/// process exit releases the runtime; it is never redirected into the shared
/// process queue or a surviving sibling.
pub(crate) fn generate_signal_for_exited_leader(
    proc_data: &ProcessData,
    leader_signal: &ThreadSignalManager,
    target_cred: &Cred,
    signal: Option<SignalInfo>,
    queue_required: bool,
) -> AxResult<()> {
    if proc_data.proc.is_zombie() || proc_data.proc.thread_count() == 0 {
        return Ok(());
    }
    let Some(signal) = signal else {
        return Ok(());
    };
    if signal.signo() == Signo::SIGCONT {
        do_continue(proc_data);
    }
    let policy = if queue_required {
        SignalQueuePolicy::QueueRequired
    } else {
        SignalQueuePolicy::BestEffortKill
    };
    leader_signal.try_send_retained_signal_with(signal, |info| {
        prepare_signal_for_target(proc_data, target_cred, info, policy)
    })?;
    Ok(())
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
    target_cred: &Cred,
    sig: SignalInfo,
    policy: SignalQueuePolicy,
) -> AxResult<bool> {
    let signo = sig.signo();
    if signo == Signo::SIGCONT {
        do_continue(proc_data);
    }

    if proc_data.ptrace_tracer().is_some() && !matches!(signo, Signo::SIGKILL | Signo::SIGCONT) {
        let prepared = prepare_signal_for_target(proc_data, target_cred, sig, policy)?;
        match try_ptrace_signal_stop(proc_data, PtraceSignalRecord::process(prepared)) {
            Ok(()) => return Ok(true),
            Err(record) => {
                let info = record.info().clone();
                return Ok(publish_prepared_process(proc_data, info, record.prepared));
            }
        }
    }

    let outcome = proc_data.signal.try_send_signal_with(sig, |info| {
        prepare_signal_for_target(proc_data, target_cred, info, policy)
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
        let target_cred = proc_data.group_leader_cred();
        let signo = sig.signo();
        info!("Send signal {signo:?} to process {}", proc_data.proc.pid());
        send_signal_to_process_data_with_policy(
            proc_data,
            &target_cred,
            sig,
            SignalQueuePolicy::BestEffortKill,
        )?;
    }

    Ok(())
}

/// Process-directed counterpart retaining the exact task credential used by
/// authorization and sigqueue accounting. The task may be a non-leader while
/// publication still targets its shared thread-group pending queue.
pub(crate) fn send_signal_to_process_data_with_credential(
    proc_data: &ProcessData,
    target_cred: &Cred,
    sig: Option<SignalInfo>,
) -> AxResult<()> {
    if proc_data.proc.is_zombie() || proc_data.proc.thread_count() == 0 {
        return Err(AxError::NoSuchProcess);
    }
    if let Some(sig) = sig {
        send_signal_to_process_data_with_policy(
            proc_data,
            target_cred,
            sig,
            SignalQueuePolicy::BestEffortKill,
        )?;
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
        let target_cred = proc_data.group_leader_cred();
        return send_signal_to_process_data_with_policy(
            proc_data,
            &target_cred,
            sig,
            SignalQueuePolicy::QueueRequired,
        );
    }
    Ok(false)
}

/// Mandatory-queue variant using the same exact credential snapshot as the
/// userspace authorization hook.
pub(crate) fn send_queued_signal_to_process_data_with_credential(
    proc_data: &ProcessData,
    target_cred: &Cred,
    sig: Option<SignalInfo>,
) -> AxResult<bool> {
    if proc_data.proc.is_zombie() || proc_data.proc.thread_count() == 0 {
        return Err(AxError::NoSuchProcess);
    }
    if let Some(sig) = sig {
        return send_signal_to_process_data_with_policy(
            proc_data,
            target_cred,
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
            let target_cred = ptrace_signal_target_cred(proc_data, &PtraceSignalTarget::Process)?;
            let prepared = prepare_signal_for_target(
                proc_data,
                &target_cred,
                info,
                SignalQueuePolicy::BestEffortKill,
            )?;
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
            let target_cred = ptrace_signal_target_cred(proc_data, &target)?;
            let prepared = prepare_signal_for_target(
                proc_data,
                &target_cred,
                info,
                SignalQueuePolicy::BestEffortKill,
            )?;
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
        for proc in pg
            .try_processes(process_domain()?.registry())
            .map_err(process_error)?
        {
            if proc.is_zombie() {
                continue;
            }
            send_signal_to_process(proc.pid(), Some(sig.clone()))?;
        }
    }

    Ok(())
}

/// Forces a synchronous signal onto the current thread.
///
/// Linux forced signals cannot be suppressed by an ignored disposition or a
/// blocked mask. A user handler is retained when it is already unblocked;
/// otherwise the disposition is reset to default before enqueueing. Ordinary
/// forced signals do not bypass the rseq pre-delivery gate: their handler
/// delivery is still a real user-frame transition.
pub(crate) fn force_signal_current_thread(sig: SignalInfo) {
    force_signal_current_thread_inner(sig, false);
}

/// Forces the SIGSEGV generated by the final rseq gate's own fault path.
///
/// This is deliberately a separate origin-bound entry point. Page faults,
/// sigreturn validation failures, and seccomp SIGSYS delivery must continue
/// through the normal rseq preflight. Only the rseq gate's recovery/fault path
/// may arm the one-shot bypass which lets an already-installed SIGSEGV handler
/// observe the fault without re-entering the same failed gate.
pub(crate) fn force_rseq_fault_signal_current_thread() -> bool {
    // A rseq gate fault cannot leave the rejected handler record ahead of an
    // ordinary SIGSEGV. Return whether an exact replacement was armed; the
    // delivery owner consumes the failed record before choosing that
    // replacement, or invokes the fatal action if no unique generation exists.
    force_signal_current_thread_inner(SignalInfo::new_kernel(Signo::SIGSEGV), true).is_some()
}

/// Terminates the current task after a rseq fault could not publish an
/// origin-bound SIGSEGV record. This is kept separate from the enqueue helper
/// because the latter may run while the signal crate's delivery mutex is held.
pub(crate) fn terminate_rseq_fault_current_thread() {
    if let Err(error) = do_exit(Signo::SIGSEGV as i32, true) {
        fail_closed_exit(error);
    }
}

fn force_signal_current_thread_inner(
    sig: SignalInfo,
    bypass_rseq: bool,
) -> Option<SignalRecordGeneration> {
    let curr = current();
    let thr = curr.as_thread();
    let signo = sig.signo();
    let was_blocked = thr.signal.signal_blocked(signo);

    let action = thr.proc_data.signal.action(signo);
    let retain_handler = if was_blocked || matches!(action.disposition, SignalDisposition::Ignore) {
        if let Err(error) = thr
            .proc_data
            .signal
            .try_replace_action(signo, SignalAction::default())
        {
            warn!("failed to reset forced signal disposition: {error:?}");
        }
        false
    } else {
        matches!(action.disposition, SignalDisposition::Handler(_))
    };

    if was_blocked {
        let mut blocked = thr.signal.blocked();
        blocked.remove(signo);
        thr.signal.set_blocked(blocked);
    }

    let published_generation = send_signal_thread_inner_with(
        &curr,
        thr,
        &thr.current_cred(),
        sig,
        SignalQueuePolicy::BestEffortKill,
    )
    .ok()
    .and_then(|(published, generation)| published.then_some(generation).flatten());
    if bypass_rseq && retain_handler {
        // Arm only after the exact record was published. A coalesced or
        // ptrace-retained same-number signal has no generation and therefore
        // cannot become an rseq bypass target.
        if let Some(generation) = published_generation {
            thr.signal.arm_signal_delivery_bypass(signo, generation);
            return Some(generation);
        }
    }
    None
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
    let tid = linux_pid_from_task_id(current().id().as_u64())
        .unwrap_or_else(|error| fail_closed_exit(error));
    while !thr.pending_exit()
        && !proc_data.should_exit_for_exec(tid)
        && proc_data.should_wait_for_stop()
    {
        match block_on_poll_set(&proc_data.stop_event, || {
            if !proc_data.should_wait_for_stop()
                || thr.pending_exit()
                || proc_data.should_exit_for_exec(tid)
            {
                Ok(())
            } else {
                Err(AxError::WouldBlock)
            }
        }) {
            Ok(()) => {}
            Err(_) => handle_stopped_interrupt(thr, uctx),
        }
    }
}

fn interrupt_stop_siblings(proc_data: &ProcessData) {
    let curr_tid = linux_pid_from_task_id(current().id().as_u64())
        .unwrap_or_else(|error| fail_closed_exit(error));
    for tid in proc_data.proc.thread_ids() {
        if tid != curr_tid
            && let Ok(task) = get_task(tid)
        {
            task.interrupt();
        }
    }
}

fn handle_stopped_interrupt(thr: &Thread, uctx: &mut UserContext) {
    let tid = linux_pid_from_task_id(current().id().as_u64())
        .unwrap_or_else(|error| fail_closed_exit(error));
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
    use linux_raw_sys::general::SI_MESGQ;
    use thekernel_linux_signal::{
        PreparedSignal, SignalInfo, SignalQueueAccount, SignalRtPayload, SignalTimerPayload, Signo,
    };

    use super::{PtraceSignalRecord, SignalQueuePolicy, prepare_signal_with_accounts};

    #[test]
    fn strict_and_kill_realtime_paths_diverge_at_zero_limit() {
        let per_user = SignalQueueAccount::try_new(4).unwrap();
        let global = SignalQueueAccount::try_new(4).unwrap();

        let strict = prepare_signal_with_accounts(
            SignalInfo::new_user(Signo::SIGRTMIN, -6, 1, 0),
            SignalQueuePolicy::QueueRequired,
            0,
            &per_user,
            &global,
        );
        assert!(matches!(strict, Err(axerrno::AxError::WouldBlock)));

        let fallback = prepare_signal_with_accounts(
            SignalInfo::new_user(Signo::SIGRTMIN, 0, 1, 0),
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
        let original =
            SignalInfo::new_timer(Signo::SIGRTMIN, SignalTimerPayload::new(7, 0, 0, 123));

        let mut record = PtraceSignalRecord::process(PreparedSignal::unqueued(original));
        let replacement = SignalInfo::new_timer(
            Signo::SIGRTMIN,
            SignalTimerPayload::new(99, 0, 0xfeed_beef, 456),
        );
        record.replace_info(replacement).unwrap();

        let timer = record.info().timer_payload();
        assert_eq!(timer.tid, 7);
        assert_eq!(timer.sys_private, 123);
        assert_eq!(timer.value, 0xfeed_beef);
    }

    #[test]
    fn ptrace_record_owns_one_rt_charge_and_retains_complete_mqueue_info() {
        let per_user = SignalQueueAccount::try_new(4).unwrap();
        let global = SignalQueueAccount::try_new(4).unwrap();
        let value = 0x1234_5678_abcd_ef01usize;
        let info = SignalInfo::new_rt(
            Signo::SIGRTMIN,
            SI_MESGQ,
            SignalRtPayload::new(42, 1000, value),
        );

        let prepared = PreparedSignal::try_accounted(info, &per_user, 4, &global).unwrap();
        let record = PtraceSignalRecord::process(prepared);
        assert_eq!(per_user.queued(), 1);
        assert_eq!(global.queued(), 1);
        assert_eq!(record.info().code(), SI_MESGQ);
        let retained = record.info().rt_payload();
        assert_eq!(retained.pid, 42);
        assert_eq!(retained.uid, 1000);
        assert_eq!(retained.value, value);

        drop(record);
        assert_eq!(per_user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }
}
