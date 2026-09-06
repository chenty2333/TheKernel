use thekernel_linux_mm::*;

const PAGE: usize = 4096;

fn access(read: bool, write: bool, execute: bool) -> MappingAccess {
    MappingAccess::new(read, write, execute)
}

fn snapshot(
    mapping: u64,
    generation: u64,
    start: usize,
    length: usize,
    permissions: MappingAccess,
) -> MappingSnapshot {
    MappingSnapshot::from_raw(
        1,
        mapping,
        generation,
        start,
        length,
        PAGE,
        permissions.bits(),
        MappingKind::AnonymousPrivate,
        true,
        false,
    )
    .unwrap()
}

fn request(
    start: usize,
    length: usize,
    access: PinAccess,
    duration: PinDuration,
    owner: u64,
) -> PinRequest {
    PinRequest::from_raw(start, length, access, duration, PinUse::BlockIo, owner).unwrap()
}

fn registry<const O: usize, const T: usize>(
    quota: PinQuota,
    first_token: u64,
) -> PinRegistry<O, T> {
    let mut registry = PinRegistry::new(PAGE, quota, first_token).unwrap();
    registry
        .configure_owner(PinOwner::new(7).unwrap(), quota)
        .unwrap();
    registry
}

fn validate_single<const O: usize, const T: usize>(
    registry: &mut PinRegistry<O, T>,
    reservation: PinReservation,
    mapping: MappingSnapshot,
    covered: PageRange,
) {
    registry
        .revalidate_next(reservation, mapping.expected(), mapping, covered)
        .unwrap();
}

#[test]
fn public_values_keep_invariants_private_and_checked() {
    assert_eq!(AddressSpaceId::new(0), Err(MmError::InvalidIdentity));
    assert_eq!(MappingId::new(0), Err(MmError::InvalidIdentity));
    assert_eq!(PinOwner::new(0), Err(MmError::InvalidIdentity));
    assert_eq!(FaultHandlerId::new(0), Err(MmError::InvalidIdentity));
    assert_eq!(UserRange::new(usize::MAX - 1, 4), Err(MmError::Overflow));
    assert_eq!(PageRange::new(0x1001, PAGE, PAGE), Err(MmError::Unaligned));
    assert_eq!(MappingAccess::from_bits(0x80), Err(MmError::InvalidAccess));
    assert_eq!(
        FaultCapacity::new(1, u32::MAX, 1),
        Err(MmError::UnboundedLimit)
    );
    assert!(matches!(
        PinBudget::<1>::new(PAGE, PinQuota::new(1, PAGE as u64, 1), 0),
        Err(MmError::InvalidIdentity)
    ));
}

#[test]
fn one_system_budget_bounds_independent_address_space_registries() {
    let quota = PinQuota::new(2, (2 * PAGE) as u64, 2);
    let mut budget = PinBudget::<3>::new(PAGE, quota, 1).unwrap();
    let mut first_registry = registry::<1, 1>(quota, 1);
    let mut second_registry = registry::<1, 1>(quota, 1);
    let first_request = request(0x1000, PAGE, PinAccess::Read, PinDuration::AsyncIo, 7);
    let second_request = request(0x8000, PAGE, PinAccess::Write, PinDuration::AsyncIo, 7);
    let first = budget.reserve(first_request).unwrap();
    let first_local = first_registry
        .reserve(first_request, AddressSpaceId::new(1).unwrap())
        .unwrap();
    let second = budget.reserve(second_request).unwrap();
    let second_local = second_registry
        .reserve(second_request, AddressSpaceId::new(2).unwrap())
        .unwrap();

    assert_eq!(budget.live_charges(), 2);
    assert_eq!(budget.accounting().pages(), 2);
    assert_eq!(budget.accounting().tokens(), 2);
    assert_eq!(
        budget.reserve(request(
            0x20_000,
            PAGE,
            PinAccess::Read,
            PinDuration::Synchronous,
            9,
        )),
        Err(MmError::QuotaExceeded)
    );
    assert_eq!(budget.accounting().pages(), 2);
    assert_eq!(budget.live_charges(), 2);

    first_registry.cancel_reservation(first_local).unwrap();
    budget.release(first).unwrap();
    assert_eq!(budget.accounting().pages(), 1);
    second_registry.cancel_reservation(second_local).unwrap();
    budget.release(second).unwrap();
    assert_eq!(budget.accounting(), PinAccounting::default());
}

#[test]
fn system_budget_rejects_foreign_and_released_charges_without_mutation() {
    let quota = PinQuota::new(1, PAGE as u64, 1);
    let mut first = PinBudget::<1>::new(PAGE, quota, 7).unwrap();
    let mut second = PinBudget::<1>::new(PAGE, quota, 7).unwrap();
    let charge = first
        .reserve(request(
            0x1000,
            PAGE,
            PinAccess::Read,
            PinDuration::AsyncIo,
            7,
        ))
        .unwrap();

    assert_eq!(
        first.reserve(request(
            0x2000,
            PAGE,
            PinAccess::Read,
            PinDuration::AsyncIo,
            7,
        )),
        Err(MmError::CapacityExceeded)
    );
    assert_eq!(second.release(charge), Err(MmError::BudgetMismatch));
    assert_eq!(second.accounting(), PinAccounting::default());
    first.release(charge).unwrap();
    assert_eq!(first.release(charge), Err(MmError::UnknownToken));
    assert_eq!(first.accounting(), PinAccounting::default());
}

#[test]
fn one_pin_can_revalidate_multiple_contiguous_mappings() {
    let quota = PinQuota::new(16, (16 * PAGE) as u64, 8);
    let mut registry = registry::<2, 8>(quota, 1);
    let address_space = AddressSpaceId::new(1).unwrap();
    let first = snapshot(10, 1, 0x1000, 0x2000, access(true, true, false));
    let second = snapshot(11, 4, 0x3000, 0x2000, access(true, true, false));
    let reservation = registry
        .reserve(
            request(0x1800, 0x2800, PinAccess::Read, PinDuration::AsyncIo, 7),
            address_space,
        )
        .unwrap();

    registry
        .revalidate_next(
            reservation,
            first.expected(),
            first,
            PageRange::new(0x1000, 0x2000, PAGE).unwrap(),
        )
        .unwrap();
    registry
        .revalidate_next(
            reservation,
            second.expected(),
            second,
            PageRange::new(0x3000, PAGE, PAGE).unwrap(),
        )
        .unwrap();
    let token = registry.commit(reservation).unwrap();
    let view = registry.view(token).unwrap();
    assert_eq!(view.token(), token);
    assert_eq!(view.request().access(), PinAccess::Read);
    assert_eq!(view.range(), PageRange::new(0x1000, 0x3000, PAGE).unwrap());
    assert_eq!(view.snapshot().address_space(), address_space);
    assert_eq!(view.snapshot().range(), view.range());
    assert_eq!(view.validated_mappings(), 2);
    assert!(view.is_active());
    assert_eq!(registry.active_count(), 1);
    assert_eq!(registry.global_accounting().pages(), 3);
    assert_eq!(registry.global_accounting().bytes(), (3 * PAGE) as u64);
}

#[test]
fn reservation_fences_mutation_across_bounded_revalidation_windows() {
    let quota = PinQuota::new(16, (16 * PAGE) as u64, 8);
    let mut registry = registry::<1, 8>(quota, 1);
    let mapping = snapshot(10, 3, 0x1000, 0x8000, access(true, true, false));
    let reservation = registry
        .reserve(
            request(0x1000, 0x4000, PinAccess::Read, PinDuration::AsyncIo, 7),
            mapping.address_space(),
        )
        .unwrap();
    let overlap =
        InvalidationRange::from_raw(mapping, 0x2000, PAGE, InvalidationReason::Protect).unwrap();
    let disjoint =
        InvalidationRange::from_raw(mapping, 0x8000, PAGE, InvalidationReason::FileInvalidation)
            .unwrap();

    // Reservation publication itself closes the gap before the first window.
    assert_eq!(
        registry.admit_mutation(overlap),
        Err(MmError::MappingPinned)
    );
    assert_eq!(
        registry.first_mutation_blocker(overlap).unwrap().token(),
        reservation.token()
    );
    assert!(registry.admit_mutation(disjoint).is_ok());

    registry
        .revalidate_next(
            reservation,
            mapping.expected(),
            mapping,
            PageRange::new(0x1000, 0x2000, PAGE).unwrap(),
        )
        .unwrap();
    // A consumer may release and reacquire its publication lock here. The
    // unvalidated suffix and validated prefix stay protected by one record.
    assert_eq!(
        registry.admit_mutation(overlap),
        Err(MmError::MappingPinned)
    );
    assert_eq!(
        registry.commit(reservation),
        Err(MmError::IncompleteRevalidation)
    );
    assert_eq!(registry.reserved_count(), 1);

    registry
        .revalidate_next(
            reservation,
            mapping.expected(),
            mapping,
            PageRange::new(0x3000, 0x2000, PAGE).unwrap(),
        )
        .unwrap();
    let token = registry.commit(reservation).unwrap();
    assert_eq!(registry.view(token).unwrap().validated_mappings(), 2);
    assert_eq!(
        registry.admit_mutation(overlap),
        Err(MmError::MappingPinned)
    );
    registry.release(token).unwrap();
    assert!(registry.admit_mutation(overlap).is_ok());
}

#[test]
fn active_long_term_pin_transfers_mutation_safety_to_lower_owner() {
    let quota = PinQuota::new(8, (8 * PAGE) as u64, 4);
    let mut registry = registry::<1, 4>(quota, 1);
    let mapping = snapshot(10, 3, 0x1000, 0x4000, access(true, true, false));
    let reservation = registry
        .reserve(
            request(0x1000, 0x2000, PinAccess::Write, PinDuration::LongTerm, 7),
            mapping.address_space(),
        )
        .unwrap();
    let overlap =
        InvalidationRange::from_raw(mapping, 0x2000, PAGE, InvalidationReason::Unmap).unwrap();

    assert_eq!(
        registry.admit_mutation(overlap),
        Err(MmError::MappingPinned)
    );
    validate_single(
        &mut registry,
        reservation,
        mapping,
        PageRange::new(0x1000, 0x2000, PAGE).unwrap(),
    );
    let token = registry.commit(reservation).unwrap();

    assert!(registry.first_mutation_blocker(overlap).is_none());
    assert!(registry.admit_mutation(overlap).is_ok());
    assert_eq!(
        registry.view(token).unwrap().request().duration(),
        PinDuration::LongTerm
    );
    assert_eq!(registry.global_accounting().pages(), 2);
    registry.release(token).unwrap();
    assert_eq!(registry.global_accounting(), PinAccounting::default());
}

