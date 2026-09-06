use core::mem::{align_of, offset_of, size_of};

use thekernel_linux_rseq::*;

const USER_LIMIT: UserAddressLimit = UserAddressLimit::new(0x10_0000);

fn descriptor(flags: u32) -> RseqDescriptor {
    RseqDescriptor::new(
        0x2000,
        RseqCriticalSection::from_raw(0, flags, 0x1000, 0x20, 0x3000),
        USER_LIMIT,
    )
    .unwrap()
}

fn area_with_descriptor(flags: u32) -> RseqArea {
    let mut area = RseqArea::initial();
    area.cpu_id_start = 1;
    area.cpu_id = 1;
    area.rseq_cs = 0x2000;
    area.flags = flags;
    area
}

#[test]
fn abi_layout_is_linux_v6_6_core() {
    assert_eq!(size_of::<RseqArea>(), 32);
    assert_eq!(align_of::<RseqArea>(), 32);
    assert_eq!(offset_of!(RseqArea, cpu_id_start), 0);
    assert_eq!(offset_of!(RseqArea, cpu_id), 4);
    assert_eq!(offset_of!(RseqArea, rseq_cs), 8);
    assert_eq!(offset_of!(RseqArea, flags), 16);
    assert_eq!(offset_of!(RseqArea, node_id), 20);
    assert_eq!(offset_of!(RseqArea, mm_cid), 24);
    assert_eq!(size_of::<RseqCriticalSection>(), 32);
    assert_eq!(align_of::<RseqCriticalSection>(), 32);
    assert_eq!(offset_of!(RseqCriticalSection, version), 0);
    assert_eq!(offset_of!(RseqCriticalSection, flags), 4);
    assert_eq!(offset_of!(RseqCriticalSection, start_ip), 8);
    assert_eq!(offset_of!(RseqCriticalSection, post_commit_offset), 16);
    assert_eq!(offset_of!(RseqCriticalSection, abort_ip), 24);
}

#[test]
fn null_registration_is_deferred_to_user_access() {
    let request = RseqRegistrationRequest::try_new(0, 32, 7).unwrap();
    assert_eq!(request.area_address(), 0);
    let mut state = RseqRegistrationState::new();
    let plan = state.prepare_register(request).unwrap();
    let registration = state.commit_register(plan);
    assert_eq!(registration.area_address(), 0);
}

#[test]
fn registration_finalize_cannot_be_staled_by_event_revision() {
    let request = RseqRegistrationRequest::new(0x1000, 32, 0x5305_3053);
    let mut state = ThreadRseq::new();
    let plan = state.prepare_register(request).unwrap();
    state.raise_events(RseqEventMask::PREEMPT).unwrap();
    let registration = state.commit_register(plan);
    assert_eq!(registration.area_address(), 0x1000);
    assert_eq!(state.pending_events(), RseqEventMask::PREEMPT);
}

#[test]
fn unregister_finalize_cannot_be_staled_by_event_revision() {
    let request = RseqRegistrationRequest::new(0x1000, 32, 9);
    let mut state = ThreadRseq::new();
    let register = state.prepare_register(request).unwrap();
    let registration = state.commit_register(register);
    assert_eq!(registration.signature(), 9);
    let plan = state.prepare_unregister(request).unwrap();
    state.raise_events(RseqEventMask::SIGNAL).unwrap();
    assert_eq!(state.commit_unregister(plan), registration);
    assert_eq!(state.lifecycle(), RegistrationLifecycle::Unregistered);
}

