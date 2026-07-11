use alloc::{collections::TryReserveError, vec::Vec};
use core::time::Duration;

use axhal::uspace::UserContext;
use starry_signal::SignalOSAction;
use syscalls::Sysno;

use super::{AlarmClock, Thread};

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
pub(crate) struct FutexWaitRestart {
    pub uaddr: usize,
    pub expected: u32,
    pub bitset: u32,
    pub deadline: Duration,
    pub clock: AlarmClock,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum RestartBlock {
    FutexWait(FutexWaitRestart),
}

#[derive(Debug, Clone, Copy)]
enum RestartActionKind {
    Replay,
    RestartBlock(RestartBlock),
}

#[derive(Debug, Clone, Copy)]
struct RestartAction {
    syscall: SavedSyscall,
    kind: RestartActionKind,
}

#[derive(Debug, Clone, Copy)]
struct PendingRestart {
    class: RestartClass,
    action: RestartAction,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RestartDecision {
    Pending,
    Restart,
    ReturnEintr,
}

#[derive(Debug, Clone, Copy)]
struct RestartState {
    action: RestartAction,
    class: RestartClass,
    decision: RestartDecision,
}

/// Maximum number of interrupted syscalls retained across nested signal
/// handlers.
///
/// Reaching this limit does not reject signal delivery. The interrupted
/// syscall at the overflowing depth simply remains `EINTR`, which is always a
/// valid Linux-visible outcome for a signal interruption. Reserving the whole
/// bounded ledger when the thread is created keeps the later admission path
/// allocation-free while it holds the restart spin lock.
// Sixteen nested restartable handlers are already far beyond ordinary signal
// use. This bounds the per-thread reservation to a few KiB; it does not cap
// signal-handler nesting itself because deeper interrupted syscalls continue
// with `EINTR`.
const MAX_RESTART_DEPTH: usize = 16;

#[derive(Debug)]
pub(crate) struct RestartTracker {
    signal_handler_depth: usize,
    current_restart: Option<PendingRestart>,
    armed_restart_block: Option<RestartBlock>,
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

    fn restore_restart_syscall(self, uctx: &mut UserContext) {
        uctx.set_sysno(Sysno::restart_syscall as usize);
        uctx.set_arg0(0);
        uctx.set_arg1(0);
        uctx.set_arg2(0);
        uctx.set_arg3(0);
        uctx.set_arg4(0);
        uctx.set_arg5(0);
        uctx.set_ip(self.restart_ip);
    }

    fn matches_return_context(self, uctx: &UserContext) -> bool {
        uctx.ip() == self.return_ip
    }
}

impl RestartAction {
    fn matches_return_context(self, uctx: &UserContext) -> bool {
        self.syscall.matches_return_context(uctx)
    }

