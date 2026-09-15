use axpoll::{
    PollRegistration, PollRegistrationError, PollSet, PreparedPollRegistration,
    live_registration_charges,
};
use std::sync::Mutex;

static CHARGE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn failed_prepare_rolls_back_tokens_and_credits() {
    let _guard = CHARGE_TEST_LOCK.lock().unwrap();
    let source = PollSet::<1>::new();
    let baseline = live_registration_charges();
    let mut prepared = PreparedPollRegistration::try_new(2).unwrap();
    prepared.arm(&source, core::task::Waker::noop()).unwrap();
    assert_eq!(live_registration_charges(), baseline + 2);
    assert!(matches!(
        prepared.arm(&source, core::task::Waker::noop()),
        Err(PollRegistrationError::Source { .. })
    ));
    drop(prepared);
    assert!(source.is_empty());
    assert_eq!(live_registration_charges(), baseline);
}

#[test]
fn commit_refunds_unused_topology_credits() {
    let _guard = CHARGE_TEST_LOCK.lock().unwrap();
    let source = PollSet::<1>::new();
    let baseline = live_registration_charges();
    let mut prepared = PreparedPollRegistration::try_new(2).unwrap();
    prepared.arm(&source, core::task::Waker::noop()).unwrap();
    let registration = prepared.commit().unwrap();
    assert_eq!(live_registration_charges(), baseline + 1);
    drop(registration);
    assert_eq!(live_registration_charges(), baseline);
}

#[test]
fn cancelled_registration_is_not_silently_rearmed() {
    let _guard = CHARGE_TEST_LOCK.lock().unwrap();
    let source = PollSet::<1>::new();
    let mut registration = PollRegistration::single(&source, core::task::Waker::noop()).unwrap();
    registration.cancel();
    assert!(registration.update(core::task::Waker::noop()).is_err());
}
