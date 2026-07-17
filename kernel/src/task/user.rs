use axhal::uspace::{ExceptionInfo, ExceptionKind, ReturnReason, UserContext};
use axtask::{TaskCreateError, TaskInner};
use linux_raw_sys::general::{BUS_ADRERR, SEGV_ACCERR, SEGV_MAPERR};
use starry_process::{LINUX_PID_MAX, Pid, try_pid_from_task_id};
use starry_signal::{SignalInfo, Signo};

use super::{
    AsThread, TimerState, check_signals, do_exit, fail_closed_exit, force_signal_current_thread,
    has_pending_fatal_signal, set_timer_state, wait_if_stopped,
};
use crate::{
    mm::{PageFaultFailure, PageFaultResult},
    syscall::handle_syscall,
};

/// Maps an `ExceptionKind::Other` exception to the correct POSIX signal using
/// arch-specific exception information.
#[allow(unused_variables)]
fn map_other_exception(exc_info: &ExceptionInfo) -> Signo {
    #[cfg(target_arch = "x86_64")]
    {
        // x86_64 exception vectors that map to specific signals:
        match exc_info.vector {
            // Division error, Overflow, x87 FP, SIMD FP → SIGFPE
            0x00 | 0x04 | 0x10 | 0x13 => return Signo::SIGFPE,
            // Debug → SIGTRAP
            0x01 => return Signo::SIGTRAP,
            // Segment not present, Stack fault, Alignment check → SIGBUS
            0x0B | 0x0C | 0x11 => return Signo::SIGBUS,
            // Bound range exceeded, General protection, Double fault → SIGSEGV
            0x05 | 0x08 | 0x0D => return Signo::SIGSEGV,
            _ => {}
        }
    }

    // Default: unknown exceptions are most likely access violations.
    // SIGSEGV is the safest default (SIGTRAP would incorrectly suggest a
    // debugger event).
    Signo::SIGSEGV
}

fn deliver_fatal_user_signal_info(info: SignalInfo) {
    force_signal_current_thread(info);
}

fn deliver_fatal_user_signal(signo: Signo) {
    deliver_fatal_user_signal_info(SignalInfo::new_kernel(signo));
}