    fn restore(self, tracker: &mut RestartTracker, uctx: &mut UserContext) {
        match self.kind {
            RestartActionKind::Replay => self.syscall.restore(uctx),
            RestartActionKind::RestartBlock(block) => {
                tracker.armed_restart_block = Some(block);
                self.syscall.restore_restart_syscall(uctx);
            }
        }
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
    pub(crate) fn try_new() -> Result<Self, TryReserveError> {
        let mut restart_states = Vec::new();
        restart_states.try_reserve_exact(MAX_RESTART_DEPTH)?;
        Ok(Self {
            signal_handler_depth: 0,
            current_restart: None,
            armed_restart_block: None,
            restart_states,
            resume_restored_context: false,
        })
    }

    fn enter_syscall(
        &mut self,
        uctx: &UserContext,
        preserve_restart_state: bool,
        restart_class: Option<RestartClass>,
    ) {
        let syscall = SavedSyscall::capture(uctx);
        self.current_restart = restart_class.map(|class| PendingRestart {
            class,
            action: RestartAction {
                syscall,
                kind: RestartActionKind::Replay,
            },
        });
        if uctx.sysno() != Sysno::restart_syscall as usize {
            self.armed_restart_block = None;
        }
        if !preserve_restart_state && self.signal_handler_depth == 0 {
            self.restart_states.clear();
            self.resume_restored_context = false;
        }
    }

    fn in_signal_handler(&self) -> bool {
        self.signal_handler_depth > 0
    }

    fn signal_handler_depth(&self) -> usize {
        self.signal_handler_depth
    }

    fn install_restart_block(&mut self, block: RestartBlock) {
        let Some(restart) = self.current_restart.as_mut() else {
            return;
        };
        restart.action.kind = RestartActionKind::RestartBlock(block);
    }

    fn begin_restart_syscall(&mut self, uctx: &UserContext) -> Option<RestartBlock> {
        let block = self.armed_restart_block.take()?;
        self.current_restart = Some(PendingRestart {
            class: RestartClass::Sys,
            action: RestartAction {
                syscall: SavedSyscall::capture(uctx),
                kind: RestartActionKind::RestartBlock(block),
            },
        });
        Some(block)
    }

    /// Records a restart candidate without allocating.
    ///
    /// `false` means that no candidate was present or that the bounded ledger
    /// was full. In either case the syscall's existing `EINTR` result remains
    /// authoritative.
    fn request_syscall_restart(&mut self) -> bool {
        let Some(restart) = self.current_restart else {
            return false;
        };
        if self.restart_states.len() == MAX_RESTART_DEPTH {
            return false;
        }
        debug_assert!(self.restart_states.capacity() >= MAX_RESTART_DEPTH);
        self.restart_states.push(RestartState {
            action: restart.action,
            class: restart.class,
            decision: RestartDecision::Pending,
        });
        true
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
        if !state.action.matches_return_context(uctx) {
            return;
        }

        let state = self.restart_states.pop().unwrap();
        if state.decision == RestartDecision::Restart {
            state.action.restore(self, uctx);
        }
    }

    fn complete_sigreturn(&mut self, uctx: &mut UserContext) {
        if self.signal_handler_depth > 0 {
            self.signal_handler_depth -= 1;
        }

        let Some(state) = self.restart_states.last().copied() else {
            return;
        };
        if !state.action.matches_return_context(uctx) {
            return;
        }

        let state = self.restart_states.pop().unwrap();
        if state.decision == RestartDecision::Restart {
            state.action.restore(self, uctx);
            self.resume_restored_context = true;
        }
    }

    fn take_resume_restored_context(&mut self) -> bool {
        core::mem::take(&mut self.resume_restored_context)
    }

    fn clear_saved_syscall(&mut self) {
        self.current_restart = None;
    }
}

impl Thread {
    pub(crate) fn enter_syscall(
        &self,
        uctx: &UserContext,
        preserve_restart_state: bool,
        restart_class: Option<RestartClass>,
    ) {
        self.restart
            .lock()
            .enter_syscall(uctx, preserve_restart_state, restart_class);
    }

    pub(crate) fn in_signal_handler(&self) -> bool {
        self.restart.lock().in_signal_handler()
    }

    pub(crate) fn signal_handler_depth(&self) -> usize {
        self.restart.lock().signal_handler_depth()
    }

    pub(crate) fn request_syscall_restart(&self) -> bool {
        self.restart.lock().request_syscall_restart()
    }

    pub(crate) fn install_restart_block(&self, block: RestartBlock) {
        self.restart.lock().install_restart_block(block);
    }

