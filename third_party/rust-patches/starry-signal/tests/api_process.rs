use std::sync::Arc;

use kspin::SpinNoIrq;
use starry_signal::{
    PreparedSignal, SignalAction, SignalActionFlags, SignalDisposition, SignalInfo,
    SignalQueueAccount, SignalSet, Signo,
    api::{ProcessSignalManager, SignalActions, ThreadRegistrationError, ThreadSignalManager},
};

struct TestEnv {
    proc: Arc<ProcessSignalManager>,
}

impl TestEnv {
    fn new() -> Self {
        let actions = Arc::new(SpinNoIrq::new(SignalActions::default()));
        let proc = Arc::new(ProcessSignalManager::new(actions, 0));
        TestEnv { proc }
    }
}

#[test]
fn send_wakes_sets_pending() {
    let env = TestEnv::new();
    let thr = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    thr.try_register(9).unwrap().commit().unwrap();
    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 100);

    assert_eq!(env.proc.send_unqueued_signal(sig.clone()), Some(9));
    assert!(env.proc.pending().has(Signo::SIGTERM));
}

#[test]
fn rolled_back_registration_is_never_selected_for_wakeup() {
    let env = TestEnv::new();
    let rolled_back = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    drop(rolled_back.try_register(8).unwrap());

    let live = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    live.try_register(9).unwrap().commit().unwrap();

    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 100);
    assert_eq!(env.proc.send_unqueued_signal(sig), Some(9));
}

#[test]
fn registration_identity_is_unique_and_failed_duplicate_keeps_endpoint_active() {
    let env = TestEnv::new();
    let thread = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    thread.try_register(41).unwrap().commit().unwrap();

    assert!(matches!(
        thread.try_register(42),
        Err(ThreadRegistrationError::AlreadyRegistered)
    ));
    assert!(thread.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 100,)));
    thread.flush_pending();

    let replacement = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    assert!(matches!(
        replacement.try_register(41),
        Err(ThreadRegistrationError::TidInUse)
    ));
}

#[test]
fn cancelled_admission_cannot_commit_and_its_endpoint_can_retry() {
    let env = TestEnv::new();
    let thread = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    let admission = thread.try_register(51).unwrap();

    thread.retire_registration(51, false);
    assert!(matches!(
        admission.commit(),
        Err(ThreadRegistrationError::Cancelled)
    ));
    assert!(!thread.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 100,)));

    thread.try_register(51).unwrap().commit().unwrap();
    assert!(thread.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 100,)));
}

#[test]
fn retained_exited_leader_is_not_routable_but_action_updates_flush_it() {
    let env = TestEnv::new();
    let exited_leader = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    exited_leader.try_register(9).unwrap().commit().unwrap();
    assert!(exited_leader.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 100,)));
    assert!(exited_leader.pending().has(Signo::SIGTERM));
    exited_leader.retire_registration(9, true);

    let live = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    live.try_register(10).unwrap().commit().unwrap();
    assert_eq!(
        env.proc
            .send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 100)),
        Some(10)
    );

    env.proc
        .try_replace_action(
            Signo::SIGTERM,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..SignalAction::default()
            },
        )
        .unwrap();
    assert!(!exited_leader.pending().has(Signo::SIGTERM));
}