/// Fallibly creates an unpublished user task.
pub fn try_new_user_task(name: String, mut uctx: UserContext) -> AxResult<TaskInner> {
    TaskInner::try_new_with_id_limit(
        move || {
            let curr = axtask::current();
            info!("Enter user space: ip={:#x}, sp={:#x}", uctx.ip(), uctx.sp());

            let thr = curr.as_thread();
            let tid = linux_pid_from_task_id(curr.id().as_u64())
                .unwrap_or_else(|error| fail_closed_exit(error));
            while !thr.pending_exit() {
                #[cfg(target_arch = "loongarch64")]
                super::restore_current_user_fpu_state();
                let reason = uctx.run();
                #[cfg(target_arch = "loongarch64")]
                super::save_current_user_fpu_state();

                set_timer_state(&curr, TimerState::Kernel);

                match reason {
                    ReturnReason::Syscall => handle_syscall(&mut uctx),
                    ReturnReason::PageFault(addr, flags) => {
                        let aspace_handle = thr.proc_data.aspace();
                        let result = aspace_handle.lock().handle_page_fault_result(
                            addr,
                            flags,
                            Some(uctx.sp().into()),
                        );
                        if result != PageFaultResult::Handled {
                            #[cfg(target_arch = "riscv64")]
                            info!(
                                "{:?}: user page fault at {:#x} {:?}, pc={:#x}, ra={:#x}, \
                                 sp={:#x}, a0={:#x}, a1={:#x}, tp={:#x}",
                                thr.proc_data.proc,
                                addr,
                                flags,
                                uctx.ip(),
                                uctx.regs.ra,
                                uctx.sp(),
                                uctx.regs.a0,
                                uctx.regs.a1,
                                uctx.regs.tp,
                            );
                            #[cfg(not(target_arch = "riscv64"))]
                            info!(
                                "{:?}: user page fault at {:#x} {:?}, pc={:#x}, sp={:#x}",
                                thr.proc_data.proc,
                                addr,
                                flags,
                                uctx.ip(),
                                uctx.sp(),
                            );
                            let info = match result {
                                PageFaultResult::Handled => unreachable!(),
                                PageFaultResult::Failed(PageFaultFailure::OutOfMemory) => {
                                    SignalInfo::new_kernel(Signo::SIGKILL)
                                }
                                PageFaultResult::Failed(
                                    PageFaultFailure::BackingUnavailable
                                    | PageFaultFailure::InternalInconsistency,
                                ) => SignalInfo::new_fault(
                                    Signo::SIGBUS,
                                    BUS_ADRERR as i32,
                                    addr.as_usize(),
                                ),
                                PageFaultResult::Failed(PageFaultFailure::AddressNotMapped) => {
                                    SignalInfo::new_fault(
                                        Signo::SIGSEGV,
                                        SEGV_MAPERR as i32,
                                        addr.as_usize(),
                                    )
                                }
                                PageFaultResult::Failed(PageFaultFailure::AccessDenied) => {
                                    SignalInfo::new_fault(
                                        Signo::SIGSEGV,
                                        SEGV_ACCERR as i32,
                                        addr.as_usize(),
                                    )
                                }
                            };
                            deliver_fatal_user_signal_info(info);
                        }
                    }
                    ReturnReason::Interrupt => {}
                    #[allow(unused_labels)]
                    ReturnReason::Exception(exc_info) => 'exc: {
                        let signo = match exc_info.kind() {
                            ExceptionKind::Misaligned => {
                                #[cfg(target_arch = "loongarch64")]
                                if unsafe { uctx.emulate_unaligned() }.is_ok() {
                                    break 'exc;
                                }
                                Signo::SIGBUS
                            }
                            ExceptionKind::Breakpoint => Signo::SIGTRAP,
                            ExceptionKind::IllegalInstruction => Signo::SIGILL,
                            ExceptionKind::Other => map_other_exception(&exc_info),
                        };
                        deliver_fatal_user_signal(signo);
                    }
                    r => {
                        warn!("Unexpected return reason: {r:?}");
                        deliver_fatal_user_signal(Signo::SIGSEGV);
                    }
                }

                if thr.pending_exit() {
                    break;
                }
                // Timer IRQ handling marks the task for preemption. Honor that
                // request at every user-return boundary, regardless of trap kind.
                axtask::resched_if_needed();

                if thr.proc_data.should_exit_for_exec(tid) {
                    if has_pending_fatal_signal(thr) {
                        while check_signals(thr, &mut uctx, None) {}
                    } else {
                        if let Err(error) = do_exit(0, false) {
                            fail_closed_exit(error);
                        }
                        continue;
                    }
                }

                while check_signals(thr, &mut uctx, None) {}

                // Block if the process has been stopped (by this or another thread).
                wait_if_stopped(thr, &mut uctx);

                if thr.proc_data.should_exit_for_exec(tid) {
                    if has_pending_fatal_signal(thr) {
                        while check_signals(thr, &mut uctx, None) {}
                    } else {
                        if let Err(error) = do_exit(0, false) {
                            fail_closed_exit(error);
                        }
                        continue;
                    }
                }

                thr.finish_signal_resume(&mut uctx);
                set_timer_state(&curr, TimerState::User);
                curr.clear_interrupt();
            }
        },
        name,
        crate::config::KERNEL_STACK_SIZE,
        LINUX_PID_MAX as u64,
    )
    .map_err(task_create_error)
}

/// Admits one generic scheduler identity into the finite nonzero Linux PID/TID
/// domain without truncation.
pub(crate) fn linux_pid_from_task_id(task_id: u64) -> AxResult<Pid> {
    try_pid_from_task_id(task_id).map_err(|_| AxError::WouldBlock)
}

fn task_create_error(error: TaskCreateError) -> AxError {
    match error {
        TaskCreateError::InvalidStackSize => AxError::BadState,
        TaskCreateError::OutOfMemory => AxError::NoMemory,
        TaskCreateError::IdentifierExhausted => AxError::WouldBlock,
    }
}
use alloc::string::String;

use axerrno::{AxError, AxResult};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_task_identity_exhaustion_maps_to_eagain_without_truncation() {
        assert_eq!(linux_pid_from_task_id(0), Err(AxError::WouldBlock));
        assert_eq!(
            linux_pid_from_task_id(LINUX_PID_MAX as u64),
            Ok(LINUX_PID_MAX)
        );
        assert_eq!(
            linux_pid_from_task_id(LINUX_PID_MAX as u64 + 1),
            Err(AxError::WouldBlock)
        );
        assert_eq!(
            task_create_error(TaskCreateError::IdentifierExhausted),
            AxError::WouldBlock
        );
    }
}
