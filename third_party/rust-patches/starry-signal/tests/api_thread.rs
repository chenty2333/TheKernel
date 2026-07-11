use axcpu::uspace::UserContext;
use linux_raw_sys::general::{SS_DISABLE, SS_ONSTACK};
use starry_signal::{
    SignalActionFlags, SignalDisposition, SignalInfo, SignalOSAction, SignalSet, SignalStack,
    Signo, api::SignalFrame, arch::SignalContextError,
};

mod common;
use common::*;

fn copy_signal_frame(uctx: &UserContext) -> SignalFrame {
    SignalFrame::read_from_user(uctx.sp() as *const SignalFrame)
        .expect("signal frame must be readable from the test VM")
}

#[test]
fn dequeue_signal() {
    let (proc, thr) = new_test_env();

    let sig1 = SignalInfo::new_user(Signo::SIGINT, 9, 9);
    assert!(thr.send_unqueued_signal(sig1));

    let sig2 = SignalInfo::new_user(Signo::SIGTERM, 9, 9);
    assert_eq!(proc.send_unqueued_signal(sig2), Some(TID));

    let mask = !SignalSet::default();
    assert_eq!(thr.dequeue_signal(&mask).unwrap().signo(), Signo::SIGINT);
    assert_eq!(thr.dequeue_signal(&mask).unwrap().signo(), Signo::SIGTERM);
    assert!(thr.dequeue_signal(&mask).is_none());
}

#[test]
fn handle_signal() {
    let (proc, thr) = new_test_env();

    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 9, 9);

    unsafe extern "C" fn test_handler(_: i32) {}
    proc.actions.lock()[signo].disposition = SignalDisposition::Handler(test_handler as usize);

    let initial = UserContext::new(0, initial_sp().into(), 0);

    let mut uctx = initial;
    let restore_blocked = thr.blocked();
    let action = proc.actions.lock()[signo].clone();
    let result = thr.handle_signal(&mut uctx, restore_blocked, &sig, &action);

    assert_eq!(result, Some(SignalOSAction::Handler));
    assert_eq!(uctx.ip(), test_handler as *const () as usize);
    assert!(uctx.sp() < initial.sp());
    assert_eq!(uctx.arg0(), signo as usize);
}

#[test]
fn alternate_stack_status_and_bounds_are_overflow_safe() {
    let disabled = SignalStack::default();
    assert_eq!(disabled.flags_at(0x2000), SS_DISABLE);
    assert!(!disabled.contains_sp(0x2000));

    let stack = SignalStack {
        sp: 0x1000,
        flags: 0,
        size: 0x1000,
    };
    assert_eq!(stack.checked_top(), Some(0x2000));
    assert!(!stack.contains_sp(0x1000));
    assert!(stack.contains_sp(0x1001));
    assert!(stack.contains_sp(0x2000));
    assert!(!stack.contains_sp(0x2001));
    assert_eq!(stack.flags_at(0x1800), SS_ONSTACK);
    assert_eq!(stack.flags_at(0x2001), 0);
    assert!(stack.contains_range(0x1001, 0xfff));
    assert!(stack.contains_range(0x1000, 0x1000));

    let overflowing = SignalStack {
        sp: usize::MAX - 8,
        flags: 0,
        size: 16,
    };
    assert_eq!(overflowing.checked_top(), None);
    assert!(!overflowing.contains_range(usize::MAX - 4, 8));
}

#[test]
fn nested_onstack_signal_uses_remaining_stack_instead_of_reusing_top() {
    let (proc, thr) = new_test_env();
    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 9, 9);

    unsafe extern "C" fn test_handler(_: i32) {}
    {
        let mut actions = proc.actions.lock();
        let action = &mut actions[signo];
        action.disposition = SignalDisposition::Handler(test_handler as usize);
        action.flags.insert(SignalActionFlags::ONSTACK);
    }

    let alt_top = initial_sp();
    let alt_size = 0x8000;
    let alt_stack = SignalStack {
        sp: alt_top - alt_size,
        flags: 0,
        size: alt_size,
    };
    thr.set_stack(alt_stack.clone());

    let mut uctx = UserContext::new(0, initial_sp().into(), 0);
    let action = proc.actions.lock()[signo].clone();
    assert_eq!(
        thr.handle_signal(&mut uctx, thr.blocked(), &sig, &action),
        Some(SignalOSAction::Handler)
    );
    let outer_sp = uctx.sp();
    assert!(alt_stack.contains_sp(outer_sp));

    assert_eq!(
        thr.handle_signal(&mut uctx, thr.blocked(), &sig, &action),
        Some(SignalOSAction::Handler)
    );
    assert!(uctx.sp() < outer_sp);
    assert!(alt_stack.contains_sp(uctx.sp()));
}

