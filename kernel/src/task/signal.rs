use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use axtask::{
    TaskInner, current,
    future::{block_on, interruptible},
};
use starry_process::Pid;
use starry_signal::{
    DefaultSignalAction, SignalDisposition, SignalInfo, SignalOSAction, SignalSet, Signo,
};

use super::{
    AsThread, ContinueResult, ProcessData, Thread, do_exit, get_process_data, get_process_group,
    get_task, get_visible_task,
};

pub fn check_signals(
    thr: &Thread,
    uctx: &mut UserContext,
    restore_blocked: Option<SignalSet>,
) -> bool {
    let Some((sig, os_action)) = thr.signal.check_signals(uctx, restore_blocked) else {
        return false;
    };

    let signo = sig.signo();
    match os_action {
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

pub(super) fn send_signal_thread_inner(task: &TaskInner, thr: &Thread, sig: SignalInfo) {
    if sig.signo() == Signo::SIGCONT {
        do_continue(&thr.proc_data);
    }
    if thr.signal.send_signal(sig) {
        task.interrupt();
    }
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

/// Sends a signal to a process.
pub fn send_signal_to_process(pid: Pid, sig: Option<SignalInfo>) -> AxResult<()> {
    let proc_data = get_process_data(pid)?;

    if let Some(sig) = sig {
        let signo = sig.signo();

        // POSIX: SIGCONT always resumes a stopped process, regardless of disposition.
        if signo == Signo::SIGCONT {
            do_continue(&proc_data);
        }

        info!("Send signal {signo:?} to process {pid}");
        if let Some(tid) = proc_data.signal.send_signal(sig)
            && let Ok(task) = get_task(tid)
        {
            task.interrupt();
        }
    }

    Ok(())
}

/// Sends a signal to a process group.
pub fn send_signal_to_process_group(pgid: Pid, sig: Option<SignalInfo>) -> AxResult<()> {
    let pg = get_process_group(pgid)?;

    if let Some(sig) = sig {
        info!("Send signal {:?} to process group {}", sig.signo(), pgid);
        for proc in pg.processes() {
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
    if let Some(tid) = proc_data.signal.send_signal(sig)
        && let Ok(task) = get_task(tid)
    {
        task.interrupt();
    } else {
        // No task wants to handle the signal, abort the task
        do_exit(signo as i32, true);
    }

    Ok(())
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
        notify_parent_stop_continue(proc_data);
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
            notify_parent_stop_continue(proc_data);
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
    for tid in proc_data.proc.threads() {
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

/// Sends SIGCHLD to the parent and wakes its child_exit_event.
fn notify_parent_stop_continue(proc_data: &ProcessData) {
    let process = &proc_data.proc;
    if let Some(parent) = process.parent() {
        let _ = send_signal_to_process(parent.pid(), Some(SignalInfo::new_kernel(Signo::SIGCHLD)));
        if let Ok(parent_data) = get_process_data(parent.pid()) {
            parent_data.child_exit_event.wake();
        }
    }
}
