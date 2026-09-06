use std::{
    mem::MaybeUninit,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use thekernel_linux_signal::arch::{UserContext, XsaveState64};
use thekernel_linux_signal::{
    PreparedSignal, SignalAction, SignalActionFlags, SignalDisposition, SignalInfo, SignalOSAction,
    SignalQueueAccount, SignalQueueError, SignalSet, Signo,
    api::{SignalFpState, SignalFrame, SignalFrameLayoutError},
};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmResult};

mod common;
use common::*;

struct BlockingFirstWrite {
    inner: Vm,
    entered: Option<mpsc::Sender<()>>,
    release: mpsc::Receiver<()>,
}

// SAFETY: all memory accesses delegate to the range-checking test VM. The
// first write only pauses publication so the test can replace the disposition
// while an `SA_RESETHAND` claim is in flight.
unsafe impl UserMemory for BlockingFirstWrite {
    fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
        self.inner.read(start, dst)
    }

    fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap();
            self.release.recv().unwrap();
        }
        self.inner.write(start, src)
    }
}

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

/// A complete, standard-format XSAVE image for delivery tests.  The signal
/// data-plane contract deliberately does not synthesize a legacy 512-byte
/// FXSAVE image: the embedding kernel must supply every enabled component.
/// This fixture models the minimal XCR0 policy, whose full standard XSAVE
/// extent is the legacy area plus the 64-byte XSAVE header.
fn full_xsave_snapshot() -> Result<SignalFpState, SignalFrameLayoutError> {
    Ok(XsaveState64::try_from_xsave_prefix(
        &[0; XsaveState64::LEGACY_SIZE + XsaveState64::HEADER_SIZE],
        0b11,
    )
    .expect("minimal standard XSAVE fixture must be valid")
    .into())
}

#[test]
fn concurrent_send_signal() {
    let (proc, thr) = new_test_env();

    let signo = Signo::SIGTERM;
    let sig = SignalInfo::new_user(signo, 9, 9, 0);

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
    let sig = SignalInfo::new_user(signo, 9, 9, 0);

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

    let mut uctx = UserContext::new(0, 0, 0);
    let mut provider = memory_provider();
    let mut memory = UserMemoryContext::new(&mut provider);
    let res = wait_until(|| {
        if let Some(delivered) =
            thr.check_signals_with_fp_snapshot(&mut memory, &mut uctx, None, full_xsave_snapshot)
        {
            assert_eq!(delivered.info.signo(), signo);
            true
        } else {
            false
        }
    });
    assert!(res);
}

#[test]
fn concurrent_signalfd_observation_respects_blocked_mask() {
    let (_proc, thr) = new_test_env();
    let signo = Signo::SIGTERM;
    let mut fd_mask = SignalSet::default();
    fd_mask.add(signo);

    let mut blocked = SignalSet::default();
    blocked.add(signo);
    thr.set_blocked(blocked);

    let sender = {
        let thr = thr.clone();
        thread::spawn(move || thr.send_unqueued_signal(SignalInfo::new_user(signo, 0, 1, 0)))
    };
    let _ = sender.join().unwrap();
    assert!(wait_until(|| thr.pending().has(signo)));
    assert!(thr.has_pending_signal_for_signalfd(&fd_mask));
    assert_eq!(
        thr.dequeue_signal_for_signalfd(&fd_mask)
            .expect("blocked signal must remain visible to signalfd")
            .signo(),
        signo
    );

    thr.set_blocked(SignalSet::default());
    let sender = {
        let thr = thr.clone();
        thread::spawn(move || thr.send_unqueued_signal(SignalInfo::new_user(signo, 0, 2, 0)))
    };
    assert!(sender.join().unwrap());
    assert!(wait_until(|| thr.pending().has(signo)));
    assert!(!thr.has_pending_signal_for_signalfd(&fd_mask));
    assert!(thr.dequeue_signal_for_signalfd(&fd_mask).is_none());

    thr.set_blocked(blocked);
    assert!(thr.has_pending_signal_for_signalfd(&fd_mask));
    assert_eq!(
        thr.dequeue_signal_for_signalfd(&fd_mask)
            .expect("mask update must make the pending signal visible")
            .signo(),
        signo
    );
}