#[test]
fn descriptor_requires_exclusive_user_limit_and_checks_range() {
    let cs = RseqCriticalSection::new(0x1000, 0x20, 0x3000);
    assert_eq!(cs.post_commit_ip(), Ok(0x1020));
    assert_eq!(cs.signature_address(), Ok(0x2ffc));
    assert_eq!(
        RseqDescriptor::new(0x2000, cs, UserAddressLimit::new(0x2000)),
        Err(RseqError::AddressOutOfRange)
    );
    assert_eq!(
        RseqDescriptor::new(
            0x2000,
            RseqCriticalSection::new(u64::MAX - 3, 4, 0x3000),
            USER_LIMIT,
        ),
        Err(RseqError::AddressOverflow)
    );
    assert_eq!(
        RseqDescriptor::new(
            0x2000,
            RseqCriticalSection::new(0x1000, 0x200, 0x1100),
            USER_LIMIT,
        ),
        Err(RseqError::AbortInCriticalSection)
    );
    assert_eq!(
        RseqDescriptor::new(
            0x2000,
            RseqCriticalSection::from_raw(1, 0, 0x1000, 1, 0x3000),
            USER_LIMIT,
        ),
        Err(RseqError::InvalidVersion)
    );
}

#[test]
fn descriptor_pointer_check_does_not_preclassify_crossing_usercopy_as_einval() {
    let cs = RseqCriticalSection::new(0x1000, 0x20, 0x3000);
    // The pointer itself is below the exclusive limit, while the adapter's
    // 32-byte copy would cross it.  That copy must be the adapter's EFAULT
    // decision, not this pure component's EINVAL proof.
    assert!(RseqDescriptor::new(USER_LIMIT.exclusive() - 16, cs, USER_LIMIT).is_ok());
    assert_eq!(
        RseqDescriptor::new(USER_LIMIT.exclusive(), cs, USER_LIMIT),
        Err(RseqError::AddressOutOfRange)
    );
}

#[test]
fn restart_checks_signature_before_interval_then_flags() {
    let mut area = area_with_descriptor(1);
    let invalid_descriptor_flags = descriptor(1);
    assert_eq!(
        decide_restart(
            area,
            Some(invalid_descriptor_flags),
            0x0fff,
            RseqEventMask::PREEMPT,
            9,
            9,
        ),
        Ok(RestartDecision::ClearOnly)
    );

    area.flags = 0;
    let valid_descriptor = descriptor(0);
    assert_eq!(
        decide_restart(
            area,
            Some(valid_descriptor),
            0x1004,
            RseqEventMask::PREEMPT,
            9,
            10,
        ),
        Err(RseqError::RestartSignatureMismatch)
    );
    assert_eq!(
        RseqError::RestartSignatureMismatch.errno(),
        ErrnoClass::InvalidArgument
    );
    assert_eq!(
        decide_restart(
            area,
            Some(valid_descriptor),
            0x1004,
            RseqEventMask::PREEMPT,
            9,
            9,
        ),
        Ok(RestartDecision::Abort)
    );
}

#[test]
fn null_area_descriptor_path_never_reads_descriptor() {
    let area = RseqArea::initial();
    assert_eq!(
        decide_restart(area, None, 0x1004, RseqEventMask::PREEMPT, 1, 2),
        Ok(RestartDecision::NoActive)
    );
}

#[test]
fn no_event_skips_active_descriptor_copy() {
    let area = area_with_descriptor(0);
    assert_eq!(
        decide_restart(area, None, 0x0fff, RseqEventMask::EMPTY, 9, 9),
        Ok(RestartDecision::NoActive)
    );
}

#[test]
fn pending_event_still_requires_active_descriptor_copy() {
    let area = area_with_descriptor(0);
    assert_eq!(
        decide_restart(area, None, 0x1004, RseqEventMask::PREEMPT, 9, 9),
        Err(RseqError::DescriptorReadFault)
    );
}

#[test]
fn active_descriptor_outside_range_is_not_cleared_without_events() {
    let area = area_with_descriptor(0);
    assert_eq!(
        decide_restart(
            area,
            Some(descriptor(0)),
            0x0fff,
            RseqEventMask::EMPTY,
            1,
            1
        ),
        Ok(RestartDecision::NoActive)
    );
}