#[test]
fn clone_blocker_tracks_mapping_fences_not_long_term_ownership() {
    let quota = PinQuota::new(8, (8 * PAGE) as u64, 4);
    let mapping = snapshot(10, 3, 0x1000, 0x4000, access(true, true, false));
    let covered = PageRange::new(0x1000, PAGE, PAGE).unwrap();

    let mut long_term = registry::<1, 4>(quota, 1);
    let reservation = long_term
        .reserve(
            request(0x1000, PAGE, PinAccess::Write, PinDuration::LongTerm, 7),
            mapping.address_space(),
        )
        .unwrap();
    assert!(long_term.has_clone_blocker());
    validate_single(&mut long_term, reservation, mapping, covered);
    let token = long_term.commit(reservation).unwrap();
    assert!(!long_term.has_clone_blocker());
    long_term.release(token).unwrap();

    let mut async_io = registry::<1, 4>(quota, 1);
    let reservation = async_io
        .reserve(
            request(0x1000, PAGE, PinAccess::Read, PinDuration::AsyncIo, 7),
            mapping.address_space(),
        )
        .unwrap();
    validate_single(&mut async_io, reservation, mapping, covered);
    let token = async_io.commit(reservation).unwrap();
    assert!(async_io.has_clone_blocker());
    async_io.release(token).unwrap();
}

#[test]
fn read_pins_may_overlap_but_short_lived_write_overlap_is_rejected() {
    let quota = PinQuota::new(16, (16 * PAGE) as u64, 8);
    let mut registry = registry::<2, 8>(quota, 1);
    let asid = AddressSpaceId::new(1).unwrap();
    let mapping = snapshot(10, 1, 0x1000, 0x4000, access(true, true, false));
    let covered = PageRange::new(0x1000, 0x2000, PAGE).unwrap();

    let first = registry
        .reserve(
            request(0x1000, 0x2000, PinAccess::Read, PinDuration::Synchronous, 7),
            asid,
        )
        .unwrap();
    validate_single(&mut registry, first, mapping, covered);
    let first = registry.commit(first).unwrap();

    let second = registry
        .reserve(
            request(0x1800, PAGE, PinAccess::Read, PinDuration::AsyncIo, 7),
            asid,
        )
        .unwrap();
    validate_single(&mut registry, second, mapping, covered);
    let second = registry.commit(second).unwrap();

    assert_eq!(
        registry.reserve(
            request(0x2000, PAGE, PinAccess::Write, PinDuration::AsyncIo, 7),
            asid,
        ),
        Err(MmError::PinOverlap)
    );
    registry.release(first).unwrap();
    registry.release(second).unwrap();
}

#[test]
fn published_long_term_owner_does_not_block_a_reused_virtual_range() {
    let quota = PinQuota::new(8, (8 * PAGE) as u64, 4);
    let mut registry = registry::<1, 4>(quota, 1);
    let asid = AddressSpaceId::new(1).unwrap();
    let old_mapping = snapshot(10, 1, 0x1000, PAGE, access(true, true, false));
    let covered = PageRange::new(0x1000, PAGE, PAGE).unwrap();

    let old = registry
        .reserve(
            request(0x1000, PAGE, PinAccess::Write, PinDuration::LongTerm, 7),
            asid,
        )
        .unwrap();
    validate_single(&mut registry, old, old_mapping, covered);
    let old = registry.commit(old).unwrap();

    // The old VMA may be unmapped while its published long-term consumer
    // retains exact lower ownership. Reusing that VA creates a distinct
    // mapping identity, whose own pin must not alias the old virtual fence.
    let new_mapping = snapshot(11, 1, 0x1000, PAGE, access(true, true, false));
    let new = registry
        .reserve(
            request(0x1000, PAGE, PinAccess::Write, PinDuration::LongTerm, 7),
            asid,
        )
        .unwrap();
    validate_single(&mut registry, new, new_mapping, covered);
    let new = registry.commit(new).unwrap();

    registry.release(old).unwrap();
    registry.release(new).unwrap();
}

#[test]
fn quota_failure_is_mutation_free_and_page_covering_is_accounted() {
    let quota = PinQuota::new(1, PAGE as u64, 1);
    let mut registry = registry::<1, 2>(quota, 1);
    assert_eq!(
        registry.reserve(
            request(0x1fff, 2, PinAccess::Read, PinDuration::Synchronous, 7),
            AddressSpaceId::new(1).unwrap(),
        ),
        Err(MmError::QuotaExceeded)
    );
    assert_eq!(registry.global_accounting(), PinAccounting::default());
    assert_eq!(registry.progress().total(), 0);
}

#[test]
fn stale_revalidation_rolls_back_reservation_and_both_quotas() {
    let quota = PinQuota::new(4, (4 * PAGE) as u64, 2);
    let mut registry = registry::<1, 2>(quota, 10);
    let old = snapshot(10, 1, 0x1000, PAGE, access(true, true, false));
    let replacement = snapshot(10, 2, 0x1000, PAGE, access(true, true, false));
    let reservation = registry
        .reserve(
            request(0x1000, PAGE, PinAccess::Write, PinDuration::AsyncIo, 7),
            AddressSpaceId::new(1).unwrap(),
        )
        .unwrap();
    assert_eq!(registry.global_accounting().tokens(), 1);
    assert_eq!(
        registry.revalidate_next(reservation, old.expected(), replacement, old.range()),
        Err(MmError::StaleGeneration)
    );
    assert_eq!(registry.global_accounting(), PinAccounting::default());
    assert_eq!(
        registry
            .owner_accounting(PinOwner::new(7).unwrap())
            .unwrap(),
        PinAccounting::default()
    );
    assert_eq!(
        registry.view(reservation.token()),
        Err(MmError::UnknownToken)
    );
}

#[test]
fn mapping_mutation_is_blocked_only_for_overlapping_live_pin_range() {
    let quota = PinQuota::new(8, (8 * PAGE) as u64, 4);
    let mut registry = registry::<1, 4>(quota, 1);
    let mapping = snapshot(10, 1, 0x1000, 0x4000, access(true, true, false));
    let reservation = registry
        .reserve(
            request(0x1000, PAGE, PinAccess::Read, PinDuration::AsyncIo, 7),
            mapping.address_space(),
        )
        .unwrap();
    validate_single(
        &mut registry,
        reservation,
        mapping,
        PageRange::new(0x1000, PAGE, PAGE).unwrap(),
    );
    let token = registry.commit(reservation).unwrap();

    let overlap =
        InvalidationRange::from_raw(mapping, 0x1000, PAGE, InvalidationReason::Unmap).unwrap();
    let disjoint =
        InvalidationRange::from_raw(mapping, 0x3000, PAGE, InvalidationReason::Protect).unwrap();
    assert_eq!(
        registry.admit_mutation(overlap),
        Err(MmError::MappingPinned)
    );
    let blocker = registry.first_mutation_blocker(overlap).unwrap();
    assert_eq!(blocker.token(), token);
    assert_eq!(blocker.request().owner(), PinOwner::new(7).unwrap());
    assert!(registry.admit_mutation(disjoint).is_ok());
    registry.release(token).unwrap();
    assert!(registry.admit_mutation(overlap).is_ok());
}

#[test]
fn close_and_forced_teardown_have_explicit_drain_states() {
    let quota = PinQuota::new(8, (8 * PAGE) as u64, 4);
    let mut closing = registry::<1, 4>(quota, 1);
    let mapping = snapshot(10, 1, 0x1000, PAGE, access(true, true, false));
    let reservation = closing
        .reserve(
            request(0x1000, PAGE, PinAccess::Read, PinDuration::AsyncIo, 7),
            mapping.address_space(),
        )
        .unwrap();
    validate_single(&mut closing, reservation, mapping, mapping.range());
    let token = closing.commit(reservation).unwrap();
    assert_eq!(closing.begin_close().unwrap().active(), 1);
    assert_eq!(
        closing.reserve(
            request(0x2000, PAGE, PinAccess::Read, PinDuration::AsyncIo, 7),
            mapping.address_space(),
        ),
        Err(MmError::Closing)
    );
    assert_eq!(closing.finish_teardown(), Err(MmError::Busy));
    closing.release(token).unwrap();
    closing.finish_teardown().unwrap();
    assert_eq!(closing.state(), PinRegistryState::Closed);

    let mut teardown = registry::<1, 4>(quota, 20);
    let pending = teardown
        .reserve(
            request(0x1000, PAGE, PinAccess::Read, PinDuration::AsyncIo, 7),
            mapping.address_space(),
        )
        .unwrap();
    assert!(!teardown.view(pending.token()).unwrap().is_active());
    let report = teardown.begin_teardown().unwrap();
    assert_eq!(report.cancelled_reservations(), 1);
    assert_eq!(report.active_remaining(), 0);
    assert_eq!(teardown.global_accounting(), PinAccounting::default());
    teardown.finish_teardown().unwrap();
}

#[test]
fn pin_token_sequence_exhausts_instead_of_wrapping() {
    let quota = PinQuota::new(2, (2 * PAGE) as u64, 1);
    let mut registry = registry::<1, 1>(quota, u64::MAX);
    let mapping = snapshot(10, 1, 0x1000, PAGE, access(true, false, false));
    let reservation = registry
        .reserve(
            request(0x1000, PAGE, PinAccess::Read, PinDuration::Synchronous, 7),
            mapping.address_space(),
        )
        .unwrap();
    validate_single(&mut registry, reservation, mapping, mapping.range());
    let token = registry.commit(reservation).unwrap();
    assert_eq!(token.get(), u64::MAX);
    registry.release(token).unwrap();
    assert_eq!(
        registry.reserve(
            request(0x1000, PAGE, PinAccess::Read, PinDuration::Synchronous, 7),
            mapping.address_space(),
        ),
        Err(MmError::IdExhausted)
    );
    assert_eq!(registry.global_accounting(), PinAccounting::default());
}

