use std::sync::{Arc, Barrier};

use thekernel_linux_signal::{
    PreparedSignal, SignalAction, SignalActionFlags, SignalDisposition, SignalInfo,
    SignalQueueAccount, SignalSet, Signo,
    api::{ProcessSignalManager, SharedSignalActions, SignalActions, ThreadSignalManager},
};

struct TestEnv {
    proc: Arc<ProcessSignalManager>,
}

impl TestEnv {
    fn new() -> Self {
        let actions = SharedSignalActions::try_new(SignalActions::default()).unwrap();
        let proc = Arc::new(ProcessSignalManager::new(actions, 0));
        TestEnv { proc }
    }
}

#[test]
fn shared_sighand_identity_is_visible_and_snapshot_is_independent() {
    let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let first = ProcessSignalManager::new(shared.clone(), 0);
    let second = ProcessSignalManager::new(shared.clone(), 0);
    assert!(SharedSignalActions::ptr_eq(
        first.shared_actions(),
        second.shared_actions()
    ));

    let handler = SignalAction {
        disposition: SignalDisposition::Handler(0x1234),
        ..SignalAction::default()
    };
    first.try_replace_action(Signo::SIGUSR1, handler).unwrap();
    assert!(matches!(
        second.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Handler(0x1234)
    ));

    let snapshot = shared.try_snapshot().unwrap();
    assert!(!SharedSignalActions::ptr_eq(&shared, &snapshot));
    let independent = ProcessSignalManager::new(snapshot, 0);
    first
        .try_replace_action(Signo::SIGUSR1, SignalAction::default())
        .unwrap();
    assert!(matches!(
        second.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Default
    ));
    assert!(matches!(
        independent.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Handler(0x1234)
    ));
}

#[test]
fn exec_unshare_preserves_manager_state_and_isolates_peer_sighand() {
    let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let execing = Arc::new(ProcessSignalManager::new(shared.clone(), 0));
    let peer = Arc::new(ProcessSignalManager::new(shared.clone(), 0));
    let thread = ThreadSignalManager::try_new(execing.clone()).unwrap();
    thread.try_register(123).unwrap().commit().unwrap();

    let caught = SignalAction {
        disposition: SignalDisposition::Handler(0x1234),
        ..SignalAction::default()
    };
    let ignored = SignalAction {
        disposition: SignalDisposition::Ignore,
        ..SignalAction::default()
    };
    execing.try_replace_action(Signo::SIGUSR1, caught).unwrap();
    execing.try_replace_action(Signo::SIGUSR2, ignored).unwrap();
    assert_eq!(
        execing.send_unqueued_signal(SignalInfo::new_user(Signo::SIGUSR1, 0, 100, 1000,)),
        Some(123)
    );

    execing.try_prepare_exec_unshare().unwrap().commit();

    assert!(!SharedSignalActions::ptr_eq(
        execing.shared_actions(),
        peer.shared_actions()
    ));
    assert!(matches!(
        execing.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Default
    ));
    assert!(matches!(
        execing.action(Signo::SIGUSR2).disposition,
        SignalDisposition::Ignore
    ));
    assert!(execing.pending().has(Signo::SIGUSR1));
    assert!(thread.is_registered());
    assert!(matches!(
        peer.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Handler(0x1234)
    ));
    assert!(matches!(
        peer.action(Signo::SIGUSR2).disposition,
        SignalDisposition::Ignore
    ));
}

#[test]
fn exec_unshare_linearizes_with_peer_action_updates() {
    let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let execing = Arc::new(ProcessSignalManager::new(shared.clone(), 0));
    let peer = Arc::new(ProcessSignalManager::new(shared, 0));
    let start = Arc::new(Barrier::new(2));
    let updater = {
        let peer = peer.clone();
        let start = start.clone();
        std::thread::spawn(move || {
            start.wait();
            for handler in 1..=128 {
                peer.try_replace_action(
                    Signo::SIGUSR1,
                    SignalAction {
                        disposition: SignalDisposition::Handler(handler),
                        ..SignalAction::default()
                    },
                )
                .unwrap();
            }
        })
    };
    start.wait();
    execing.try_prepare_exec_unshare().unwrap().commit();
    updater.join().unwrap();

    assert!(!SharedSignalActions::ptr_eq(
        execing.shared_actions(),
        peer.shared_actions()
    ));
    assert!(matches!(
        peer.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Handler(128)
    ));
    assert!(matches!(
        execing.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Default
    ));
}