#[test]
fn bad_signature_is_deferred_without_events() {
    let area = area_with_descriptor(0);
    let descriptor = descriptor(0);
    assert_eq!(
        decide_restart(area, Some(descriptor), 0x0fff, RseqEventMask::EMPTY, 9, 10,),
        Ok(RestartDecision::NoActive)
    );
    assert_eq!(
        decide_restart(area, Some(descriptor), 0x1004, RseqEventMask::EMPTY, 9, 10,),
        Ok(RestartDecision::NoActive)
    );
}

#[test]
fn pending_registration_blocks_resume_until_cancelled() {
    let mut state = ThreadRseq::new();
    let request = RseqRegistrationRequest::new(0x1000, 32, 9);
    let register = state.prepare_register(request).unwrap();
    assert_eq!(
        state.begin_resume(area_with_descriptor(0), Some(descriptor(0)), 0x1004, 9, 9,),
        Err(RseqError::OperationInProgress)
    );
    state.cancel_register(register);
    assert!(!state.has_pending_operation());
}

#[test]
fn resume_gate_consumes_event_snapshot_and_new_events_survive() {
    let mut state = ThreadRseq::new();
    state.raise_events(RseqEventMask::PREEMPT).unwrap();
    let area = area_with_descriptor(0);
    let plan = state
        .begin_resume(area, Some(descriptor(0)), 0x1004, 9, 9)
        .unwrap();
    assert_eq!(plan.decision(), RestartDecision::Abort);
    state.raise_events(RseqEventMask::SIGNAL).unwrap();
    assert_eq!(state.commit_resume(plan), RestartDecision::Abort);
    assert_eq!(state.pending_events(), RseqEventMask::SIGNAL);

    let clear = state
        .begin_resume(area, Some(descriptor(0)), 0x0fff, 9, 9)
        .unwrap();
    assert_eq!(clear.decision(), RestartDecision::ClearOnly);
    assert_eq!(state.commit_resume(clear), RestartDecision::ClearOnly);
    assert!(state.pending_events().is_empty());
}

#[test]
fn no_active_gate_consumes_pending_events() {
    let mut state = ThreadRseq::new();
    state.raise_events(RseqEventMask::SIGNAL).unwrap();

    let plan = state
        .begin_resume(RseqArea::initial(), None, 0x1004, 9, 10)
        .unwrap();
    assert_eq!(plan.decision(), RestartDecision::NoActive);
    assert_eq!(plan.events(), RseqEventMask::SIGNAL);
    assert!(state.pending_events().is_empty());
    assert_eq!(state.commit_resume(plan), RestartDecision::NoActive);
    assert!(state.pending_events().is_empty());
}

#[test]
fn clear_only_consumes_old_event_before_a_new_critical_section() {
    let mut state = ThreadRseq::new();
    state.raise_events(RseqEventMask::PREEMPT).unwrap();

    let old_area = area_with_descriptor(0);
    let clear = state
        .begin_resume(old_area, Some(descriptor(0)), 0x0fff, 9, 9)
        .unwrap();
    assert_eq!(clear.decision(), RestartDecision::ClearOnly);
    assert_eq!(clear.events(), RseqEventMask::PREEMPT);
    state.commit_resume(clear);

    // A newly published critical section must not inherit the old event and
    // abort merely because its IP is in range.
    let new_area = area_with_descriptor(0);
    let next = state
        .begin_resume(new_area, Some(descriptor(0)), 0x1004, 9, 9)
        .unwrap();
    assert_eq!(next.decision(), RestartDecision::NoActive);
    assert!(next.events().is_empty());
    state.commit_resume(next);
}

