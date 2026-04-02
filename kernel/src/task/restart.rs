use alloc::vec::Vec;

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
struct SavedSyscall {
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
struct RestartState {
    syscall: SavedSyscall,
    class: RestartClass,
    decision: RestartDecision,
}

#[derive(Debug, Default)]
pub(crate) struct RestartTracker {
    signal_handler_depth: usize,
    current_syscall: Option<SavedSyscall>,
    restart_states: Vec<RestartState>,
    preserve_restored_context: bool,
}

#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
const SYSCALL_INSN_LEN: usize = 4;
#[cfg(target_arch = "x86_64")]
const SYSCALL_INSN_LEN: usize = 2;

impl SavedSyscall {
    fn capture(uctx: &UserContext) -> Self {
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

    fn restart_without_handler(self) -> bool {
        matches!(
            self,
            RestartClass::Sys | RestartClass::NoIntr | RestartClass::NoHand
        )
    }
}

impl Thread {
    pub(crate) fn enter_syscall(&self, uctx: &UserContext, preserve_restart_state: bool) {
        let mut restart = self.restart.lock();
        restart.current_syscall = Some(SavedSyscall::capture(uctx));
        restart.preserve_restored_context = false;
        if !preserve_restart_state && restart.signal_handler_depth == 0 {
            restart.restart_states.clear();
        }
    }

    pub(crate) fn request_syscall_restart(&self, class: RestartClass) {
        let mut restart = self.restart.lock();
        let Some(syscall) = restart.current_syscall else {
            return;
        };
        restart.restart_states.push(RestartState {
            syscall,
            class,
            decision: RestartDecision::Pending,
        });
    }

    pub(crate) fn finish_signal_delivery(
        &self,
        signo: Signo,
        os_action: SignalOSAction,
        uctx: &mut UserContext,
    ) {
        let mut restart = self.restart.lock();
        match os_action {
            SignalOSAction::Handler => {
                restart.signal_handler_depth += 1;
                if let Some(state) = restart.restart_states.last_mut()
                    && state.decision == RestartDecision::Pending
                {
                    state.decision = state.class.decide(self.proc_data.signal.can_restart(signo));
                }
            }
            _ => {
                if let Some(state) = restart.restart_states.pop()
                    && state.class.restart_without_handler()
                {
                    state.syscall.restore(uctx);
                }
            }
        }
    }

    pub(crate) fn complete_sigreturn(&self, uctx: &mut UserContext) {
        let mut restart = self.restart.lock();
        if restart.signal_handler_depth == 0 {
            return;
        }
        restart.signal_handler_depth -= 1;

        let Some(state) = restart.restart_states.last().copied() else {
            return;
        };
        if !state.syscall.matches_return_context(uctx) {
            return;
        }

        let state = restart.restart_states.pop().unwrap();
        if state.decision == RestartDecision::Restart {
            state.syscall.restore(uctx);
            restart.preserve_restored_context = true;
        }
    }

    pub(crate) fn take_resume_restored_context(&self) -> bool {
        let mut restart = self.restart.lock();
        let preserve = restart.preserve_restored_context;
        restart.preserve_restored_context = false;
        preserve
    }

    pub(crate) fn clear_saved_syscall(&self) {
        self.restart.lock().current_syscall = None;
    }
}
