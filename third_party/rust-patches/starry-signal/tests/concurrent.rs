use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use axcpu::uspace::UserContext;
use starry_signal::{
    PreparedSignal, SignalAction, SignalDisposition, SignalInfo, SignalOSAction,
    SignalQueueAccount, SignalQueueError, SignalSet, Signo,
    api::{SignalFrame, ThreadSignalManager},
};

mod common;
use common::*;

fn wait_until<F>(mut check: F) -> bool
where
    F: FnMut() -> bool,
{
    const TIMEOUT: Duration = Duration::from_millis(100);

    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        if check() {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    false
}

#[test]
fn concurrent_send_signal() {
    let (proc, thr) = new_test_env();

    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 9, 9);

    thread::spawn({
        let thr = thr.clone();
        move || {
            thread::sleep(Duration::from_millis(10));
            let _ = thr.send_unqueued_signal(sig);
        }
    });

    assert!(wait_until(
        || thr.pending().has(signo) || proc.pending().has(signo)
    ));
}

#[test]
fn concurrent_blocked() {
    let (_proc, thr) = new_test_env();

    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 9, 9);

    let mut blocked = SignalSet::default();
    blocked.add(signo);
    let prev = thr.set_blocked(blocked);
    assert!(!prev.has(signo));
    assert!(thr.signal_blocked(signo));

    thread::spawn({
        let thr = thr.clone();
        move || {
            thread::sleep(Duration::from_millis(10));
            let _ = thr.send_unqueued_signal(sig);
        }
    });

    assert!(wait_until(|| thr.pending().has(signo)));

    thr.set_blocked(SignalSet::default());
    assert!(!thr.signal_blocked(signo));

    let mut uctx = UserContext::new(0, 0.into(), 0);
    let res = wait_until(|| {
        if let Some(delivered) = thr.check_signals(&mut uctx, None) {
            assert_eq!(delivered.info.signo(), signo);
            true
        } else {
            false
        }
    });
    assert!(res);
}

#[test]
fn concurrent_check_signals() {
    let (proc, thr) = new_test_env();

    unsafe extern "C" fn test_handler(_: i32) {}
    proc.actions.lock()[Signo::SIGTERM].disposition =
        SignalDisposition::Handler(test_handler as usize);

    let mut uctx = UserContext::new(0, initial_sp().into(), 0);

    let first = SignalInfo::new_user(Signo::SIGTERM, 9, 9);
    assert!(thr.send_unqueued_signal(first.clone()));

    let delivered = thr.check_signals(&mut uctx, None).unwrap();
    assert_eq!(delivered.info.signo(), Signo::SIGTERM);
    assert_eq!(delivered.os_action, SignalOSAction::Handler);
    assert!(thr.signal_blocked(Signo::SIGTERM));

    thread::spawn({
        let thr = thr.clone();
        move || {
            let _ = thr.send_unqueued_signal(SignalInfo::new_user(Signo::SIGINT, 2, 2));
            let _ = thr.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 3, 3));
        }
    });

    assert!(wait_until(|| thr.pending().has(Signo::SIGTERM)));
    assert!(wait_until(|| thr.pending().has(Signo::SIGINT)));

    let new_sp = uctx.sp() + 8;
    uctx.set_sp(new_sp);
    let frame = SignalFrame::read_from_user(uctx.sp() as *const SignalFrame)
        .expect("signal frame must remain isolated from concurrent tests");
    let prepared = thr
        .prepare_restore(&uctx, frame, |_| true, |_| true)
        .unwrap();
    thr.commit_restore(&mut uctx, prepared);

    assert!(!thr.signal_blocked(Signo::SIGTERM));

    let mut delivered = SignalSet::default();
    assert!(wait_until(|| {
        if let Some(signal) = thr.check_signals(&mut uctx, None) {
            delivered.add(signal.info.signo());
        }
        delivered.has(Signo::SIGINT) && delivered.has(Signo::SIGTERM)
    }));
}

