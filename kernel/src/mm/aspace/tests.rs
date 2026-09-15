use alloc::vec;
use core::cell::Cell;

use tk_linux_mm::{MappingKind, UFFD_API, UffdApiState, UffdRegisterMode};

use super::*;
use crate::mm::{UffdAddressSpaceState, UffdPollSet};

const TEST_SPACE_SIZE: usize = 0x6000;

#[test]
fn address_space_keeps_the_fixed_pin_ledger_off_stack() {
    assert!(core::mem::size_of::<UserIoPinRegistry>() > PAGE_SIZE_4K);
    assert!(core::mem::size_of::<AddrSpace>() <= PAGE_SIZE_4K);
}

#[test]
fn cold_resident_pages_preserves_anonymous_frames() {
    let start = VirtAddr::from(0x1000);
    let mut aspace =
        AddrSpace::new_empty(VirtAddr::from(0x1000), TEST_SPACE_SIZE - 0x1000).unwrap();
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    aspace
        .map(
            start,
            PAGE_SIZE_4K * 2,
            flags,
            true,
            Backend::new_alloc(start, PageSize::Size4K),
        )
        .unwrap();
    let before = aspace.page_table().query(start).unwrap().0;

    assert_eq!(
        aspace.cold_resident_pages(VirtAddrRange::from_start_size(start, PAGE_SIZE_4K * 2,)),
        Ok(PAGE_SIZE_4K * 2)
    );

    // No-swap advice must not detach private frames: both leaves stay
    // present and retain their original physical identity.
    assert_eq!(aspace.page_table().query(start).unwrap().0, before);
    assert!(aspace.page_table().query(start + PAGE_SIZE_4K).is_ok());
}

fn eligible_collapse_2m_facts() -> Collapse2MCandidateFacts {
    Collapse2MCandidateFacts {
        start: COLLAPSE_2M_SIZE,
        length: COLLAPSE_2M_SIZE,
        vma_covers_range: true,
        private_cow: true,
        has_uffd_write_protect: false,
        has_locked_pages: false,
        has_exact_long_term_cow_pin: false,
        has_fork_policy: false,
    }
}

#[test]
fn collapse_2m_eligibility_requires_one_aligned_private_cow_vma() {
    let facts = eligible_collapse_2m_facts();
    assert!(collapse_2m_candidate_eligible(facts));

    let mut unaligned = facts;
    unaligned.start += PAGE_SIZE_4K;
    assert!(!collapse_2m_candidate_eligible(unaligned));

    let mut partial = facts;
    partial.length -= PAGE_SIZE_4K;
    assert!(!collapse_2m_candidate_eligible(partial));

    let mut crosses_vma = facts;
    crosses_vma.vma_covers_range = false;
    assert!(!collapse_2m_candidate_eligible(crosses_vma));

    let mut non_private = facts;
    non_private.private_cow = false;
    assert!(!collapse_2m_candidate_eligible(non_private));
}

#[test]
fn collapse_2m_eligibility_rejects_each_vma_sidecar_boundary() {
    let facts = eligible_collapse_2m_facts();

    let mut uffd = facts;
    uffd.has_uffd_write_protect = true;
    assert!(!collapse_2m_candidate_eligible(uffd));

    let mut locked = facts;
    locked.has_locked_pages = true;
    assert!(!collapse_2m_candidate_eligible(locked));

    let mut pinned = facts;
    pinned.has_exact_long_term_cow_pin = true;
    assert!(!collapse_2m_candidate_eligible(pinned));

    let mut fork_policy = facts;
    fork_policy.has_fork_policy = true;
    assert!(!collapse_2m_candidate_eligible(fork_policy));
}

#[test]
fn pkey_partial_prot_none_huge_preserves_neighbor_keys() {
    let start = VirtAddr::from(0x4000_0000usize);
    let mut aspace = AddrSpace::new_empty(start, PageSize::Size2M as usize).unwrap();
    aspace
        .pt
        .cursor()
        .map(
            start,
            PhysAddr::from(0usize),
            PageSize::Size2M,
            MappingFlags::empty().with_pkey(3),
        )
        .unwrap();
    let target = start + PAGE_SIZE_4K;
    assert_eq!(
        aspace
            .preflight_set_pkey(target, PAGE_SIZE_4K)
            .unwrap()
            .len(),
        1
    );
    let mut demotion = aspace.prepare_pkey_demotion(target, PAGE_SIZE_4K).unwrap();
    demotion.commit(&mut aspace.pt).unwrap();
    demotion
        .apply_key(&mut aspace.pt, Pkey::new(5).unwrap())
        .unwrap();
    for (offset, key) in [(0, 3), (PAGE_SIZE_4K, 5), (2 * PAGE_SIZE_4K, 3)] {
        let (physical, flags, size) = aspace.pt.query_mapped(start + offset).unwrap();
        assert_eq!(physical, PhysAddr::from(offset));
        assert_eq!(flags, MappingFlags::empty().with_pkey(key));
        assert_eq!(size, PageSize::Size4K);
        assert!(aspace.pt.query(start + offset).is_err());
    }
    aspace
        .pt
        .cursor()
        .drain_mapped_leaves(start, PageSize::Size2M as usize)
        .unwrap();
}

#[test]
fn pkey_partial_1g_middle_and_cross_chunk_geometry() {
    let start = VirtAddr::from(0x4000_0000usize);
    let chunk = PageSize::Size2M as usize;
    for (offset, length) in [
        (17 * chunk + PAGE_SIZE_4K, PAGE_SIZE_4K),
        (17 * chunk + PAGE_SIZE_4K, 3 * chunk),
        (17 * chunk, 3 * chunk),
    ] {
        let mut aspace = AddrSpace::new_empty(start, PageSize::Size1G as usize).unwrap();
        aspace
            .pt
            .cursor()
            .map(
                start,
                PhysAddr::from(0usize),
                PageSize::Size1G,
                MappingFlags::empty().with_pkey(3),
            )
            .unwrap();
        let target = start + offset;
        let mut demotion = aspace.prepare_pkey_demotion(target, length).unwrap();
        demotion.commit(&mut aspace.pt).unwrap();
        demotion
            .apply_key(&mut aspace.pt, Pkey::new(5).unwrap())
            .unwrap();
        for (address, key) in [
            (offset - PAGE_SIZE_4K, 3),
            (offset, 5),
            (offset + length - PAGE_SIZE_4K, 5),
            (offset + length, 3),
        ] {
            let (pa, flags, _) = aspace.pt.query_mapped(start + address).unwrap();
            assert_eq!(pa, PhysAddr::from(address));
            assert_eq!(flags, MappingFlags::empty().with_pkey(key));
        }
        assert_eq!(aspace.pt.query_mapped(start).unwrap().2, PageSize::Size2M);
        aspace
            .pt
            .cursor()
            .drain_mapped_leaves(start, PageSize::Size1G as usize)
            .unwrap();
    }
}

#[test]
fn resident_bytes_counts_sparse_user_vmas_only() {
    let start = VirtAddr::from(0x1000);
    let mut aspace = AddrSpace::new_empty(start, TEST_SPACE_SIZE).unwrap();
    let user = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    aspace
        .map(
            start,
            PAGE_SIZE_4K * 4,
            user,
            false,
            Backend::new_alloc(start, PageSize::Size4K),
        )
        .unwrap();
    aspace
        .populate_area(start + PAGE_SIZE_4K, PAGE_SIZE_4K, user)
        .unwrap();
    let supervisor = start + PAGE_SIZE_4K * 4;
    aspace
        .map(
            supervisor,
            PAGE_SIZE_4K,
            MappingFlags::READ | MappingFlags::WRITE,
            true,
            Backend::new_alloc(supervisor, PageSize::Size4K),
        )
        .unwrap();
    assert!(aspace.pt.query_mapped(supervisor).is_ok());
    assert_eq!(aspace.resident_user_bytes(), PAGE_SIZE_4K);
}

#[test]
fn resident_highwater_is_mm_owned_and_monotonic() {
    let aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), TEST_SPACE_SIZE).unwrap();
    assert_eq!(aspace.merge_resident_highwater(12), 12);
    assert_eq!(aspace.merge_resident_highwater(7), 12);
    assert_eq!(aspace.merge_resident_highwater(19), 19);
}