#[test]
fn fault_admission_is_bounded_and_completion_rejects_stale_generation() {
    let mapping = snapshot(20, 7, 0x8000, 0x2000, access(true, true, false));
    let key = FaultKey::from_address(mapping, 0x9000, FaultAccess::Write).unwrap();
    let request = FaultRequest::new(
        key,
        FaultHandlerId::new(5).unwrap(),
        FaultType::WriteProtect,
    );
    let capacity = FaultCapacity::new(2, 2, 4).unwrap();
    let permit = FaultAdmission::check(
        request,
        mapping,
        FaultAdmissionContext::new_request(capacity, FaultLoad::new(1, 1, 3)),
        FaultLifecycleState::Open,
    )
    .unwrap();
    assert_eq!(permit.request(), request);
    assert_eq!(permit.kind(), FaultAdmissionKind::NewRequest);
    assert!(permit.kind().consumes_request_credit());
    for load in [
        FaultLoad::new(2, 1, 3),
        FaultLoad::new(1, 2, 3),
        FaultLoad::new(1, 1, 4),
    ] {
        assert_eq!(
            FaultAdmission::check(
                request,
                mapping,
                FaultAdmissionContext::new_request(capacity, load),
                FaultLifecycleState::Open,
            ),
            Err(MmError::QuotaExceeded)
        );
    }

    let completion = validate_fault_completion(
        mapping_request(request),
        mapping,
        FaultDisposition::Continue,
    )
    .unwrap();
    assert_eq!(completion.disposition(), FaultDisposition::Continue);
    let replacement = snapshot(20, 8, 0x8000, 0x2000, access(true, true, false));
    assert_eq!(
        validate_fault_completion(request, replacement, FaultDisposition::Supply),
        Err(MmError::StaleGeneration)
    );
}

fn mapping_request(request: FaultRequest) -> FaultRequest {
    request
}

fn uffd_snapshot(
    address_space: u64,
    mapping: u64,
    generation: u64,
    start: usize,
    length: usize,
    kind: MappingKind,
) -> MappingSnapshot {
    MappingSnapshot::from_raw(
        address_space,
        mapping,
        generation,
        start,
        length,
        PAGE,
        access(true, true, false).bits(),
        kind,
        true,
        false,
    )
    .unwrap()
}

fn initialized_uffd_api() -> UffdApiState {
    let mut api = UffdApiState::new();
    let negotiation = api.prepare_raw(UFFD_API, 0).unwrap();
    assert_eq!(api.lifecycle(), UffdApiLifecycle::Created);
    let response = api.commit(negotiation).unwrap();
    assert_eq!(response.api(), UFFD_API);
    assert_eq!(response.features(), UffdFeatures::AVAILABLE);
    assert_eq!(response.ioctls(), UffdIoctls::API_PROFILE);
    api
}

fn uffd_registration_request(
    handler: u64,
    mapping: MappingSnapshot,
    range: PageRange,
) -> UffdRegistrationRequest {
    UffdRegistrationRequest::new(
        FaultHandlerId::new(handler).unwrap(),
        mapping,
        range,
        UffdRegisterMode::MISSING,
    )
    .unwrap()
}

fn collect_uffd_register_delta<const CAPACITY: usize>(
    table: &UffdRegistrationTable<CAPACITY>,
    api: &UffdApiState,
    intent: UffdRegistrationIntent,
    vmas: &[MappingSnapshot],
) -> (
    UffdRegistrationDeltaPlan,
    Vec<UffdRegistrationId>,
    Vec<UffdRegistrationRequest>,
) {
    let plan = table.preflight_register_delta(api, intent, vmas).unwrap();
    let mut removed = Vec::with_capacity(plan.removed());
    let mut replacements = Vec::with_capacity(plan.replacements());
    table
        .replay_register_delta(
            plan,
            intent,
            vmas,
            |id| removed.push(id),
            |request| replacements.push(request),
        )
        .unwrap();
    assert_eq!(removed.len(), plan.removed());
    assert_eq!(replacements.len(), plan.replacements());
    (plan, removed, replacements)
}

fn commit_uffd_register_delta<const CAPACITY: usize>(
    table: &mut UffdRegistrationTable<CAPACITY>,
    api: &UffdApiState,
    intent: UffdRegistrationIntent,
    vmas: &[MappingSnapshot],
) {
    let (plan, removed, replacements) = collect_uffd_register_delta(table, api, intent, vmas);
    if plan.is_noop() {
        return;
    }
    if removed.is_empty() {
        table.register_batch(api, &replacements, |_| {}).unwrap();
    } else {
        table
            .replace_batch(api, &removed, &replacements, |_| {})
            .unwrap();
    }
}

