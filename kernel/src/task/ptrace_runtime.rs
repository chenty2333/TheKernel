//! User-boundary tracing. Snapshots are owned by the parked task.
use core::sync::atomic::Ordering;

use axhal::uspace::UserContext;
use tk_linux_signal::{SignalInfo, Signo};

use super::{Thread, notify_ptrace_attach_stop, wait_if_stopped};

pub(crate) const TRACE_SYSCALL: u8 = 1;
pub(crate) const EMULATE_SYSCALL: u8 = 2;

fn mode_for_session(
    stored: Option<(super::PtraceSession, u8)>,
    active: Option<super::PtraceSession>,
) -> u8 {
    stored.filter(|(session, _)| Some(*session) == active).map_or(0, |(_, mode)| mode)
}

fn mode(thr: &Thread) -> u8 {
    let active = thr.proc_data.ptrace_active_session();
    mode_for_session(*thr.ptrace_syscall_mode.lock(), active)
}

fn syscall_stop(thr: &Thread, uctx: &mut UserContext, op: u8) {
    // Complete a separately published signal/event stop first. In particular,
    // don't overwrite an event inside the syscall with an exit notification.
    wait_if_stopped(thr, uctx);
    if thr.pending_exit() {
        return;
    }
    let Some(session) = thr.proc_data.ptrace_active_session() else {
        return;
    };
    // Publish provenance before waking the tracer; scheduler inactivity then
    // guarantees the corresponding GPR image is ready for remote access.
    thr.ptrace_syscall_stop.store(op, Ordering::Release);
    if let Some(good) = thr.proc_data.ptrace_syscall_stop(session, op) {
        thr.ptrace_syscall_stop.store(op | if good { 0x80 } else { 0 }, Ordering::Release);
        super::signal::interrupt_stop_siblings(&thr.proc_data);
        notify_ptrace_attach_stop(&thr.proc_data);
        wait_if_stopped(thr, uctx);
    }
    thr.ptrace_syscall_stop.store(0, Ordering::Release);
}

pub(crate) fn syscall_stop_signal_info(thr: &Thread) -> Option<SignalInfo> {
    let provenance = thr.ptrace_syscall_stop.load(Ordering::Acquire);
    if provenance == 0 || !thr.proc_data.current_stop_report().is_some_and(|stop| stop.ptrace_event == 0) {
        return None;
    }
    Some(SignalInfo::new_user(
        Signo::SIGTRAP,
        Signo::SIGTRAP as i32 | (provenance as i32 & 0x80),
        thr.proc_data.pid_ns().visible_pid(thr.tid()),
        thr.proc_data.user_ns().from_kuid_munged(thr.current_cred().ids().ruid),
    ))
}

fn skip_syscall(entry_mode: u8, orig_rax: u64) -> bool {
    entry_mode == EMULATE_SYSCALL || (orig_rax as i64) < 0
}

pub(crate) fn syscall_info_operation(provenance: u8, event: u8) -> u8 {
    if event == 0 && provenance & 0x80 != 0 { provenance & 0x7f } else { 0 }
}

pub(crate) fn handle_traced_syscall(thr: &Thread, uctx: &mut UserContext) {
    // Entry work is sampled once. Resuming an emulation stop with CONT still
    // skips that syscall; resuming an untraced signal stop with SYSCALL must
    // not retroactively manufacture an exit for the enclosing syscall.
    let entry_mode = mode(thr);
    if entry_mode != 0 {
        uctx.rax = -38i64 as u64; // native ENOSYS entry sentinel
        syscall_stop(thr, uctx, 1);
    }
    let exit_trace = mode(thr) == TRACE_SYSCALL;
    let sysno = thr.ptrace_orig_rax.load(Ordering::Acquire);
    if entry_mode == 0 && (sysno as i64) < 0 {
        uctx.rax = -38i64 as u64;
    }
    if !thr.pending_exit() && !skip_syscall(entry_mode, sysno) {
        // Generic dispatch uses RAX, while debugger admission uses orig_rax.
        uctx.rax = sysno;
        crate::syscall::handle_syscall(uctx);
    }
    if exit_trace && !thr.pending_exit() {
        wait_if_stopped(thr, uctx);
        if mode(thr) == TRACE_SYSCALL {
            syscall_stop(thr, uctx, 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_relationship_does_not_inherit_an_old_resume_mode() {
        let session = super::super::PtraceSession { tracer: 1, tracer_kernel_tid: 1, generation: 5 };
        let replacement = super::super::PtraceSession { generation: 6, ..session };
        assert_eq!(mode_for_session(Some((session, TRACE_SYSCALL)), Some(session)), TRACE_SYSCALL);
        assert_eq!(mode_for_session(Some((session, TRACE_SYSCALL)), Some(replacement)), 0);
        assert_eq!(mode_for_session(Some((session, TRACE_SYSCALL)), None), 0);
        assert_eq!(mode_for_session(None, Some(session)), 0);
    }

    #[test]
    fn syscall_skip_uses_entry_work_not_the_resume_request() {
        assert!(skip_syscall(EMULATE_SYSCALL, 1));
        assert!(skip_syscall(TRACE_SYSCALL, u64::MAX));
        assert!(skip_syscall(0, 1 << 63));
        assert!(!skip_syscall(TRACE_SYSCALL, 1));
        assert!(!skip_syscall(0, 1));
    }

    #[test]
    fn syscall_info_requires_the_published_good_stop_not_an_event() {
        assert_eq!(syscall_info_operation(1, 0), 0);
        assert_eq!(syscall_info_operation(0x81, 0), 1);
        assert_eq!(syscall_info_operation(0x82, 0), 2);
        assert_eq!(syscall_info_operation(0x81, 128), 0);
        assert_eq!(syscall_info_operation(0x82, 4), 0);
        assert_eq!(syscall_info_operation(0, 0), 0);
    }
}