#[test]
fn populate_preserves_resident_peak_across_retries_and_partial_failure() {
    let start = VirtAddr::from(0x1000);
    let mut aspace = AddrSpace::new_empty(start, TEST_SPACE_SIZE).unwrap();
    let user = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    aspace
        .map(
            start,
            PAGE_SIZE_4K * 2,
            user,
            false,
            Backend::new_alloc(start, PageSize::Size4K),
        )
        .unwrap();
    assert_eq!(aspace.maxrss_kb.load(Ordering::Acquire), 0);

    aspace.populate_area(start, PAGE_SIZE_4K, user).unwrap();
    assert_eq!(aspace.maxrss_kb.load(Ordering::Acquire), 4);
    aspace.populate_area(start, PAGE_SIZE_4K, user).unwrap();
    assert_eq!(aspace.maxrss_kb.load(Ordering::Acquire), 4);

    // The second page is populated before the unmapped third page fails.
    assert_eq!(
        aspace.populate_area(start, PAGE_SIZE_4K * 3, user),
        Err(AxError::NoMemory)
    );
    assert_eq!(aspace.resident_user_bytes(), PAGE_SIZE_4K * 2);
    assert_eq!(aspace.maxrss_kb.load(Ordering::Acquire), 8);
    assert!(aspace.oom_reap_private_pages());
    assert_eq!(aspace.resident_user_bytes(), 0);
    assert_eq!(aspace.maxrss_kb.load(Ordering::Acquire), 8);
}

#[test]
fn oom_reap_drains_private_cow_ptes_but_keeps_vmas() {
    let start = VirtAddr::from(0x1000);
    let mut aspace = AddrSpace::new_empty(start, TEST_SPACE_SIZE).unwrap();
    aspace
        .map(
            start,
            PAGE_SIZE_4K * 2,
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            true,
            Backend::new_alloc(start, PageSize::Size4K),
        )
        .unwrap();
    assert_eq!(aspace.current_mapping_bytes(), PAGE_SIZE_4K * 2);
    assert_eq!(aspace.resident_user_bytes(), PAGE_SIZE_4K * 2);
    aspace
        .pt
        .cursor()
        .protect_region(start, PAGE_SIZE_4K * 2, MappingFlags::empty())
        .unwrap();
    assert!(aspace.pt.query(start).is_err());
    assert_eq!(aspace.resident_user_bytes(), PAGE_SIZE_4K * 2);

    assert!(aspace.oom_reap_private_pages());
    assert_eq!(aspace.current_mapping_bytes(), PAGE_SIZE_4K * 2);
    assert_eq!(aspace.resident_user_bytes(), 0);
    // A completed reaper pass is idempotent at the page-table layer.
    assert!(aspace.oom_reap_private_pages());

    // Completion is not a permanent OOM_SKIP: a later fault generation
    // can populate and then be drained by another pass.
    aspace
        .populate_area(
            start,
            PAGE_SIZE_4K,
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
        )
        .unwrap();
    assert_eq!(aspace.resident_user_bytes(), PAGE_SIZE_4K);
    assert!(aspace.oom_reap_private_pages());
    assert_eq!(aspace.resident_user_bytes(), 0);
}

#[test]
fn oom_reaper_mm_owner_is_exactly_once_and_retryable() {
    let aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), TEST_SPACE_SIZE).unwrap();
    assert_eq!(aspace.begin_oom_reap(), Ok(true));
    assert_eq!(aspace.begin_oom_reap(), Err(AxError::ResourceBusy));
    aspace.finish_oom_reap();
    assert_eq!(aspace.begin_oom_reap(), Ok(true));
    aspace.finish_oom_reap();
    // A new fault generation may be reaped by a subsequent caller.
    assert_eq!(aspace.begin_oom_reap(), Ok(true));
    aspace.finish_oom_reap();
}

#[test]
fn cold_demotes_a_shared_huge_leaf_before_walking_partial_range() {
    let start = VirtAddr::from(COLLAPSE_2M_SIZE);
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let pages = Arc::new(SharedPages::new(COLLAPSE_2M_SIZE, PageSize::Size2M).unwrap());
    let mut aspace = AddrSpace::new_empty(start, COLLAPSE_2M_SIZE).unwrap();
    aspace
        .map(
            start,
            COLLAPSE_2M_SIZE,
            flags,
            true,
            Backend::new_shared(start, pages),
        )
        .unwrap();
    let source = aspace.page_table().query(start).unwrap().0;

    assert_eq!(
        aspace.cold_resident_pages(VirtAddrRange::from_start_size(
            start + PAGE_SIZE_4K,
            PAGE_SIZE_4K
        )),
        Ok(PAGE_SIZE_4K)
    );
    assert_eq!(
        aspace.page_table().query(start).unwrap().2,
        PageSize::Size4K
    );
    assert_eq!(
        aspace.page_table().query(start + PAGE_SIZE_4K).unwrap().0,
        source + PAGE_SIZE_4K
    );

    // This host test has no task context for SharedPages reclamation.
    core::mem::forget(aspace);
}

#[test]
fn wipe_on_fork_child_keeps_parent_seal_without_its_backing() {
    let child = wipe_on_fork_backend(VirtAddr::from(0x4000), PageSize::Size4K, true, false);
    assert!(child.is_sealed());
    assert!(child.is_private_anonymous());
    assert!(child.file_mapping().is_none());
    assert!(!child.is_droppable());
}

#[test]
fn wipe_on_fork_child_keeps_the_parent_droppable_flag() {
    // `dup_mmap()` copies `vm_flags` (`mm/mmap.c:1130-1140`), so the child of a
    // `MAP_DROPPABLE` mapping is droppable too even though its pages are fresh.
    let child = wipe_on_fork_backend(VirtAddr::from(0x4000), PageSize::Size4K, false, true);
    assert!(child.is_droppable());
    assert!(child.is_private_anonymous());
}

#[test]
fn intrinsic_file_like_mapping_is_skipped_by_fork_and_coredump_selection() {
    let _context = crate::test_support::scheduler_test_context();
    let page = PAGE_SIZE_4K;
    let start = VirtAddr::from(0x4000);
    let mut aspace = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
    let excluded = FileLikeMappingLease::new_detached(
        17,
        29,
        start,
        0,
        MappingFlags::USER | MappingFlags::READ,
        MappingFlags::READ,
        FileMappingSharing::Shared,
    )
    .with_excluded_fork_and_dump(true);
    aspace
        .map(
            start,
            page,
            MappingFlags::USER | MappingFlags::READ,
            false,
            Backend::new_linear(start, PhysAddr::from(0x8000), page)
                .with_file_like_mapping(excluded),
        )
        .unwrap();

    assert_eq!(aspace.fork_fragment_count().unwrap(), 0);
    assert!(aspace.coredump_segments().unwrap().is_empty());
}

fn mock_lineage(raw: u64) -> MappingLineage {
    MappingLineage::new(raw).unwrap()
}

fn mock_identity(raw: u64, generation: u64) -> MappingIdentityEntry {
    MappingIdentityEntry {
        lineage: mock_lineage(raw),
        state: MappingIdentityState {
            id: MappingId::new(raw).unwrap(),
            generation: MappingGeneration::new(generation).unwrap(),
        },
    }
}

fn mock_identities(
    entries: impl IntoIterator<Item = MappingIdentityEntry>,
) -> MappingIdentityIndex {
    let entries: Vec<_> = entries.into_iter().collect();
    let mut identities = MappingIdentityIndex::new();
    identities.states.reserve(entries.len());
    for entry in entries {
        identities
            .insert_reserved(entry.lineage, entry.state)
            .unwrap();
    }
    identities
}

#[test]
fn long_term_pin_policy_follows_exact_lower_owner_capability() {
    let start = VirtAddr::from(0x4000);
    let cow = Backend::new_alloc(start, PageSize::Size4K);
    let linear = Backend::new_linear(start, PhysAddr::from(0x8000), PAGE_SIZE_4K);

    assert_eq!(mapping_user_io_pin_policy(&cow), (true, false));
    assert_eq!(mapping_user_io_pin_policy(&linear), (false, false));
}

#[test]
fn anonymous_shared_long_term_pin_does_not_claim_file_writeback() {
    let shared = Backend::new_shared(
        VirtAddr::from(0x4000),
        Arc::new(SharedPages::new(0, PageSize::Size4K).unwrap()),
    );
    assert_eq!(mapping_user_io_pin_policy(&shared), (true, false));

    // SharedPages teardown takes the kernel mutex; the host unit-test
    // environment has no current task even for this zero-page fixture.
    core::mem::forget(shared);
}

