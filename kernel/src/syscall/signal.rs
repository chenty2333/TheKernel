use alloc::sync::Arc;
use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axtask::{
    current,
    future::{self, block_on},
};
use linux_raw_sys::general::{
    CAP_KILL, MINSIGSTKSZ, SI_TKILL, SI_USER, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK, SS_DISABLE,
    SS_ONSTACK, siginfo, timespec,
};
use starry_process::Pid;
use starry_signal::{
    RawSignalAction, SignalAction, SignalInfo, SignalSet, SignalStack, Signo, api::SignalFrame,
};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    task::{
        AsThread, ProcessData, acknowledge_posix_timer_signal, check_current_signal_access,
        check_signals, force_signal_current_thread, get_process_data, get_process_group,
        get_process_including_zombie, get_visible_task, send_queued_signal_to_process_data,
        send_queued_signal_to_visible_thread, send_signal_to_process, send_signal_to_process_data,
        send_signal_to_visible_thread, try_processes,
    },
    time::TimeValueLike,
};

pub(crate) fn check_sigset_size(size: usize) -> AxResult<()> {
    if size != size_of::<SignalSet>() && size != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

pub(crate) fn parse_signo(signo: u32) -> AxResult<Signo> {
    u8::try_from(signo)
        .ok()
        .and_then(Signo::from_repr)
        .ok_or(AxError::InvalidInput)
}

fn current_visible_tid() -> Pid {
    current().as_thread().tid()
}

pub fn sys_rt_sigprocmask(
    how: i32,
    set: *const SignalSet,
    oldset: *mut SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let curr = current();
    let sig = &curr.as_thread().signal;
    let old = sig.blocked();

    if let Some(oldset) = oldset.nullable() {
        oldset.vm_write(old)?;
    }

    if let Some(set) = set.nullable() {
        let set = unsafe { set.vm_read_uninit()?.assume_init() };

        let set = match how as u32 {
            SIG_BLOCK => old | set,
            SIG_UNBLOCK => old & !set,
            SIG_SETMASK => set,
            _ => return Err(AxError::InvalidInput),
        };

        debug!("sys_rt_sigprocmask <= {set:?}");
        sig.set_blocked(set);
    }

    Ok(0)
}

pub fn sys_rt_sigaction(
    signo: u32,
    act: *const RawSignalAction,
    oldact: *mut RawSignalAction,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let signo = parse_signo(signo)?;
    if matches!(signo, Signo::SIGKILL | Signo::SIGSTOP) {
        return Err(AxError::InvalidInput);
    }

    let new_action = if let Some(act) = act.nullable() {
        let mut action: SignalAction = RawSignalAction::read_from_user(act)?.into();
        action.mask.remove(Signo::SIGKILL);
        action.mask.remove(Signo::SIGSTOP);
        Some(action)
    } else {
        None
    };

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let old_action = if let Some(action) = new_action {
        debug!("sys_rt_sigaction <= signo: {signo:?}, act: {action:?}");
        proc_data
            .signal
            .try_replace_action(signo, action)
            .map_err(|_| AxError::NoMemory)?
    } else {
        proc_data.signal.actions.lock()[signo].clone()
    };

    // Linux commits the new action before copying the previous one out. If
    // this user copy faults, the action transition and required queue flush
    // therefore remain visible.
    if let Some(oldact) = oldact.nullable() {
        RawSignalAction::from(old_action).write_to_user(oldact)?;
    }
    Ok(0)
}

pub fn sys_rt_sigpending(set: *mut SignalSet, sigsetsize: usize) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;
    set.vm_write(current().as_thread().signal.pending())?;
    Ok(0)
}

fn make_siginfo(signo: u32, code: i32) -> AxResult<Option<SignalInfo>> {
    if signo == 0 {
        return Ok(None);
    }
    let signo = parse_signo(signo)?;
    Ok(Some(SignalInfo::new_user(
        signo,
        code,
        current().as_thread().proc_data.proc.pid(),
    )))
}