#[test]
fn events_published_during_no_active_and_clear_only_gates_survive() {
    let mut state = ThreadRseq::new();
    state.raise_events(RseqEventMask::PREEMPT).unwrap();

    let no_active = state
        .begin_resume(RseqArea::initial(), None, 0x1004, 9, 9)
        .unwrap();
    assert_eq!(no_active.decision(), RestartDecision::NoActive);
    assert_eq!(no_active.events(), RseqEventMask::PREEMPT);
    state.raise_events(RseqEventMask::SIGNAL).unwrap();
    state.commit_resume(no_active);
    assert_eq!(state.pending_events(), RseqEventMask::SIGNAL);

    let clear_only = state
        .begin_resume(area_with_descriptor(0), Some(descriptor(0)), 0x0fff, 9, 9)
        .unwrap();
    assert_eq!(clear_only.decision(), RestartDecision::ClearOnly);
    assert_eq!(clear_only.events(), RseqEventMask::SIGNAL);
    state.raise_events(RseqEventMask::MIGRATE).unwrap();
    state.commit_resume(clear_only);
    assert_eq!(state.pending_events(), RseqEventMask::MIGRATE);
}

#[test]
fn failed_non_abort_gate_restores_snapshot_and_keeps_new_events() {
    let mut state = ThreadRseq::new();
    state.raise_events(RseqEventMask::PREEMPT).unwrap();

    let plan = state
        .begin_resume(RseqArea::initial(), None, 0x1004, 9, 9)
        .unwrap();
    state.raise_events(RseqEventMask::SIGNAL).unwrap();
    state.cancel_resume(plan);
    assert_eq!(
        state.pending_events(),
        RseqEventMask::PREEMPT.union(RseqEventMask::SIGNAL)
    );
}

#[test]
fn failed_abort_side_effect_can_cancel_without_losing_events() {
    let mut state = ThreadRseq::new();
    state.raise_events(RseqEventMask::PREEMPT).unwrap();
    let plan = state
        .begin_resume(area_with_descriptor(0), Some(descriptor(0)), 0x1004, 9, 9)
        .unwrap();
    state.cancel_resume(plan);
    assert_eq!(state.pending_events(), RseqEventMask::PREEMPT);
}

#[test]
fn fork_modes_match_linux_and_preserve_generation_identity() {
    let request = RseqRegistrationRequest::new(0x2000, 32, 11);
    let mut parent = ThreadRseq::new();
    let register = parent.prepare_register(request).unwrap();
    parent.commit_register(register);
    parent.raise_events(RseqEventMask::SIGNAL).unwrap();
    let parent_epoch = parent.epoch().get();
    let parent_revision = parent.revision().get();

    let clone_plan = parent.prepare_fork(ForkMode::CloneVm).unwrap();
    assert_eq!(
        parent.raise_events(RseqEventMask::MIGRATE),
        Err(RseqError::OperationInProgress)
    );
    let clone = parent.commit_fork(clone_plan);
    assert_eq!(clone.lifecycle(), RegistrationLifecycle::Unregistered);
    assert!(clone.pending_events().is_empty());
    assert!(clone.epoch().get() > parent_epoch);
    assert!(clone.revision().get() > parent_revision);

    parent.raise_events(RseqEventMask::MIGRATE).unwrap();
    let fork_plan = parent.prepare_fork(ForkMode::PrivateVm).unwrap();
    assert_eq!(parent.prepare_exec(), Err(RseqError::OperationInProgress));
    let fork = parent.commit_fork(fork_plan);
    assert_eq!(
        fork.registration().map(RseqRegistration::request),
        parent.registration().map(RseqRegistration::request)
    );
    assert_eq!(fork.pending_events(), parent.pending_events());
    assert!(fork.epoch().get() > parent_epoch);
    assert!(fork.revision().get() > parent_revision);
}