#[test]
fn tlb_state_admits_before_generation_repair_and_bounds_membership() {
    let state = TlbState::new();
    assert!(
        state
            .resident_cpus
            .iter()
            .all(|cpu| !cpu.load(Ordering::Relaxed))
    );
    assert!(
        state
            .seen_generations
            .iter()
            .all(|generation| { generation.load(Ordering::Relaxed) == 0 })
    );

    state.generation.store(4, Ordering::SeqCst);
    assert_eq!(state.admit_cpu(0), Some(4));
    assert!(state.resident_cpus[0].load(Ordering::SeqCst));
    assert_eq!(state.seen_generations[0].load(Ordering::SeqCst), 0);

    state.seen_generations[0].store(4, Ordering::SeqCst);
    assert_eq!(state.admit_cpu(0), None);
    assert!(state.resident_cpus[0].load(Ordering::SeqCst));
}

fn map_mock_area(
    areas: &mut MemorySet<MockBackend>,
    page_table: &mut Vec<u8>,
    start: usize,
    size: usize,
    flags: u8,
    backend: u8,
    lineage: MappingLineage,
) {
    areas
        .map(
            MemoryArea::new_with_lineage(
                VirtAddr::from(start),
                size,
                flags,
                MockBackend(backend),
                lineage,
            ),
            page_table,
            false,
        )
        .unwrap();
}

fn initialized_uffd_api() -> UffdApiState {
    let mut api = UffdApiState::new();
    let negotiation = api.prepare_raw(UFFD_API, 0).unwrap();
    api.commit(negotiation).unwrap();
    api
}

fn uffd_test_snapshot(start: usize, size: usize) -> MappingSnapshot {
    MappingSnapshot::from_raw(
        1,
        2,
        17,
        start,
        size,
        PAGE_SIZE_4K,
        MappingAccess::new(true, true, false).bits(),
        MappingKind::AnonymousPrivate,
        true,
        false,
    )
    .unwrap()
}

fn uffd_test_fragment(mapping: MappingSnapshot, range: PageRange) -> MappingSnapshot {
    MappingSnapshot::new(
        mapping.address_space(),
        mapping.mapping(),
        mapping.generation(),
        range,
        mapping.access(),
        mapping.kind(),
        mapping.long_term_pinnable(),
        mapping.writable_file_pin_supported(),
    )
}

#[test]
fn uffd_vma_scan_clamps_a_low_leading_hole_to_the_address_space() {
    let range = PageRange::new(0, 0x4000, PAGE_SIZE_4K).unwrap();
    let scan = uffd_vma_scan_range(
        range,
        VirtAddr::from(0x1000),
        VirtAddr::from(TEST_SPACE_SIZE),
    )
    .unwrap();
    assert_eq!(scan.start, VirtAddr::from(0x1000));
    assert_eq!(scan.end, VirtAddr::from(0x4000));

    let wholly_below = PageRange::new(0, 0x1000, PAGE_SIZE_4K).unwrap();
    assert_eq!(
        uffd_vma_scan_range(
            wholly_below,
            VirtAddr::from(0x1000),
            VirtAddr::from(TEST_SPACE_SIZE),
        ),
        Err(AxError::InvalidInput)
    );
}

#[test]
fn first_uffd_profile_rejects_huge_backend_granules() {
    assert_eq!(
        validate_uffd_missing_backend_granule(PageSize::Size4K),
        Ok(())
    );
    assert_eq!(
        validate_uffd_missing_backend_granule(PageSize::Size2M),
        Err(AxError::InvalidInput)
    );
    assert_eq!(
        validate_uffd_missing_backend_granule(PageSize::Size1G),
        Err(AxError::InvalidInput)
    );
}

#[test]
fn failed_remap_effect_marks_visible_destination_residue_as_changed() {
    assert_eq!(
        classify_failed_remap_effect(false, false),
        RemapTransactionEffect::Preserved
    );
    for (destination_changed, rollback_failed) in [(true, false), (false, true), (true, true)] {
        assert_eq!(
            classify_failed_remap_effect(destination_changed, rollback_failed),
            RemapTransactionEffect::Destructive
        );
    }
}

#[test]
fn existing_lineage_population_failure_reports_visible_residue_as_published() {
    let preserved = classify_existing_lineage_population_failure(AxError::NoMemory, false);
    assert!(!preserved.published());
    assert_eq!(preserved.into_error(), AxError::NoMemory);

    let published = classify_existing_lineage_population_failure(AxError::NoMemory, true);
    assert!(published.published());
    assert_eq!(published.into_error(), AxError::NoMemory);
}

#[test]
fn topology_and_area_mapping_ids_share_one_namespace() {
    let (_, topology_mapping_id, topology_generation, _) = new_user_io_policy().unwrap();
    let (lineage, area_identity) = allocate_mapping_identity().unwrap();
    let (_, next_topology_mapping_id, ..) = new_user_io_policy().unwrap();

    assert_ne!(topology_mapping_id, area_identity.id);
    assert_ne!(area_identity.id, next_topology_mapping_id);
    assert_ne!(topology_mapping_id, next_topology_mapping_id);
    assert_eq!(lineage.get(), area_identity.id.get());
    assert_eq!(topology_generation.get(), 1);
    assert_eq!(area_identity.generation.get(), 1);
}

#[test]
fn fork_preparation_keeps_parent_identity_stable_and_allocates_child_identity() {
    let parent = mock_identity(u64::MAX - 1, 17);
    let parent_before = parent;
    let (child_lineage, child) = allocate_mapping_identity().unwrap();

    assert_eq!(parent, parent_before);
    assert_ne!(child.id, parent.state.id);
    assert_ne!(child_lineage, parent.lineage);
    assert_eq!(child.generation.get(), 1);
}

#[test]
fn advancing_one_lineage_does_not_stale_an_unrelated_mapping() {
    let lineage_a = mock_lineage(2);
    let lineage_b = mock_lineage(3);
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage_a);
    map_mock_area(&mut areas, &mut page_table, 0x3000, 0x1000, 1, 2, lineage_b);
    let mut identities = mock_identities([mock_identity(2, 7), mock_identity(3, 11)]);
    let before_a = mapping_identity(&identities, lineage_a).unwrap();

    let mutations = prepare_mapping_generation_advances_for_range(
        &areas,
        &identities,
        VirtAddr::from(0x3000),
        0x1000,
    )
    .unwrap();
    assert_eq!(
        mutations,
        vec![MappingIdentityMutation::Advance {
            lineage: lineage_b,
            generation: MappingGeneration::new(12).unwrap(),
        }]
    );
    commit_mapping_identity_mutations(&mut identities, &mutations);

    assert_eq!(mapping_identity(&identities, lineage_a).unwrap(), before_a);
    assert_eq!(
        mapping_identity(&identities, lineage_b)
            .unwrap()
            .generation
            .get(),
        12
    );
}

#[test]
fn resident_only_discard_keeps_mapping_identity_and_generation() {
    let lineage = mock_lineage(2);
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
    let identities = mock_identities([mock_identity(2, 9)]);
    let before = mapping_identity(&identities, lineage).unwrap();

    // Model MADV_DONTNEED's residency-only PTE teardown: the VMA and its
    // sidecar are deliberately untouched.
    page_table[0x1000..0x2000].fill(0);

    assert_eq!(mapping_identity(&identities, lineage).unwrap(), before);
    assert_eq!(
        areas.find(VirtAddr::from(0x1000)).unwrap().lineage(),
        lineage
    );
}

#[test]
fn protect_split_keeps_lineage_and_advances_once() {
    let lineage = mock_lineage(2);
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(&mut areas, &mut page_table, 0x1000, 0x3000, 1, 1, lineage);
    let mut identities = mock_identities([mock_identity(2, 1)]);
    let mutations = prepare_mapping_generation_advances_for_range(
        &areas,
        &identities,
        VirtAddr::from(0x2000),
        0x1000,
    )
    .unwrap();

    areas
        .protect(VirtAddr::from(0x2000), 0x1000, |_| Some(3), &mut page_table)
        .unwrap();
    commit_mapping_identity_mutations(&mut identities, &mutations);

    assert_eq!(areas.len(), 3);
    assert!(areas.iter().all(|area| area.lineage() == lineage));
    assert_eq!(
        mapping_identity(&identities, lineage)
            .unwrap()
            .generation
            .get(),
        2
    );
}

