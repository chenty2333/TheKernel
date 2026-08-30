use axhal::uspace::{
    ExceptionInfo, ExceptionKind, ReturnReason, UserContext, UserReturnHookResult,
};
use axtask::{TaskCreateError, TaskInner};
use linux_raw_sys::general::{BUS_ADRERR, SEGV_ACCERR, SEGV_MAPERR};
use thekernel_linux_process_adapter::{LINUX_PID_MAX, Pid, try_pid_from_task_id};
use thekernel_linux_signal::{SignalInfo, Signo};

use super::{
    AsThread, TimerState, check_signals, do_exit, fail_closed_exit,
    force_rseq_fault_signal_current_thread, force_signal_current_thread, has_pending_fatal_signal,
    set_timer_state, terminate_rseq_fault_current_thread, wait_if_stopped,
};
use crate::{
    mm::{
        PageFaultFailure, PageFaultResult, UserMemoryCapability, handle_user_page_fault,
        map_usercopy_error,
    },
    syscall::handle_syscall,
};

/// Maps an `ExceptionKind::Other` exception to the correct POSIX signal using
/// x86_64 exception information.
fn map_other_exception(exc_info: &ExceptionInfo) -> Signo {
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
            let child_tid = thr.take_child_tid_address() as *mut Pid;
            if !child_tid.is_null() {
                // Linux publishes CLONE_CHILD_SETTID from schedule_tail() in
                // the child context. A copy fault does not cancel the clone.
                let capability = UserMemoryCapability::new(thr.proc_data.aspace());
                let _ = capability
                    .write_value(child_tid, thr.proc_data.pid_ns().visible_pid(thr.tid()))
                    .map_err(map_usercopy_error);
            }
            while !thr.pending_exit() {
                // The final rseq gate runs while interrupts are disabled by
                // `run_with_return_hook`; a Retry returns here with IRQs
                // restored so task-context scheduling/fault handling can run
                // before the next attempt.
                let reason = loop {
                    let aspace = thr.proc_data.aspace();
                    match uctx.run_with_return_hook(|uctx| {
                        let action = thr.rseq_return_gate(uctx, &aspace);
                        if matches!(action, axhal::uspace::UserReturnHookAction::EnterUser) {
                            // The TSS map is CPU-local, while ioperm/iopl is
                            // task-local. Refresh or invalidate it only at
                            // this IRQ-disabled final return edge so a
                            // migration cannot expose a prior task's ports.
                            thr.install_user_io_permissions();
                        }
                        action
                    }) {
                        UserReturnHookResult::Returned(reason) => break reason,
                        UserReturnHookResult::Retry => {
                            set_timer_state(&curr, TimerState::Kernel);
                            if thr.pending_exit() {
                                break ReturnReason::Interrupt;
                            }
                            // A nofault rseq snapshot may have observed a
                            // missing PTE or a writable VMA's read-only COW
                            // leaf. The hook has already restored IRQ state;
                            // resolve the complete area/descriptor span in
                            // task context before attempting the gate again.
                            if thr.prepare_rseq_retry(&aspace).is_err() {
                                if !force_rseq_fault_signal_current_thread() {
                                    terminate_rseq_fault_current_thread();
                                }
                                break ReturnReason::Interrupt;
                            }
                            axtask::resched_if_needed();
                        }
                        UserReturnHookResult::Fault => {
                            set_timer_state(&curr, TimerState::Kernel);
                            // A registered area/descriptor which cannot be
                            // observed without faulting is visible as a fatal
                            // user-memory fault; signal processing remains in
                            // the normal kernel return path.
                            if !force_rseq_fault_signal_current_thread() {
                                terminate_rseq_fault_current_thread();
                            }
                            break ReturnReason::Interrupt;
                        }
                    }
                };

                set_timer_state(&curr, TimerState::Kernel);

                match reason {
                    ReturnReason::Syscall => handle_syscall(&mut uctx),
                    ReturnReason::PageFault(addr, flags) => {
                        let aspace_handle = thr.proc_data.aspace();
                        let result =
                            handle_user_page_fault(aspace_handle, addr, flags, uctx.sp().into());
                        if result != PageFaultResult::Handled {
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
                    ReturnReason::Exception(exc_info) => {
                        let signo = match exc_info.kind() {
                            ExceptionKind::Misaligned => Signo::SIGBUS,
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
                if set_timer_state(&curr, TimerState::User) {
                    // The CPU-timer worker became runnable after the earlier
                    // user-return safe point. Consume that wake before leaving
                    // the kernel so a pure CPU loop cannot defer publication.
                    axtask::resched_if_needed();
                }

                // `interrupt` is also the wake edge for a sibling exec gate.
                // Clear it before the final gate read: a gate published
                // before this store is observed below, while a publication
                // after the read leaves the interrupt set for the next trap
                // or blocking point.
                curr.clear_interrupt();

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