#[test]
fn concurrent_check_signals() {
    let (proc, thr) = new_test_env();

    unsafe extern "C" fn test_handler(_: i32) {}
    proc.try_replace_action(
        Signo::SIGTERM,
        SignalAction {
            disposition: SignalDisposition::Handler(test_handler as *const () as usize),
            ..SignalAction::default()
        },
    )
    .unwrap();

    let mut uctx = UserContext::new(0, initial_sp(), 0);
    let mut provider = memory_provider();
    let mut memory = UserMemoryContext::new(&mut provider);

    let first = SignalInfo::new_user(Signo::SIGTERM, 9, 9, 0);
    assert!(thr.send_unqueued_signal(first.clone()));

    let delivered = thr
        .check_signals_with_fp_snapshot(&mut memory, &mut uctx, None, full_xsave_snapshot)
        .unwrap();
    assert_eq!(delivered.info.signo(), Signo::SIGTERM);
    assert_eq!(delivered.os_action, SignalOSAction::Handler);
    assert!(thr.signal_blocked(Signo::SIGTERM));

    thread::spawn({
        let thr = thr.clone();
        move || {
            let _ = thr.send_unqueued_signal(SignalInfo::new_user(Signo::SIGINT, 2, 2, 0));
            let _ = thr.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 3, 3, 0));
        }
    });

    assert!(wait_until(|| thr.pending().has(Signo::SIGTERM)));
    assert!(wait_until(|| thr.pending().has(Signo::SIGINT)));

    let new_sp = uctx.sp() + 8;
    uctx.set_sp(new_sp);
    let frame = SignalFrame::read_from_user(&mut memory, uctx.sp() as *const SignalFrame)
        .expect("signal frame must remain isolated from concurrent tests");
    let prepared = thr
        .prepare_restore(&uctx, frame, |_| true, |_| true, |_, _, _| Ok(()))
        .unwrap();
    thr.commit_restore(&mut uctx, prepared);

    assert!(!thr.signal_blocked(Signo::SIGTERM));

    let mut delivered = SignalSet::default();
    assert!(wait_until(|| {
        if let Some(signal) =
            thr.check_signals_with_fp_snapshot(&mut memory, &mut uctx, None, full_xsave_snapshot)
        {
            delivered.add(signal.info.signo());
        }
        delivered.has(Signo::SIGINT) && delivered.has(Signo::SIGTERM)
    }));
}

#[test]
fn reset_hand_completion_cannot_overwrite_a_concurrent_replacement() {
    let (proc, thr) = new_test_env();
    let signo = Signo::SIGTERM;
    proc.try_replace_action(
        signo,
        SignalAction {
            disposition: SignalDisposition::Handler(0x4000),
            flags: SignalActionFlags::RESETHAND,
            ..SignalAction::default()
        },
    )
    .unwrap();
    assert!(thr.send_unqueued_signal(SignalInfo::new_user(signo, 1, 1, 0)));

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let delivery = {
        let thr = thr.clone();
        thread::spawn(move || {
            let mut provider = BlockingFirstWrite {
                inner: memory_provider(),
                entered: Some(entered_tx),
                release: release_rx,
            };
            let mut memory = UserMemoryContext::new(&mut provider);
            let mut context = UserContext::new(0x1000, initial_sp(), 0);
            thr.check_signals_with_fp_snapshot(&mut memory, &mut context, None, full_xsave_snapshot)
                .unwrap()
        })
    };

    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    proc.try_replace_action(
        signo,
        SignalAction {
            disposition: SignalDisposition::Handler(0x5000),
            ..SignalAction::default()
        },
    )
    .unwrap();
    release_tx.send(()).unwrap();

    assert_eq!(delivery.join().unwrap().os_action, SignalOSAction::Handler);
    assert!(matches!(
        proc.action(signo).disposition,
        SignalDisposition::Handler(0x5000)
    ));
}