#[test]
fn full_unmap_retires_max_generation_but_partial_unmap_is_preflight_rejected() {
    let lineage = mock_lineage(2);
    let mut full_areas = MemorySet::new();
    let mut full_page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(
        &mut full_areas,
        &mut full_page_table,
        0x1000,
        0x3000,
        1,
        1,
        lineage,
    );
    let mut full_identities = mock_identities([mock_identity(2, u64::MAX)]);
    let retire = prepare_unmap_mapping_mutations(
        &full_areas,
        &full_identities,
        VirtAddr::from(0x1000),
        0x3000,
    )
    .unwrap();
    assert_eq!(retire, vec![MappingIdentityMutation::Retire { lineage }]);
    full_areas
        .unmap(VirtAddr::from(0x1000), 0x3000, &mut full_page_table)
        .unwrap();
    commit_mapping_identity_mutations(&mut full_identities, &retire);
    assert!(full_areas.is_empty());
    assert!(full_identities.is_empty());

    let mut partial_areas = MemorySet::new();
    let mut partial_page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(
        &mut partial_areas,
        &mut partial_page_table,
        0x1000,
        0x3000,
        1,
        1,
        lineage,
    );
    let partial_identities = mock_identities([mock_identity(2, u64::MAX)]);
    let before_areas = area_snapshot(&partial_areas);
    let before_page_table = partial_page_table.clone();
    assert_eq!(
        prepare_unmap_mapping_mutations(
            &partial_areas,
            &partial_identities,
            VirtAddr::from(0x2000),
            0x1000,
        ),
        Err(AxError::ResourceBusy)
    );
    assert_eq!(area_snapshot(&partial_areas), before_areas);
    assert_eq!(partial_page_table, before_page_table);
    assert_eq!(
        mapping_identity(&partial_identities, lineage).unwrap(),
        mock_identity(2, u64::MAX).state
    );
}

#[test]
fn unmap_plan_is_deduplicated_and_validates_retired_sidecars_before_mutation() {
    let lineage = mock_lineage(2);
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
    map_mock_area(&mut areas, &mut page_table, 0x3000, 0x1000, 3, 1, lineage);
    let identities = mock_identities([mock_identity(2, 4)]);
    let mutations =
        prepare_unmap_mapping_mutations(&areas, &identities, VirtAddr::from(0x1000), 0x3000)
            .unwrap();
    assert_eq!(mutations, vec![MappingIdentityMutation::Retire { lineage }]);

    let before_areas = area_snapshot(&areas);
    let before_page_table = page_table.clone();
    let no_identities = MappingIdentityIndex::new();
    assert_eq!(
        prepare_unmap_mapping_mutations(&areas, &no_identities, VirtAddr::from(0x1000), 0x3000,),
        Err(AxError::BadState)
    );
    assert_eq!(area_snapshot(&areas), before_areas);
    assert_eq!(page_table, before_page_table);
}

#[test]
fn unmap_plan_advances_a_lineage_that_survives_in_an_unaffected_fragment() {
    let lineage = mock_lineage(2);
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
    map_mock_area(&mut areas, &mut page_table, 0x4000, 0x1000, 3, 1, lineage);
    let identities = mock_identities([mock_identity(2, 8)]);

    let mutations = prepare_unmap_mapping_mutations_for_ranges(
        &areas,
        &identities,
        &[
            VirtAddrRange::new(VirtAddr::from(0x1800), VirtAddr::from(0x2000)),
            VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x1900)),
        ],
    )
    .unwrap();

    assert_eq!(
        mutations,
        [MappingIdentityMutation::Advance {
            lineage,
            generation: MappingGeneration::new(9).unwrap(),
        }]
    );
}

#[test]
fn multi_range_planner_handles_thousands_of_vmas_without_nested_scans() {
    const VMA_COUNT: usize = 2_048;
    const STRIDE: usize = 0x2000;
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; (VMA_COUNT + 1) * STRIDE];
    let mut identities = MappingIdentityIndex::new();
    identities.states.reserve(VMA_COUNT);
    let mut ranges = Vec::with_capacity(VMA_COUNT / 64);

    for index in 0..VMA_COUNT {
        let start = index * STRIDE;
        let lineage = mock_lineage(index as u64 + 2);
        map_mock_area(
            &mut areas,
            &mut page_table,
            start,
            PAGE_SIZE_4K,
            1,
            1,
            lineage,
        );
        identities
            .insert_reserved(lineage, mock_identity(index as u64 + 2, 1).state)
            .unwrap();
        if index.is_multiple_of(64) {
            ranges.push(VirtAddrRange::from_start_size(
                VirtAddr::from(start),
                PAGE_SIZE_4K,
            ));
        }
    }
    ranges.reverse();

    let mutations =
        prepare_unmap_mapping_mutations_for_ranges(&areas, &identities, &ranges).unwrap();
    assert_eq!(mutations.len(), VMA_COUNT / 64);
    assert!(
        mutations
            .iter()
            .all(|mutation| matches!(mutation, MappingIdentityMutation::Retire { .. }))
    );
}