#[test]
fn overflowing_alternate_stack_fails_without_publishing_handler_context() {
    let (proc, thr) = new_test_env();
    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 9, 9);

    unsafe extern "C" fn test_handler(_: i32) {}
    {
        let mut actions = proc.actions.lock();
        let action = &mut actions[signo];
        action.disposition = SignalDisposition::Handler(test_handler as usize);
        action.flags.insert(SignalActionFlags::ONSTACK);
    }
    thr.set_stack(SignalStack {
        sp: usize::MAX - 8,
        flags: 0,
        size: 16,
    });

    let initial = UserContext::new(0x1234, initial_sp().into(), 0);
    let mut uctx = initial;
    let action = proc.actions.lock()[signo].clone();
    assert_eq!(
        thr.handle_signal(&mut uctx, thr.blocked(), &sig, &action),
        Some(SignalOSAction::CoreDump)
    );
    assert_eq!(uctx.ip(), initial.ip());
    assert_eq!(uctx.sp(), initial.sp());
}

#[test]
fn block_ignore_send_signal() {
    let (proc, thr) = new_test_env();

    let signo = Signo::SIGINT;
    let sig = SignalInfo::new_user(signo, 0, 1);
    assert!(thr.send_unqueued_signal(sig.clone()));
    assert_eq!(
        thr.dequeue_signal(&!SignalSet::default()).unwrap().signo(),
        sig.signo()
    );

    proc.actions.lock()[signo].disposition = SignalDisposition::Ignore;
    assert!(!thr.send_unqueued_signal(sig.clone()));
    assert!(!thr.pending().has(signo));

    let mut set = SignalSet::default();
    set.add(signo);
    thr.set_blocked(set);
    assert!(thr.signal_blocked(signo));
    assert!(!thr.send_unqueued_signal(sig.clone()));
    assert!(thr.pending().has(signo));

    proc.actions.lock()[signo].disposition = SignalDisposition::Default;
    assert!(!thr.send_unqueued_signal(sig.clone()));
    assert!(thr.pending().has(signo));

    let empty = SignalSet::default();
    thr.set_blocked(empty);
    assert!(!thr.signal_blocked(signo));
}

#[test]
fn check_signals() {
    let (proc, thr) = new_test_env();

    let mut uctx = UserContext::new(0, 0.into(), 0);

    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 0, 1);

    assert_eq!(proc.send_unqueued_signal(sig.clone()), Some(TID));
    let delivered = thr.check_signals(&mut uctx, None).unwrap();
    assert_eq!(delivered.info.signo(), signo);

    assert!(thr.send_unqueued_signal(sig.clone()));
    let delivered = thr.check_signals(&mut uctx, None).unwrap();
    assert_eq!(delivered.info.signo(), signo);
}

#[test]
fn check_signals_preserves_restartability_for_reset_hand() {
    let (proc, thr) = new_test_env();
    let mut uctx = UserContext::new(0, initial_sp().into(), 0);

    unsafe extern "C" fn test_handler(_: i32) {}

    let signo = Signo::SIGTERM;
    {
        let mut actions = proc.actions.lock();
        let action = &mut actions[signo];
        action.disposition = SignalDisposition::Handler(test_handler as usize);
        action
            .flags
            .insert(SignalActionFlags::RESTART | SignalActionFlags::RESETHAND);
    }

    assert_eq!(
        proc.send_unqueued_signal(SignalInfo::new_user(signo, 0, 1)),
        Some(TID)
    );
    let delivered = thr.check_signals(&mut uctx, None).unwrap();
    assert_eq!(delivered.os_action, SignalOSAction::Handler);
    assert!(delivered.restartable_handler);
    assert!(matches!(
        proc.actions.lock()[signo].disposition,
        SignalDisposition::Default
    ));
}

