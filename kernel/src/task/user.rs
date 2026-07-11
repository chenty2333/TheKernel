use axhal::uspace::{ExceptionInfo, ExceptionKind, ReturnReason, UserContext};
use axtask::TaskInner;
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};

use super::{
    AsThread, TimerState, check_signals, do_exit, has_pending_fatal_signal, raise_signal_fatal,
    set_timer_state, wait_if_stopped,
};
use crate::{mm::PageFaultResult, syscall::handle_syscall};

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

fn deliver_fatal_user_signal(signo: Signo) {
    if let Err(err) = raise_signal_fatal(SignalInfo::new_kernel(signo)) {
        error!("Failed to deliver fatal user signal {signo:?}: {err:?}");
        do_exit(signo as i32, true);
    }
}

/// Fallibly creates an unpublished user task.
pub fn try_new_user_task(name: String, mut uctx: UserContext) -> AxResult<TaskInner> {
    TaskInner::try_new(
        move || {
            let curr = axtask::current();
            info!("Enter user space: ip={:#x}, sp={:#x}", uctx.ip(), uctx.sp());

            let thr = curr.as_thread();
            let tid = curr.id().as_u64() as Pid;
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
                                "{:?}: segmentation fault at {:#x} {:?}, pc={:#x}, ra={:#x}, \
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
                                "{:?}: segmentation fault at {:#x} {:?}, pc={:#x}, sp={:#x}",
                                thr.proc_data.proc,
                                addr,
                                flags,
                                uctx.ip(),
                                uctx.sp(),
                            );
                            let signo = if result == PageFaultResult::SigBus {
                                Signo::SIGBUS
                            } else {
                                Signo::SIGSEGV
                            };
                            deliver_fatal_user_signal(signo);
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
                        do_exit(0, false);
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
                        do_exit(0, false);
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
    )
    .map_err(|_| AxError::NoMemory)
}
use alloc::string::String;

use axerrno::{AxError, AxResult};