#[test]
fn fault_keys_keep_absolute_page_identity_across_surviving_range_splits() {
    let original = uffd_snapshot(1, 30, 7, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let key = FaultKey::from_address(original, 0x3001, FaultAccess::Write).unwrap();
    assert_eq!(key.page_address().get(), 0x3000);

    let shifted_survivor = uffd_snapshot(1, 30, 7, 0x2000, 3 * PAGE, MappingKind::AnonymousPrivate);
    key.revalidate_admission(shifted_survivor).unwrap();
    key.revalidate_completion(shifted_survivor).unwrap();
    assert_eq!(
        key,
        FaultKey::from_address(shifted_survivor, 0x3001, FaultAccess::Write).unwrap()
    );
    assert_ne!(
        key,
        FaultKey::from_address(shifted_survivor, 0x4001, FaultAccess::Write).unwrap()
    );
    let split_survivor = uffd_snapshot(1, 30, 7, 0x3000, 2 * PAGE, MappingKind::AnonymousPrivate);
    key.revalidate_completion(split_survivor).unwrap();

    let removed_page = uffd_snapshot(1, 30, 7, 0x4000, PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        key.revalidate_completion(removed_page),
        Err(MmError::RangeNotMapped)
    );
}

#[test]
fn fault_key_revalidation_rejects_authority_access_and_alignment_changes() {
    let original = uffd_snapshot(1, 30, 7, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let key = FaultKey::from_address(original, 0x3001, FaultAccess::Write).unwrap();
    let different_access_key = FaultKey::from_address(original, 0x3001, FaultAccess::Read).unwrap();
    assert_ne!(key, different_access_key);

    let different_address_space =
        uffd_snapshot(2, 30, 7, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        key.revalidate_completion(different_address_space),
        Err(MmError::StaleGeneration)
    );
    let replacement = uffd_snapshot(1, 31, 7, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        key.revalidate_completion(replacement),
        Err(MmError::StaleGeneration)
    );
    let new_epoch = uffd_snapshot(1, 30, 8, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        key.revalidate_completion(new_epoch),
        Err(MmError::StaleGeneration)
    );

    let read_only = MappingSnapshot::from_raw(
        1,
        30,
        7,
        0x1000,
        4 * PAGE,
        PAGE,
        access(true, false, false).bits(),
        MappingKind::AnonymousPrivate,
        true,
        false,
    )
    .unwrap();
    assert_eq!(
        key.revalidate_admission(read_only),
        Err(MmError::AccessDenied)
    );
    key.revalidate_completion(read_only).unwrap();

    let larger_pages = MappingSnapshot::from_raw(
        1,
        30,
        7,
        0,
        8 * PAGE,
        2 * PAGE,
        access(true, true, false).bits(),
        MappingKind::AnonymousPrivate,
        true,
        false,
    )
    .unwrap();
    assert_eq!(
        key.revalidate_completion(larger_pages),
        Err(MmError::Unaligned)
    );
}

#[test]
fn userfaultfd_refreshed_fragments_preserve_the_source_fault_epoch() {
    let api = initialized_uffd_api();
    let source = uffd_snapshot(7, 70, 11, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<2>::new(1).unwrap();
    let registration = table
        .register(&api, uffd_registration_request(9, source, source.range()))
        .unwrap();
    assert_eq!(
        table
            .epoch_for_mapping(source.address_space(), source.mapping())
            .unwrap(),
        Some(source.generation())
    );

    let survivor = MappingSnapshot::from_raw(
        7,
        70,
        99,
        0x3000,
        2 * PAGE,
        PAGE,
        access(false, false, false).bits(),
        MappingKind::AnonymousPrivate,
        true,
        false,
    )
    .unwrap();
    let projected = survivor.with_generation(registration.generation());
    assert_eq!(projected.address_space(), survivor.address_space());
    assert_eq!(projected.mapping(), survivor.mapping());
    assert_eq!(projected.generation(), registration.generation());
    assert_eq!(projected.range(), survivor.range());
    assert_eq!(projected.access(), survivor.access());
    assert_eq!(projected.kind(), survivor.kind());
    assert_eq!(
        projected.long_term_pinnable(),
        survivor.long_term_pinnable()
    );
    assert_eq!(
        projected.writable_file_pin_supported(),
        survivor.writable_file_pin_supported()
    );
    let retained_range = PageRange::new(0x3000, PAGE, PAGE).unwrap();
    let retained = registration
        .refreshed_fragment(survivor, retained_range)
        .unwrap();
    assert_eq!(retained.handler(), registration.handler());
    assert_eq!(retained.address_space(), registration.address_space());
    assert_eq!(retained.mapping(), registration.mapping());
    assert_eq!(retained.generation(), registration.generation());
    assert_ne!(retained.generation(), survivor.generation());
    assert_eq!(retained.range(), retained_range);
    assert_eq!(retained.mode(), registration.mode());

    let replacement = UffdRegistrationReplacement::new(registration.id(), retained);
    let plan = table
        .preflight_mapping_replace(&[registration.id()], &[replacement])
        .unwrap();
    table
        .commit_mapping_replace(plan, &[registration.id()], &[replacement], |_| {})
        .unwrap();
    assert_eq!(
        table
            .epoch_for_mapping(source.address_space(), source.mapping())
            .unwrap(),
        Some(source.generation())
    );

    let foreign_mapping = uffd_snapshot(7, 71, 99, 0x3000, 2 * PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        registration.refreshed_fragment(foreign_mapping, retained_range),
        Err(MmError::StaleGeneration)
    );
    let outside_current = PageRange::new(0x5000, PAGE, PAGE).unwrap();
    assert_eq!(
        registration.refreshed_fragment(survivor, outside_current),
        Err(MmError::RangeNotMapped)
    );
}

#[test]
fn userfaultfd_canonical_replacement_union_is_source_bound_and_atomic() {
    let api = initialized_uffd_api();
    let source = uffd_snapshot(7, 70, 11, 0x1000, 3 * PAGE, MappingKind::AnonymousPrivate);
    let post_vma = uffd_snapshot(7, 70, 99, 0x1000, 3 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<4>::new(1).unwrap();
    let mut registered = Vec::new();
    for start in [0x1000, 0x2000, 0x3000] {
        let range = PageRange::new(start, PAGE, PAGE).unwrap();
        registered.push(
            table
                .register(&api, uffd_registration_request(9, source, range))
                .unwrap(),
        );
    }

    let replacements = registered
        .iter()
        .copied()
        .map(|registration| {
            UffdRegistrationReplacement::new(
                registration.id(),
                registration
                    .refreshed_fragment(post_vma, registration.range())
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let folded = replacements[0]
        .canonical_union(replacements[1], post_vma)
        .unwrap()
        .unwrap()
        .canonical_union(replacements[2], post_vma)
        .unwrap()
        .unwrap();
    assert_eq!(folded.source(), registered[0].id());
    assert_eq!(folded.request().range(), post_vma.range());
    assert_eq!(folded.request().generation(), source.generation());
    assert_ne!(folded.request().generation(), post_vma.generation());

    // Keeping a folded source live would overlap the union. The one-shot
    // mapping transaction must still retire every consumed source token even
    // though the canonical replacement retains only one representative ID.
    let incomplete_removed = [registered[0].id(), registered[2].id()];
    assert_eq!(
        table.preflight_mapping_replace(&incomplete_removed, &[folded]),
        Err(MmError::UffdRegistrationOverlap)
    );
    assert_eq!(table.len(), 3);

    let removed = registered
        .iter()
        .map(|registration| registration.id())
        .collect::<Vec<_>>();
    let plan = table
        .preflight_mapping_replace(&removed, &[folded])
        .unwrap();
    assert_eq!(plan.removed(), 3);
    assert_eq!(plan.published(), 1);
    let commit = table
        .commit_mapping_replace(plan, &removed, &[folded], |_| {})
        .unwrap();
    assert_eq!(commit.removed(), 3);
    assert_eq!(commit.published(), 1);
    assert_eq!(table.len(), 1);
    let canonical = table.iter().next().unwrap();
    assert_eq!(canonical.range(), post_vma.range());
    assert_eq!(canonical.generation(), source.generation());
}

#[test]
fn userfaultfd_canonical_replacement_union_rejects_bad_geometry_and_boundaries() {
    let source = uffd_snapshot(7, 70, 11, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let post_vma = uffd_snapshot(7, 70, 99, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let replacement = |id, handler, generation, start| {
        let mapping = uffd_snapshot(
            7,
            70,
            generation,
            0x1000,
            4 * PAGE,
            MappingKind::AnonymousPrivate,
        );
        UffdRegistrationReplacement::new(
            UffdRegistrationId::new(id).unwrap(),
            uffd_registration_request(handler, mapping, PageRange::new(start, PAGE, PAGE).unwrap()),
        )
    };
    let left = replacement(1, 9, source.generation().get(), 0x1000);

    assert_eq!(
        left.canonical_union(replacement(2, 10, 11, 0x2000), post_vma),
        Ok(None)
    );
    assert_eq!(
        left.canonical_union(replacement(2, 9, 12, 0x2000), post_vma),
        Ok(None)
    );
    assert_eq!(
        left.canonical_union(replacement(2, 9, 11, 0x3000), post_vma),
        Ok(None)
    );
    assert_eq!(
        left.canonical_union(replacement(2, 9, 11, 0x1000), post_vma),
        Err(MmError::InvalidUffdRegistrationBatch)
    );
    assert_eq!(
        replacement(2, 9, 11, 0x2000).canonical_union(left, post_vma),
        Err(MmError::InvalidUffdRegistrationBatch)
    );

    let too_short = uffd_snapshot(7, 70, 99, 0x1000, PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        left.canonical_union(replacement(2, 9, 11, 0x2000), too_short),
        Err(MmError::RangeNotMapped)
    );
    let foreign = uffd_snapshot(7, 71, 99, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        left.canonical_union(replacement(2, 9, 11, 0x2000), foreign),
        Err(MmError::StaleGeneration)
    );
    let file_vma = uffd_snapshot(7, 70, 99, 0x1000, 4 * PAGE, MappingKind::FilePrivate);
    assert_eq!(
        left.canonical_union(replacement(2, 9, 11, 0x2000), file_vma),
        Err(MmError::UnsupportedUffdMapping)
    );
}

#[test]
fn userfaultfd_refreshed_fragment_preserves_epoch_across_in_place_grow() {
    let api = initialized_uffd_api();
    let old = uffd_snapshot(9, 90, 21, 0x1000, 2 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<1>::new(1).unwrap();
    let registration = table
        .register(&api, uffd_registration_request(12, old, old.range()))
        .unwrap();
    let grown = uffd_snapshot(9, 90, 22, 0x1000, 4 * PAGE, MappingKind::AnonymousPrivate);

    let refreshed = registration
        .refreshed_fragment(grown, grown.range())
        .unwrap();
    assert_eq!(refreshed.range(), grown.range());
    assert_eq!(refreshed.handler(), registration.handler());
    assert_eq!(refreshed.mapping(), registration.mapping());
    assert_eq!(refreshed.generation(), registration.generation());
    assert_ne!(refreshed.generation(), grown.generation());
    assert_eq!(refreshed.mode(), registration.mode());
}

#[test]
fn userfaultfd_tail_extension_replacement_preflights_without_a_post_grow_snapshot() {
    let api = initialized_uffd_api();
    let old = uffd_snapshot(9, 90, 21, 0x2000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<1>::new(1).unwrap();
    let registration = table
        .register(&api, uffd_registration_request(12, old, old.range()))
        .unwrap();
    let grown_range = PageRange::new(0x2000, 8 * PAGE, PAGE).unwrap();

    let replacement = registration
        .tail_extension_replacement(old.address_space(), old.mapping(), grown_range)
        .unwrap();
    assert_eq!(replacement.source(), registration.id());
    let request = replacement.request();
    assert_eq!(request.handler(), registration.handler());
    assert_eq!(request.address_space(), registration.address_space());
    assert_eq!(request.mapping(), registration.mapping());
    assert_eq!(request.generation(), registration.generation());
    assert_eq!(request.range(), grown_range);
    assert_eq!(request.mode(), registration.mode());

    let plan = table
        .preflight_mapping_replace(&[registration.id()], &[replacement])
        .unwrap();
    let commit = table
        .commit_mapping_replace(plan, &[registration.id()], &[replacement], |_| {})
        .unwrap();
    assert_eq!(commit.removed(), 1);
    assert_eq!(commit.published(), 1);
    let grown = table.iter().next().unwrap();
    assert_eq!(grown.range(), grown_range);
    assert_eq!(grown.generation(), registration.generation());
}

#[test]
fn userfaultfd_tail_extension_replacement_rejects_wrong_identity_or_shape() {
    let api = initialized_uffd_api();
    let old = uffd_snapshot(9, 90, 21, 0x2000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<1>::new(1).unwrap();
    let registration = table
        .register(&api, uffd_registration_request(12, old, old.range()))
        .unwrap();
    let grown_range = PageRange::new(0x2000, 8 * PAGE, PAGE).unwrap();

    assert_eq!(
        registration.tail_extension_replacement(
            AddressSpaceId::new(10).unwrap(),
            old.mapping(),
            grown_range,
        ),
        Err(MmError::StaleGeneration)
    );
    assert_eq!(
        registration.tail_extension_replacement(
            old.address_space(),
            MappingId::new(91).unwrap(),
            grown_range,
        ),
        Err(MmError::StaleGeneration)
    );

    let same = old.range();
    let shorter = PageRange::new(0x2000, 2 * PAGE, PAGE).unwrap();
    let shifted_cover = PageRange::new(0x1000, 8 * PAGE, PAGE).unwrap();
    let different_page_size = PageRange::new(0x2000, 8 * PAGE, 2 * PAGE).unwrap();
    for invalid in [same, shorter, shifted_cover, different_page_size] {
        assert_eq!(
            registration.tail_extension_replacement(old.address_space(), old.mapping(), invalid,),
            Err(MmError::RangeNotMapped)
        );
    }
    assert_eq!(table.iter().collect::<Vec<_>>(), vec![registration]);
}

#[test]
fn userfaultfd_head_extension_replacement_preflights_without_a_post_grow_snapshot() {
    let api = initialized_uffd_api();
    let old = uffd_snapshot(9, 90, 21, 0x6000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<1>::new(1).unwrap();
    let registration = table
        .register(&api, uffd_registration_request(12, old, old.range()))
        .unwrap();
    let grown_range = PageRange::new(0x2000, 8 * PAGE, PAGE).unwrap();

    let replacement = registration
        .head_extension_replacement(old.address_space(), old.mapping(), grown_range)
        .unwrap();
    assert_eq!(replacement.source(), registration.id());
    let request = replacement.request();
    assert_eq!(request.handler(), registration.handler());
    assert_eq!(request.address_space(), registration.address_space());
    assert_eq!(request.mapping(), registration.mapping());
    assert_eq!(request.generation(), registration.generation());
    assert_eq!(request.range(), grown_range);
    assert_eq!(request.mode(), registration.mode());

    let plan = table
        .preflight_mapping_replace(&[registration.id()], &[replacement])
        .unwrap();
    let commit = table
        .commit_mapping_replace(plan, &[registration.id()], &[replacement], |_| {})
        .unwrap();
    assert_eq!(commit.removed(), 1);
    assert_eq!(commit.published(), 1);
    let grown = table.iter().next().unwrap();
    assert_eq!(grown.range(), grown_range);
    assert_eq!(grown.generation(), registration.generation());
}

#[test]
fn userfaultfd_head_extension_replacement_rejects_wrong_identity_or_shape() {
    let api = initialized_uffd_api();
    let old = uffd_snapshot(9, 90, 21, 0x6000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<1>::new(1).unwrap();
    let registration = table
        .register(&api, uffd_registration_request(12, old, old.range()))
        .unwrap();
    let grown_range = PageRange::new(0x2000, 8 * PAGE, PAGE).unwrap();

    assert_eq!(
        registration.head_extension_replacement(
            AddressSpaceId::new(10).unwrap(),
            old.mapping(),
            grown_range,
        ),
        Err(MmError::StaleGeneration)
    );
    assert_eq!(
        registration.head_extension_replacement(
            old.address_space(),
            MappingId::new(91).unwrap(),
            grown_range,
        ),
        Err(MmError::StaleGeneration)
    );

    let same = old.range();
    let shorter = PageRange::new(0x7000, 2 * PAGE, PAGE).unwrap();
    let shifted_cover = PageRange::new(0x1000, 10 * PAGE, PAGE).unwrap();
    let different_page_size = PageRange::new(0x2000, 8 * PAGE, 2 * PAGE).unwrap();
    for invalid in [same, shorter, shifted_cover, different_page_size] {
        assert_eq!(
            registration.head_extension_replacement(old.address_space(), old.mapping(), invalid,),
            Err(MmError::RangeNotMapped)
        );
    }
    assert_eq!(table.iter().collect::<Vec<_>>(), vec![registration]);
}

#[test]
fn userfaultfd_mapping_epoch_lookup_fails_closed_on_inconsistent_fragments() {
    let api = initialized_uffd_api();
    let first = uffd_snapshot(8, 80, 11, 0x1000, PAGE, MappingKind::AnonymousPrivate);
    let second = uffd_snapshot(8, 80, 12, 0x2000, PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<2>::new(1).unwrap();
    table
        .register(&api, uffd_registration_request(9, first, first.range()))
        .unwrap();
    table
        .register(&api, uffd_registration_request(9, second, second.range()))
        .unwrap();

    assert_eq!(
        table.epoch_for_mapping(first.address_space(), first.mapping()),
        Err(MmError::StaleGeneration)
    );
    assert_eq!(
        table
            .epoch_for_mapping(first.address_space(), MappingId::new(81).unwrap())
            .unwrap(),
        None
    );
}

#[test]
fn userfaultfd_create_and_api_negotiation_preserve_linux_error_classes() {
    assert_eq!(
        UffdCreateFlags::from_bits(1 << 4),
        Err(MmError::InvalidUffdFlags)
    );
    assert_eq!(
        UffdCreateFlags::from_bits(UFFD_O_CLOEXEC)
            .unwrap()
            .validate_profile(),
        Err(MmError::AccessDenied)
    );
    let flags = UffdCreateFlags::from_bits(UFFD_USER_MODE_ONLY | UFFD_O_NONBLOCK | UFFD_O_CLOEXEC)
        .unwrap()
        .validate_profile()
        .unwrap();
    assert!(flags.user_mode_only());
    assert!(flags.nonblocking());
    assert!(flags.close_on_exec());

    let mut api = UffdApiState::new();
    assert_eq!(
        api.prepare_raw(UFFD_API + 1, 0),
        Err(MmError::InvalidUffdApi)
    );
    assert_eq!(
        api.prepare_raw(UFFD_API, 1 << 63),
        Err(MmError::InvalidUffdFeatures)
    );
    assert_eq!(
        api.prepare_raw(UFFD_API, UffdFeatures::EVENT_FORK.bits()),
        Err(MmError::UnsupportedUffdFeatures)
    );

    let abandoned_copyout = api.prepare_raw(UFFD_API, 0).unwrap();
    assert_eq!(api.lifecycle(), UffdApiLifecycle::Created);
    let retry = api.prepare_raw(UFFD_API, 0).unwrap();
    assert_eq!(retry, abandoned_copyout);
    api.commit(retry).unwrap();
    assert_eq!(api.lifecycle(), UffdApiLifecycle::Initialized);
    assert_eq!(api.enabled_features(), UffdFeatures::AVAILABLE);
    assert_eq!(
        api.prepare_raw(UFFD_API, 0),
        Err(MmError::UffdAlreadyInitialized)
    );
}

#[test]
fn userfaultfd_registration_modes_and_mapping_profile_are_explicit() {
    assert_eq!(
        UffdRegisterMode::from_bits(0),
        Err(MmError::InvalidUffdMode)
    );
    assert_eq!(
        UffdRegisterMode::from_bits(1 << 8),
        Err(MmError::InvalidUffdMode)
    );
    assert_eq!(
        UffdRegisterMode::from_bits(UffdRegisterMode::WP.bits()),
        Err(MmError::UnsupportedUffdMode)
    );
    assert_eq!(
        UffdRegisterMode::from_bits(UffdRegisterMode::MINOR.bits()),
        Err(MmError::UnsupportedUffdMode)
    );

    let shared = uffd_snapshot(1, 40, 1, 0x1000, PAGE, MappingKind::AnonymousShared);
    assert_eq!(
        UffdRegistrationRequest::new(
            FaultHandlerId::new(1).unwrap(),
            shared,
            shared.range(),
            UffdRegisterMode::MISSING,
        ),
        Err(MmError::UnsupportedUffdMapping)
    );
}

#[test]
fn userfaultfd_unregister_vma_validation_owns_api_and_profile_policy() {
    let table = UffdRegistrationTable::<1>::new(1).unwrap();
    let range = PageRange::new(0x1000, 9 * PAGE, PAGE).unwrap();
    let first = uffd_snapshot(3, 40, 1, 0x1000, 2 * PAGE, MappingKind::AnonymousPrivate);
    let second = uffd_snapshot(3, 41, 1, 0x8000, 2 * PAGE, MappingKind::AnonymousPrivate);

    let uninitialized = UffdApiState::new();
    assert_eq!(
        table.validate_unregister_vmas(&uninitialized, range, &[first, second]),
        Err(MmError::UffdNotInitialized)
    );

    let api = initialized_uffd_api();
    assert_eq!(
        table.validate_unregister_vmas(&api, range, &[]),
        Err(MmError::RangeNotMapped)
    );
    assert_eq!(
        table
            .validate_unregister_vmas(&api, range, &[first, second])
            .unwrap(),
        first.address_space()
    );

    let foreign_address_space =
        uffd_snapshot(4, 41, 1, 0x8000, 2 * PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        table.validate_unregister_vmas(&api, range, &[first, foreign_address_space]),
        Err(MmError::InvalidUffdRegistrationBatch)
    );
    let incompatible_kind = uffd_snapshot(3, 41, 1, 0x8000, 2 * PAGE, MappingKind::Device);
    assert_eq!(
        table.validate_unregister_vmas(&api, range, &[first, incompatible_kind]),
        Err(MmError::InvalidUffdRegistrationBatch)
    );
    let overlap = uffd_snapshot(3, 42, 1, 0x2000, 2 * PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        table.validate_unregister_vmas(&api, range, &[first, overlap]),
        Err(MmError::InvalidUffdRegistrationBatch)
    );
    let outside = uffd_snapshot(3, 43, 1, 0xb000, PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        table.validate_unregister_vmas(&api, range, &[outside]),
        Err(MmError::InvalidUffdRegistrationBatch)
    );
    let larger_pages = MappingSnapshot::from_raw(
        3,
        44,
        1,
        0,
        2 * PAGE,
        2 * PAGE,
        access(true, true, false).bits(),
        MappingKind::AnonymousPrivate,
        true,
        false,
    )
    .unwrap();
    assert_eq!(
        table.validate_unregister_vmas(&api, range, &[larger_pages]),
        Err(MmError::InvalidUffdRegistrationBatch)
    );
}

#[test]
fn userfaultfd_registration_batch_is_atomic_idempotent_and_owner_aware() {
    let api = initialized_uffd_api();
    let first = uffd_snapshot(1, 50, 1, 0x1000, PAGE, MappingKind::AnonymousPrivate);
    let second = uffd_snapshot(1, 51, 1, 0x4000, PAGE, MappingKind::AnonymousPrivate);
    let first_request = uffd_registration_request(7, first, first.range());
    let second_request = uffd_registration_request(7, second, second.range());

    let mut table = UffdRegistrationTable::<4>::new(10).unwrap();
    let mut ids = Vec::new();
    let commit = table
        .register_batch(&api, &[first_request, second_request], |registration| {
            ids.push(registration.id().get());
        })
        .unwrap();
    assert_eq!(commit.published(), 2);
    assert_eq!(commit.reused(), 0);
    assert_eq!(commit.registrations(), 2);
    assert_eq!(table.len(), 2);
    assert_eq!(ids, vec![10, 11]);

    let duplicate = table.register(&api, first_request).unwrap();
    assert_eq!(duplicate.id().get(), 10);
    assert_eq!(table.len(), 2);

    let foreign = uffd_registration_request(8, first, first.range());
    assert_eq!(table.register(&api, foreign), Err(MmError::Busy));
    assert_eq!(table.len(), 2);

    let mut too_small = UffdRegistrationTable::<1>::new(1).unwrap();
    assert!(matches!(
        too_small.register_batch(&api, &[first_request, second_request], |_| {}),
        Err(MmError::CapacityExceeded)
    ));
    assert!(too_small.is_empty());
    assert_eq!(
        too_small.register(&api, first_request).unwrap().id().get(),
        1
    );

    let mut exhausted = UffdRegistrationTable::<2>::new(u64::MAX).unwrap();
    assert!(matches!(
        exhausted.register_batch(&api, &[first_request, second_request], |_| {}),
        Err(MmError::IdExhausted)
    ));
    assert!(exhausted.is_empty());
    assert_eq!(
        exhausted.register(&api, first_request).unwrap().id().get(),
        u64::MAX
    );
}

#[test]
fn userfaultfd_partial_register_handles_subset_prefix_and_suffix_canonically() {
    let api = initialized_uffd_api();
    let mapping = uffd_snapshot(6, 100, 1, 0x1000, 5 * PAGE, MappingKind::AnonymousPrivate);
    let handler = FaultHandlerId::new(50).unwrap();
    let mut table = UffdRegistrationTable::<4>::new(1).unwrap();
    let middle = PageRange::new(0x2000, 2 * PAGE, PAGE).unwrap();
    let original = table
        .register(&api, uffd_registration_request(50, mapping, middle))
        .unwrap();

    let subset = UffdRegistrationIntent::new(
        handler,
        PageRange::new(0x3000, PAGE, PAGE).unwrap(),
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    let (plan, removed, replacements) =
        collect_uffd_register_delta(&table, &api, subset, &[mapping]);
    assert!(plan.is_noop());
    assert!(removed.is_empty());
    assert!(replacements.is_empty());
    assert_eq!(table.get(original.id()), Ok(original));

    let prefix = UffdRegistrationIntent::new(
        handler,
        PageRange::new(0x1000, 2 * PAGE, PAGE).unwrap(),
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    let (plan, removed, replacements) =
        collect_uffd_register_delta(&table, &api, prefix, &[mapping]);
    assert_eq!(plan.removed(), 1);
    assert_eq!(plan.replacements(), 1);
    assert_eq!(removed, vec![original.id()]);
    assert_eq!(
        replacements[0].range(),
        PageRange::new(0x1000, 3 * PAGE, PAGE).unwrap()
    );
    commit_uffd_register_delta(&mut table, &api, prefix, &[mapping]);

    let suffix = UffdRegistrationIntent::new(
        handler,
        PageRange::new(0x3000, 3 * PAGE, PAGE).unwrap(),
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    commit_uffd_register_delta(&mut table, &api, suffix, &[mapping]);
    let registrations = table.iter().collect::<Vec<_>>();
    assert_eq!(registrations.len(), 1);
    assert_eq!(
        registrations[0].range(),
        PageRange::new(0x1000, 5 * PAGE, PAGE).unwrap()
    );
}

#[test]
fn userfaultfd_partial_register_bridges_fragments_but_preserves_vma_gaps() {
    let api = initialized_uffd_api();
    let handler = FaultHandlerId::new(60).unwrap();
    let bridge_vma = uffd_snapshot(7, 110, 1, 0x10_000, 5 * PAGE, MappingKind::AnonymousPrivate);
    let mut bridge_table = UffdRegistrationTable::<4>::new(1).unwrap();
    let left = uffd_registration_request(
        60,
        bridge_vma,
        PageRange::new(0x10_000, PAGE, PAGE).unwrap(),
    );
    let right = uffd_registration_request(
        60,
        bridge_vma,
        PageRange::new(0x12_000, PAGE, PAGE).unwrap(),
    );
    bridge_table
        .register_batch(&api, &[left, right], |_| {})
        .unwrap();
    let bridge = UffdRegistrationIntent::new(
        handler,
        PageRange::new(0x11_000, PAGE, PAGE).unwrap(),
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    let (plan, removed, replacements) =
        collect_uffd_register_delta(&bridge_table, &api, bridge, &[bridge_vma]);
    assert_eq!(plan.removed(), 2);
    assert_eq!(plan.replacements(), 1);
    assert_eq!(removed.len(), 2);
    assert_eq!(
        replacements[0].range(),
        PageRange::new(0x10_000, 3 * PAGE, PAGE).unwrap()
    );
    commit_uffd_register_delta(&mut bridge_table, &api, bridge, &[bridge_vma]);
    assert_eq!(bridge_table.len(), 1);

    let first_vma = uffd_snapshot(8, 120, 1, 0x20_000, 2 * PAGE, MappingKind::AnonymousPrivate);
    let second_vma = uffd_snapshot(8, 121, 1, 0x24_000, 2 * PAGE, MappingKind::AnonymousPrivate);
    let across_gap = UffdRegistrationIntent::new(
        handler,
        PageRange::new(0x20_000, 6 * PAGE, PAGE).unwrap(),
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    let mut gap_table = UffdRegistrationTable::<4>::new(1).unwrap();
    let (plan, removed, replacements) =
        collect_uffd_register_delta(&gap_table, &api, across_gap, &[first_vma, second_vma]);
    assert_eq!(plan.removed(), 0);
    assert_eq!(plan.replacements(), 2);
    assert!(removed.is_empty());
    assert_eq!(replacements[0].range(), first_vma.range());
    assert_eq!(replacements[1].range(), second_vma.range());
    commit_uffd_register_delta(&mut gap_table, &api, across_gap, &[first_vma, second_vma]);
    assert_eq!(gap_table.len(), 2);
}

#[test]
fn userfaultfd_register_delta_failures_never_mutate_the_table() {
    let api = initialized_uffd_api();
    let vma = uffd_snapshot(9, 130, 1, 0x30_000, 4 * PAGE, MappingKind::AnonymousPrivate);
    let foreign_intent = UffdRegistrationIntent::new(
        FaultHandlerId::new(70).unwrap(),
        PageRange::new(0x31_000, PAGE, PAGE).unwrap(),
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    let mut foreign = UffdRegistrationTable::<2>::new(1).unwrap();
    let foreign_record = foreign
        .register(
            &api,
            uffd_registration_request(71, vma, PageRange::new(0x31_000, PAGE, PAGE).unwrap()),
        )
        .unwrap();
    assert_eq!(
        foreign.preflight_register_delta(&api, foreign_intent, &[vma]),
        Err(MmError::Busy)
    );
    assert_eq!(foreign.iter().collect::<Vec<_>>(), vec![foreign_record]);

    let first = uffd_snapshot(10, 140, 1, 0x40_000, PAGE, MappingKind::AnonymousPrivate);
    let second = uffd_snapshot(10, 141, 1, 0x42_000, PAGE, MappingKind::AnonymousPrivate);
    let third = uffd_snapshot(10, 142, 1, 0x44_000, PAGE, MappingKind::AnonymousPrivate);
    let mut full = UffdRegistrationTable::<2>::new(1).unwrap();
    full.register(&api, uffd_registration_request(72, first, first.range()))
        .unwrap();
    full.register(&api, uffd_registration_request(72, second, second.range()))
        .unwrap();
    let before = full.iter().collect::<Vec<_>>();
    let new_intent = UffdRegistrationIntent::new(
        FaultHandlerId::new(72).unwrap(),
        third.range(),
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    assert_eq!(
        full.preflight_register_delta(&api, new_intent, &[third]),
        Err(MmError::CapacityExceeded)
    );
    assert_eq!(full.iter().collect::<Vec<_>>(), before);

    let mut exhausted = UffdRegistrationTable::<2>::new(u64::MAX).unwrap();
    let old = exhausted
        .register(
            &api,
            uffd_registration_request(73, vma, PageRange::new(0x31_000, PAGE, PAGE).unwrap()),
        )
        .unwrap();
    let extension = UffdRegistrationIntent::new(
        FaultHandlerId::new(73).unwrap(),
        PageRange::new(0x30_000, 2 * PAGE, PAGE).unwrap(),
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    assert_eq!(
        exhausted.preflight_register_delta(&api, extension, &[vma]),
        Err(MmError::IdExhausted)
    );
    assert_eq!(exhausted.iter().collect::<Vec<_>>(), vec![old]);

    let mut stale = UffdRegistrationTable::<3>::new(1).unwrap();
    let stale_plan = stale
        .preflight_register_delta(&api, new_intent, &[third])
        .unwrap();
    stale
        .register(&api, uffd_registration_request(72, first, first.range()))
        .unwrap();
    let mut emitted_removed = 0usize;
    let mut emitted_replaced = 0usize;
    assert_eq!(
        stale.replay_register_delta(
            stale_plan,
            new_intent,
            &[third],
            |_| emitted_removed += 1,
            |_| emitted_replaced += 1,
        ),
        Err(MmError::StaleGeneration)
    );
    assert_eq!(emitted_removed, 0);
    assert_eq!(emitted_replaced, 0);
    assert_eq!(stale.len(), 1);
}

#[test]
fn userfaultfd_register_delta_requires_explicit_lineage_refresh() {
    let api = initialized_uffd_api();
    let old = uffd_snapshot(
        12,
        160,
        1,
        0x70_000,
        3 * PAGE,
        MappingKind::AnonymousPrivate,
    );
    let new_generation = uffd_snapshot(
        12,
        160,
        2,
        0x70_000,
        3 * PAGE,
        MappingKind::AnonymousPrivate,
    );
    let new_lineage = uffd_snapshot(
        12,
        161,
        2,
        0x70_000,
        3 * PAGE,
        MappingKind::AnonymousPrivate,
    );
    let handler = FaultHandlerId::new(81).unwrap();
    let registered_range = PageRange::new(0x71_000, PAGE, PAGE).unwrap();
    let intent = UffdRegistrationIntent::new(
        handler,
        PageRange::new(0x70_000, 2 * PAGE, PAGE).unwrap(),
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    let mut table = UffdRegistrationTable::<2>::new(1).unwrap();
    let old_registration = table
        .register(
            &api,
            UffdRegistrationRequest::new(handler, old, registered_range, UffdRegisterMode::MISSING)
                .unwrap(),
        )
        .unwrap();

    assert_eq!(
        table.preflight_register_delta(&api, intent, &[new_generation]),
        Err(MmError::StaleGeneration)
    );
    assert_eq!(
        table.preflight_register_delta(&api, intent, &[new_lineage]),
        Err(MmError::StaleGeneration)
    );
    assert_eq!(table.iter().collect::<Vec<_>>(), vec![old_registration]);

    let refreshed = UffdRegistrationRequest::new(
        handler,
        new_lineage,
        registered_range,
        UffdRegisterMode::MISSING,
    )
    .unwrap();
    table
        .replace_batch(&api, &[old_registration.id()], &[refreshed], |_| {})
        .unwrap();
    let (plan, removed, replacements) =
        collect_uffd_register_delta(&table, &api, intent, &[new_lineage]);
    assert_eq!(plan.removed(), 1);
    assert_eq!(plan.replacements(), 1);
    assert_eq!(removed.len(), 1);
    assert_eq!(
        replacements[0].range(),
        PageRange::new(0x70_000, 2 * PAGE, PAGE).unwrap()
    );
}

#[test]
fn userfaultfd_register_delta_matches_a_reference_interval_union() {
    let api = initialized_uffd_api();
    let base = 0x60_000usize;
    let mapping = uffd_snapshot(11, 150, 1, base, 8 * PAGE, MappingKind::AnonymousPrivate);
    let handler = FaultHandlerId::new(80).unwrap();
    let candidate_pages = [0usize, 2, 4, 6];

    for mask in 0u8..(1 << candidate_pages.len()) {
        let mut table = UffdRegistrationTable::<8>::new(1).unwrap();
        let mut initial = Vec::new();
        for (bit, page) in candidate_pages.iter().copied().enumerate() {
            if mask & (1 << bit) == 0 {
                continue;
            }
            initial.push(
                UffdRegistrationRequest::new(
                    handler,
                    mapping,
                    PageRange::new(base + page * PAGE, PAGE, PAGE).unwrap(),
                    UffdRegisterMode::MISSING,
                )
                .unwrap(),
            );
        }
        if !initial.is_empty() {
            table.register_batch(&api, &initial, |_| {}).unwrap();
        }
        let existing = table.iter().collect::<Vec<_>>();

        for start_page in 0usize..8 {
            for end_page in (start_page + 1)..=8 {
                let requested = PageRange::new(
                    base + start_page * PAGE,
                    (end_page - start_page) * PAGE,
                    PAGE,
                )
                .unwrap();
                let intent =
                    UffdRegistrationIntent::new(handler, requested, UffdRegisterMode::MISSING)
                        .unwrap();
                let (plan, removed, replacements) =
                    collect_uffd_register_delta(&table, &api, intent, &[mapping]);

                let mut expected_start = requested.start();
                let mut expected_end = requested.end();
                loop {
                    let before = (expected_start, expected_end);
                    for registration in &existing {
                        if registration.range().start() <= expected_end
                            && expected_start <= registration.range().end()
                        {
                            expected_start = expected_start.min(registration.range().start());
                            expected_end = expected_end.max(registration.range().end());
                        }
                    }
                    if before == (expected_start, expected_end) {
                        break;
                    }
                }
                let expected_range =
                    PageRange::new(expected_start, expected_end - expected_start, PAGE).unwrap();
                let connected = existing
                    .iter()
                    .copied()
                    .filter(|registration| {
                        registration.range().start() <= expected_range.end()
                            && expected_range.start() <= registration.range().end()
                    })
                    .collect::<Vec<_>>();
                let no_op = connected.len() == 1 && connected[0].range() == expected_range;
                let expected_removed = if no_op {
                    Vec::new()
                } else {
                    connected
                        .iter()
                        .map(|registration| registration.id())
                        .collect::<Vec<_>>()
                };

                assert_eq!(
                    plan.is_noop(),
                    no_op,
                    "mask={mask:#x} request={requested:?}"
                );
                assert_eq!(
                    removed, expected_removed,
                    "mask={mask:#x} request={requested:?}"
                );
                if no_op {
                    assert!(replacements.is_empty());
                } else {
                    assert_eq!(replacements.len(), 1);
                    assert_eq!(
                        replacements[0].range(),
                        expected_range,
                        "mask={mask:#x} request={requested:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn userfaultfd_registration_refresh_is_transactional_and_stale_plan_safe() {
    let api = initialized_uffd_api();
    let old = uffd_snapshot(4, 80, 1, 0x20_000, 2 * PAGE, MappingKind::AnonymousPrivate);
    let refreshed = uffd_snapshot(4, 80, 2, 0x20_000, 2 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<4>::new(100).unwrap();
    let old_registration = table
        .register(&api, uffd_registration_request(30, old, old.range()))
        .unwrap();
    let left =
        uffd_registration_request(30, refreshed, PageRange::new(0x20_000, PAGE, PAGE).unwrap());
    let right =
        uffd_registration_request(30, refreshed, PageRange::new(0x21_000, PAGE, PAGE).unwrap());
    let replacements = [left, right];

    let plan = table
        .preflight_replace(&api, &[old_registration.id()], &replacements)
        .unwrap();
    assert_eq!(plan.removed(), 1);
    assert_eq!(plan.published(), 2);
    assert_eq!(table.len(), 1);
    assert_eq!(table.get(old_registration.id()), Ok(old_registration));

    let mut refreshed_ids = Vec::new();
    let commit = table
        .commit_replace(
            plan,
            &[old_registration.id()],
            &replacements,
            |registration| refreshed_ids.push(registration.id().get()),
        )
        .unwrap();
    assert_eq!(commit.removed(), 1);
    assert_eq!(commit.published(), 2);
    assert_eq!(table.len(), 2);
    assert_eq!(
        table.get(old_registration.id()),
        Err(MmError::UnknownUffdRegistration)
    );
    assert_eq!(refreshed_ids, vec![101, 102]);
    for id in refreshed_ids {
        assert_eq!(
            table
                .get(UffdRegistrationId::new(id).unwrap())
                .unwrap()
                .generation(),
            refreshed.generation()
        );
    }

    let disjoint = uffd_snapshot(4, 81, 1, 0x30_000, PAGE, MappingKind::AnonymousPrivate);
    let disjoint_request = uffd_registration_request(30, disjoint, disjoint.range());
    let refreshed_left = table
        .resolver_registration(
            refreshed.address_space(),
            PageRange::new(0x20_000, PAGE, PAGE).unwrap(),
        )
        .unwrap();
    let stale_plan = table
        .preflight_replace(&api, &[refreshed_left.id()], &[left])
        .unwrap();
    table.register(&api, disjoint_request).unwrap();
    assert_eq!(
        table.commit_replace(stale_plan, &[refreshed_left.id()], &[left], |_| {}),
        Err(MmError::StaleGeneration)
    );
    assert!(table.get(refreshed_left.id()).is_ok());
}

#[test]
fn userfaultfd_mapping_refresh_is_all_or_none_across_multiple_handlers() {
    let api = initialized_uffd_api();
    let first_old = uffd_snapshot(5, 90, 1, 0x40_000, PAGE, MappingKind::AnonymousPrivate);
    let second_old = uffd_snapshot(5, 91, 1, 0x42_000, PAGE, MappingKind::AnonymousPrivate);
    let first_new = uffd_snapshot(5, 90, 2, 0x40_000, PAGE, MappingKind::AnonymousPrivate);
    let second_new = uffd_snapshot(5, 91, 2, 0x42_000, PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<6>::new(1).unwrap();
    let first = table
        .register(
            &api,
            uffd_registration_request(40, first_old, first_old.range()),
        )
        .unwrap();
    let second = table
        .register(
            &api,
            uffd_registration_request(41, second_old, second_old.range()),
        )
        .unwrap();
    let removed = [first.id(), second.id()];

    let wrong_owner = UffdRegistrationReplacement::new(
        first.id(),
        uffd_registration_request(99, first_new, first_new.range()),
    );
    assert_eq!(
        table.preflight_mapping_replace(&removed, &[wrong_owner]),
        Err(MmError::InvalidUffdRegistrationBatch)
    );
    assert_eq!(table.len(), 2);

    let replacements = [
        UffdRegistrationReplacement::new(
            first.id(),
            uffd_registration_request(40, first_new, first_new.range()),
        ),
        UffdRegistrationReplacement::new(
            second.id(),
            uffd_registration_request(41, second_new, second_new.range()),
        ),
    ];
    let stale_plan = table
        .preflight_mapping_replace(&removed, &replacements)
        .unwrap();
    assert_eq!(stale_plan.removed(), 2);
    assert_eq!(stale_plan.published(), 2);

    let third = uffd_snapshot(5, 92, 1, 0x50_000, PAGE, MappingKind::AnonymousPrivate);
    table
        .register(&api, uffd_registration_request(40, third, third.range()))
        .unwrap();
    assert_eq!(
        table.commit_mapping_replace(stale_plan, &removed, &replacements, |_| {}),
        Err(MmError::StaleGeneration)
    );
    assert_eq!(table.get(first.id()), Ok(first));
    assert_eq!(table.get(second.id()), Ok(second));

    let plan = table
        .preflight_mapping_replace(&removed, &replacements)
        .unwrap();
    let mut refreshed = Vec::new();
    let commit = table
        .commit_mapping_replace(plan, &removed, &replacements, |registration| {
            refreshed.push(registration);
        })
        .unwrap();
    assert_eq!(commit.removed(), 2);
    assert_eq!(commit.published(), 2);
    assert_eq!(refreshed.len(), 2);
    assert_eq!(refreshed[0].handler(), FaultHandlerId::new(40).unwrap());
    assert_eq!(refreshed[1].handler(), FaultHandlerId::new(41).unwrap());
    assert_eq!(refreshed[0].generation(), first_new.generation());
    assert_eq!(refreshed[1].generation(), second_new.generation());

    let covering = PageRange::new(0x40_000, 3 * PAGE, PAGE).unwrap();
    let intersecting = table
        .intersecting(first_new.address_space(), covering)
        .map(|registration| registration.id())
        .collect::<Vec<_>>();
    assert_eq!(intersecting.len(), 2);
    let remove_plan = table.preflight_mapping_remove(&intersecting).unwrap();
    let removed_commit = table
        .commit_mapping_remove(remove_plan, &intersecting)
        .unwrap();
    assert_eq!(removed_commit.removed(), 2);
    assert_eq!(table.len(), 1);
}

#[test]
fn userfaultfd_fault_policy_uses_lower_broker_permits_without_queue_state() {
    let api = initialized_uffd_api();
    let mapping = uffd_snapshot(2, 60, 4, 0x8000, 2 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<2>::new(1).unwrap();
    let registration = table
        .register(&api, uffd_registration_request(9, mapping, mapping.range()))
        .unwrap();
    let request = FaultRequest::new(
        FaultKey::from_address(mapping, 0x9001, FaultAccess::Write).unwrap(),
        FaultHandlerId::new(9).unwrap(),
        FaultType::Missing,
    );
    let capacity = FaultCapacity::new(2, 2, 4).unwrap();
    let permit = UffdFaultPolicy::admit(
        registration,
        mapping,
        request,
        FaultAdmissionContext::new_request(capacity, FaultLoad::new(1, 1, 3)),
        FaultLifecycleState::Open,
    )
    .unwrap();
    assert_eq!(permit.request(), request);
    assert_eq!(permit.kind(), FaultAdmissionKind::NewRequest);
    assert!(permit.kind().consumes_request_credit());
    assert_eq!(
        UffdFaultPolicy::admit(
            registration,
            mapping,
            request,
            FaultAdmissionContext::new_request(capacity, FaultLoad::new(2, 1, 3)),
            FaultLifecycleState::Open,
        ),
        Err(MmError::QuotaExceeded)
    );

    let wrong_handler = FaultRequest::new(
        request.key(),
        FaultHandlerId::new(10).unwrap(),
        FaultType::Missing,
    );
    assert_eq!(
        UffdFaultPolicy::admit(
            registration,
            mapping,
            wrong_handler,
            FaultAdmissionContext::new_request(capacity, FaultLoad::new(0, 0, 0)),
            FaultLifecycleState::Open,
        ),
        Err(MmError::UffdRegistrationMismatch)
    );
    assert_eq!(
        UffdFaultPolicy::prepare_completion(request, mapping, FaultDisposition::Continue),
        Err(MmError::UnsupportedUffdDisposition)
    );

    let replacement = uffd_snapshot(2, 60, 5, 0x8000, 2 * PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        UffdFaultPolicy::prepare_completion(request, replacement, FaultDisposition::ZeroFill),
        Err(MmError::StaleGeneration)
    );
    let completion =
        UffdFaultPolicy::prepare_completion(request, mapping, FaultDisposition::ZeroFill).unwrap();
    assert_eq!(completion.request(), request);
    assert_eq!(completion.disposition(), FaultDisposition::ZeroFill);

    let protected = MappingSnapshot::from_raw(
        2,
        60,
        4,
        0x8000,
        2 * PAGE,
        PAGE,
        access(false, false, false).bits(),
        MappingKind::AnonymousPrivate,
        true,
        false,
    )
    .unwrap();
    assert_eq!(
        UffdFaultPolicy::admit(
            registration,
            protected,
            request,
            FaultAdmissionContext::new_request(capacity, FaultLoad::new(0, 0, 0)),
            FaultLifecycleState::Open,
        ),
        Err(MmError::AccessDenied)
    );
    let protected_completion =
        UffdFaultPolicy::prepare_completion(request, protected, FaultDisposition::Supply).unwrap();
    assert_eq!(protected_completion.request(), request);
    assert_eq!(protected_completion.disposition(), FaultDisposition::Supply);
}

#[test]
fn userfaultfd_exact_coalescing_skips_only_request_quota() {
    let api = initialized_uffd_api();
    let mapping = uffd_snapshot(2, 61, 4, 0x8000, 2 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<1>::new(1).unwrap();
    let registration = table
        .register(&api, uffd_registration_request(9, mapping, mapping.range()))
        .unwrap();
    let request = FaultRequest::new(
        FaultKey::from_address(mapping, 0x9001, FaultAccess::Write).unwrap(),
        FaultHandlerId::new(9).unwrap(),
        FaultType::Missing,
    );
    let other_page = FaultRequest::new(
        FaultKey::from_address(mapping, 0x8001, FaultAccess::Write).unwrap(),
        FaultHandlerId::new(9).unwrap(),
        FaultType::Missing,
    );
    let capacity = FaultCapacity::new(2, 2, 4).unwrap();
    let full_load = FaultLoad::new(2, 2, 4);

    let coalesced = UffdFaultPolicy::admit(
        registration,
        mapping,
        request,
        FaultAdmissionContext::exact_request(request),
        FaultLifecycleState::Open,
    )
    .unwrap();
    assert_eq!(coalesced.request(), request);
    assert_eq!(coalesced.kind(), FaultAdmissionKind::CoalescedWaiter);
    assert!(!coalesced.kind().consumes_request_credit());

    assert_eq!(
        UffdFaultPolicy::admit(
            registration,
            mapping,
            other_page,
            FaultAdmissionContext::new_request(capacity, full_load),
            FaultLifecycleState::Open,
        ),
        Err(MmError::QuotaExceeded)
    );
    assert_eq!(
        UffdFaultPolicy::admit(
            registration,
            mapping,
            request,
            FaultAdmissionContext::exact_request(other_page),
            FaultLifecycleState::Open,
        ),
        Err(MmError::FaultRequestMismatch)
    );
    let foreign_broker_request = FaultRequest::new(
        request.key(),
        FaultHandlerId::new(10).unwrap(),
        FaultType::Missing,
    );
    assert_eq!(
        UffdFaultPolicy::admit(
            registration,
            mapping,
            request,
            FaultAdmissionContext::exact_request(foreign_broker_request),
            FaultLifecycleState::Open,
        ),
        Err(MmError::FaultRequestMismatch)
    );

    for (lifecycle, expected) in [
        (FaultLifecycleState::Closing, MmError::Closing),
        (FaultLifecycleState::Detached, MmError::TearingDown),
        (FaultLifecycleState::Closed, MmError::Closed),
    ] {
        assert_eq!(
            UffdFaultPolicy::admit(
                registration,
                mapping,
                request,
                FaultAdmissionContext::exact_request(request),
                lifecycle,
            ),
            Err(expected)
        );
        assert_eq!(
            UffdFaultPolicy::admit(
                registration,
                mapping,
                request,
                FaultAdmissionContext::new_request(capacity, FaultLoad::new(0, 0, 0)),
                lifecycle,
            ),
            Err(expected)
        );
    }

    let replacement = uffd_snapshot(2, 61, 5, 0x8000, 2 * PAGE, MappingKind::AnonymousPrivate);
    assert_eq!(
        UffdFaultPolicy::admit(
            registration,
            replacement,
            request,
            FaultAdmissionContext::exact_request(request),
            FaultLifecycleState::Open,
        ),
        Err(MmError::StaleGeneration)
    );

    let protected = MappingSnapshot::from_raw(
        2,
        61,
        4,
        0x8000,
        2 * PAGE,
        PAGE,
        access(true, false, false).bits(),
        MappingKind::AnonymousPrivate,
        true,
        false,
    )
    .unwrap();
    assert_eq!(
        UffdFaultPolicy::admit(
            registration,
            protected,
            request,
            FaultAdmissionContext::exact_request(request),
            FaultLifecycleState::Open,
        ),
        Err(MmError::AccessDenied)
    );

    let foreign_handler = FaultRequest::new(
        request.key(),
        FaultHandlerId::new(10).unwrap(),
        FaultType::Missing,
    );
    assert_eq!(
        UffdFaultPolicy::admit(
            registration,
            mapping,
            foreign_handler,
            FaultAdmissionContext::exact_request(foreign_handler),
            FaultLifecycleState::Open,
        ),
        Err(MmError::UffdRegistrationMismatch)
    );
}

#[test]
fn userfaultfd_resolver_is_bound_mm_capability_with_signed_prefix_progress() {
    let api = initialized_uffd_api();
    let mapping = uffd_snapshot(3, 70, 1, 0x10_000, 2 * PAGE, MappingKind::AnonymousPrivate);
    let mut table = UffdRegistrationTable::<1>::new(1).unwrap();
    let registered = table
        .register(
            &api,
            uffd_registration_request(20, mapping, mapping.range()),
        )
        .unwrap();
    let destination = PageRange::new(0x10_000, 2 * PAGE, PAGE).unwrap();
    assert_eq!(
        table
            .resolver_registration(mapping.address_space(), destination)
            .unwrap(),
        registered
    );

    assert_eq!(
        UffdCopyMode::from_bits(1 << 2),
        Err(MmError::InvalidUffdCopyMode)
    );
    let copy_wp = UffdCopyMode::from_bits(1 << 1).unwrap();
    assert!(copy_wp.write_protect());
    assert!(!copy_wp.dontwake());
    let copy_wp_dontwake = UffdCopyMode::from_bits((1 << 1) | 1).unwrap();
    assert!(copy_wp_dontwake.write_protect());
    assert!(copy_wp_dontwake.dontwake());
    assert_eq!(
        UffdZeroPageMode::from_bits(1 << 1),
        Err(MmError::InvalidUffdZeroPageMode)
    );

    let copy = UffdCopyRequest::new(
        UserRange::new(0x1801, 2 * PAGE).unwrap(),
        destination,
        UffdCopyMode::from_bits(0).unwrap(),
    )
    .unwrap();
    let partial = UffdResolverResult::for_copy(copy, PAGE).unwrap();
    assert_eq!(partial.reported_bytes(), PAGE as i64);
    assert_eq!(partial.outcome(), UffdResolverOutcome::Retry);
    assert_eq!(
        partial.completed(),
        Some(PageRange::new(0x10_000, PAGE, PAGE).unwrap())
    );
    assert_eq!(partial.wake_range(), partial.completed());

    let zero = UffdZeroPageRequest::new(destination, UffdZeroPageMode::from_bits(1).unwrap());
    let complete = UffdResolverResult::for_zeropage(zero, 2 * PAGE).unwrap();
    assert_eq!(complete.outcome(), UffdResolverOutcome::Complete);
    assert_eq!(complete.reported_bytes(), (2 * PAGE) as i64);
    assert_eq!(complete.wake_range(), None);
    assert_eq!(
        UffdResolverResult::for_zeropage(zero, PAGE / 2),
        Err(MmError::InvalidUffdProgress)
    );
    let failure = UffdResolverResult::failure(14).unwrap();
    assert_eq!(failure.reported_bytes(), -14);
    assert_eq!(failure.outcome(), UffdResolverOutcome::Failed);
    assert_eq!(failure.completed(), None);
    assert_eq!(failure.wake_range(), None);
}

#[test]
fn remap_fragments_share_one_anchor_and_low_address_affine_rebases() {
    let old = PageRange::new(0x8000, 0x4000, PAGE).unwrap();
    let remap = RemapGeometry::new(old, 0x1000, 0x6000).unwrap();
    let first = remap
        .segment(PageRange::new(0x8000, PAGE, PAGE).unwrap())
        .unwrap();
    let second = remap
        .segment(PageRange::new(0xa000, 0x2000, PAGE).unwrap())
        .unwrap();
    assert_eq!(first.destination().start(), 0x1000);
    assert_eq!(second.destination().start(), 0x3000);
    assert_eq!(first.backend_old_start(), old.start());
    assert_eq!(second.backend_old_start(), old.start());
    assert_eq!(first.backend_new_start(), 0x1000);
    assert_eq!(second.backend_new_start(), 0x1000);
    assert_eq!(remap.growth_tail().unwrap().unwrap().start(), 0x5000);

    let relocation = relocate_affine_origin(0x4000, 0x8000, 0x1000).unwrap();
    assert_eq!(relocation.origin(), 0x1000);
    assert_eq!(relocation.backing_advance(), 0x4000);
    assert_eq!(
        remap.segment(PageRange::new(0xc000, PAGE, PAGE).unwrap()),
        Err(MmError::InvalidRemap)
    );
}
