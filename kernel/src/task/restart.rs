use core::sync::atomic::Ordering;

use axhal::uspace::UserContext;
use starry_signal::{SignalOSAction, Signo};

use super::Thread;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum RestartClass {
    Sys,
    NoIntr,
    NoHand,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SavedSyscall {
    sysno: usize,
    args: [usize; 6],
    return_ip: usize,
    restart_ip: usize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RestartDecision {
    Pending,
    Restart,
    ReturnEintr,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RestartState {
    syscall: SavedSyscall,
    class: RestartClass,
    decision: RestartDecision,
}

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
const SYSCALL_INSN_LEN: usize = 4;
#[cfg(target_arch = "x86_64")]
const SYSCALL_INSN_LEN: usize = 2;

impl SavedSyscall {
    pub(crate) fn capture(uctx: &UserContext) -> Self {
        let return_ip = uctx.ip();
        Self {
            sysno: uctx.sysno(),
            args: [
                uctx.arg0(),
                uctx.arg1(),
                uctx.arg2(),
                uctx.arg3(),
                uctx.arg4(),
                uctx.arg5(),
            ],
            return_ip,
            restart_ip: return_ip.saturating_sub(SYSCALL_INSN_LEN),
        }
    }

    fn restore(self, uctx: &mut UserContext) {
        uctx.set_sysno(self.sysno);
        uctx.set_arg0(self.args[0]);
        uctx.set_arg1(self.args[1]);
        uctx.set_arg2(self.args[2]);
        uctx.set_arg3(self.args[3]);
        uctx.set_arg4(self.args[4]);
        uctx.set_arg5(self.args[5]);
        uctx.set_ip(self.restart_ip);
    }

    fn matches_return_context(self, uctx: &UserContext) -> bool {
        uctx.ip() == self.return_ip
    }
}

impl RestartClass {
    fn decide(self, restartable_handler: bool) -> RestartDecision {
        match self {
            RestartClass::Sys => {
                if restartable_handler {
                    RestartDecision::Restart
                } else {
                    RestartDecision::ReturnEintr
                }
            }
            RestartClass::NoIntr => RestartDecision::Restart,
            RestartClass::NoHand => RestartDecision::ReturnEintr,
        }
    }
}

impl Thread {
    pub(crate) fn enter_syscall(&self, uctx: &UserContext, preserve_restart_state: bool) {
        *self.current_syscall.lock() = Some(SavedSyscall::capture(uctx));
        if !preserve_restart_state && !self.in_signal_handler() {
            self.restart_states.lock().clear();
        }
    }

    pub(crate) fn in_signal_handler(&self) -> bool {
        self.signal_handler_depth.load(Ordering::SeqCst) > 0
    }

    pub(crate) fn request_syscall_restart(&self, class: RestartClass) {
        let Some(syscall) = *self.current_syscall.lock() else {
            return;
        };
        self.restart_states.lock().push(RestartState {
            syscall,
            class,
            decision: RestartDecision::Pending,
        });
    }

    pub(crate) fn finish_signal_delivery(&self, signo: Signo, os_action: SignalOSAction) {
        match os_action {
            SignalOSAction::Handler => {
                self.signal_handler_depth.fetch_add(1, Ordering::SeqCst);
                let mut restart_states = self.restart_states.lock();
                if let Some(state) = restart_states.last_mut()
                    && state.decision == RestartDecision::Pending
                {
                    state.decision = state.class.decide(self.proc_data.signal.can_restart(signo));
                }
            }
            _ => {
                self.restart_states.lock().pop();
            }
        }
    }

    pub(crate) fn complete_sigreturn(&self, uctx: &mut UserContext) {
        let depth = self.signal_handler_depth.load(Ordering::SeqCst);
        debug_assert!(depth > 0, "sigreturn without an active signal handler");
        if depth > 0 {
            self.signal_handler_depth.fetch_sub(1, Ordering::SeqCst);
        }
        let state = {
            let mut restart_states = self.restart_states.lock();
            let Some(state) = restart_states.last().copied() else {
                return;
            };
            if !state.syscall.matches_return_context(uctx) {
                return;
            }
            restart_states.pop().unwrap()
        };

        if state.decision == RestartDecision::Restart {
            state.syscall.restore(uctx);
            self.resume_restored_context.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn take_resume_restored_context(&self) -> bool {
        self.resume_restored_context.swap(false, Ordering::SeqCst)
    }

    pub(crate) fn clear_saved_syscall(&self) {
        *self.current_syscall.lock() = None;
    }
}