#[test]
fn concurrent_account_admission_never_exceeds_limit() {
    const SENDERS: usize = 32;
    const LIMIT: usize = 7;

    let (_proc, signal) = new_test_env();
    let user = SignalQueueAccount::try_new(SENDERS).unwrap();
    let global = SignalQueueAccount::try_new(SENDERS).unwrap();
    let barrier = Arc::new(Barrier::new(SENDERS));

    let senders: Vec<_> = (0..SENDERS)
        .map(|sender| {
            let signal = signal.clone();
            let user = user.clone();
            let global = global.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                signal
                    .try_send_signal_with(
                        SignalInfo::new_user(Signo::SIGRTMIN, sender as i32, sender as u32),
                        |info| PreparedSignal::try_accounted(info, &user, LIMIT as u64, &global),
                    )
                    .is_ok()
            })
        })
        .collect();

    let admitted = senders
        .into_iter()
        .map(|sender| sender.join().unwrap())
        .filter(|admitted| *admitted)
        .count();
    assert_eq!(admitted, LIMIT);
    assert_eq!(user.queued(), LIMIT);
    assert_eq!(global.queued(), LIMIT);

    let mask = !SignalSet::default();
    for delivered in 0..LIMIT {
        assert!(
            signal.dequeue_signal(&mask).is_some(),
            "missing queued instance {delivered}; pending={:?}, user={}, global={}",
            signal.pending(),
            user.queued(),
            global.queued(),
        );
    }
    assert!(signal.dequeue_signal(&mask).is_none());
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn ignore_transition_linearizes_with_prepared_realtime_publication() {
    let (process, signal) = new_test_env();
    let user = SignalQueueAccount::try_new(1).unwrap();
    let global = SignalQueueAccount::try_new(1).unwrap();
    let prepared = Arc::new(Barrier::new(2));
    let publish = Arc::new(Barrier::new(2));

    let sender = {
        let signal = signal.clone();
        let user = user.clone();
        let global = global.clone();
        let prepared_barrier = prepared.clone();
        let publish_barrier = publish.clone();
        thread::spawn(move || {
            signal
                .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 1, 1), |info| {
                    let signal = PreparedSignal::try_accounted(info, &user, 1, &global)?;
                    prepared_barrier.wait();
                    publish_barrier.wait();
                    Ok::<_, SignalQueueError>(signal)
                })
                .unwrap()
        })
    };

    prepared.wait();
    process
        .try_replace_action(
            Signo::SIGRTMIN,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..SignalAction::default()
            },
        )
        .unwrap();
    publish.wait();

    let outcome = sender.join().unwrap();
    assert!(!outcome.published);
    assert!(!outcome.wake);
    assert!(!signal.pending().has(Signo::SIGRTMIN));
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn ordinary_retirement_rejects_a_prepared_private_realtime_commit() {
    let (_process, signal) = new_test_env();
    let user = SignalQueueAccount::try_new(1).unwrap();
    let global = SignalQueueAccount::try_new(1).unwrap();
    let prepared = Arc::new(Barrier::new(2));
    let publish = Arc::new(Barrier::new(2));

    let sender = {
        let signal = signal.clone();
        let user = user.clone();
        let global = global.clone();
        let prepared_barrier = prepared.clone();
        let publish_barrier = publish.clone();
        thread::spawn(move || {
            signal
                .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 1, 1), |info| {
                    let signal = PreparedSignal::try_accounted(info, &user, 1, &global)?;
                    prepared_barrier.wait();
                    publish_barrier.wait();
                    Ok::<_, SignalQueueError>(signal)
                })
                .unwrap()
        })
    };

    prepared.wait();
    signal.retire_registration(TID, false);
    publish.wait();

    let outcome = sender.join().unwrap();
    assert!(!outcome.published);
    assert!(!outcome.wake);
    assert!(!signal.pending().has(Signo::SIGRTMIN));
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn retained_retirement_rejects_a_prepared_private_realtime_commit() {
    let (_process, signal) = new_test_env();
    signal.retire_registration(TID, true);
    let user = SignalQueueAccount::try_new(1).unwrap();
    let global = SignalQueueAccount::try_new(1).unwrap();
    let prepared = Arc::new(Barrier::new(2));
    let publish = Arc::new(Barrier::new(2));

    let sender = {
        let signal = signal.clone();
        let user = user.clone();
        let global = global.clone();
        let prepared_barrier = prepared.clone();
        let publish_barrier = publish.clone();
        thread::spawn(move || {
            signal
                .try_send_retained_signal_with(
                    SignalInfo::new_user(Signo::SIGRTMIN, 1, 1),
                    |info| {
                        let signal = PreparedSignal::try_accounted(info, &user, 1, &global)?;
                        prepared_barrier.wait();
                        publish_barrier.wait();
                        Ok::<_, SignalQueueError>(signal)
                    },
                )
                .unwrap()
        })
    };

    prepared.wait();
    signal.retire_registration(TID, false);
    publish.wait();

    let outcome = sender.join().unwrap();
    assert!(!outcome.published);
    assert!(!outcome.wake);
    assert!(!signal.pending().has(Signo::SIGRTMIN));
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn final_process_retention_rejects_a_prepared_shared_realtime_commit() {
    let (process, _signal) = new_test_env();
    let user = SignalQueueAccount::try_new(1).unwrap();
    let global = SignalQueueAccount::try_new(1).unwrap();
    let prepared = Arc::new(Barrier::new(2));
    let publish = Arc::new(Barrier::new(2));

    let sender = {
        let process = process.clone();
        let user = user.clone();
        let global = global.clone();
        let prepared_barrier = prepared.clone();
        let publish_barrier = publish.clone();
        thread::spawn(move || {
            process
                .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 1, 1), |info| {
                    let signal = PreparedSignal::try_accounted(info, &user, 1, &global)?;
                    prepared_barrier.wait();
                    publish_barrier.wait();
                    Ok::<_, SignalQueueError>(signal)
                })
                .unwrap()
        })
    };

    prepared.wait();
    process.retain_pending_only();
    publish.wait();

    let outcome = sender.join().unwrap();
    assert!(!outcome.published);
    assert_eq!(outcome.wake_tid, None);
    assert!(!process.pending().has(Signo::SIGRTMIN));
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn action_update_does_not_fail_under_registration_churn() {
    let (process, _signal) = new_test_env();
    let running = Arc::new(AtomicBool::new(true));
    let churn = {
        let process = process.clone();
        let running = running.clone();
        thread::spawn(move || {
            let mut tid = 100;
            while running.load(Ordering::Acquire) {
                let signal = ThreadSignalManager::try_new(process.clone()).unwrap();
                if let Ok(registration) = signal.try_register(tid) {
                    drop(registration);
                }
                tid = tid.wrapping_add(1);
            }
        })
    };

    let started = Instant::now();
    for _ in 0..128 {
        process
            .try_replace_action(Signo::SIGTERM, SignalAction::default())
            .expect("registration churn must not become a user-visible contention error");
    }
    running.store(false, Ordering::Release);
    churn.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "action update retried without a finite contention bound"
    );
}

#[test]
fn registration_commit_linearizes_with_ignored_action_flush() {
    for _ in 0..32 {
        let (process, signal, registration) = new_unregistered_test_env();
        let user = SignalQueueAccount::try_new(1).unwrap();
        let global = SignalQueueAccount::try_new(1).unwrap();
        let start = Arc::new(Barrier::new(2));
        let (committed_tx, committed_rx) = mpsc::channel();

        let commit = {
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                registration.commit().unwrap();
                committed_tx.send(()).unwrap();
            })
        };
        let update = {
            let process = process.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                process
                    .try_replace_action(
                        Signo::SIGRTMIN,
                        SignalAction {
                            disposition: SignalDisposition::Ignore,
                            ..SignalAction::default()
                        },
                    )
                    .unwrap();
            })
        };

        committed_rx.recv().unwrap();
        signal
            .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 1, 1), |info| {
                PreparedSignal::try_accounted(info, &user, 1, &global)
            })
            .unwrap();
        commit.join().unwrap();
        update.join().unwrap();

        assert!(!signal.pending().has(Signo::SIGRTMIN));
        assert_eq!(user.queued(), 0);
        assert_eq!(global.queued(), 0);
    }
}