#[test]
fn prepared_exec_unshare_refreshes_actions_changed_before_commit() {
    let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let execing = ProcessSignalManager::new(shared.clone(), 0);
    let peer = ProcessSignalManager::new(shared, 0);
    peer.try_replace_action(
        Signo::SIGUSR1,
        SignalAction {
            disposition: SignalDisposition::Ignore,
            ..SignalAction::default()
        },
    )
    .unwrap();

    let prepared = execing.try_prepare_exec_unshare().unwrap();
    peer.try_replace_action(
        Signo::SIGUSR1,
        SignalAction {
            disposition: SignalDisposition::Handler(0x4321),
            ..SignalAction::default()
        },
    )
    .unwrap();
    prepared.commit();

    assert!(matches!(
        peer.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Handler(0x4321)
    ));
    assert!(matches!(
        execing.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Default
    ));
}

#[test]
fn dropped_prepared_exec_unshare_leaves_old_owner_for_retry() {
    let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let manager = ProcessSignalManager::new(shared.clone(), 0);
    manager
        .try_replace_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..SignalAction::default()
            },
        )
        .unwrap();

    let prepared = manager.try_prepare_exec_unshare().unwrap();
    drop(prepared);
    assert!(SharedSignalActions::ptr_eq(
        manager.shared_actions(),
        &shared
    ));
    assert!(matches!(
        manager.action(Signo::SIGUSR1).disposition,
        SignalDisposition::Ignore
    ));

    manager.try_prepare_exec_unshare().unwrap().commit();
    assert!(!SharedSignalActions::ptr_eq(
        manager.shared_actions(),
        &shared
    ));
}

#[test]
fn send_wakes_sets_pending() {
    let env = TestEnv::new();
    let thr = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    thr.try_register(9).unwrap().commit().unwrap();
    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 100, 0);

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

    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 100, 0);
    assert_eq!(env.proc.send_unqueued_signal(sig), Some(9));
}

#[test]
fn signal_ignore() {
    let env = TestEnv::new();
    env.proc
        .try_replace_action(
            Signo::SIGTERM,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..SignalAction::default()
            },
        )
        .unwrap();
    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 100, 0);

    assert_eq!(env.proc.send_unqueued_signal(sig), None);
    assert!(!env.proc.pending().has(Signo::SIGTERM));
}