#[test]
fn cancellation_waits_for_an_already_started_delivery() {
    let (proc, thr) = new_test_env();
    let signo = Signo::SIGTERM;
    proc.try_replace_action(
        signo,
        SignalAction {
            disposition: SignalDisposition::Handler(0x4000),
            ..SignalAction::default()
        },
    )
    .unwrap();
    assert!(thr.send_unqueued_signal(SignalInfo::new_user(signo, 1, 1, 0)));

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let delivery = {
        let thr = thr.clone();
        thread::spawn(move || {
            let mut provider = BlockingFirstWrite {
                inner: memory_provider(),
                entered: Some(entered_tx),
                release: release_rx,
            };
            let mut memory = UserMemoryContext::new(&mut provider);
            let mut context = UserContext::new(0x1000, initial_sp(), 0);
            thr.check_signals_with_fp_snapshot(&mut memory, &mut context, None, full_xsave_snapshot)
                .unwrap()
        })
    };
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    let (cancelled_tx, cancelled_rx) = mpsc::channel();
    let cancellation = {
        let thr = thr.clone();
        thread::spawn(move || {
            let cancelled = thr.cancel_registration();
            cancelled_tx.send(cancelled).unwrap();
        })
    };
    assert!(matches!(
        cancelled_rx.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    release_tx.send(()).unwrap();
    assert_eq!(delivery.join().unwrap().os_action, SignalOSAction::Handler);
    assert!(cancelled_rx.recv_timeout(Duration::from_secs(1)).unwrap());
    cancellation.join().unwrap();
    assert!(!thr.is_registered());
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
                        SignalInfo::new_user(Signo::SIGRTMIN, sender as i32, sender as u32, 0),
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
                .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 1, 1, 0), |info| {
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
fn deferred_process_send_linearizes_with_ignored_transition() {
    let (process, _signal) = new_test_env();
    let route = process.try_prepare_signal_send().unwrap();
    let user = SignalQueueAccount::try_new(1).unwrap();
    let global = SignalQueueAccount::try_new(1).unwrap();
    let prepared = PreparedSignal::try_accounted(
        SignalInfo::new_user(Signo::SIGRTMIN, 1, 1, 0),
        &user,
        1,
        &global,
    )
    .unwrap();
    let start = Arc::new(Barrier::new(2));

    let sender = {
        let start = start.clone();
        thread::spawn(move || {
            start.wait();
            let deferred = route.publish(prepared);
            let outcome = deferred.outcome();
            let (_, unused) = deferred.finish();
            drop(unused);
            outcome
        })
    };
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

    let _ = sender.join().unwrap();
    assert!(!process.pending().has(Signo::SIGRTMIN));
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);
}

#[test]
fn deferred_thread_send_linearizes_with_endpoint_cancellation() {
    let (_process, signal) = new_test_env();
    let endpoint = signal.try_prepare_signal_send().unwrap();
    let user = SignalQueueAccount::try_new(1).unwrap();
    let global = SignalQueueAccount::try_new(1).unwrap();
    let prepared = PreparedSignal::try_accounted(
        SignalInfo::new_user(Signo::SIGRTMIN, 1, 1, 0),
        &user,
        1,
        &global,
    )
    .unwrap();
    let start = Arc::new(Barrier::new(2));

    let sender = {
        let start = start.clone();
        thread::spawn(move || {
            start.wait();
            let deferred = endpoint.publish(prepared);
            let outcome = deferred.outcome();
            let (_, unused) = deferred.finish();
            drop(unused);
            outcome
        })
    };
    start.wait();
    assert!(signal.cancel_registration());

    let _ = sender.join().unwrap();
    assert!(!signal.pending().has(Signo::SIGRTMIN));
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
                let signal =
                    thekernel_linux_signal::api::ThreadSignalManager::try_new(process.clone())
                        .unwrap();
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
fn concurrent_cancellation_linearizes_and_refunds_every_queue_charge() {
    const SENDERS: usize = 32;

    let (_process, signal) = new_test_env();
    let user = SignalQueueAccount::try_new(SENDERS).unwrap();
    let global = SignalQueueAccount::try_new(SENDERS).unwrap();
    let start = Arc::new(Barrier::new(SENDERS + 1));

    let senders: Vec<_> = (0..SENDERS)
        .map(|sender| {
            let signal = signal.clone();
            let user = user.clone();
            let global = global.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                signal
                    .try_send_signal_with(
                        SignalInfo::new_user(Signo::SIGRTMIN, sender as i32, sender as u32, 0),
                        |info| PreparedSignal::try_accounted(info, &user, SENDERS as u64, &global),
                    )
                    .unwrap()
            })
        })
        .collect();

    start.wait();
    assert!(signal.cancel_registration());
    for sender in senders {
        let _ = sender.join().unwrap();
    }

    assert!(!signal.is_registered());
    assert!(!signal.pending().has(Signo::SIGRTMIN));
    assert_eq!(user.queued(), 0);
    assert_eq!(global.queued(), 0);

    let prepared_after_cancel = Arc::new(AtomicBool::new(false));
    let called = prepared_after_cancel.clone();
    let outcome = signal
        .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 99, 99, 0), |info| {
            called.store(true, Ordering::Release);
            Ok::<_, core::convert::Infallible>(PreparedSignal::unqueued(info))
        })
        .unwrap();
    assert!(!outcome.published);
    assert!(!outcome.wake);
    assert!(!prepared_after_cancel.load(Ordering::Acquire));
}

#[test]
fn registration_commit_cannot_race_teardown_into_resurrection() {
    for _ in 0..64 {
        let (process, signal, registration) = new_unregistered_test_env();
        let start = Arc::new(Barrier::new(2));
        let commit = {
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                registration.commit()
            })
        };
        let cancel = {
            let signal = signal.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                signal.cancel_registration()
            })
        };

        let _ = commit.join().unwrap();
        assert!(cancel.join().unwrap());
        assert!(!signal.is_registered());
        assert_eq!(
            process.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 1, 1, 0)),
            None
        );
    }
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
            .try_send_signal_with(SignalInfo::new_user(Signo::SIGRTMIN, 1, 1, 0), |info| {
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