#[test]
fn exact_registration_tid_controls_retirement_and_retained_publication() {
    let env = TestEnv::new();
    let leader = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    leader.try_register(9).unwrap().commit().unwrap();

    leader.retire_registration(8, true);
    let mut retained_prepared = false;
    let retained_while_active = leader
        .try_send_retained_signal_with(SignalInfo::new_user(Signo::SIGTERM, 0, 100), |info| {
            retained_prepared = true;
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(!retained_prepared);
    assert!(!retained_while_active.published);
    assert!(leader.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 100)));
    leader.flush_pending();

    leader.retire_registration(9, true);
    let normal = leader
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGTERM, 0, 100), |info| {
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(!normal.published);
    assert!(!normal.wake);

    let user = SignalQueueAccount::try_new(1).unwrap();
    let global = SignalQueueAccount::try_new(1).unwrap();
    let retained = leader
        .try_send_retained_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 0, 100), |info| {
            PreparedSignal::try_accounted(info, &user, 1, &global)
        })
        .unwrap();
    assert!(retained.published);
    assert_eq!(user.queued(), 1);

    leader.retire_registration(9, false);
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn process_retirement_drains_once_and_rejects_late_publication() {
    let env = TestEnv::new();
    let thread = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    thread.try_register(9).unwrap().commit().unwrap();
    let user = SignalQueueAccount::try_new(1).unwrap();
    let global = SignalQueueAccount::try_new(1).unwrap();

    let published = env
        .proc
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 0, 100), |info| {
            PreparedSignal::try_accounted(info, &user, 1, &global)
        })
        .unwrap();
    assert!(published.published);
    assert_eq!((user.queued(), global.queued()), (1, 1));

    env.proc.retain_pending_only();
    env.proc.retire_pending();
    env.proc.retire_pending();
    assert_eq!((user.queued(), global.queued()), (0, 0));

    let mut prepared = false;
    let late = env
        .proc
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 0, 100), |info| {
            prepared = true;
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(!prepared);
    assert!(!late.published);
    assert_eq!(late.wake_tid, None);
}

#[test]
fn job_control_generation_effect_covers_shared_live_and_retained_queues() {
    let env = TestEnv::new();
    let retained = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    retained.try_register(9).unwrap().commit().unwrap();
    let live = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    live.try_register(10).unwrap().commit().unwrap();

    let _ = env
        .proc
        .send_unqueued_signal(SignalInfo::new_user(Signo::SIGSTOP, 0, 100));
    let _ = live.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTSTP, 0, 100));
    let _ = retained.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTTIN, 0, 100));
    retained.retire_registration(9, true);

    env.proc.actions.lock()[Signo::SIGCONT].disposition = SignalDisposition::Ignore;
    let _ = env
        .proc
        .send_unqueued_signal(SignalInfo::new_user(Signo::SIGCONT, 0, 100));
    for signo in [
        Signo::SIGSTOP,
        Signo::SIGTSTP,
        Signo::SIGTTIN,
        Signo::SIGTTOU,
    ] {
        assert!(!env.proc.pending().has(signo));
        assert!(!live.pending().has(signo));
        assert!(!retained.pending().has(signo));
    }

    env.proc.actions.lock()[Signo::SIGCONT] = SignalAction::default();
    let _ = live.send_unqueued_signal(SignalInfo::new_user(Signo::SIGCONT, 0, 100));
    retained
        .try_send_retained_signal_with(SignalInfo::new_user(Signo::SIGCONT, 0, 100), |info| {
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    let _ = live.send_unqueued_signal(SignalInfo::new_user(Signo::SIGSTOP, 0, 100));
    assert!(!env.proc.pending().has(Signo::SIGCONT));
    assert!(!live.pending().has(Signo::SIGCONT));
    assert!(!retained.pending().has(Signo::SIGCONT));
}

#[test]
fn retained_endpoint_block_mask_does_not_keep_default_ignored_signal_pending() {
    let env = TestEnv::new();
    let retained = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    retained.try_register(9).unwrap().commit().unwrap();
    let mut blocked = SignalSet::default();
    blocked.add(Signo::SIGCHLD);
    retained.set_blocked(blocked);
    retained.retire_registration(9, true);

    assert_eq!(
        env.proc
            .send_unqueued_signal(SignalInfo::new_user(Signo::SIGCHLD, 0, 100)),
        None
    );
    assert!(!env.proc.pending().has(Signo::SIGCHLD));
    assert!(!retained.pending().has(Signo::SIGCHLD));
}

#[test]
fn signal_ignore() {
    let env = TestEnv::new();
    env.proc.actions.lock()[Signo::SIGTERM].disposition = SignalDisposition::Ignore;
    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 100);

    assert_eq!(env.proc.send_unqueued_signal(sig), None);
    assert!(!env.proc.pending().has(Signo::SIGTERM));
}

#[test]
fn can_restart() {
    let env = TestEnv::new();
    assert!(!env.proc.can_restart(Signo::SIGTERM));

    env.proc.actions.lock()[Signo::SIGTERM]
        .flags
        .insert(SignalActionFlags::RESTART);
    assert!(env.proc.can_restart(Signo::SIGTERM));
}