pub(crate) fn queued_signal_required(signal: &Option<SignalInfo>) -> bool {
    signal
        .as_ref()
        .is_some_and(|info| info.signo().is_realtime() && info.code() != SI_USER as i32)
}

fn check_signal_permission(pid: Pid, signal: Option<Signo>) -> AxResult<()> {
    let target = get_process_data(pid)?;
    check_current_signal_access(&target, signal)
}

fn check_zombie_signal_permission(pid: Pid, signal: Option<Signo>) -> AxResult<bool> {
    let process = get_process_including_zombie(pid)?;
    if !process.is_zombie() {
        return Ok(false);
    }

    let actor = current();
    let actor_proc = &actor.as_thread().proc_data;
    let actor_cred = actor_proc.current_cred();
    let actor_ids = actor_cred.ids();
    let snapshot = process.zombie_snapshot().ok_or(AxError::NoSuchProcess)?;
    let allowed = [actor_ids.ruid, actor_ids.euid]
        .into_iter()
        .any(|id| id == snapshot.uid)
        || actor_cred.has_effective_capability(CAP_KILL)
        || (signal == Some(Signo::SIGCONT)
            && actor_proc.proc.group().session().sid() == process.group().session().sid());
    if allowed {
        Ok(true)
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn zombie_signal_succeeds(pid: Pid, signal: Option<Signo>) -> AxResult<bool> {
    check_zombie_signal_permission(pid, signal)
}

fn signal_signo(signal: &Option<SignalInfo>) -> Option<Signo> {
    signal.as_ref().map(SignalInfo::signo)
}

fn send_user_signal_to_targets(
    targets: impl IntoIterator<Item = Arc<ProcessData>>,
    signal: Option<SignalInfo>,
) -> AxResult<()> {
    let signo = signal_signo(&signal);
    let mut had_target = false;
    let mut had_permission = false;
    let mut delivered = false;
    let mut first_error = None;

    for target in targets {
        had_target = true;
        if check_current_signal_access(&target, signo).is_err() {
            continue;
        }
        had_permission = true;
        match send_signal_to_process_data(&target, signal.clone()) {
            Ok(()) => delivered = true,
            Err(err) if first_error.is_none() => first_error = Some(err),
            Err(_) => {}
        }
    }

    if delivered || (signal.is_none() && had_permission) {
        Ok(())
    } else if !had_target {
        Err(AxError::NoSuchProcess)
    } else if !had_permission {
        Err(AxError::OperationNotPermitted)
    } else {
        Err(first_error.unwrap_or(AxError::NoSuchProcess))
    }
}

fn check_visible_thread_signal_access(
    tgid: Option<Pid>,
    tid: Pid,
    signal: Option<Signo>,
) -> AxResult<()> {
    let task = get_visible_task(tid)?;
    let thread = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
    if tgid.is_some_and(|tgid| thread.proc_data.proc.pid() != tgid) {
        return Err(AxError::NoSuchProcess);
    }
    check_current_signal_access(&thread.proc_data, signal)
}

pub fn sys_kill(pid: i32, signo: u32) -> AxResult<isize> {
    debug!("sys_kill: pid = {pid}, signo = {signo}");
    let sig = make_siginfo(signo, SI_USER as _)?;
    let permission_signal = signal_signo(&sig);

    match pid {
        1.. => {
            let target = pid as Pid;
            match check_signal_permission(target, permission_signal) {
                Ok(()) => match send_signal_to_process(target, sig) {
                    Ok(()) => {}
                    Err(AxError::NoSuchProcess)
                        if zombie_signal_succeeds(target, permission_signal)? => {}
                    Err(err) => return Err(err),
                },
                Err(AxError::NoSuchProcess)
                    if zombie_signal_succeeds(target, permission_signal)? => {}
                Err(err) => return Err(err),
            }
        }
        0 => {
            let pgid = current().as_thread().proc_data.proc.group().pgid();
            let targets = get_process_group(pgid)?
                .try_processes()
                .map_err(|_| AxError::NoMemory)?
                .into_iter()
                .filter_map(|process| get_process_data(process.pid()).ok());
            send_user_signal_to_targets(targets, sig)?;
        }
        -1 => {
            let curr_pid = current().as_thread().proc_data.proc.pid();
            let targets = try_processes()?
                .into_iter()
                .filter(|proc_data| !proc_data.proc.is_init() && proc_data.proc.pid() != curr_pid);
            send_user_signal_to_targets(targets, sig)?;
        }
        ..-1 => {
            let targets = get_process_group((-pid) as Pid)?
                .try_processes()
                .map_err(|_| AxError::NoMemory)?
                .into_iter()
                .filter_map(|process| get_process_data(process.pid()).ok());
            send_user_signal_to_targets(targets, sig)?;
        }
    }
    Ok(0)
}

pub fn sys_tkill(tid: i32, signo: u32) -> AxResult<isize> {
    if tid <= 0 {
        return Err(AxError::InvalidInput);
    }
    let sig = make_siginfo(signo, SI_TKILL)?;
    check_visible_thread_signal_access(None, tid as Pid, signal_signo(&sig))?;
    send_queued_signal_to_visible_thread(None, tid as Pid, sig)?;
    Ok(0)
}

pub fn sys_tgkill(tgid: i32, tid: i32, signo: u32) -> AxResult<isize> {
    if tgid <= 0 || tid <= 0 {
        return Err(AxError::InvalidInput);
    }
    let sig = make_siginfo(signo, SI_TKILL)?;
    check_visible_thread_signal_access(Some(tgid as Pid), tid as Pid, signal_signo(&sig))?;
    send_queued_signal_to_visible_thread(Some(tgid as Pid), tid as Pid, sig)?;
    Ok(0)
}

pub(crate) fn make_queue_signal_info(
    target_tid: Pid,
    signo: u32,
    sig: *const SignalInfo,
) -> AxResult<Option<SignalInfo>> {
    if signo == 0 {
        return Ok(None);
    }

    let signo = parse_signo(signo)?;
    let mut sig = unsafe { sig.vm_read_uninit()?.assume_init() };
    sig.set_signo(signo);
    let target_process_pid = get_visible_task(target_tid)
        .ok()
        .and_then(|task| {
            task.try_as_thread()
                .map(|thread| thread.proc_data.proc.pid())
        })
        .unwrap_or(target_tid);
    if (sig.code() >= 0 || sig.code() == SI_TKILL)
        && current_visible_tid() != target_tid
        && current().as_thread().proc_data.proc.pid() != target_process_pid
    {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(Some(sig))
}

pub fn sys_rt_sigqueueinfo(pid: Pid, signo: u32, sig: *const SignalInfo) -> AxResult<isize> {
    let sig = make_queue_signal_info(pid, signo, sig)?;
    let permission_signal = signal_signo(&sig);
    let queue_required = queued_signal_required(&sig);
    if let Ok(task) = get_visible_task(pid) {
        let thread = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
        check_current_signal_access(&thread.proc_data, permission_signal)?;
        if thread.proc_data.proc.pid() == pid {
            if queue_required {
                send_queued_signal_to_process_data(&thread.proc_data, sig)?;
            } else {
                send_signal_to_process_data(&thread.proc_data, sig)?;
            }
        } else {
            if queue_required {
                send_queued_signal_to_visible_thread(None, pid, sig)?;
            } else {
                send_signal_to_visible_thread(None, pid, sig)?;
            }
        }
    } else {
        let target = get_process_data(pid)?;
        check_current_signal_access(&target, permission_signal)?;
        if queue_required {
            send_queued_signal_to_process_data(&target, sig)?;
        } else {
            send_signal_to_process_data(&target, sig)?;
        }
    }
    Ok(0)
}

pub fn sys_rt_tgsigqueueinfo(
    tgid: i32,
    tid: i32,
    signo: u32,
    sig: *const SignalInfo,
) -> AxResult<isize> {
    if tgid <= 0 || tid <= 0 {
        return Err(AxError::InvalidInput);
    }

    let sig = make_queue_signal_info(tid as Pid, signo, sig)?;
    check_visible_thread_signal_access(Some(tgid as Pid), tid as Pid, signal_signo(&sig))?;
    if queued_signal_required(&sig) {
        send_queued_signal_to_visible_thread(Some(tgid as Pid), tid as Pid, sig)?;
    } else {
        send_signal_to_visible_thread(Some(tgid as Pid), tid as Pid, sig)?;
    }
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
const SIGNAL_PC_ALIGNMENT: usize = 1;
#[cfg(target_arch = "riscv64")]
const SIGNAL_PC_ALIGNMENT: usize = 2;
#[cfg(any(target_arch = "loongarch64", target_arch = "aarch64"))]
const SIGNAL_PC_ALIGNMENT: usize = 4;

#[cfg(target_arch = "aarch64")]
const SIGNAL_SP_ALIGNMENT: usize = 16;
#[cfg(not(target_arch = "aarch64"))]
const SIGNAL_SP_ALIGNMENT: usize = 1;

fn valid_signal_user_address(address: usize, alignment: usize) -> bool {
    let end = crate::config::USER_SPACE_BASE + crate::config::USER_SPACE_SIZE;
    address >= crate::config::USER_SPACE_BASE && address < end && address % alignment == 0
}

fn reject_bad_sigreturn(reason: &str) -> AxResult<isize> {
    warn!("rejecting invalid rt_sigreturn frame: {reason}");
    force_signal_current_thread(SignalInfo::new_kernel(Signo::SIGSEGV));
    Ok(0)
}

pub fn sys_rt_sigreturn(uctx: &mut UserContext) -> AxResult<isize> {
    let curr = current();
    let thr = curr.as_thread();

    if !thr.in_signal_handler() {
        return reject_bad_sigreturn("no active signal handler");
    }

    let frame = match SignalFrame::read_from_user(uctx.sp() as *const SignalFrame) {
        Ok(frame) => frame,
        Err(_) => return reject_bad_sigreturn("frame copy-in fault"),
    };

    let prepared = match thr.signal.prepare_restore(
        uctx,
        frame,
        |pc| valid_signal_user_address(pc, SIGNAL_PC_ALIGNMENT),
        |sp| valid_signal_user_address(sp, SIGNAL_SP_ALIGNMENT),
    ) {
        Ok(prepared) => prepared,
        Err(err) => {
            warn!("rt_sigreturn context validation failed: {err:?}");
            return reject_bad_sigreturn("invalid machine context");
        }
    };

    // No operation after this point may fail: context, mask and restart state
    // become visible only after the complete frame has passed validation.
    thr.signal.commit_restore(uctx, prepared);
    thr.complete_sigreturn(uctx);
    Ok(uctx.retval() as isize)
}

pub fn sys_rt_sigtimedwait(
    uctx: &mut UserContext,
    set: *const SignalSet,
    info: *mut siginfo,
    timeout: *const timespec,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let set = unsafe { set.vm_read_uninit()?.assume_init() };

    let timeout = if let Some(ts) = timeout.nullable() {
        let ts = unsafe { ts.vm_read_uninit()?.assume_init() };
        Some(ts.try_into_time_value()?)
    } else {
        None
    };

    debug!("sys_rt_sigtimedwait => set = {set:?}, timeout = {timeout:?}");

    let curr = current();
    let thr = curr.as_thread();
    let signal = &thr.signal;

    let old_blocked = signal.blocked();
    signal.set_real_blocked(Some(old_blocked));
    signal.set_blocked(old_blocked & !set);

    uctx.set_retval(-LinuxError::EINTR.code() as usize);
    let fut = poll_fn(|cx| {
        loop {
            if let Some(sig) = signal.dequeue_signal(&set) {
                signal.set_real_blocked(None);
                signal.set_blocked(old_blocked);
                return Poll::Ready(Some(sig));
            }
            if check_signals(thr, uctx, Some(old_blocked)) {
                signal.set_real_blocked(None);
                return Poll::Ready(None);
            }

            if curr.poll_interrupt(cx).is_pending() {
                return Poll::Pending;
            }
        }
    });

    let Ok(sig) = block_on(future::timeout(timeout, fut)) else {
        // Timeout
        signal.set_real_blocked(None);
        signal.set_blocked(old_blocked);
        return Err(AxError::WouldBlock);
    };
    let Some(sig) = sig else {
        // Interrupted
        return Ok(0);
    };
    acknowledge_posix_timer_signal(&thr.proc_data, &sig);

    if let Some(info) = info.nullable() {
        info.vm_write(sig.0)?;
    }

    Ok(sig.signo() as _)
}

pub fn sys_rt_sigsuspend(
    uctx: &mut UserContext,
    set: *const SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let curr = current();
    let thr = curr.as_thread();

    let set = unsafe { set.vm_read_uninit()?.assume_init() };
    let old_blocked = thr.signal.blocked();
    thr.signal.set_real_blocked(Some(old_blocked));
    thr.signal.set_blocked(set);

    // sigsuspend always returns -EINTR when a signal is caught
    // We set this in uctx before check_signals so it's saved in SignalFrame
    uctx.set_retval(-LinuxError::EINTR.code() as usize);

    block_on(poll_fn(|cx| {
        loop {
            if check_signals(thr, uctx, Some(old_blocked)) {
                thr.signal.set_real_blocked(None);
                return Poll::Ready(());
            }

            if curr.poll_interrupt(cx).is_pending() {
                return Poll::Pending;
            }
            // A stale task interrupt can be consumed without a deliverable
            // signal. Poll again so the current waker is registered before the
            // task blocks; otherwise the next signal can be lost.
        }
    }));

    // sigsuspend always returns -EINTR
    Err(AxError::Interrupted)
}

pub fn sys_sigaltstack(ss: *const SignalStack, old_ss: *mut SignalStack) -> AxResult<isize> {
    let curr = current();
    let sig = &curr.as_thread().signal;

    if let Some(old_ss) = old_ss.nullable() {
        old_ss.vm_write(sig.stack())?;
    }

    if let Some(ss) = ss.nullable() {
        let ss = unsafe { ss.vm_read_uninit()?.assume_init() };
        let valid_flags = SS_DISABLE as u32;
        if ss.flags & !valid_flags != 0 || ss.flags & SS_ONSTACK as u32 != 0 {
            return Err(AxError::InvalidInput);
        }
        if ss.flags == SS_DISABLE as u32 {
            sig.set_stack(SignalStack::default());
            return Ok(0);
        }
        if ss.size < MINSIGSTKSZ as usize {
            return Err(AxError::NoMemory);
        }
        sig.set_stack(ss);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use linux_raw_sys::general::{SI_TKILL, SI_USER};
    use starry_signal::{SignalInfo, Signo};

    use super::{parse_signo, queued_signal_required};

    #[test]
    fn signal_numbers_are_range_checked_before_narrowing() {
        assert!(parse_signo(0).is_err());
        assert_eq!(parse_signo(1).unwrap(), Signo::SIGHUP);
        assert_eq!(parse_signo(64).unwrap(), Signo::SIGRT32);
        assert!(parse_signo(65).is_err());
        assert!(parse_signo(257).is_err());
        assert!(parse_signo(u32::MAX).is_err());
    }

    #[test]
    fn realtime_queue_policy_matches_linux_siginfo_classification() {
        assert!(!queued_signal_required(&None));
        assert!(!queued_signal_required(&Some(SignalInfo::new_user(
            Signo::SIGTERM,
            SI_TKILL,
            1,
        ))));
        assert!(!queued_signal_required(&Some(SignalInfo::new_user(
            Signo::SIGRTMIN,
            SI_USER as i32,
            1,
        ))));
        assert!(queued_signal_required(&Some(SignalInfo::new_user(
            Signo::SIGRTMIN,
            SI_TKILL,
            1,
        ))));
    }
}
