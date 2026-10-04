//! User-boundary tracing. Snapshots are owned by the parked task.
use core::sync::atomic::Ordering;

use axhal::uspace::UserContext;
use tk_linux_signal::{SignalInfo, Signo};

use super::{Thread, notify_ptrace_attach_stop, wait_if_stopped};

pub(crate) const TRAP_FLAG: u64 = 1 << 8;
pub(crate) const STEP_INSTRUCTION: u8 = 4;
pub(crate) const INHERITED_SIGNAL_STOP: u8 = 3;
const LEGACY_EXEC_STOP: u8 = 4;

pub(crate) const TRACE_SYSCALL: u8 = 1;
pub(crate) const EMULATE_SYSCALL: u8 = 2;

fn mode_for_session(
    stored: Option<(super::PtraceSession, u8)>,
    active: Option<super::PtraceSession>,
) -> u8 {
    stored
        .filter(|(session, _)| Some(*session) == active)
        .map_or(0, |(_, mode)| mode)
}

fn mode(thr: &Thread) -> u8 {
    let active = thr.proc_data.ptrace_active_session();
    mode_for_session(*thr.ptrace_syscall_mode.lock(), active)
}

/// Opcode facts only: x86 POPF/IRET can change TF themselves. Prefixes do not
/// turn that user-owned flag into a debugger-owned one. x86_64 is the sole ABI.
pub(crate) fn instruction_changes_tf(bytes: &[u8]) -> bool {
    for &byte in bytes.iter().take(15) {
        match byte {
            0x9d | 0xcf => return true,
            0x26
            | 0x2e
            | 0x36
            | 0x3e
            | 0x64
            | 0x65
            | 0x66
            | 0x67
            | 0xf0
            | 0xf2
            | 0xf3
            | 0x40..=0x4f => {}
            _ => return false,
        }
    }
    false
}

pub(crate) fn commit_resume_mode(
    thr: &Thread,
    session: super::PtraceSession,
    resume_mode: u8,
    changes_tf: bool,
) {
    if resume_mode & STEP_INSTRUCTION != 0 {
        let flags = thr
            .ptrace_registers
            .lock()
            .as_ref()
            .map_or(0, |regs| regs[18]);
        thr.ptrace_forced_tf
            .store(flags & TRAP_FLAG == 0 && !changes_tf, Ordering::Release);
    } else if thr.ptrace_forced_tf.swap(false, Ordering::AcqRel)
        && let Some(regs) = thr.ptrace_registers.lock().as_mut()
    {
        regs[18] &= !TRAP_FLAG;
    }
    *thr.ptrace_syscall_mode.lock() = Some((session, resume_mode));
}

/// Final IRQ-disabled user-entry edge; unrelated IRQ returns may not lose TF.
pub(crate) fn prepare_user_step(thr: &Thread, uctx: &mut UserContext) {
    if mode(thr) & STEP_INSTRUCTION != 0 {
        uctx.rflags |= TRAP_FLAG;
    } else if thr.ptrace_forced_tf.load(Ordering::Acquire) {
        uctx.rflags &= !TRAP_FLAG;
    }
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
    thr.ptrace_stop_provenance.store(op, Ordering::Release);
    if let Some(good) = thr.proc_data.ptrace_syscall_stop(session, op) {
        thr.ptrace_stop_provenance
            .store(op | if good { 0x80 } else { 0 }, Ordering::Release);
        super::signal::interrupt_stop_siblings(&thr.proc_data);
        notify_ptrace_attach_stop(&thr.proc_data);
        wait_if_stopped(thr, uctx);
    }
    thr.ptrace_stop_provenance.store(0, Ordering::Release);
}

pub(crate) fn report_exec(thr: &Thread, session: super::PtraceSession, old_pid: usize) {
    thr.ptrace_stop_provenance
        .store(LEGACY_EXEC_STOP, Ordering::Release);
    match thr.proc_data.ptrace_exec_stop(session, old_pid) {
        Some(0) => notify_ptrace_attach_stop(&thr.proc_data),
        Some(_) => {
            thr.ptrace_stop_provenance.store(0, Ordering::Release);
            notify_ptrace_attach_stop(&thr.proc_data);
        }
        None => thr.ptrace_stop_provenance.store(0, Ordering::Release),
    }
}