#[test]
fn restore() {
    let (proc, thr) = new_test_env();

    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 0, 1);

    unsafe extern "C" fn test_handler(_: i32) {}
    proc.actions.lock()[signo].disposition = SignalDisposition::Handler(test_handler as usize);

    let initial = UserContext::new(0x219, initial_sp().into(), 0);

    let mut uctx = initial;
    let restore_blocked = thr.blocked();
    let action = proc.actions.lock()[sig.signo()].clone();
    thr.handle_signal(&mut uctx, restore_blocked, &sig, &action);

    let new_sp = uctx.sp() + 8;
    uctx.set_sp(new_sp);
    let frame = copy_signal_frame(&uctx);
    let prepared = thr
        .prepare_restore(&uctx, frame, |_| true, |_| true)
        .unwrap();
    thr.commit_restore(&mut uctx, prepared);

    assert_eq!(uctx.ip(), initial.ip());
    assert_eq!(uctx.sp(), initial.sp());
}

#[test]
fn restore_rejects_bad_context_without_partial_commit() {
    let (proc, thr) = new_test_env();
    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 0, 1);

    unsafe extern "C" fn test_handler(_: i32) {}
    proc.actions.lock()[signo].disposition = SignalDisposition::Handler(test_handler as usize);

    let initial = UserContext::new(0x4000, initial_sp().into(), 0);
    let mut current = initial;
    let action = proc.actions.lock()[signo].clone();
    thr.handle_signal(&mut current, thr.blocked(), &sig, &action);
    let frame_sp = current.sp() + 8;
    current.set_sp(frame_sp);

    let frame = copy_signal_frame(&current);
    let handler_ip = current.ip();
    let handler_sp = current.sp();
    let blocked_before = thr.blocked();
    let result = thr.prepare_restore(&current, frame, |_| false, |_| true);

    assert!(matches!(
        result,
        Err(SignalContextError::InvalidProgramCounter)
    ));
    assert_eq!(current.ip(), handler_ip);
    assert_eq!(current.sp(), handler_sp);
    assert_eq!(
        format!("{:?}", thr.blocked()),
        format!("{blocked_before:?}")
    );
}

#[test]
fn restore_never_blocks_sigkill_or_sigstop() {
    let (proc, thr) = new_test_env();
    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 0, 1);

    unsafe extern "C" fn test_handler(_: i32) {}
    proc.actions.lock()[signo].disposition = SignalDisposition::Handler(test_handler as usize);

    let mut current = UserContext::new(0x4000, initial_sp().into(), 0);
    let action = proc.actions.lock()[signo].clone();
    thr.handle_signal(&mut current, thr.blocked(), &sig, &action);
    let frame_sp = current.sp() + 8;
    current.set_sp(frame_sp);

    let mut frame = copy_signal_frame(&current);
    frame.ucontext_mut().sigmask.add(Signo::SIGKILL);
    frame.ucontext_mut().sigmask.add(Signo::SIGSTOP);
    let prepared = thr
        .prepare_restore(&current, frame, |_| true, |_| true)
        .unwrap();
    thr.commit_restore(&mut current, prepared);

    assert!(!thr.blocked().has(Signo::SIGKILL));
    assert!(!thr.blocked().has(Signo::SIGSTOP));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn restore_sanitizes_x86_privileged_flags_and_rejects_bad_cs() {
    let (proc, thr) = new_test_env();
    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 0, 1);

    unsafe extern "C" fn test_handler(_: i32) {}
    proc.actions.lock()[signo].disposition = SignalDisposition::Handler(test_handler as usize);

    let mut current = UserContext::new(0x4000, initial_sp().into(), 0);
    let action = proc.actions.lock()[signo].clone();
    thr.handle_signal(&mut current, thr.blocked(), &sig, &action);
    let frame_sp = current.sp() + 8;
    current.set_sp(frame_sp);

    let mut frame = copy_signal_frame(&current);
    let trusted_flags = current.rflags;
    frame
        .ucontext_mut()
        .mcontext
        .set_processor_flags(trusted_flags as usize | (0b11 << 12));
    let prepared = thr
        .prepare_restore(&current, frame.clone(), |_| true, |_| true)
        .unwrap();
    assert_eq!(prepared.context().rflags & (0b11 << 12), 0);
    assert_eq!(
        prepared.context().rflags & (1 << 9),
        trusted_flags & (1 << 9)
    );

    frame.ucontext_mut().mcontext.set_code_segment(0);
    assert!(matches!(
        thr.prepare_restore(&current, frame, |_| true, |_| true),
        Err(SignalContextError::InvalidProcessorState)
    ));
}

#[test]
fn signal_frame_copy_rejects_unmapped_and_unaligned_addresses() {
    assert!(SignalFrame::read_from_user(std::ptr::dangling::<SignalFrame>()).is_err());

    let unaligned = initial_sp() - 1;
    assert!(SignalFrame::read_from_user(unaligned as *const SignalFrame).is_err());
}