#[test]
fn zero_length_plans_are_identity_noops() {
    let lineage = mock_lineage(2);
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
    let no_identities = MappingIdentityIndex::new();
    assert!(
        prepare_mapping_generation_advances_for_range(
            &areas,
            &no_identities,
            VirtAddr::from(0x1000),
            0,
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        prepare_unmap_mapping_mutations(&areas, &no_identities, VirtAddr::from(0x1000), 0,)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn move_uses_fresh_destination_identity_and_rollback_preserves_source() {
    let source = mock_lineage(2);
    let destination = mock_lineage(3);
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, source);
    let source_state = mock_identity(2, 6);
    let mut identities = mock_identities([source_state, mock_identity(3, 1)]);
    let source_retire =
        prepare_unmap_mapping_mutations(&areas, &identities, VirtAddr::from(0x1000), 0x1000)
            .unwrap();
    map_mock_area(
        &mut areas,
        &mut page_table,
        0x3000,
        0x1000,
        1,
        1,
        destination,
    );
    areas
        .unmap(VirtAddr::from(0x1000), 0x1000, &mut page_table)
        .unwrap();
    commit_mapping_identity_mutations(&mut identities, &source_retire);
    assert!(mapping_identity(&identities, source).is_err());
    assert_eq!(
        mapping_identity(&identities, destination)
            .unwrap()
            .generation
            .get(),
        1
    );

    let mut rollback_areas = MemorySet::new();
    let mut rollback_page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(
        &mut rollback_areas,
        &mut rollback_page_table,
        0x1000,
        0x1000,
        1,
        1,
        source,
    );
    map_mock_area(
        &mut rollback_areas,
        &mut rollback_page_table,
        0x3000,
        0x1000,
        1,
        1,
        destination,
    );
    let mut rollback_identities = mock_identities([source_state, mock_identity(3, 1)]);
    let destination_retire = prepare_unmap_mapping_mutations(
        &rollback_areas,
        &rollback_identities,
        VirtAddr::from(0x3000),
        0x1000,
    )
    .unwrap();
    rollback_areas
        .unmap(VirtAddr::from(0x3000), 0x1000, &mut rollback_page_table)
        .unwrap();
    commit_mapping_identity_mutations(&mut rollback_identities, &destination_retire);
    assert_eq!(
        mapping_identity(&rollback_identities, source).unwrap(),
        source_state.state
    );
    assert!(mapping_identity(&rollback_identities, destination).is_err());

    // old_size == 0 duplication keeps the source and installs one fresh
    // generation-1 destination incarnation.
    let duplicate_identities = mock_identities([source_state, mock_identity(3, 1)]);
    assert_eq!(
        mapping_identity(&duplicate_identities, source).unwrap(),
        source_state.state
    );
    assert_eq!(
        mapping_identity(&duplicate_identities, destination)
            .unwrap()
            .generation
            .get(),
        1
    );
}

#[test]
fn staged_coverage_mismatch_rolls_back_and_out_of_range_lineage_is_rejected() {
    let lineage = mock_lineage(2);
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(&mut areas, &mut page_table, 0x1000, 0x1000, 1, 1, lineage);
    let mut identities = mock_identities([mock_identity(2, 1)]);
    assert!(!lineage_exactly_covers_range(
        &areas,
        lineage,
        VirtAddr::from(0x1000),
        0x2000,
    ));
    assert!(lineage_is_contained_in_range(
        &areas,
        lineage,
        VirtAddr::from(0x1000),
        0x2000,
    ));
    let rollback =
        prepare_unmap_mapping_mutations(&areas, &identities, VirtAddr::from(0x1000), 0x2000)
            .unwrap();
    areas
        .unmap(VirtAddr::from(0x1000), 0x2000, &mut page_table)
        .unwrap();
    commit_mapping_identity_mutations(&mut identities, &rollback);
    assert!(areas.is_empty());
    assert!(identities.is_empty());

    let mut outside = MemorySet::new();
    let mut outside_page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(
        &mut outside,
        &mut outside_page_table,
        0x3000,
        0x1000,
        1,
        1,
        lineage,
    );
    assert!(!lineage_is_contained_in_range(
        &outside,
        lineage,
        VirtAddr::from(0x1000),
        0x2000,
    ));
    assert!(!lineage_exactly_covers_range(
        &outside,
        lineage,
        VirtAddr::from(0x1000),
        0x2000,
    ));
}

#[test]
fn mapping_lineage_limit_is_admitted_before_growth_and_exec_releases_capacity() {
    let mut identities = mock_identities([mock_identity(2, 1), mock_identity(3, 1)]);
    assert_eq!(
        reserve_mapping_identity_slot(&mut identities, 2),
        Err(AxError::NoMemory)
    );
    assert_eq!(identities.len(), 2);

    identities.states.reserve(32);
    assert!(identities.capacity() > 2);
    drop(core::mem::take(&mut identities));
    assert_eq!(identities.len(), 0);
    assert_eq!(identities.capacity(), 0);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MockBackend(u8);

impl memory_set::MappingBackend for MockBackend {
    type Addr = VirtAddr;
    type Flags = u8;
    type PageTable = Vec<u8>;

    fn map(
        &self,
        start: VirtAddr,
        size: usize,
        flags: u8,
        page_table: &mut Self::PageTable,
    ) -> bool {
        let range = start.as_usize()..start.as_usize() + size;
        if page_table[range.clone()].iter().any(|entry| *entry != 0) {
            return false;
        }
        page_table[range].fill(flags);
        true
    }

    fn unmap(&self, start: VirtAddr, size: usize, page_table: &mut Self::PageTable) -> bool {
        let range = start.as_usize()..start.as_usize() + size;
        if page_table[range.clone()].contains(&0) {
            return false;
        }
        page_table[range].fill(0);
        true
    }

    fn protect(
        &self,
        start: VirtAddr,
        size: usize,
        new_flags: u8,
        page_table: &mut Self::PageTable,
    ) -> bool {
        let range = start.as_usize()..start.as_usize() + size;
        if page_table[range.clone()].contains(&0) {
            return false;
        }
        page_table[range].fill(new_flags);
        true
    }

    fn can_merge(&self, other: &Self) -> bool {
        self == other || matches!((self.0, other.0), (10, 11) | (11, 10) | (11, 12) | (12, 11))
    }
}

fn area_snapshot(set: &MemorySet<MockBackend>) -> Vec<(usize, usize, u8, u8)> {
    set.iter()
        .map(|area| {
            (
                area.start().as_usize(),
                area.end().as_usize(),
                area.flags(),
                area.backend().0,
            )
        })
        .collect()
}

fn expect_mapping_error<T>(result: MappingResult<T>) -> memory_set::MappingError {
    match result {
        Ok(value) => {
            drop(value);
            panic!("mapping transaction unexpectedly committed")
        }
        Err(error) => error,
    }
}

#[test]
fn prepared_area_failure_aborts_uffd_and_success_commits_after_mm() {
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(
        &mut areas,
        &mut page_table,
        0x1000,
        0x3000,
        1,
        1,
        mock_lineage(2),
    );

    let mut uffd = *UffdAddressSpaceState::try_new_boxed().unwrap();
    let handler = uffd.attach_handler(Arc::new(UffdPollSet::new())).unwrap();
    let mapping = uffd_test_snapshot(0x1000, 0x3000);
    let mut current = [mapping];
    uffd.register_range(
        &initialized_uffd_api(),
        handler,
        mapping.range(),
        UffdRegisterMode::MISSING,
        &mut current,
    )
    .unwrap();
    let before_registrations: Vec<_> = uffd.registrations.iter().collect();
    let before_areas = area_snapshot(&areas);
    let before_page_table = page_table.clone();

    let plan = uffd
        .preflight_protect(
            0,
            PageRange::new(0x2000, 0x1000, PAGE_SIZE_4K).unwrap(),
            |_, fragment| Ok(Some(uffd_test_fragment(mapping, fragment))),
        )
        .unwrap();
    let synchronized = Cell::new(false);
    let error = expect_mapping_error(commit_area_before_sidecar(
        PreparedAreaProtect {
            areas: &mut areas,
            page_table: &mut page_table,
            start: VirtAddr::from(0x2000),
            end: VirtAddr::from(0x3000),
            ranges: vec![PreparedProtectRange {
                start: VirtAddr::from(0x2000),
                end: VirtAddr::from(0x3000),
                flags: 3,
            }],
            max_areas: 1,
        },
        PreparedUffdMutation::new(&mut uffd, plan),
        || synchronized.set(true),
    ));
    assert_eq!(error, memory_set::MappingError::NoMemory);
    assert!(synchronized.get());
    assert_eq!(area_snapshot(&areas), before_areas);
    assert_eq!(page_table, before_page_table);
    assert_eq!(
        uffd.registrations.iter().collect::<Vec<_>>(),
        before_registrations
    );

    // A second admission proves that the failed main-MM transaction's
    // RAII drop released the bounded UFFD plan slot. The helper below is
    // the same coordinator used by PreparedProtect::commit. Host axhal's
    // dummy address translation cannot safely instantiate a real
    // AddrSpace PageTable; runtime architecture gates cover that final
    // wiring.
    let plan = uffd
        .preflight_protect(
            0,
            PageRange::new(0x2000, 0x1000, PAGE_SIZE_4K).unwrap(),
            |_, fragment| Ok(Some(uffd_test_fragment(mapping, fragment))),
        )
        .unwrap();
    synchronized.set(false);
    let (committed_areas, mutation) = commit_area_before_sidecar(
        PreparedAreaProtect {
            areas: &mut areas,
            page_table: &mut page_table,
            start: VirtAddr::from(0x2000),
            end: VirtAddr::from(0x3000),
            ranges: vec![PreparedProtectRange {
                start: VirtAddr::from(0x2000),
                end: VirtAddr::from(0x3000),
                flags: 3,
            }],
            max_areas: usize::MAX,
        },
        PreparedUffdMutation::new(&mut uffd, plan),
        || synchronized.set(true),
    )
    .unwrap();
    assert!(synchronized.get());
    assert_eq!(
        area_snapshot(committed_areas),
        vec![
            (0x1000, 0x2000, 1, 1),
            (0x2000, 0x3000, 3, 1),
            (0x3000, 0x4000, 1, 1),
        ]
    );
    assert!(page_table[0x2000..0x3000].iter().all(|entry| *entry == 3));
    mutation.commit().finish();

    let mut registrations: Vec<_> = uffd.registrations.iter().collect();
    registrations.sort_by_key(|registration| registration.range().start());
    assert_eq!(registrations.len(), 3);
    assert_eq!(
        registrations[0].range(),
        PageRange::new(0x1000, 0x1000, PAGE_SIZE_4K).unwrap()
    );
    assert_eq!(
        registrations[1].range(),
        PageRange::new(0x2000, 0x1000, PAGE_SIZE_4K).unwrap()
    );
    assert_eq!(
        registrations[2].range(),
        PageRange::new(0x3000, 0x1000, PAGE_SIZE_4K).unwrap()
    );
    assert!(registrations.iter().all(|registration| {
        registration.mapping() == mapping.mapping()
            && registration.generation() == mapping.generation()
    }));
}

#[test]
fn prepared_protect_exposes_all_segments_and_drop_aborts() {
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    areas
        .map(
            MemoryArea::new_with_lineage(
                VirtAddr::from(0x1000),
                0x1000,
                1,
                MockBackend(1),
                mock_lineage(2),
            ),
            &mut page_table,
            false,
        )
        .unwrap();
    areas
        .map(
            MemoryArea::new_with_lineage(
                VirtAddr::from(0x2000),
                0x1000,
                3,
                MockBackend(2),
                mock_lineage(3),
            ),
            &mut page_table,
            false,
        )
        .unwrap();
    let before_areas = area_snapshot(&areas);
    let before_page_table = page_table.clone();

    let plan = PreparedAreaProtect {
        areas: &mut areas,
        page_table: &mut page_table,
        start: VirtAddr::from(0x1800),
        end: VirtAddr::from(0x2800),
        ranges: vec![
            PreparedProtectRange {
                start: VirtAddr::from(0x1800),
                end: VirtAddr::from(0x2000),
                flags: 5,
            },
            PreparedProtectRange {
                start: VirtAddr::from(0x2000),
                end: VirtAddr::from(0x2800),
                flags: 7,
            },
        ],
        max_areas: usize::MAX,
    };
    let segments: Vec<_> = plan
        .segments()
        .map(|(area, affected_start, affected_end, flags)| {
            (
                area.start().as_usize(),
                area.end().as_usize(),
                affected_start.as_usize(),
                affected_end.as_usize(),
                area.flags(),
                area.backend().0,
                flags,
            )
        })
        .collect();
    assert_eq!(
        segments,
        vec![
            (0x1000, 0x2000, 0x1800, 0x2000, 1, 1, 5),
            (0x2000, 0x3000, 0x2000, 0x2800, 3, 2, 7),
        ]
    );

    // A future policy hook may reject after inspecting every segment.
    drop(plan);
    assert_eq!(area_snapshot(&areas), before_areas);
    assert_eq!(page_table, before_page_table);
}

#[test]
fn prepared_protect_segments_skip_disjoint_and_touching_areas() {
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    for (start, flags, backend) in [(0x1000, 1, 1), (0x2000, 3, 2), (0x3000, 5, 3)] {
        map_mock_area(
            &mut areas,
            &mut page_table,
            start,
            0x1000,
            flags,
            backend,
            mock_lineage(2),
        );
    }

    let plan = PreparedAreaProtect {
        areas: &mut areas,
        page_table: &mut page_table,
        start: VirtAddr::from(0x2000),
        end: VirtAddr::from(0x3000),
        ranges: vec![PreparedProtectRange {
            start: VirtAddr::from(0x2000),
            end: VirtAddr::from(0x3000),
            flags: 7,
        }],
        max_areas: usize::MAX,
    };
    let segments: Vec<_> = plan
        .segments()
        .map(|(area, start, end, flags)| {
            (
                area.start().as_usize(),
                start.as_usize(),
                end.as_usize(),
                flags,
            )
        })
        .collect();

    assert_eq!(segments, vec![(0x2000, 0x2000, 0x3000, 7)]);
}

#[test]
fn prepared_protect_commit_splits_and_remerges_areas() {
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    areas
        .map(
            MemoryArea::new_with_lineage(
                VirtAddr::from(0x1000),
                0x3000,
                1,
                MockBackend(1),
                mock_lineage(2),
            ),
            &mut page_table,
            false,
        )
        .unwrap();

    let protect = VirtAddrRange::new(VirtAddr::from(0x2000), VirtAddr::from(0x3000));
    for (address, expected_start, expected_end, expected_flags) in [
        (0x1000, 0x1000, 0x2000, 1),
        (0x2000, 0x2000, 0x3000, 3),
        (0x3000, 0x3000, 0x4000, 1),
    ] {
        let projected =
            AddrSpace::projected_protect_run_at(&areas, protect, 3, VirtAddr::from(address))
                .unwrap();
        assert_eq!(projected.start.as_usize(), expected_start);
        assert_eq!(projected.end.as_usize(), expected_end);
        assert_eq!(projected.flags, expected_flags);
    }

    PreparedAreaProtect {
        areas: &mut areas,
        page_table: &mut page_table,
        start: VirtAddr::from(0x2000),
        end: VirtAddr::from(0x3000),
        ranges: vec![PreparedProtectRange {
            start: VirtAddr::from(0x2000),
            end: VirtAddr::from(0x3000),
            flags: 3,
        }],
        max_areas: usize::MAX,
    }
    .commit()
    .unwrap();
    assert_eq!(
        area_snapshot(&areas),
        vec![
            (0x1000, 0x2000, 1, 1),
            (0x2000, 0x3000, 3, 1),
            (0x3000, 0x4000, 1, 1),
        ]
    );
    assert!(page_table[0x1000..0x2000].iter().all(|entry| *entry == 1));
    assert!(page_table[0x2000..0x3000].iter().all(|entry| *entry == 3));
    assert!(page_table[0x3000..0x4000].iter().all(|entry| *entry == 1));

    for address in [0x1000, 0x2000, 0x3000] {
        let projected =
            AddrSpace::projected_protect_run_at(&areas, protect, 1, VirtAddr::from(address))
                .unwrap();
        assert_eq!(projected.start.as_usize(), 0x1000);
        assert_eq!(projected.end.as_usize(), 0x4000);
        assert_eq!(projected.flags, 1);
    }

    PreparedAreaProtect {
        areas: &mut areas,
        page_table: &mut page_table,
        start: VirtAddr::from(0x2000),
        end: VirtAddr::from(0x3000),
        ranges: vec![PreparedProtectRange {
            start: VirtAddr::from(0x2000),
            end: VirtAddr::from(0x3000),
            flags: 1,
        }],
        max_areas: usize::MAX,
    }
    .commit()
    .unwrap();
    assert_eq!(area_snapshot(&areas), vec![(0x1000, 0x4000, 1, 1)]);
    assert!(page_table[0x1000..0x4000].iter().all(|entry| *entry == 1));
}

#[test]
fn projected_protect_run_respects_backend_and_lineage_barriers() {
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    map_mock_area(
        &mut areas,
        &mut page_table,
        0x1000,
        0x1000,
        1,
        1,
        mock_lineage(2),
    );
    map_mock_area(
        &mut areas,
        &mut page_table,
        0x2000,
        0x1000,
        3,
        2,
        mock_lineage(2),
    );
    map_mock_area(
        &mut areas,
        &mut page_table,
        0x3000,
        0x1000,
        1,
        2,
        mock_lineage(3),
    );

    let projected = AddrSpace::projected_protect_run_at(
        &areas,
        VirtAddrRange::new(VirtAddr::from(0x2000), VirtAddr::from(0x3000)),
        1,
        VirtAddr::from(0x2000),
    )
    .unwrap();
    assert_eq!(projected.start.as_usize(), 0x2000);
    assert_eq!(projected.end.as_usize(), 0x3000);
    assert_eq!(projected.flags, 1);
}

#[test]
fn projected_protect_run_respects_holes_and_flag_barriers() {
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    for (start, flags) in [(0x1000, 1), (0x3000, 1), (0x4000, 2)] {
        map_mock_area(
            &mut areas,
            &mut page_table,
            start,
            0x1000,
            flags,
            1,
            mock_lineage(2),
        );
    }

    let protect = VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x2000));
    let left =
        AddrSpace::projected_protect_run_at(&areas, protect, 1, VirtAddr::from(0x1000)).unwrap();
    assert_eq!(left.start.as_usize(), 0x1000);
    assert_eq!(left.end.as_usize(), 0x2000);

    let right =
        AddrSpace::projected_protect_run_at(&areas, protect, 1, VirtAddr::from(0x3000)).unwrap();
    assert_eq!(right.start.as_usize(), 0x3000);
    assert_eq!(right.end.as_usize(), 0x4000);
    assert_eq!(right.flags, 1);
}

#[test]
fn projected_protect_run_keeps_memory_set_left_backend_across_right_merges() {
    let mut areas = MemorySet::new();
    let mut page_table = vec![0; TEST_SPACE_SIZE];
    for (start, flags, backend) in [(0x1000, 1, 10), (0x2000, 2, 11), (0x3000, 3, 12)] {
        map_mock_area(
            &mut areas,
            &mut page_table,
            start,
            0x1000,
            flags,
            backend,
            mock_lineage(2),
        );
    }

    let protect = VirtAddrRange::new(VirtAddr::from(0x1000), VirtAddr::from(0x4000));
    for (address, start, end, backend) in [
        (0x1000, 0x1000, 0x3000, 10),
        (0x2000, 0x1000, 0x3000, 10),
        (0x3000, 0x3000, 0x4000, 12),
    ] {
        let projected =
            AddrSpace::projected_protect_run_at(&areas, protect, 9, VirtAddr::from(address))
                .unwrap();
        assert_eq!(projected.start.as_usize(), start);
        assert_eq!(projected.end.as_usize(), end);
        assert_eq!(projected.left_area.backend().0, backend);
    }

    PreparedAreaProtect {
        areas: &mut areas,
        page_table: &mut page_table,
        start: VirtAddr::from(0x1000),
        end: VirtAddr::from(0x4000),
        ranges: vec![PreparedProtectRange {
            start: VirtAddr::from(0x1000),
            end: VirtAddr::from(0x4000),
            flags: 9,
        }],
        max_areas: usize::MAX,
    }
    .commit()
    .unwrap();
    assert_eq!(
        area_snapshot(&areas),
        vec![(0x1000, 0x3000, 9, 10), (0x3000, 0x4000, 9, 12)]
    );
}

#[test]
fn projected_protect_run_matches_memory_set_for_nontransitive_backends() {
    let boundaries = [0x1000, 0x2000, 0x3000, 0x4000];
    for left_flags in 1..=3 {
        for middle_flags in 1..=3 {
            for right_flags in 1..=3 {
                for protect_start in 0..3 {
                    for protect_end in (protect_start + 1)..=3 {
                        for new_flags in 1..=3 {
                            let mut areas = MemorySet::new();
                            let mut page_table = vec![0; TEST_SPACE_SIZE];
                            for (start, flags, backend) in [
                                (0x1000, left_flags, 10),
                                (0x2000, middle_flags, 11),
                                (0x3000, right_flags, 12),
                            ] {
                                map_mock_area(
                                    &mut areas,
                                    &mut page_table,
                                    start,
                                    0x1000,
                                    flags,
                                    backend,
                                    mock_lineage(2),
                                );
                            }

                            let protect = VirtAddrRange::new(
                                VirtAddr::from(boundaries[protect_start]),
                                VirtAddr::from(boundaries[protect_end]),
                            );
                            let projected = [0x1000, 0x2000, 0x3000].map(|address| {
                                let run = AddrSpace::projected_protect_run_at(
                                    &areas,
                                    protect,
                                    new_flags,
                                    VirtAddr::from(address),
                                )
                                .unwrap();
                                (
                                    run.start.as_usize(),
                                    run.end.as_usize(),
                                    run.flags,
                                    run.left_area.backend().0,
                                )
                            });

                            PreparedAreaProtect {
                                areas: &mut areas,
                                page_table: &mut page_table,
                                start: protect.start,
                                end: protect.end,
                                ranges: vec![PreparedProtectRange {
                                    start: protect.start,
                                    end: protect.end,
                                    flags: new_flags,
                                }],
                                max_areas: usize::MAX,
                            }
                            .commit()
                            .unwrap();
                            for (index, address) in [0x1000, 0x2000, 0x3000].into_iter().enumerate()
                            {
                                let area = areas.find(VirtAddr::from(address)).unwrap();
                                assert_eq!(
                                    projected[index],
                                    (
                                        area.start().as_usize(),
                                        area.end().as_usize(),
                                        area.flags(),
                                        area.backend().0,
                                    ),
                                    "flags={left_flags}/{middle_flags}/{right_flags}, \
                                     protect={protect_start}..{protect_end}, new={new_flags}, \
                                     address={address:#x}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn execute_publication_is_required_only_when_execute_is_added() {
    let read_write = MappingFlags::READ | MappingFlags::WRITE;
    let read_execute = MappingFlags::READ | MappingFlags::EXECUTE;

    assert!(adds_execute_permission(read_write, read_execute));
    assert!(!adds_execute_permission(read_execute, read_execute));
    assert!(!adds_execute_permission(read_execute, read_write));
    assert!(!adds_execute_permission(read_write, read_write));
}

#[test]
fn present_leaf_repairs_only_faults_already_granted_by_the_pte() {
    let read_execute = MappingFlags::READ | MappingFlags::EXECUTE;
    assert!(present_leaf_satisfies_fault(
        read_execute,
        MappingFlags::READ
    ));
    assert!(present_leaf_satisfies_fault(
        read_execute,
        MappingFlags::EXECUTE
    ));
    assert!(!present_leaf_satisfies_fault(
        read_execute,
        MappingFlags::WRITE
    ));
}

#[test]
fn page_population_failure_preserves_vma_failure_causes() {
    assert_eq!(classify_page_population(Ok(1)), PageFaultResult::Handled);
    assert_eq!(
        classify_page_population(Ok(0)),
        PageFaultResult::Failed(PageFaultFailure::InternalInconsistency)
    );
    assert_eq!(
        classify_page_population(Err(AxError::NoMemory)),
        PageFaultResult::Failed(PageFaultFailure::OutOfMemory)
    );
    assert_eq!(
        classify_page_population(Err(AxError::BadAddress)),
        PageFaultResult::Failed(PageFaultFailure::InternalInconsistency)
    );
    assert_eq!(
        classify_page_population(Err(AxError::Io)),
        PageFaultResult::Failed(PageFaultFailure::BackingUnavailable)
    );
}

#[cfg(target_arch = "x86_64")]
fn test_cet_stack(aspace: &mut AddrSpace, start: VirtAddr, size: usize) {
    aspace
        .map(
            start,
            size,
            MappingFlags::USER
                | MappingFlags::READ
                | MappingFlags::WRITE
                | MappingFlags::SHADOW_STACK,
            false,
            Backend::new_alloc(start, PageSize::Size4K),
        )
        .unwrap();
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cet_default_owners_rebase_for_cleanup_without_authorizing_ssp() {
    let page = PAGE_SIZE_4K;
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x20_000).unwrap();
    let first = VirtAddr::from(0x4000);
    let second = VirtAddr::from(0x8000);
    test_cet_stack(&mut mm, first, page * 2);
    test_cet_stack(&mut mm, second, page);
    // These model two distinct ProcessData values sharing CLONE_VM: both
    // owners are discoverable solely through this one address space.
    mm.register_cet_default_shadow_stack(101, first, page * 2)
        .unwrap();
    mm.register_cet_default_shadow_stack(202, second, page)
        .unwrap();

    let moved = VirtAddr::from(0xc000);
    mm.unmap_areas_with_tlb_grace(first, page * 2).unwrap();
    test_cet_stack(&mut mm, moved, page * 2);
    mm.rebase_cet_default_shadow_stacks_after_mremap(first, page * 2, page * 2, moved, false);
    assert_eq!(mm.cet_default_shadow_stack(101).unwrap().start, moved);
    assert_eq!(mm.cet_default_shadow_stack(202).unwrap().start, second);

    // Shrinking leaves the low VMA fragment live and updates its cleanup
    // extent through the same raw retirement/rebase sequence as mremap.
    mm.unmap_areas_with_tlb_grace(moved + page, page).unwrap();
    mm.rebase_cet_default_shadow_stacks_after_mremap(moved, page * 2, page, moved, false);
    assert_eq!(mm.cet_default_shadow_stack(101).unwrap().size, page);
    mm.unmap(second, page).unwrap().finish();
    mm.map(
        second,
        page,
        MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
        false,
        Backend::new_alloc(second, PageSize::Size4K),
    )
    .unwrap();
    let detached = mm.cet_default_shadow_stack(202).unwrap();
    assert!(detached.extents.is_empty());
    assert_eq!(detached.start, VirtAddr::from(0));
    assert_eq!(detached.size, 0);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cet_default_owner_detach_is_exactly_once() {
    let page = PAGE_SIZE_4K;
    let stack = VirtAddr::from(0x4000);
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
    test_cet_stack(&mut mm, stack, page);
    mm.register_cet_default_shadow_stack(101, stack, page)
        .unwrap();
    let wake = mm.retire_cet_default_shadow_stack(101);
    assert!(mm.take_cet_default_shadow_stack(101).is_none());
    assert!(mm.find_area(stack).is_none());
    wake.finish();
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cet_default_owner_detaches_after_peer_unmap() {
    let page = PAGE_SIZE_4K;
    let stack = VirtAddr::from(0x4000);
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
    test_cet_stack(&mut mm, stack, page);
    mm.register_cet_default_shadow_stack(101, stack, page)
        .unwrap();

    // A CLONE_VM peer can retire the VMA before this task disables CET.
    // Cleanup must consume the owner even though its unmap is now stale.
    mm.unmap(stack, page).unwrap().finish();
    mm.retire_cet_default_shadow_stack(101).finish();
    assert!(mm.cet_default_shadow_stack(101).is_none());
    mm.retire_cet_default_shadow_stack(101).finish();
    assert!(mm.cet_default_shadow_stack(101).is_none());
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cet_default_owner_peer_partial_unmap_does_not_reclaim_reused_explicit_mapping() {
    let page = PAGE_SIZE_4K;
    let stack = VirtAddr::from(0x4000);
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
    test_cet_stack(&mut mm, stack, page * 2);
    mm.register_cet_default_shadow_stack(101, stack, page * 2)
        .unwrap();

    // A CLONE_VM peer tears down the upper half, then uses that address
    // for an explicit map_shadow_stack VMA. The old automatic owner must
    // retain only the lower fragment and exit must not remove the new
    // explicit mapping.
    mm.unmap(stack + page, page).unwrap().finish();
    test_cet_stack(&mut mm, stack + page, page);
    let owner = mm.cet_default_shadow_stack(101).unwrap();
    assert_eq!(
        owner.extents,
        vec![CetDefaultShadowStackExtent {
            start: stack,
            size: page
        }]
    );

    mm.retire_cet_default_shadow_stack(101).finish();
    assert!(mm.find_area(stack).is_none());
    assert!(mm.find_area(stack + page).is_some());
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cet_default_owner_rebases_interior_move_and_duplicate_extents() {
    let page = PAGE_SIZE_4K;
    let stack = VirtAddr::from(0x4000);
    let moved = VirtAddr::from(0x9000);
    let copied = VirtAddr::from(0xc000);
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x20_000).unwrap();
    test_cet_stack(&mut mm, stack, page * 4);
    mm.register_cet_default_shadow_stack(101, stack, page * 4)
        .unwrap();

    // Model an interior two-page move. The owner has source-side head and
    // tail fragments plus the relocated middle, all reclaimed on exit.
    mm.prepare_cet_default_shadow_stacks_for_mremap(stack + page, page * 2, false)
        .unwrap();
    mm.rebase_cet_default_shadow_stacks_after_mremap(
        stack + page,
        page * 2,
        page * 2,
        moved,
        false,
    );
    assert_eq!(
        mm.cet_default_shadow_stack(101).unwrap().extents,
        vec![
            CetDefaultShadowStackExtent {
                start: stack,
                size: page
            },
            CetDefaultShadowStackExtent {
                start: stack + page * 3,
                size: page
            },
            CetDefaultShadowStackExtent {
                start: moved,
                size: page * 2
            },
        ]
    );

    // DONTUNMAP-style duplication preserves every existing extent and
    // adds a cleanup extent at the destination.
    mm.prepare_cet_default_shadow_stacks_for_mremap(moved, page * 2, true)
        .unwrap();
    mm.rebase_cet_default_shadow_stacks_after_mremap(moved, page * 2, page * 2, copied, true);
    assert!(mm.cet_default_shadow_stack(101).unwrap().extents.contains(
        &CetDefaultShadowStackExtent {
            start: copied,
            size: page * 2,
        }
    ));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cet_mprotect_none_preserves_shadow_stack_type() {
    let page = PAGE_SIZE_4K;
    let stack = VirtAddr::from(0x4000);
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
    test_cet_stack(&mut mm, stack, page);
    mm.prepare_protect(stack, page, MappingFlags::USER | MappingFlags::SHADOW_STACK)
        .unwrap()
        .commit()
        .unwrap()
        .finish();
    let flags = mm.find_area(stack).unwrap().flags();
    assert!(flags.contains(MappingFlags::SHADOW_STACK));
    assert!(!flags.intersects(MappingFlags::READ | MappingFlags::WRITE));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cet_default_owner_survives_partial_shadow_stack_rekey_split() {
    let page = PAGE_SIZE_4K;
    let stack = VirtAddr::from(0x4000);
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
    test_cet_stack(&mut mm, stack, page * 2);
    mm.register_cet_default_shadow_stack(101, stack, page * 2)
        .unwrap();

    // This is the VMA half of pkey_mprotect(PROT_READ) over one page:
    // the logical stack now spans differently keyed adjacent fragments.
    mm.prepare_protect(
        stack + page,
        page,
        (MappingFlags::USER | MappingFlags::READ | MappingFlags::SHADOW_STACK).with_pkey(3),
    )
    .unwrap()
    .commit()
    .unwrap()
    .finish();

    assert!(mm.cet_shadow_stack_pointer_valid((stack + page * 2).as_usize() as u64));
    // Fork/vfork registration consumes the same logical extent rather
    // than requiring one unsplit VMA at the recorded start.
    mm.register_cet_default_shadow_stack(202, stack, page * 2)
        .unwrap();
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cet_explicit_shadow_stack_pivot_is_valid_without_default_owner() {
    let page = PAGE_SIZE_4K;
    let stack = VirtAddr::from(0x4000);
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
    test_cet_stack(&mut mm, stack, page);

    // map_shadow_stack(2) mappings are intentionally absent from the
    // automatic-owner registry, yet an SSP pivot to their top is valid.
    assert!(mm.cet_default_shadow_stack(101).is_none());
    assert!(mm.cet_shadow_stack_pointer_valid((stack + page).as_usize() as u64));
    assert!(!mm.cet_shadow_stack_pointer_valid((stack + page + 8).as_usize() as u64));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cet_explicit_shadow_stack_accepts_signal_frame_without_owner() {
    let page = PAGE_SIZE_4K;
    let stack = VirtAddr::from(0x4000);
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
    test_cet_stack(&mut mm, stack, page);

    let saved_ssp = (stack + page).as_usize() as u64;
    let token = saved_ssp | (1 << 63);
    let frame = mm
        .write_cet_signal_frame(saved_ssp, [0x1000, token])
        .unwrap();
    assert_eq!(frame, saved_ssp - 16);
    assert_eq!(mm.read_cet_signal_frame(frame).unwrap(), [0x1000, token]);
    assert_eq!(mm.read_cet_signal_restore_token(frame + 8).unwrap(), token);
    assert!(mm.cet_signal_frame_fits(saved_ssp));
    assert!(!mm.cet_signal_frame_fits(stack.as_usize() as u64 + 8));
    assert!(mm.cet_default_shadow_stack(101).is_none());
}

#[test]
#[cfg(target_arch = "x86_64")]
fn vfork_clone_shares_every_fragment_of_a_rekeyed_shadow_stack_lease() {
    let page = PAGE_SIZE_4K;
    let stack = VirtAddr::from(0x4000);
    let mut mm = AddrSpace::new_empty(VirtAddr::from(0x1000), 0x10_000).unwrap();
    test_cet_stack(&mut mm, stack, page * 2);
    mm.populate_area(
        stack,
        page * 2,
        MappingFlags::SHADOW_STACK | MappingFlags::WRITE,
    )
    .unwrap();
    mm.register_cet_default_shadow_stack(101, stack, page * 2)
        .unwrap();

    // Model pkey_mprotect(PROT_READ) on only the upper page. The prepared
    // protection changes the VMA half; the syscall then publishes the
    // same key in every resident leaf after that commit succeeds.
    mm.prepare_protect(
        stack + page,
        page,
        (MappingFlags::USER | MappingFlags::READ | MappingFlags::SHADOW_STACK).with_pkey(3),
    )
    .unwrap()
    .commit()
    .unwrap()
    .finish();
    {
        let mut page_table = mm.page_table_mut().cursor();
        page_table
            .set_pkey(stack + page, Pkey::new(3).unwrap())
            .unwrap();
    }
    let owner = mm.cet_default_shadow_stack(101).unwrap();

    let vfork_child = mm
        .try_clone_with_shared_shadow_stack(Some(owner.clone()))
        .unwrap();
    let mut vfork_child = vfork_child.lock();
    vfork_child
        .register_borrowed_cet_default_shadow_stack_extents(202, owner.extents.clone())
        .unwrap();
    for offset in [0, page] {
        let parent_leaf = mm.page_table().query(stack + offset).unwrap();
        let child_leaf = vfork_child.page_table().query(stack + offset).unwrap();
        let parent_pkey = mm.page_table_mut().cursor().pkey(stack + offset).unwrap();
        let child_pkey = vfork_child
            .page_table_mut()
            .cursor()
            .pkey(stack + offset)
            .unwrap();
        assert_eq!(child_leaf.0, parent_leaf.0);
        assert_eq!(child_leaf.1, parent_leaf.1);
        assert_eq!(child_pkey, parent_pkey);
        if offset == 0 {
            assert!(child_leaf.1.contains(MappingFlags::SHADOW_STACK));
            assert_eq!(parent_pkey.0.get(), 0);
        } else {
            assert!(!child_leaf.1.contains(MappingFlags::SHADOW_STACK));
            assert_eq!(parent_pkey.0.get(), 3);
        }
    }
    drop(vfork_child);

    // The same fragmented source still takes the ordinary fork COW
    // path; only an authenticated vfork lease may preserve CET leaves.
    let cow_child = mm.try_clone().unwrap();
    let cow_child = cow_child.lock();
    for offset in [0, page] {
        assert!(
            !cow_child
                .page_table()
                .query(stack + offset)
                .unwrap()
                .1
                .contains(MappingFlags::SHADOW_STACK)
        );
    }
}