pub(crate) fn synthetic_stop_signal_info(thr: &Thread) -> Option<SignalInfo> {
    let provenance = thr.ptrace_stop_provenance.load(Ordering::Acquire);
    let stop = thr.proc_data.current_stop_report()?;
    if stop.ptrace_event != 0 {
        return None;
    }
    if provenance == INHERITED_SIGNAL_STOP && stop.signal == Signo::SIGSTOP as u8 {
        // Linux adds a bare pending SIGSTOP to a non-seized inherited child.
        return Some(SignalInfo::new_user(Signo::SIGSTOP, 0, 0, 0));
    }
    if provenance == LEGACY_EXEC_STOP && stop.signal == Signo::SIGTRAP as u8 {
        return Some(SignalInfo::new_user(
            Signo::SIGTRAP,
            0,
            thr.proc_data.pid_ns().visible_pid(thr.tid()),
            thr.proc_data
                .user_ns()
                .from_kuid_munged(thr.current_cred().ids().ruid),
        ));
    }
    if !matches!(provenance & 0x7f, 1 | 2)
        || stop.signal != (Signo::SIGTRAP as u8 | (provenance & 0x80))
    {
        return None;
    }
    Some(SignalInfo::new_user(
        Signo::SIGTRAP,
        Signo::SIGTRAP as i32 | (provenance as i32 & 0x80),
        thr.proc_data.pid_ns().visible_pid(thr.tid()),
        thr.proc_data
            .user_ns()
            .from_kuid_munged(thr.current_cred().ids().ruid),
    ))
}

fn skip_syscall(entry_mode: u8, orig_rax: u64) -> bool {
    entry_mode & EMULATE_SYSCALL != 0 || (orig_rax as i64) < 0
}

pub(crate) fn syscall_info_operation(provenance: u8, event: u8) -> u8 {
    if event == 0 && provenance & 0x80 != 0 {
        provenance & 0x7f
    } else {
        0
    }
}

pub(crate) fn handle_traced_syscall(thr: &Thread, uctx: &mut UserContext) {
    // Entry work is sampled once. Resuming an emulation stop with CONT still
    // skips that syscall; resuming an untraced signal stop with SYSCALL must
    // not retroactively manufacture an exit for the enclosing syscall.
    let entry_mode = mode(thr);
    if entry_mode & (TRACE_SYSCALL | EMULATE_SYSCALL) != 0 {
        uctx.rax = -38i64 as u64; // native ENOSYS entry sentinel
        syscall_stop(thr, uctx, 1);
    }
    let exit_mode = mode(thr);
    let exit_trace = exit_mode & TRACE_SYSCALL != 0;
    let exit_step = (exit_mode & STEP_INSTRUCTION != 0 || uctx.rflags & TRAP_FLAG != 0)
        && exit_mode & EMULATE_SYSCALL == 0;
    let sysno = thr.ptrace_orig_rax.load(Ordering::Acquire);
    if entry_mode & (TRACE_SYSCALL | EMULATE_SYSCALL) == 0 && (sysno as i64) < 0 {
        uctx.rax = -38i64 as u64;
    }
    if !thr.pending_exit() && !skip_syscall(entry_mode, sysno) {
        // Generic dispatch uses RAX, while debugger admission uses orig_rax.
        uctx.rax = sysno;
        crate::syscall::handle_syscall(uctx);
    }
    if exit_step && !thr.pending_exit() {
        wait_if_stopped(thr, uctx);
        if (mode(thr) & STEP_INSTRUCTION != 0 || uctx.rflags & TRAP_FLAG != 0)
            && mode(thr) & EMULATE_SYSCALL == 0
        {
            // Linux x86 user_single_step_report uses TRAP_BRKPT at syscall exit,
            // distinct from the architectural TRAP_TRACE instruction exception.
            super::force_signal_current_thread(SignalInfo::new_fault(
                Signo::SIGTRAP,
                linux_raw_sys::general::TRAP_BRKPT as i32,
                uctx.ip(),
            ));
            wait_if_stopped(thr, uctx);
        }
    } else if exit_trace && !thr.pending_exit() {
        wait_if_stopped(thr, uctx);
        if mode(thr) & TRACE_SYSCALL != 0 {
            syscall_stop(thr, uctx, 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tf_changing_instructions_are_not_claimed_by_debugger_flag_cleanup() {
        assert!(instruction_changes_tf(&[0x9d]));
        assert!(instruction_changes_tf(&[0x66, 0x48, 0xcf]));
        assert!(!instruction_changes_tf(&[0x90, 0x9d]));
        assert!(!instruction_changes_tf(&[0x0f, 0x05]));
        assert!(!instruction_changes_tf(&[0x66; 16]));
        assert!(!instruction_changes_tf(&[]));
    }

    #[test]
    fn a_new_relationship_does_not_inherit_an_old_resume_mode() {
        let session = super::super::PtraceSession {
            tracer: 1,
            tracer_kernel_tid: 1,
            generation: 5,
        };
        let replacement = super::super::PtraceSession {
            generation: 6,
            ..session
        };
        assert_eq!(
            mode_for_session(Some((session, TRACE_SYSCALL)), Some(session)),
            TRACE_SYSCALL
        );
        assert_eq!(
            mode_for_session(Some((session, TRACE_SYSCALL)), Some(replacement)),
            0
        );
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
