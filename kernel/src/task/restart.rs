use alloc::vec::Vec;

use axhal::uspace::UserContext;
use starry_signal::SignalOSAction;

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
    resume_restored_context: bool,
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
    fn decide_for_handler(self, restartable_handler: bool) -> RestartDecision {
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

    fn decide_for_non_handler(self) -> RestartDecision {
        match self {
            RestartClass::Sys | RestartClass::NoIntr | RestartClass::NoHand => {
                RestartDecision::Restart
            }
        }
    }
}

impl RestartTracker {
    fn enter_syscall(&mut self, uctx: &UserContext, preserve_restart_state: bool) {
        self.current_syscall = Some(SavedSyscall::capture(uctx));
        if !preserve_restart_state && self.signal_handler_depth == 0 {
            self.restart_states.clear();
            self.resume_restored_context = false;
        }
    }

    fn in_signal_handler(&self) -> bool {
        self.signal_handler_depth > 0
    }

    fn request_syscall_restart(&mut self, class: RestartClass) {
        let Some(syscall) = self.current_syscall else {
            return;
        };
        self.restart_states.push(RestartState {
            syscall,
            class,
            decision: RestartDecision::Pending,
        });
    }

    fn finish_signal_delivery(&mut self, os_action: SignalOSAction, restartable_handler: bool) {
        match os_action {
            SignalOSAction::Handler => {
                self.signal_handler_depth += 1;
                if let Some(state) = self.restart_states.last_mut()
                    && state.decision == RestartDecision::Pending
                {
                    state.decision = state.class.decide_for_handler(restartable_handler);
                }
            }
            _ => {
                if let Some(state) = self.restart_states.last_mut()
                    && state.decision == RestartDecision::Pending
                {
                    state.decision = state.class.decide_for_non_handler();
                }
            }
        }
    }

    fn finish_signal_resume(&mut self, uctx: &mut UserContext) {
        if self.signal_handler_depth != 0 {
            return;
        }

        let Some(state) = self.restart_states.last().copied() else {
            return;
        };
        if !state.syscall.matches_return_context(uctx) {
            return;
        }

        let state = self.restart_states.pop().unwrap();
        if state.decision == RestartDecision::Restart {
            state.syscall.restore(uctx);
        }
    }

    fn complete_sigreturn(&mut self, uctx: &mut UserContext) {
        debug_assert!(
            self.signal_handler_depth > 0,
            "sigreturn without an active signal handler"
        );
        if self.signal_handler_depth > 0 {
            self.signal_handler_depth -= 1;
        }

        let Some(state) = self.restart_states.last().copied() else {
            return;
        };
        if !state.syscall.matches_return_context(uctx) {
            return;
        }

        let state = self.restart_states.pop().unwrap();
        if state.decision == RestartDecision::Restart {
            state.syscall.restore(uctx);
            self.resume_restored_context = true;
        }
    }

    fn take_resume_restored_context(&mut self) -> bool {
        core::mem::take(&mut self.resume_restored_context)
    }

    fn clear_saved_syscall(&mut self) {
        self.current_syscall = None;
    }
}

impl Thread {
    pub(crate) fn enter_syscall(&self, uctx: &UserContext, preserve_restart_state: bool) {
        self.restart
            .lock()
            .enter_syscall(uctx, preserve_restart_state);
    }

    pub(crate) fn in_signal_handler(&self) -> bool {
        self.restart.lock().in_signal_handler()
    }

    pub(crate) fn request_syscall_restart(&self, class: RestartClass) {
        self.restart.lock().request_syscall_restart(class);
    }

    pub(crate) fn finish_signal_delivery(
        &self,
        os_action: SignalOSAction,
        restartable_handler: bool,
    ) {
        self.restart
            .lock()
            .finish_signal_delivery(os_action, restartable_handler);
    }

    pub(crate) fn complete_sigreturn(&self, uctx: &mut UserContext) {
        self.restart.lock().complete_sigreturn(uctx);
    }

    pub(crate) fn finish_signal_resume(&self, uctx: &mut UserContext) {
        self.restart.lock().finish_signal_resume(uctx);
    }

    pub(crate) fn take_resume_restored_context(&self) -> bool {
        self.restart.lock().take_resume_restored_context()
    }

    pub(crate) fn clear_saved_syscall(&self) {
        self.restart.lock().clear_saved_syscall();
    }
}