    pub(crate) fn begin_restart_syscall(&self, uctx: &UserContext) -> Option<RestartBlock> {
        self.restart.lock().begin_restart_syscall(uctx)
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
    use syscalls::Sysno;

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
    fn sigreturn_without_active_handler_does_not_panic_or_mutate_state() {
        let mut tracker = RestartTracker::try_new().unwrap();
        let mut context = make_uctx(0x11, 0x77, 0x1000);

        tracker.complete_sigreturn(&mut context);

        assert_eq!(tracker.signal_handler_depth(), 0);
        assert!(tracker.restart_states.is_empty());
        assert_eq!(context.ip(), 0x1000);
        assert_eq!(context.sysno(), 0x77);
        assert_eq!(context.arg0(), 0x11);
    }

    #[test]
    fn handler_syscall_preserves_outer_restart_state() {
        let mut tracker = RestartTracker::try_new().unwrap();
        let outer = make_uctx(0x11, 0x3d, 0x1000);

        tracker.enter_syscall(&outer, false, Some(RestartClass::Sys));
        tracker.request_syscall_restart();
        tracker.finish_signal_delivery(SignalOSAction::Handler, true);
        assert!(tracker.in_signal_handler());
        assert_eq!(tracker.restart_states.len(), 1);

        let handler_syscall = make_uctx(0x20, 0x25, 0x4000);
        tracker.enter_syscall(&handler_syscall, false, Some(RestartClass::Sys));
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
        let mut tracker = RestartTracker::try_new().unwrap();
        let outer = make_uctx(0x11, 0x3d, 0x1000);

        tracker.enter_syscall(&outer, false, Some(RestartClass::Sys));
        tracker.request_syscall_restart();
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
        let mut tracker = RestartTracker::try_new().unwrap();
        let outer = make_uctx(0x11, 0x3d, 0x1000);

        tracker.enter_syscall(&outer, false, Some(RestartClass::Sys));
        tracker.request_syscall_restart();
        tracker.finish_signal_delivery(SignalOSAction::Continue, false);

        let mut resumed = outer;
        tracker.finish_signal_resume(&mut resumed);

        assert_eq!(resumed.sysno(), 0x3d);
        assert_eq!(resumed.arg0(), 0x11);
        assert_eq!(resumed.arg1(), 0x22);
        assert_eq!(resumed.ip(), 0x1000 - SYSCALL_INSN_LEN);
        assert!(tracker.restart_states.is_empty());
    }

    #[test]
    fn restart_block_sigreturn_restores_restart_syscall_and_arms_block() {
        let mut tracker = RestartTracker::try_new().unwrap();
        let outer = make_uctx(0x11, Sysno::futex as usize, 0x1000);
        let block = RestartBlock::FutexWait(FutexWaitRestart {
            uaddr: 0x1234,
            expected: 7,
            bitset: u32::MAX,
            deadline: Duration::from_millis(200),
            clock: AlarmClock::Monotonic,
        });

        tracker.enter_syscall(&outer, false, Some(RestartClass::Sys));
        tracker.install_restart_block(block);
        tracker.request_syscall_restart();
        tracker.finish_signal_delivery(SignalOSAction::Handler, true);

        let mut restored = outer;
        tracker.complete_sigreturn(&mut restored);

        assert_eq!(restored.sysno(), Sysno::restart_syscall as usize);
        assert_eq!(restored.ip(), 0x1000 - SYSCALL_INSN_LEN);
        assert_eq!(tracker.armed_restart_block, Some(block));
        assert!(tracker.take_resume_restored_context());
    }

    #[test]
    fn begin_restart_syscall_rearms_restart_block_after_another_interrupt() {
        let mut tracker = RestartTracker::try_new().unwrap();
        let block = RestartBlock::FutexWait(FutexWaitRestart {
            uaddr: 0x1234,
            expected: 7,
            bitset: u32::MAX,
            deadline: Duration::from_millis(200),
            clock: AlarmClock::Monotonic,
        });
        tracker.armed_restart_block = Some(block);

        let restart = make_uctx(0, Sysno::restart_syscall as usize, 0x2000);
        assert_eq!(tracker.begin_restart_syscall(&restart), Some(block));
        tracker.request_syscall_restart();
        tracker.finish_signal_delivery(SignalOSAction::Continue, false);

        let mut resumed = restart;
        tracker.finish_signal_resume(&mut resumed);

        assert_eq!(resumed.sysno(), Sysno::restart_syscall as usize);
        assert_eq!(resumed.ip(), 0x2000 - SYSCALL_INSN_LEN);
        assert_eq!(tracker.armed_restart_block, Some(block));
    }

    #[test]
    fn nested_restart_ledger_is_bounded_and_overflow_stays_eintr() {
        let mut tracker = RestartTracker::try_new().unwrap();
        let reserved_capacity = tracker.restart_states.capacity();

        for depth in 0..MAX_RESTART_DEPTH {
            let interrupted = make_uctx(depth, Sysno::read as usize, 0x2000 + depth * 8);
            tracker.enter_syscall(&interrupted, true, Some(RestartClass::Sys));
            assert!(tracker.request_syscall_restart());
            tracker.finish_signal_delivery(SignalOSAction::Handler, true);
        }

        assert_eq!(tracker.restart_states.len(), MAX_RESTART_DEPTH);
        assert_eq!(tracker.restart_states.capacity(), reserved_capacity);

        let overflow = make_uctx(0xfeed, Sysno::read as usize, 0x8000);
        tracker.enter_syscall(&overflow, true, Some(RestartClass::Sys));
        assert!(!tracker.request_syscall_restart());
        tracker.finish_signal_delivery(SignalOSAction::Handler, true);

        let mut returned = overflow;
        returned.set_retval((-4isize) as usize);
        tracker.complete_sigreturn(&mut returned);

        assert_eq!(returned.ip(), overflow.ip());
        assert_eq!(returned.retval(), (-4isize) as usize);
        assert_eq!(tracker.restart_states.len(), MAX_RESTART_DEPTH);
        assert_eq!(tracker.restart_states.capacity(), reserved_capacity);
    }
}
