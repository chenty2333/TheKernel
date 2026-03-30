use axhal::uspace::{ExceptionInfo, ExceptionKind, ReturnReason, UserContext};
use axtask::TaskInner;
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};
use starry_vm::{VmMutPtr, VmPtr};

use super::{
    AsThread, TimerState, check_signals, do_exit, has_pending_fatal_signal, raise_signal_fatal,
    set_timer_state, wait_if_stopped,
};
use crate::syscall::handle_syscall;

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

/// Create a new user task.
pub fn new_user_task(name: &str, mut uctx: UserContext, set_child_tid: usize) -> TaskInner {
    TaskInner::new(
        move || {
            let curr = axtask::current();

            if let Some(tid) = (set_child_tid as *mut Pid).nullable() {
                tid.vm_write(curr.id().as_u64() as Pid).ok();
            }

            info!("Enter user space: ip={:#x}, sp={:#x}", uctx.ip(), uctx.sp());

            let thr = curr.as_thread();
            let tid = curr.id().as_u64() as Pid;
            while !thr.pending_exit() {
                let reason = uctx.run();

                set_timer_state(&curr, TimerState::Kernel);

                match reason {
                    ReturnReason::Syscall => handle_syscall(&mut uctx),
                    ReturnReason::PageFault(addr, flags) => {
                        if !thr.proc_data.aspace.lock().handle_page_fault(addr, flags) {
                            info!(
                                "{:?}: segmentation fault at {:#x} {:?}",
                                thr.proc_data.proc, addr, flags
                            );
                            raise_signal_fatal(SignalInfo::new_kernel(Signo::SIGSEGV))
                                .expect("Failed to send SIGSEGV");
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
                        raise_signal_fatal(SignalInfo::new_kernel(signo))
                            .expect("Failed to send signal");
                    }
                    r => {
                        warn!("Unexpected return reason: {r:?}");
                        raise_signal_fatal(SignalInfo::new_kernel(Signo::SIGSEGV))
                            .expect("Failed to send SIGSEGV");
                    }
                }

                if thr.proc_data.should_exit_for_exec(tid) {
                    if has_pending_fatal_signal(thr) {
                        while check_signals(thr, &mut uctx, None) {}
                    } else {
                        do_exit(0, false);
                        continue;
                    }
                }

                if !thr.take_block_next_signal_check() {
                    while check_signals(thr, &mut uctx, None) {}
                }

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

                set_timer_state(&curr, TimerState::User);
                curr.clear_interrupt();
            }
        },
        name.into(),
        crate::config::KERNEL_STACK_SIZE,
    )
}