#[test]
fn failed_fork_requires_cancel_and_keeps_reserved_identity_consumed() {
    let mut parent = ThreadRseq::new();
    let before_revision = parent.revision().get();
    let before_epoch = parent.epoch().get();
    let plan = parent.prepare_fork(ForkMode::PrivateVm).unwrap();
    assert_eq!(parent.revision().get(), plan.revision().get());
    assert!(parent.epoch().get() > before_epoch);
    parent.cancel_fork(plan);
    assert!(!parent.has_pending_operation());
    parent.raise_events(RseqEventMask::PREEMPT).unwrap();
    assert!(parent.revision().get() > before_revision);
    assert!(parent.epoch().get() > before_epoch);
}

#[test]
fn exec_clears_only_after_successful_commit() {
    let request = RseqRegistrationRequest::new(0x2000, 32, 11);
    let mut state = ThreadRseq::new();
    let register = state.prepare_register(request).unwrap();
    state.commit_register(register);
    state.raise_events(RseqEventMask::SIGNAL).unwrap();
    let plan = state.prepare_exec().unwrap();
    state.cancel_exec(plan);
    assert_eq!(state.lifecycle(), RegistrationLifecycle::Registered);
    assert_eq!(state.pending_events(), RseqEventMask::SIGNAL);

    let plan = state.prepare_exec().unwrap();
    assert!(state.on_exec_success(plan).is_some());
    assert_eq!(state.lifecycle(), RegistrationLifecycle::Unregistered);
    assert!(state.pending_events().is_empty());
}

#[test]
fn underflow_is_adapter_fault_and_restart_range_errors_are_einval() {
    let descriptor = RseqDescriptor::new(
        0x2000,
        RseqCriticalSection::new(0x1000, 0x20, 0),
        USER_LIMIT,
    )
    .unwrap();
    assert_eq!(
        decide_restart(
            area_with_descriptor(0),
            Some(descriptor),
            0x1004,
            RseqEventMask::PREEMPT,
            9,
            9,
        ),
        Err(RseqError::SignatureAddressUnderflow)
    );
    assert_eq!(
        RseqError::SignatureAddressUnderflow.errno(),
        ErrnoClass::Fault
    );
    assert_eq!(
        RseqError::AddressOverflow.errno(),
        ErrnoClass::InvalidArgument
    );
}

#[test]
fn signature_address_fault_is_deferred_without_events() {
    let descriptor = RseqDescriptor::new(
        0x2000,
        RseqCriticalSection::new(0x1000, 0x20, 0),
        USER_LIMIT,
    )
    .unwrap();
    assert_eq!(
        decide_restart(
            area_with_descriptor(0),
            Some(descriptor),
            0x0fff,
            RseqEventMask::EMPTY,
            9,
            9,
        ),
        Ok(RestartDecision::NoActive)
    );
}

#[test]
fn cpu_sentinels_remain_linux_compatible() {
    let mut area = RseqArea::initial();
    assert_eq!(area.cpu_check(), CpuCheck::Uninitialized);
    area.cpu_id_start = 3;
    area.cpu_id = 4;
    assert_eq!(area.cpu_check(), CpuCheck::Mismatch);
    assert_eq!(area.check_cpu(3), Err(RseqError::CpuIdMismatch));
    area.cpu_id = 3;
    assert_eq!(area.cpu_check(), CpuCheck::Match);
    assert_eq!(area.check_cpu(3), Ok(()));
}

#[test]
fn linux_7_2_extended_registration_keeps_legacy_alignment() {
    assert!(RseqRegistrationRequest::new(0x1020, 32, 0).validate().is_ok());
    assert!(RseqRegistrationRequest::new(0x1020, 33, 0).validate().is_err());
    for length in [33, 64, 4096] {
        assert!(RseqRegistrationRequest::new(0x1000, length, 0).validate().is_ok());
    }
    assert!(RseqRegistrationRequest::new(0x1000, 31, 0).validate().is_err());
    assert_eq!(RseqRegistrationOperation::from_flags(2), Ok(RseqRegistrationOperation::Register));
    assert!(RseqRegistrationOperation::from_flags(3).is_err());
    assert!(RseqRegistrationOperation::from_flags(4).is_err());
}