#[test]
fn prepared_send_defers_unused_queue_ownership_until_finish() {
    let env = TestEnv::new();
    let thread = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    thread.try_register(9).unwrap().commit().unwrap();
    env.proc
        .try_replace_action(
            Signo::SIGRTMIN,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..SignalAction::default()
            },
        )
        .unwrap();

    let route = env.proc.try_prepare_signal_send().unwrap();
    let per_user = SignalQueueAccount::try_new(4).unwrap();
    let global = SignalQueueAccount::try_new(4).unwrap();
    let prepared = PreparedSignal::try_accounted(
        SignalInfo::new_user(Signo::SIGRTMIN, -1, 100, 0),
        &per_user,
        4,
        &global,
    )
    .unwrap();

    let deferred = route.publish(prepared);
    assert!(!deferred.outcome().published);
    assert_eq!(deferred.outcome().wake_tid, None);
    assert_eq!(per_user.queued(), 1);
    assert_eq!(global.queued(), 1);

    let (outcome, unused) = deferred.finish();
    assert!(!outcome.published);
    assert!(unused.is_some());
    assert_eq!(per_user.queued(), 1);
    drop(unused);
    assert_eq!(per_user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn prepared_route_rejects_cancelled_registration_identity_after_tid_reuse() {
    let env = TestEnv::new();
    let thread = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    thread.try_register(41).unwrap().commit().unwrap();
    let stale_route = env.proc.try_prepare_signal_send().unwrap();

    assert!(thread.cancel_registration());
    thread.try_register(42).unwrap().commit().unwrap();
    let stale = stale_route.publish(PreparedSignal::unqueued(SignalInfo::new_user(
        Signo::SIGTERM,
        1,
        100,
        0,
    )));
    assert!(stale.outcome().published);
    assert_eq!(stale.outcome().wake_tid, None);
    assert!(stale.finish().1.is_none());

    let current_route = env.proc.try_prepare_signal_send().unwrap();
    let current = current_route.publish(PreparedSignal::unqueued(SignalInfo::new_user(
        Signo::SIGTERM,
        2,
        100,
        0,
    )));
    assert!(!current.outcome().published);
    assert_eq!(current.outcome().wake_tid, Some(42));
    let (_, unused) = current.finish();
    assert!(unused.is_some());
    drop(unused);
    env.proc.flush_pending();
}

#[test]
fn can_restart() {
    let env = TestEnv::new();
    assert!(!env.proc.can_restart(Signo::SIGTERM));

    let mut action = SignalAction::default();
    action.flags.insert(SignalActionFlags::RESTART);
    env.proc.try_replace_action(Signo::SIGTERM, action).unwrap();
    assert!(env.proc.can_restart(Signo::SIGTERM));
}

#[test]
fn registration_identity_is_explicit_unique_and_reusable_after_cancel() {
    use thekernel_linux_signal::api::ThreadRegistrationError;

    let env = TestEnv::new();
    let first = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    first.try_register(41).unwrap().commit().unwrap();
    assert!(first.is_registered());
    assert!(matches!(
        first.try_register(42),
        Err(ThreadRegistrationError::AlreadyRegistered)
    ));

    let replacement = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    assert!(matches!(
        replacement.try_register(41),
        Err(ThreadRegistrationError::TidInUse)
    ));

    assert!(first.cancel_registration());
    assert!(!first.is_registered());
    assert!(!first.cancel_registration());
    replacement.try_register(41).unwrap().commit().unwrap();
    assert!(replacement.is_registered());
}

#[test]
fn thread_registry_limit_is_finite_refunded_and_configurable() {
    use thekernel_linux_signal::api::{SignalManagerConfigError, ThreadRegistrationError};

    assert!(matches!(
        ProcessSignalManager::try_with_thread_limit(
            SharedSignalActions::try_new(SignalActions::default()).unwrap(),
            0,
            usize::MAX,
        ),
        Err(SignalManagerConfigError::UnboundedThreadRegistry)
    ));
    let process = Arc::new(
        ProcessSignalManager::try_with_thread_limit(
            SharedSignalActions::try_new(SignalActions::default()).unwrap(),
            0,
            1,
        )
        .unwrap(),
    );
    assert_eq!(process.thread_limit(), 1);
    let first = ThreadSignalManager::try_new(process.clone()).unwrap();
    first.try_register(1).unwrap().commit().unwrap();
    let second = ThreadSignalManager::try_new(process.clone()).unwrap();
    assert!(matches!(
        second.try_register(2),
        Err(ThreadRegistrationError::Capacity)
    ));

    assert!(first.cancel_registration());
    second.try_register(2).unwrap().commit().unwrap();
}

#[test]
fn cancelled_admission_token_cannot_resurrect_a_stale_endpoint() {
    use thekernel_linux_signal::api::ThreadRegistrationError;

    let env = TestEnv::new();
    let thread = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    let admission = thread.try_register(51).unwrap();
    assert!(thread.cancel_registration());
    assert!(matches!(
        admission.commit(),
        Err(ThreadRegistrationError::Cancelled)
    ));
    assert!(!thread.is_registered());

    assert_eq!(
        env.proc
            .send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 100, 0)),
        None
    );
    thread.try_register(51).unwrap().commit().unwrap();
    assert!(thread.is_registered());
}

