use std::sync::Arc;

use thekernel_linux_signal::{
    PreparedSignal, SignalAction, SignalDisposition, SignalInfo, SignalQueueAccount, SignalSet,
    Signo,
    api::{ProcessSignalManager, SharedSignalActions, SignalActions, ThreadSignalManager},
};

fn new_test_env() -> (Arc<ProcessSignalManager>, Arc<ThreadSignalManager>) {
    let actions = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let process = Arc::new(ProcessSignalManager::new(actions, 0));
    let thread = ThreadSignalManager::try_new(process.clone()).unwrap();
    thread.try_register(7).unwrap().commit().unwrap();
    (process, thread)
}

fn accounts(limit: usize) -> (Arc<SignalQueueAccount>, Arc<SignalQueueAccount>) {
    (
        SignalQueueAccount::try_new(limit).unwrap(),
        SignalQueueAccount::try_new(limit).unwrap(),
    )
}

fn send_accounted(
    thread: &ThreadSignalManager,
    info: SignalInfo,
    user: &Arc<SignalQueueAccount>,
    global: &Arc<SignalQueueAccount>,
    limit: u64,
) {
    thread
        .try_send_signal_with(info, |info| {
            PreparedSignal::try_accounted(info, user, limit, global)
        })
        .unwrap();
}

fn all_signals() -> SignalSet {
    !SignalSet::default()
}

#[test]
fn send_outcome_distinguishes_owned_publication_from_coalescing() {
    fn assert_send<T: Send>() {}
    assert_send::<PreparedSignal>();

    let (_proc, thread) = new_test_env();
    let first = thread
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGINT, 1, 1, 0), |info| {
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(first.published);

    let coalesced = thread
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGINT, 2, 2, 0), |info| {
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(!coalesced.published);
    assert_eq!(thread.dequeue_signal(&all_signals()).unwrap().code(), 1);
}

#[test]
fn prepared_signal_replaces_siginfo_without_changing_signo() {
    let mut prepared = PreparedSignal::unqueued(SignalInfo::new_user(Signo::SIGRTMIN, 1, 10, 0));
    let replacement = SignalInfo::new_user(Signo::SIGRTMIN, 2, 20, 0);
    let old = prepared.replace_info(replacement).unwrap();
    assert_eq!(old.code(), 1);
    assert_eq!(prepared.info().code(), 2);

    let wrong_signo = SignalInfo::new_user(Signo::SIGRT1, 3, 30, 0);
    assert!(prepared.replace_info(wrong_signo).is_none());
    assert_eq!(prepared.signo(), Signo::SIGRTMIN);
}

#[test]
fn standard_signal_uses_fixed_slots_and_coalesces() {
    let (_proc, thread) = new_test_env();
    let (user, global) = accounts(1);
    send_accounted(
        &thread,
        SignalInfo::new_user(Signo::SIGINT, 9, 9, 0),
        &user,
        &global,
        0,
    );
    send_accounted(
        &thread,
        SignalInfo::new_user(Signo::SIGINT, 10, 10, 0),
        &user,
        &global,
        0,
    );

    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
    assert_eq!(thread.dequeue_signal(&all_signals()).unwrap().code(), 9);
    assert!(thread.dequeue_signal(&all_signals()).is_none());
}

#[test]
fn realtime_signal_is_fifo_with_lowest_signo_priority() {
    let (_proc, thread) = new_test_env();
    let (user, global) = accounts(8);
    for (signo, code) in [
        (Signo::SIGRT2, 20),
        (Signo::SIGRTMIN, 10),
        (Signo::SIGRTMIN, 11),
        (Signo::SIGRT1, 15),
    ] {
        send_accounted(
            &thread,
            SignalInfo::new_user(signo, code, 9, 0),
            &user,
            &global,
            8,
        );
    }

    assert_eq!(user.queued(), 4);
    let delivered: Vec<_> = (0..4)
        .map(|_| thread.dequeue_signal(&all_signals()).unwrap())
        .map(|info| (info.signo(), info.code()))
        .collect();
    assert_eq!(
        delivered,
        [
            (Signo::SIGRTMIN, 10),
            (Signo::SIGRTMIN, 11),
            (Signo::SIGRT1, 15),
            (Signo::SIGRT2, 20),
        ]
    );
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn process_and_thread_pending_share_one_account() {
    let (process, thread) = new_test_env();
    let (user, global) = accounts(2);
    process
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 1, 1, 0), |info| {
            PreparedSignal::try_accounted(info, &user, 2, &global)
        })
        .unwrap();
    send_accounted(
        &thread,
        SignalInfo::new_user(Signo::SIGRT1, 2, 2, 0),
        &user,
        &global,
        2,
    );
    assert_eq!(user.queued(), 2);

    let rejected = thread
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGRT2, 3, 3, 0), |info| {
            PreparedSignal::try_accounted(info, &user, 2, &global)
        });
    assert!(rejected.is_err());
    assert_eq!(user.queued(), 2);

    assert!(thread.dequeue_signal(&all_signals()).is_some());
    assert!(thread.dequeue_signal(&all_signals()).is_some());
    assert_eq!(user.queued(), 0);
}

#[test]
fn flush_one_signal_refunds_process_and_thread_queues_precisely() {
    let (process, thread) = new_test_env();
    let (user, global) = accounts(8);
    for (signo, code) in [(Signo::SIGRTMIN, 1), (Signo::SIGRT1, 2)] {
        process
            .try_send_signal_with(SignalInfo::new_user(signo, code, 1, 0), |info| {
                PreparedSignal::try_accounted(info, &user, 8, &global)
            })
            .unwrap();
        send_accounted(
            &thread,
            SignalInfo::new_user(signo, code + 10, 1, 0),
            &user,
            &global,
            8,
        );
    }
    assert_eq!(user.queued(), 4);

    process.flush_signal(Signo::SIGRTMIN);
    assert_eq!(user.queued(), 3);
    thread.flush_signal(Signo::SIGRTMIN);
    assert_eq!(user.queued(), 2);
    assert_eq!(global.queued(), 2);

    let delivered: Vec<_> = (0..2)
        .map(|_| thread.dequeue_signal(&all_signals()).unwrap())
        .map(|info| (info.signo(), info.code()))
        .collect();
    assert_eq!(delivered, [(Signo::SIGRT1, 12), (Signo::SIGRT1, 2)]);
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn dropping_signal_managers_refunds_all_queue_charges() {
    let (user, global) = accounts(4);
    {
        let (process, thread) = new_test_env();
        process
            .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 1, 1, 0), |info| {
                PreparedSignal::try_accounted(info, &user, 4, &global)
            })
            .unwrap();
        send_accounted(
            &thread,
            SignalInfo::new_user(Signo::SIGRT1, 2, 2, 0),
            &user,
            &global,
            4,
        );
        assert_eq!(user.queued(), 2);
    }
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn ignored_action_transition_flushes_process_and_thread_instances() {
    let (process, thread) = new_test_env();
    let (user, global) = accounts(4);
    process
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 1, 1, 0), |info| {
            PreparedSignal::try_accounted(info, &user, 4, &global)
        })
        .unwrap();
    send_accounted(
        &thread,
        SignalInfo::new_user(Signo::SIGRTMIN, 2, 2, 0),
        &user,
        &global,
        4,
    );
    assert_eq!(user.queued(), 2);

    process
        .try_replace_action(
            Signo::SIGRTMIN,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..SignalAction::default()
            },
        )
        .unwrap();

    assert!(!process.pending().has(Signo::SIGRTMIN));
    assert!(!thread.pending().has(Signo::SIGRTMIN));
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}
