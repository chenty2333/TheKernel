use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axtask::{
    current,
    future::{self, block_on},
};
use linux_raw_sys::general::{
    MINSIGSTKSZ, RLIMIT_SIGPENDING, SI_TKILL, SI_USER, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK,
    SS_DISABLE, SS_ONSTACK, kernel_sigaction, siginfo, timespec,
};
use starry_process::Pid;
use starry_signal::{SignalInfo, SignalSet, SignalStack, Signo};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    task::{
        AsThread, acknowledge_posix_timer_signal, check_signals, get_process_data,
        get_process_including_zombie, get_visible_task, processes, send_signal_to_process,
        send_signal_to_process_group, send_signal_to_visible_thread,
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
    Signo::from_repr(signo as u8).ok_or(AxError::InvalidInput)
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
    act: *const kernel_sigaction,
    oldact: *mut kernel_sigaction,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;

    let signo = parse_signo(signo)?;
    if matches!(signo, Signo::SIGKILL | Signo::SIGSTOP) {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let mut actions = curr.as_thread().proc_data.signal.actions.lock();
    if let Some(oldact) = oldact.nullable() {
        oldact.vm_write(actions[signo].clone().into())?;
    }
    if let Some(act) = act.nullable() {
        let act = unsafe { act.vm_read_uninit()?.assume_init() }.into();
        debug!("sys_rt_sigaction <= signo: {signo:?}, act: {act:?}");
        actions[signo] = act;
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

fn ensure_realtime_signal_queue_capacity_for_thread(tgid: Option<Pid>, tid: Pid) -> AxResult<()> {
    let task = get_visible_task(tid)?;
    let thread = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
    if tgid.is_some_and(|tgid| thread.proc_data.proc.pid() != tgid) {
        return Err(AxError::NoSuchProcess);
    }

    let limit = thread.proc_data.rlim.read()[RLIMIT_SIGPENDING].current as usize;
    if thread.signal.pending_realtime_count() >= limit {
        return Err(AxError::WouldBlock);
    }
    Ok(())
}

fn ensure_realtime_signal_queue_capacity_for_process(pid: Pid) -> AxResult<()> {
    let proc_data = get_process_data(pid)?;
    let limit = proc_data.rlim.read()[RLIMIT_SIGPENDING].current as usize;
    if proc_data.signal.pending_realtime_count() >= limit {
        return Err(AxError::WouldBlock);
    }
    Ok(())
}

fn check_signal_permission(pid: Pid) -> AxResult<()> {
    let actor = current();
    let actor_proc = &actor.as_thread().proc_data;
    if actor_proc.euid() == 0 {
        return Ok(());
    }

    let target = get_process_data(pid)?;
    let allowed = [actor_proc.uid(), actor_proc.euid()]
        .into_iter()
        .any(|id| id == target.uid() || id == target.euid() || id == target.suid());
    if allowed {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn check_zombie_signal_permission(pid: Pid) -> AxResult<bool> {
    let process = get_process_including_zombie(pid)?;
    if !process.is_zombie() {
        return Ok(false);
    }

    let actor = current();
    let actor_proc = &actor.as_thread().proc_data;
    if actor_proc.euid() == 0 {
        return Ok(true);
    }

    let snapshot = process.zombie_snapshot().ok_or(AxError::NoSuchProcess)?;
    let allowed = [actor_proc.uid(), actor_proc.euid()]
        .into_iter()
        .any(|id| id == snapshot.uid);
    if allowed {
        Ok(true)
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn zombie_signal_succeeds(pid: Pid) -> AxResult<bool> {
    check_zombie_signal_permission(pid)
}

pub fn sys_kill(pid: i32, signo: u32) -> AxResult<isize> {
    debug!("sys_kill: pid = {pid}, signo = {signo}");
    let sig = make_siginfo(signo, SI_USER as _)?;

    match pid {
        1.. => {
            let target = pid as Pid;
            match check_signal_permission(target) {
                Ok(()) => match send_signal_to_process(target, sig) {
                    Ok(()) => {}
                    Err(AxError::NoSuchProcess) if zombie_signal_succeeds(target)? => {}
                    Err(err) => return Err(err),
                },
                Err(AxError::NoSuchProcess) if zombie_signal_succeeds(target)? => {}
                Err(err) => return Err(err),
            }
        }
        0 => {
            let pgid = current().as_thread().proc_data.proc.group().pgid();
            send_signal_to_process_group(pgid, sig)?;
        }
        -1 => {
            let curr_pid = current().as_thread().proc_data.proc.pid();
            if let Some(sig) = sig {
                for proc_data in processes() {
                    // POSIX.1 requires that kill(-1,sig) send sig to all processes that
                    //    the calling process may send signals to, except possibly for some
                    //    implementation-defined system processes.  Linux allows a process
                    //    to signal itself, but on Linux the call kill(-1,sig) does not
                    //    signal the calling process.
                    if proc_data.proc.is_init() || proc_data.proc.pid() == curr_pid {
                        continue;
                    }
                    let _ = send_signal_to_process(proc_data.proc.pid(), Some(sig.clone()));
                }
            }
        }
        ..-1 => {
            send_signal_to_process_group((-pid) as Pid, sig)?;
        }
    }
    Ok(0)
}

pub fn sys_tkill(tid: i32, signo: u32) -> AxResult<isize> {
    if tid <= 0 {
        return Err(AxError::InvalidInput);
    }
    let sig = make_siginfo(signo, SI_TKILL)?;
    send_signal_to_visible_thread(None, tid as Pid, sig)?;
    Ok(0)
}

pub fn sys_tgkill(tgid: i32, tid: i32, signo: u32) -> AxResult<isize> {
    if tgid <= 0 || tid <= 0 {
        return Err(AxError::InvalidInput);
    }
    if signo != 0 && parse_signo(signo)?.is_realtime() {
        ensure_realtime_signal_queue_capacity_for_thread(Some(tgid as Pid), tid as Pid)?;
    }
    let sig = make_siginfo(signo, SI_TKILL)?;
    send_signal_to_visible_thread(Some(tgid as Pid), tid as Pid, sig)?;
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
    if let Ok(task) = get_visible_task(pid) {
        let thread = task.try_as_thread().ok_or(AxError::OperationNotPermitted)?;
        if thread.proc_data.proc.pid() == pid {
            if signo != 0 && parse_signo(signo)?.is_realtime() {
                ensure_realtime_signal_queue_capacity_for_process(pid)?;
            }
            send_signal_to_process(pid, sig)?;
        } else {
            if signo != 0 && parse_signo(signo)?.is_realtime() {
                ensure_realtime_signal_queue_capacity_for_thread(None, pid)?;
            }
            send_signal_to_visible_thread(None, pid, sig)?;
        }
    } else {
        if signo != 0 && parse_signo(signo)?.is_realtime() {
            ensure_realtime_signal_queue_capacity_for_process(pid)?;
        }
        send_signal_to_process(pid, sig)?;
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
    if signo != 0 && parse_signo(signo)?.is_realtime() {
        ensure_realtime_signal_queue_capacity_for_thread(Some(tgid as Pid), tid as Pid)?;
    }

    let sig = make_queue_signal_info(tid as Pid, signo, sig)?;
    send_signal_to_visible_thread(Some(tgid as Pid), tid as Pid, sig)?;
    Ok(0)
}

pub fn sys_rt_sigreturn(uctx: &mut UserContext) -> AxResult<isize> {
    let curr = current();
    let thr = curr.as_thread();
    thr.signal.restore(uctx);
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