#[test]
fn retained_endpoint_is_not_a_process_wakeup_target_but_accepts_exact_send() {
    let env = TestEnv::new();
    let retained = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    retained.try_register(71).unwrap().commit().unwrap();
    retained
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGTERM, 1, 1, 0), |info| {
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    retained.retire_registration(71, true);

    assert!(!retained.is_registered());
    assert_eq!(
        env.proc
            .send_unqueued_signal(SignalInfo::new_user(Signo::SIGUSR1, 2, 2, 0)),
        None,
        "retained endpoints do not participate in process wake routing"
    );

    let exact = retained
        .try_send_retained_signal_with(SignalInfo::new_user(Signo::SIGUSR2, 3, 3, 0), |info| {
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(exact.published);
    assert!(!exact.wake);
    assert!(retained.pending().has(Signo::SIGUSR2));
}

#[test]
fn process_retention_and_reap_reject_late_publications_without_leaking_charge() {
    let env = TestEnv::new();
    let route = env.proc.try_prepare_signal_send().unwrap();
    let per_user = SignalQueueAccount::try_new(2).unwrap();
    let global = SignalQueueAccount::try_new(2).unwrap();
    let prepared = PreparedSignal::try_accounted(
        SignalInfo::new_user(Signo::SIGRTMIN, 1, 1, 0),
        &per_user,
        2,
        &global,
    )
    .unwrap();

    env.proc.retain_pending_only();
    let deferred = route.publish(prepared);
    assert!(!deferred.outcome().published);
    let (_, unused) = deferred.finish();
    assert!(unused.is_some());
    drop(unused);
    assert_eq!(per_user.queued(), 0);
    assert_eq!(global.queued(), 0);

    env.proc.retire_pending();
    env.proc.retire_pending();
    let called = std::cell::Cell::new(false);
    let outcome = env
        .proc
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 2, 2, 0), |info| {
            called.set(true);
            PreparedSignal::try_accounted(info, &per_user, 2, &global)
        })
        .unwrap();
    assert!(!outcome.published);
    assert!(!called.get());
    assert_eq!(per_user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn job_control_generation_flushes_shared_live_and_retained_queues() {
    let env = TestEnv::new();
    let active = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    active.try_register(80).unwrap().commit().unwrap();
    let retained = ThreadSignalManager::try_new(env.proc.clone()).unwrap();
    retained.try_register(81).unwrap().commit().unwrap();

    assert!(active.send_unqueued_signal(SignalInfo::new_kernel(Signo::SIGSTOP)));
    assert!(retained.send_unqueued_signal(SignalInfo::new_kernel(Signo::SIGTSTP)));
    assert_eq!(
        env.proc
            .send_unqueued_signal(SignalInfo::new_kernel(Signo::SIGTTIN)),
        Some(80)
    );
    retained.retire_registration(81, true);

    assert!(active.pending().has(Signo::SIGSTOP));
    assert!(retained.pending().has(Signo::SIGTSTP));
    assert!(env.proc.pending().has(Signo::SIGTTIN));

    assert!(active.send_unqueued_signal(SignalInfo::new_kernel(Signo::SIGCONT)));
    assert!(
        !retained
            .try_send_retained_signal_with(SignalInfo::new_kernel(Signo::SIGCONT), |info| Ok::<
                _,
                core::convert::Infallible,
            >(
                PreparedSignal::unqueued(info)
            ),)
            .unwrap()
            .wake
    );
    assert_eq!(
        env.proc
            .send_unqueued_signal(SignalInfo::new_kernel(Signo::SIGCONT)),
        Some(80)
    );
    assert!(!active.pending().has(Signo::SIGSTOP));
    assert!(!retained.pending().has(Signo::SIGTSTP));
    assert!(!env.proc.pending().has(Signo::SIGTTIN));

    assert!(active.pending().has(Signo::SIGCONT));
    assert!(retained.pending().has(Signo::SIGCONT));
    assert!(env.proc.pending().has(Signo::SIGCONT));
    assert!(active.send_unqueued_signal(SignalInfo::new_kernel(Signo::SIGSTOP)));
    assert!(!active.pending().has(Signo::SIGCONT));
    assert!(!retained.pending().has(Signo::SIGCONT));
    assert!(!env.proc.pending().has(Signo::SIGCONT));
    assert!(active.pending().has(Signo::SIGSTOP));
}

fn decorated_action(disposition: SignalDisposition) -> SignalAction {
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    SignalAction {
        flags: SignalActionFlags::RESTART | SignalActionFlags::RESTORER,
        mask,
        disposition,
        restorer: Some(0x1234),
    }
}

fn assert_canonical_default(action: SignalAction) {
    assert!(matches!(action.disposition, SignalDisposition::Default));
    assert!(action.flags.is_empty());
    assert!(action.mask.is_empty());
    assert_eq!(action.restorer, None);
}

fn assert_canonical_ignore(action: SignalAction) {
    assert!(matches!(action.disposition, SignalDisposition::Ignore));
    assert!(action.flags.is_empty());
    assert!(action.mask.is_empty());
    assert_eq!(action.restorer, None);
}

#[test]
fn exec_unshare_canonicalizes_default_and_ignore_actions() {
    let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let manager = ProcessSignalManager::new(shared, 0);
    manager
        .try_replace_action(Signo::SIGUSR1, decorated_action(SignalDisposition::Default))
        .unwrap();
    manager
        .try_replace_action(Signo::SIGUSR2, decorated_action(SignalDisposition::Ignore))
        .unwrap();
    manager
        .try_replace_action(
            Signo::SIGTERM,
            decorated_action(SignalDisposition::Handler(0x5678)),
        )
        .unwrap();

    manager.try_prepare_exec_unshare().unwrap().commit();

    assert_canonical_default(manager.action(Signo::SIGUSR1));
    assert_canonical_ignore(manager.action(Signo::SIGUSR2));
    assert_canonical_default(manager.action(Signo::SIGTERM));
}

#[test]
fn sigkill_and_sigstop_actions_are_rejected_and_remain_default() {
    for signo in [Signo::SIGKILL, Signo::SIGSTOP] {
        for disposition in [
            SignalDisposition::Default,
            SignalDisposition::Ignore,
            SignalDisposition::Handler(0x1234),
        ] {
            let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
            let manager = ProcessSignalManager::new(shared, 0);

            assert!(matches!(
                manager.try_replace_action(signo, decorated_action(disposition)),
                Err(thekernel_linux_signal::api::SignalActionUpdateError::UncatchableSignal)
            ));
            assert_canonical_default(manager.action(signo));
        }
    }
}

#[derive(Default)]
struct PendingReadinessWake {
    hits: std::sync::atomic::AtomicUsize,
    reader: std::sync::Mutex<Option<std::sync::Weak<ThreadSignalManager>>>,
}

impl std::task::Wake for PendingReadinessWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // A real readiness callback may immediately recheck readiness. This
        // also catches notifications invoked while pending/blocked locks remain held.
        if let Some(reader) = self
            .reader
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|r| r.upgrade())
        {
            let _ = reader.pending();
            let _ = reader.blocked();
        }
        self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn blocked_signal_readiness_covers_ordinary_and_deferred_publication() {
    use std::{convert::Infallible, sync::atomic::Ordering, task::Waker};
    let shared = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let process_wake = Arc::new(PendingReadinessWake::default());
    let mut manager = ProcessSignalManager::new(shared, 0);
    manager.set_pending_waker(Waker::from(process_wake.clone()));
    let process = Arc::new(manager);
    let thread = ThreadSignalManager::try_new(process.clone()).unwrap();
    thread.try_register(42).unwrap().commit().unwrap();
    *process_wake.reader.lock().unwrap() = Some(Arc::downgrade(&thread));
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    thread.set_blocked(mask);
    process_wake.hits.store(0, Ordering::SeqCst);
    let info = || SignalInfo::new_user(Signo::SIGUSR1, 0, 7, 0);

    let sent = process
        .try_send_signal_with(info(), |info| {
            Ok::<_, Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(sent.published);
    assert_eq!(
        sent.wake_tid, None,
        "blocked signals do not use delivery interrupts"
    );
    assert_eq!(process_wake.hits.swap(0, Ordering::SeqCst), 1);
    assert!(thread.dequeue_signal_for_signalfd(&mask).is_some());

    let sent = thread
        .try_send_signal_with(info(), |info| {
            Ok::<_, Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(sent.published && !sent.wake);
    assert_eq!(process_wake.hits.swap(0, Ordering::SeqCst), 1);
    assert!(thread.dequeue_signal_for_signalfd(&mask).is_some());

    // Both finishing and dropping deferred owners must notify, but the
    // publication call itself must not run arbitrary readiness callbacks.
    for finish in [false, true] {
        let deferred = process
            .try_prepare_signal_send()
            .unwrap()
            .publish(PreparedSignal::unqueued(info()));
        assert!(deferred.outcome().published);
        assert_eq!(process_wake.hits.load(Ordering::SeqCst), 0);
        if finish {
            drop(deferred.finish());
        } else {
            drop(deferred);
        }
        assert_eq!(process_wake.hits.swap(0, Ordering::SeqCst), 1);
        assert!(thread.dequeue_signal_for_signalfd(&mask).is_some());

        let deferred = thread
            .try_prepare_signal_send()
            .unwrap()
            .publish(PreparedSignal::unqueued(info()));
        assert!(deferred.outcome().published);
        assert_eq!(process_wake.hits.load(Ordering::SeqCst), 0);
        if finish {
            drop(deferred.finish());
        } else {
            drop(deferred);
        }
        assert_eq!(process_wake.hits.swap(0, Ordering::SeqCst), 1);
        assert!(thread.dequeue_signal_for_signalfd(&mask).is_some());
    }

    // The readiness subscription belongs to the process, even if one
    // sibling installed epoll and another receives a private signal.
    let sibling = ThreadSignalManager::try_new(process.clone()).unwrap();
    sibling.try_register(43).unwrap().commit().unwrap();
    sibling.set_blocked(mask);
    assert_eq!(process_wake.hits.swap(0, Ordering::SeqCst), 1);
    let sent = sibling
        .try_send_signal_with(info(), |info| {
            Ok::<_, Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(sent.published && !sent.wake);
    assert_eq!(process_wake.hits.swap(0, Ordering::SeqCst), 1);
    assert!(thread.dequeue_signal_for_signalfd(&mask).is_none());
    assert!(sibling.dequeue_signal_for_signalfd(&mask).is_some());

    thread.retire_registration(42, true);
    let sent = thread
        .try_send_retained_signal_with(info(), |info| {
            Ok::<_, Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(sent.published && !sent.wake);
    assert_eq!(process_wake.hits.swap(0, Ordering::SeqCst), 1);
    assert!(thread.dequeue_signal_for_signalfd(&mask).is_some());
}