#[cfg(test)]
mod tests {
    use axhal::uspace::UserContext;
    use memory_addr::VirtAddr;

    use super::*;

    fn make_uctx(arg0: usize, sysno: usize, ip: usize) -> UserContext {
        let mut uctx = UserContext::new(ip, VirtAddr::from_usize(0x8000), arg0);
        uctx.set_sysno(sysno);
        uctx.set_arg1(0x22);
        uctx.set_arg2(0x33);
        uctx.set_arg3(0x44);
        uctx.set_arg4(0x55);
        uctx.set_arg5(0x66);
        uctx
    }

    #[test]
    fn saved_syscall_round_trips_registers() {
        let mut uctx = make_uctx(0x11, 0x77, 0x1000);
        let saved = SavedSyscall::capture(&uctx);

        uctx.set_sysno(0x99);
        uctx.set_arg0(0xaa);
        uctx.set_arg1(0xbb);
        uctx.set_arg2(0xcc);
        uctx.set_arg3(0xdd);
        uctx.set_arg4(0xee);
        uctx.set_arg5(0xff);
        uctx.set_ip(0x2000);

        saved.restore(&mut uctx);

        assert_eq!(uctx.sysno(), 0x77);
        assert_eq!(uctx.arg0(), 0x11);
        assert_eq!(uctx.arg1(), 0x22);
        assert_eq!(uctx.arg2(), 0x33);
        assert_eq!(uctx.arg3(), 0x44);
        assert_eq!(uctx.arg4(), 0x55);
        assert_eq!(uctx.arg5(), 0x66);
        assert_eq!(uctx.ip(), 0x1000 - SYSCALL_INSN_LEN);
    }

    #[test]
    fn handler_syscall_preserves_outer_restart_state() {
        let mut tracker = RestartTracker::default();
        let outer = make_uctx(0x11, 0x3d, 0x1000);

        tracker.enter_syscall(&outer, false);
        tracker.request_syscall_restart(RestartClass::Sys);
        tracker.finish_signal_delivery(SignalOSAction::Handler, true);
        assert!(tracker.in_signal_handler());
        assert_eq!(tracker.restart_states.len(), 1);

        let handler_syscall = make_uctx(0x20, 0x25, 0x4000);
        tracker.enter_syscall(&handler_syscall, false);
        assert_eq!(tracker.restart_states.len(), 1);

        let mut restored = outer;
        tracker.complete_sigreturn(&mut restored);

        assert_eq!(restored.sysno(), 0x3d);
        assert_eq!(restored.arg0(), 0x11);
        assert_eq!(restored.arg1(), 0x22);
        assert_eq!(restored.ip(), 0x1000 - SYSCALL_INSN_LEN);
        assert!(tracker.take_resume_restored_context());
        assert!(tracker.restart_states.is_empty());
    }

    #[test]
    fn non_restart_handler_leaves_eintr_result_in_place() {
        let mut tracker = RestartTracker::default();
        let outer = make_uctx(0x11, 0x3d, 0x1000);

        tracker.enter_syscall(&outer, false);
        tracker.request_syscall_restart(RestartClass::Sys);
        tracker.finish_signal_delivery(SignalOSAction::Handler, false);

        let mut restored = outer;
        restored.set_retval((-4isize) as usize);
        tracker.complete_sigreturn(&mut restored);

        assert_eq!(restored.ip(), 0x1000);
        assert_eq!(restored.retval(), (-4isize) as usize);
        assert!(!tracker.take_resume_restored_context());
        assert!(tracker.restart_states.is_empty());
    }

    #[test]
    fn non_handler_restart_restores_syscall_before_return_to_user() {
        let mut tracker = RestartTracker::default();
        let outer = make_uctx(0x11, 0x3d, 0x1000);

        tracker.enter_syscall(&outer, false);
        tracker.request_syscall_restart(RestartClass::Sys);
        tracker.finish_signal_delivery(SignalOSAction::Continue, false);

        let mut resumed = outer;
        tracker.finish_signal_resume(&mut resumed);

        assert_eq!(resumed.sysno(), 0x3d);
        assert_eq!(resumed.arg0(), 0x11);
        assert_eq!(resumed.arg1(), 0x22);
        assert_eq!(resumed.ip(), 0x1000 - SYSCALL_INSN_LEN);
        assert!(tracker.restart_states.is_empty());
    }
}
