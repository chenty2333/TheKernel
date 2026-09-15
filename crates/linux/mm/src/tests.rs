use crate::*;

const PAGE: usize = 4096;

#[test]
fn mincore_plan_preserves_linux_access_and_output_order() {
    let user_limit = 0x7fff_ffff_ffff;
    let empty = MincorePlan::new(0, 0, PAGE, user_limit).unwrap();
    assert!(empty.is_empty());
    assert_eq!(empty.page_count(), 0);

    assert_eq!(
        MincorePlan::new(0x1001, 0, PAGE, user_limit),
        Err(MmError::Unaligned)
    );
    assert_eq!(
        MincorePlan::new(user_limit + 1, 0, PAGE, user_limit),
        Err(MmError::AddressOutOfRange)
    );
    assert_eq!(
        MincorePlan::new(user_limit - PAGE + 1, PAGE, PAGE, user_limit),
        Err(MmError::AddressOutOfRange)
    );

    let one = MincorePlan::new(0x4000, 1, PAGE, user_limit).unwrap();
    assert_eq!((one.page_count(), one.rounded_len()), (1, PAGE));
    let two = MincorePlan::new(0x4000, PAGE + 1, PAGE, user_limit).unwrap();
    assert_eq!((two.page_count(), two.rounded_len()), (2, 2 * PAGE));
}

#[test]
fn checked_ranges_reject_zero_overflow_and_unaligned_pages() {
    assert_eq!(UserRange::new(0x1000, 0), Err(MmError::ZeroLength));
    assert_eq!(UserRange::new(usize::MAX, 2), Err(MmError::Overflow));
    assert_eq!(
        UserRange::new_bounded(0x3000, 0x2000, 0x4000),
        Err(MmError::AddressOutOfRange)
    );
    assert_eq!(
        UserRange::new_bounded(0x3000, 0x1000, 0x4000)
            .unwrap()
            .end(),
        0x4000
    );
    assert_eq!(PageSize::new(3000), Err(MmError::InvalidPageSize));
    assert_eq!(PageRange::new(0x1001, PAGE, PAGE), Err(MmError::Unaligned));
    assert_eq!(
        PageRange::new(0x1000, PAGE - 1, PAGE),
        Err(MmError::Unaligned)
    );
}

#[test]
fn page_covering_reports_both_partial_edges() {
    let requested = UserRange::new(0x1801, 0x1000).unwrap();
    let plan = PageCoveringPlan::new(requested, PAGE).unwrap();
    assert_eq!(plan.pages(), PageRange::new(0x1000, 0x2000, PAGE).unwrap());
    assert_eq!(plan.leading_bytes(), 0x801);
    assert_eq!(plan.trailing_bytes(), 0x7ff);
}

#[test]
fn generations_and_affine_origins_never_wrap_or_underflow() {
    assert_eq!(MappingGeneration::new(0), Err(MmError::InvalidIdentity));
    assert_eq!(
        MappingGeneration::new(u64::MAX).unwrap().next(),
        Err(MmError::IdExhausted)
    );

    let rebased = relocate_affine_origin(0x4000, 0x8000, 0x1000).unwrap();
    assert_eq!(rebased.origin(), 0x1000);
    assert_eq!(rebased.backing_advance(), 0x4000);

    let affine = relocate_affine_origin(0x4000, 0x8000, 0x10_000).unwrap();
    assert_eq!(affine.origin(), 0xc000);
    assert_eq!(affine.backing_advance(), 0);
}

#[test]
fn memlock_planner_charges_only_new_bytes() {
    let plan = MemlockPlan::new(0x4000, 0x1000, 0x3000, MemlockLimit::Limited(0x6000)).unwrap();
    assert_eq!(plan.additional_bytes(), 0x2000);
    assert_eq!(plan.total_locked_bytes(), 0x6000);
    assert_eq!(
        MemlockPlan::new(0, 0, PAGE as u64, MemlockLimit::Disabled),
        Err(MmError::MemlockDenied)
    );
    assert_eq!(
        MemlockPlan::new(0, 2, 1, MemlockLimit::Unlimited),
        Err(MmError::InconsistentAccounting)
    );
}

// --- Linux v7.2.3 memory-management ABI alignment -------------------------

const RLIM_INFINITY: u64 = u64::MAX;
const TASK_SIZE: u64 = 0x7fff_ffff_f000;
const MMAP_MIN_ADDR: u64 = 0x1_0000;

#[test]
fn brk_data_rlimit_matches_linux_check_data_rlimit() {
    // include/linux/mm.h:check_data_rlimit(): `(new - start) + (end_data -
    // start_data) > rlim` with `RLIM_INFINITY` skipping the check entirely.
    // The heap's own growth is not bounded by any kernel-internal constant.
    assert_eq!(
        check_data_rlimit(
            RLIM_INFINITY,
            1 << 40,
            0x4000_0000,
            0x4000_0000,
            0x4000_0000
        ),
        Ok(())
    );
    assert_eq!(
        check_data_rlimit(
            0x1000,
            0x4000_0000 + 0x1001,
            0x4000_0000,
            0x4000_0000,
            0x4000_0000
        ),
        Err(MmError::DataRlimitExceeded)
    );
    assert_eq!(
        check_data_rlimit(
            0x1000,
            0x4000_0000 + 0x1000,
            0x4000_0000,
            0x4000_0000,
            0x4000_0000
        ),
        Ok(())
    );
    // A nonzero initial data segment is added to the heap growth.
    assert_eq!(
        check_data_rlimit(
            0x2000,
            0x4000_0000 + 0x1000,
            0x4000_0000,
            0x3fff_0000,
            0x3ffe_0000
        ),
        Err(MmError::DataRlimitExceeded)
    );
    assert_eq!(
        data_segment_size(0x3fff_0000, 0x4000_0000, 0x4000_0000, 0x4000_0000),
        None,
        "new < start_brk is an underflow, never a wrap"
    );
}

#[test]
fn brk_classification_orders_minimum_rlimit_shrink_and_task_size() {
    let heap = 0x4000_0000u64;
    let classify = |requested: u64, current: u64, rlim: u64| {
        classify_brk(
            requested,
            current,
            heap,
            rlim,
            heap,
            heap,
            heap,
            PAGE as u64,
            TASK_SIZE,
            MMAP_MIN_ADDR,
        )
    };

    assert_eq!(
        classify(heap - 1, heap, RLIM_INFINITY),
        Ok(BrkAdmission::BelowMinimum)
    );
    // RLIMIT_DATA is checked before the shrink branch, so a shrink below the
    // old break but above a lowered limit still fails.
    assert_eq!(
        classify(heap + 0x2000, heap + 0x8000, 0x1000),
        Err(MmError::DataRlimitExceeded)
    );
    assert_eq!(
        classify(heap + 0x8000, heap + 0x8000, RLIM_INFINITY),
        Ok(BrkAdmission::Shrink)
    );
    assert_eq!(
        classify(heap + 0x1000, heap + 0x8000, RLIM_INFINITY),
        Ok(BrkAdmission::Shrink)
    );
    // An unaligned request below the old break is a shrink...
    assert_eq!(
        classify(heap + 0xfff, heap + 0x1000, RLIM_INFINITY),
        Ok(BrkAdmission::Shrink)
    );
    // ...while an unaligned request above it rounds up into a real growth.
    assert_eq!(
        classify(heap + 0x1001, heap + 0x1000, RLIM_INFINITY),
        Ok(BrkAdmission::Grow { growth: 0x1000 })
    );
    assert_eq!(
        classify(heap + 0x3000, heap + 0x1000, RLIM_INFINITY),
        Ok(BrkAdmission::Grow { growth: 0x2000 })
    );
    // The 512MiB-era internal cap is gone: 4GiB of heap grows freely.
    assert_eq!(
        classify(heap + 0x1_0000_0000, heap, RLIM_INFINITY),
        Ok(BrkAdmission::Grow {
            growth: 0x1_0000_0000
        })
    );
    // The only remaining address-space bound is TASK_SIZE.
    assert_eq!(
        growth_fits_task_size(TASK_SIZE, TASK_SIZE, MMAP_MIN_ADDR),
        Err(MmError::AddressSpaceExceeded)
    );
    assert_eq!(
        growth_start_in_range(TASK_SIZE - 0x1000, 0x2000, TASK_SIZE),
        Err(MmError::AddressSpaceExceeded)
    );
    assert_eq!(
        classify(heap + (TASK_SIZE - heap), heap, RLIM_INFINITY),
        Ok(BrkAdmission::Grow {
            growth: TASK_SIZE - heap
        })
    );
}

#[test]
fn brk_growth_respects_the_following_vma_guard_gap() {
    // mm/mmap.c:184-191 rejects growth when the first VMA at or after the new
    // break sits inside its own `vm_start_gap()`:
    //   next = vma_find(&vmi, newbrk + PAGE_SIZE + stack_guard_gap);
    //   if (next && newbrk + PAGE_SIZE > vm_start_gap(next)) goto out;
    let brk = 0x4000_0000u64;
    let gap = STACK_GUARD_GAP_DEFAULT;
    assert_eq!(gap, 1 << 20);

    // A GROWSDOWN VMA reserves the whole gap below its start, so growth is
    // refused while `newbrk + PAGE_SIZE` lands inside that reservation...
    let growdown = StartGap::GrowDown;
    let start = brk + 0x20_0000;
    assert!(growth_crosses_next_guard_gap(brk + 0x10_0000, start, growdown, gap));
    // ...and allowed once it stops at the guarded start exactly.
    assert!(!growth_crosses_next_guard_gap(
        start - gap - PAGE as u64,
        start,
        growdown,
        gap
    ));
    // One page higher crosses again: the comparison is `>` not `>=`.
    assert!(growth_crosses_next_guard_gap(
        start - gap,
        start,
        growdown,
        gap
    ));

    // An ordinary VMA reserves nothing, so only a real overlap is refused.
    assert!(!growth_crosses_next_guard_gap(
        start - PAGE as u64,
        start,
        StartGap::None,
        gap
    ));
    assert!(growth_crosses_next_guard_gap(
        start - 0x800,
        start,
        StartGap::None,
        gap
    ));

    // A shadow stack reserves exactly one page.
    assert!(!growth_crosses_next_guard_gap(
        start - 2 * PAGE as u64,
        start,
        StartGap::ShadowStack,
        gap
    ));
    assert!(growth_crosses_next_guard_gap(
        start - PAGE as u64,
        start,
        StartGap::ShadowStack,
        gap
    ));

    // vm_start_gap() saturates at zero instead of wrapping.
    assert_eq!(vm_start_gap(0, StartGap::GrowDown, gap), 0);
    assert_eq!(vm_start_gap(0x1000, StartGap::GrowDown, gap), 0);
    assert_eq!(vm_start_gap(start, StartGap::GrowDown, gap), start - gap);
    assert_eq!(vm_start_gap(start, StartGap::ShadowStack, gap), start - PAGE as u64);
    assert_eq!(vm_start_gap(start, StartGap::None, gap), start);

    // An unrepresentable `newbrk + PAGE_SIZE` is conservative, not a wrap.
    assert!(growth_crosses_next_guard_gap(
        u64::MAX,
        start,
        StartGap::None,
        gap
    ));
}

#[test]
fn mmap_reserve_plan_matches_accountable_mapping() {
    let private_anon = ReserveRequest {
        kind: MappingKind::AnonymousPrivate,
        writable: true,
        map_noreserve: false,
        file_is_hugepages: false,
        droppable: false,
    };
    assert_eq!(
        plan_reserve(private_anon, false),
        ReservePlan {
            noreserve: false,
            accountable: true
        }
    );
    // MAP_NORESERVE is honoured unless the sysctl is OVERCOMMIT_NEVER.
    let noreserve = ReserveRequest {
        map_noreserve: true,
        ..private_anon
    };
    assert_eq!(
        plan_reserve(noreserve, false),
        ReservePlan {
            noreserve: true,
            accountable: false
        }
    );
    assert_eq!(
        plan_reserve(noreserve, true),
        ReservePlan {
            noreserve: false,
            accountable: true
        }
    );
    // ...but hugetlbfs keeps VM_NORESERVE even with OVERCOMMIT_NEVER.
    let hugetlb = ReserveRequest {
        file_is_hugepages: true,
        map_noreserve: true,
        ..private_anon
    };
    assert_eq!(
        plan_reserve(hugetlb, true),
        ReservePlan {
            noreserve: true,
            accountable: false
        }
    );
    // A private writable *file* mapping is accounted exactly like anon.
    let private_file = ReserveRequest {
        kind: MappingKind::FilePrivate,
        ..private_anon
    };
    assert_eq!(
        plan_reserve(private_file, false),
        ReservePlan {
            noreserve: false,
            accountable: true
        }
    );
    // Shared and read-only mappings are never accounted.
    for kind in [MappingKind::AnonymousShared, MappingKind::FileShared] {
        assert_eq!(
            plan_reserve(
                ReserveRequest {
                    kind,
                    ..private_anon
                },
                false
            )
            .accountable,
            false
        );
    }
    assert_eq!(
        plan_reserve(
            ReserveRequest {
                writable: false,
                ..private_anon
            },
            false
        )
        .accountable,
        false
    );
    // MAP_DROPPABLE always sets VM_NORESERVE.
    assert_eq!(
        plan_reserve(
            ReserveRequest {
                droppable: true,
                ..private_anon
            },
            true
        ),
        ReservePlan {
            noreserve: true,
            accountable: false
        }
    );
}

#[test]
fn madvise_advice_table_refuses_unavailable_linux_advice() {
    assert_eq!(Advice::from_raw(MADV_MERGEABLE), Some(Advice::Mergeable));
    assert_eq!(
        Advice::from_raw(MADV_UNMERGEABLE),
        Some(Advice::Unmergeable)
    );
    assert_eq!(
        Advice::from_raw(MADV_SOFT_OFFLINE),
        Some(Advice::SoftOffline)
    );
    assert_eq!(Advice::from_raw(7), None);
    assert_eq!(Advice::from_raw(104), None);

    // CONFIG_KSM=n: mm/madvise.c lists these inside #ifdef CONFIG_KSM, so the
    // syscall answers -EINVAL instead of the 0 that ksm_madvise()'s stub would
    // have returned had it ever been reached.
    assert!(!advice_valid(MADV_MERGEABLE));
    assert!(!advice_valid(MADV_UNMERGEABLE));
    // CONFIG_TRANSPARENT_HUGEPAGE=n: `#ifdef CONFIG_TRANSPARENT_HUGEPAGE`
    // lists MADV_HUGEPAGE, MADV_NOHUGEPAGE and MADV_COLLAPSE, so a kernel
    // whose oracle never sets VMA_HUGEPAGE_BIT answers -EINVAL to all three.
    assert!(!advice_valid(MADV_HUGEPAGE));
    assert!(!advice_valid(MADV_NOHUGEPAGE));
    assert!(!advice_valid(MADV_COLLAPSE));
    // CONFIG_MEMORY_FAILURE=n: `#ifdef CONFIG_MEMORY_FAILURE` lists both, so
    // MADV_HWPOISON may not silently discard a page and MADV_SOFT_OFFLINE may
    // not claim the content-preserving migration this kernel lacks.
    assert!(!advice_valid(MADV_HWPOISON));
    assert!(!advice_valid(MADV_SOFT_OFFLINE));
    for advice in [
        MADV_NORMAL,
        MADV_RANDOM,
        MADV_SEQUENTIAL,
        MADV_WILLNEED,
        MADV_DONTNEED,
        MADV_FREE,
        MADV_REMOVE,
        MADV_DONTNEED_LOCKED,
        MADV_COLD,
        MADV_PAGEOUT,
        MADV_POPULATE_READ,
        MADV_POPULATE_WRITE,
        MADV_DONTFORK,
        MADV_DOFORK,
        MADV_DONTDUMP,
        MADV_DODUMP,
        MADV_WIPEONFORK,
        MADV_KEEPONFORK,
        MADV_GUARD_INSTALL,
        MADV_GUARD_REMOVE,
    ] {
        assert!(advice_valid(advice), "advice {advice} must stay valid");
    }
    assert_eq!(Advice::Cold.raw(), MADV_COLD);
    for advice in [
        Advice::Mergeable,
        Advice::Unmergeable,
        Advice::HugePage,
        Advice::NoHugePage,
        Advice::Collapse,
        Advice::HwPoison,
        Advice::SoftOffline,
    ] {
        assert_eq!(advice.availability(), AdviceAvailability::Unavailable);
    }

    // mm/madvise.c:process_madvise_remote_valid()
    for advice in [MADV_COLD, MADV_PAGEOUT, MADV_WILLNEED, MADV_COLLAPSE] {
        assert!(process_madvise_remote_valid(advice));
    }
    for advice in [
        MADV_MERGEABLE,
        MADV_DONTNEED,
        MADV_FREE,
        MADV_SOFT_OFFLINE,
        MADV_NORMAL,
    ] {
        assert!(!process_madvise_remote_valid(advice));
    }
}

#[test]
fn only_the_two_policy_clearing_advices_are_refused_on_droppable() {
    // mm/madvise.c:madvise_behavior(): `MADV_KEEPONFORK` and `MADV_DODUMP`
    // are the only arms that test VM_DROPPABLE.  Every other advice keeps its
    // ordinary behaviour on a droppable VMA, including the two that *set* the
    // derived policies again (`MADV_WIPEONFORK`, `MADV_DONTDUMP`) and the two
    // that reach the reclaim path (`MADV_COLD`, `MADV_PAGEOUT`).
    assert!(advice_refused_on_droppable(MADV_KEEPONFORK));
    assert!(advice_refused_on_droppable(MADV_DODUMP));

    for advice in [
        MADV_NORMAL,
        MADV_RANDOM,
        MADV_SEQUENTIAL,
        MADV_WILLNEED,
        MADV_DONTNEED,
        MADV_FREE,
        MADV_REMOVE,
        MADV_DONTFORK,
        MADV_DOFORK,
        MADV_DONTDUMP,
        MADV_WIPEONFORK,
        MADV_COLD,
        MADV_PAGEOUT,
        MADV_POPULATE_READ,
        MADV_POPULATE_WRITE,
        MADV_DONTNEED_LOCKED,
        MADV_GUARD_INSTALL,
        MADV_GUARD_REMOVE,
    ] {
        assert!(
            !advice_refused_on_droppable(advice),
            "advice {advice} must keep its behaviour on a droppable VMA"
        );
    }

    // The refusal is independent of availability: both names stay valid
    // advice values, they are merely rejected for this VMA.
    assert!(advice_valid(MADV_KEEPONFORK) && advice_valid(MADV_DODUMP));
    assert!(advice_refused_on_droppable(Advice::KeepOnFork.raw()));
    assert!(advice_refused_on_droppable(Advice::DoDump.raw()));
}

#[test]
fn memfd_sanitize_flags_matches_linux_sysctl_matrix() {
    let scope = |value: u8| value;
    // Unknown bits -> EINVAL; huge-size bits need MFD_HUGETLB.
    assert_eq!(
        sanitize_flags(0x20, scope(0)),
        Err(MmError::InvalidMemfdFlags)
    );
    assert_eq!(
        sanitize_flags(0x0400_0000, scope(0)),
        Err(MmError::InvalidMemfdFlags)
    );
    assert_eq!(
        sanitize_flags(MFD_EXEC | MFD_NOEXEC_SEAL, scope(2)),
        Err(MmError::InvalidMemfdFlags)
    );

    // Neither bit: implied by the sysctl, and value 2 still succeeds.
    let plan = sanitize_flags(MFD_CLOEXEC, scope(0)).unwrap();
    assert!(!plan.noexec_seal);
    assert!(close_on_exec(plan));
    assert_eq!(plan.initial_seals(), 0);
    assert_eq!(inode_mode(plan), 0o777);
    let plan = sanitize_flags(MFD_CLOEXEC, scope(1)).unwrap();
    assert!(plan.noexec_seal);
    assert!(plan.may_seal, "MFD_NOEXEC_SEAL implies sealability");
    assert_eq!(plan.initial_seals(), F_SEAL_EXEC);
    assert_eq!(inode_mode(plan), 0o666);
    let plan = sanitize_flags(MFD_CLOEXEC, scope(2)).unwrap();
    assert!(plan.noexec_seal);

    // Explicit MFD_EXEC is refused only at scope 2.
    assert!(sanitize_flags(MFD_EXEC, scope(1)).is_ok());
    assert_eq!(
        sanitize_flags(MFD_EXEC, scope(2)),
        Err(MmError::MemfdNoexecEnforced)
    );
    // Explicit MFD_NOEXEC_SEAL is accepted at every scope.
    assert!(sanitize_flags(MFD_NOEXEC_SEAL, scope(0)).unwrap().may_seal);
    // MFD_ALLOW_SEALING without NOEXEC_SEAL still clears F_SEAL_SEAL.
    let plan = sanitize_flags(MFD_ALLOW_SEALING, scope(0)).unwrap();
    assert!(plan.may_seal);
    assert!(!plan.noexec_seal);
    assert_eq!(plan.initial_seals(), 0);
    // MFD_EXEC alone keeps F_SEAL_SEAL (not sealable).
    assert!(!sanitize_flags(MFD_EXEC, scope(0)).unwrap().may_seal);

    // Hugetlb size encoding.
    let plan = sanitize_flags(MFD_HUGETLB | (21 << MFD_HUGE_SHIFT), scope(0)).unwrap();
    assert!(plan.hugetlb);
    assert_eq!(plan.huge_size_log, 21);
    // Bits 5..25 stay invalid even with MFD_HUGETLB.
    assert_eq!(
        sanitize_flags(MFD_HUGETLB | 0x20, scope(0)),
        Err(MmError::InvalidMemfdFlags)
    );
    assert_eq!(
        sanitize_flags(MFD_HUGETLB | (1 << 25), scope(0)),
        Err(MmError::InvalidMemfdFlags)
    );

    // alloc_name()'s bound.
    assert!(name_within_limit(0));
    assert!(name_within_limit(MFD_NAME_MAX_LEN));
    assert!(!name_within_limit(MFD_NAME_MAX_LEN + 1));
    assert_eq!(MFD_NAME_MAX_LEN, 249);
    assert_eq!(MFD_NAME_COPY_LEN, 250);
}

#[test]
fn memfd_add_seals_preserves_linux_errno_order_and_exec_implication() {
    // !(f_mode & FMODE_WRITE) -> -EPERM wins over an unknown seal bit.
    assert_eq!(
        plan_add_seals(F_SEAL_SEAL, 0x1000, false, true, false),
        AddSeals::ReadOnly
    );
    // Unknown bits -> -EINVAL wins over the already-sealed check.
    assert_eq!(
        plan_add_seals(F_SEAL_SEAL, 0x1000, true, true, false),
        AddSeals::UnknownSeal
    );
    // Not a shmem/hugetlbfs inode -> -EINVAL.
    assert_eq!(
        plan_add_seals(0, F_SEAL_WRITE, true, false, false),
        AddSeals::NotSealable
    );
    // F_SEAL_SEAL already set -> -EPERM.
    assert_eq!(
        plan_add_seals(F_SEAL_SEAL, F_SEAL_WRITE, true, true, false),
        AddSeals::AlreadySealed
    );
    assert_eq!(
        plan_add_seals(0, F_SEAL_WRITE, true, true, false),
        AddSeals::Publish(F_SEAL_WRITE)
    );
    // SEAL_EXEC on an executable inode implies the write seals.
    assert_eq!(
        plan_add_seals(0, F_SEAL_EXEC, true, true, true),
        AddSeals::Publish(
            F_SEAL_EXEC | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE
        )
    );
    // ...but not on an inode whose exec bits are already clear.
    assert_eq!(
        plan_add_seals(0, F_SEAL_EXEC, true, true, false),
        AddSeals::Publish(F_SEAL_EXEC)
    );
    assert_eq!(F_ALL_SEALS, 0x3f);
    assert!(!write_seal_needs_quiescence(0, F_SEAL_FUTURE_WRITE));
    assert!(write_seal_needs_quiescence(0, F_SEAL_WRITE));
    assert!(!write_seal_needs_quiescence(F_SEAL_WRITE, F_SEAL_WRITE));
}

#[test]
fn userfaultfd_admission_matches_linux_gate_then_mechanism() {
    let facts = |ptrace, sysctl, delivery| UffdAdmissionFacts {
        capable_sys_ptrace: ptrace,
        sysctl_unprivileged_userfaultfd: sysctl,
        kernel_mode_fault_delivery: delivery,
    };
    let user_only = UffdCreateFlags::from_bits(UFFD_USER_MODE_ONLY).unwrap();
    let kernel_faults = UffdCreateFlags::from_bits(UFFD_O_NONBLOCK).unwrap();

    // UFFD_USER_MODE_ONLY is always admitted, whatever the caller is.
    for f in [facts(false, false, false), facts(true, true, true)] {
        assert_eq!(user_only.admit_creation(f), Ok(user_only));
    }
    // Unprivileged without the sysctl: -EPERM, before the unknown-bit check.
    assert_eq!(
        kernel_faults.admit_creation(facts(false, false, true)),
        Err(MmError::AccessDenied)
    );
    // The sysctl admits an unprivileged caller that can deliver kernel faults.
    assert_eq!(
        kernel_faults.admit_creation(facts(false, true, true)),
        Ok(kernel_faults)
    );
    // CAP_SYS_PTRACE admits even with the sysctl off.
    assert_eq!(
        kernel_faults.admit_creation(facts(true, false, true)),
        Ok(kernel_faults)
    );
    // Without kernel-mode delivery the privileged half is refused rather than
    // silently under-delivering.
    assert_eq!(
        kernel_faults.admit_creation(facts(true, false, false)),
        Err(MmError::UnsupportedUffdKernelFaults)
    );
}

#[test]
fn process_mrelease_predicate_matches_task_will_free_mem() {
    let dying_group = FreeMemFacts {
        group_exit: true,
        mm_users_at_most_one: true,
        ..FreeMemFacts::default()
    };
    assert!(task_will_free_mem(dying_group));
    // A single-threaded task that entered do_exit without exit_group:
    // synchronize_group_exit() has already published SIGNAL_GROUP_EXIT, and
    // the returning-to-user branch degrades to the PF_EXITING test.
    let single_exiting = FreeMemFacts {
        thread_group_empty: true,
        pf_exiting: true,
        mm_users_at_most_one: true,
        ..FreeMemFacts::default()
    };
    assert!(task_dying(single_exiting));
    assert!(task_will_free_mem(single_exiting));
    // A coredumping group is never reapable even with SIGNAL_GROUP_EXIT.
    assert!(!task_will_free_mem(FreeMemFacts {
        core_dumping: true,
        ..dying_group
    }));
    // MMF_OOM_SKIP disqualifies the mm.
    assert!(!task_will_free_mem(FreeMemFacts {
        oom_skip: true,
        ..dying_group
    }));
    // SIGNAL_GROUP_EXIT alone is enough: live siblings do not disqualify the
    // mm, because find_lock_task_mm() already found a thread holding it.
    assert!(task_will_free_mem(dying_group_keeplive()));
    // Shared mm: every other user has to be dying too.
    assert!(!task_will_free_mem(FreeMemFacts {
        mm_users_at_most_one: false,
        other_sharers_dying: false,
        ..dying_group
    }));
    assert!(task_will_free_mem(FreeMemFacts {
        mm_users_at_most_one: false,
        other_sharers_dying: true,
        ..dying_group
    }));

    // find_lock_task_mm() failure is -ESRCH and precedes eligibility.
    assert_eq!(eligibility(false, false, false), Err(MmError::NoMmOwner));
    assert_eq!(eligibility(false, true, true), Err(MmError::NoMmOwner));
    assert_eq!(eligibility(true, true, false), Ok(MreleaseEligible::Reap));
    assert_eq!(
        eligibility(true, false, true),
        Ok(MreleaseEligible::AlreadySkipped)
    );
    assert_eq!(
        eligibility(true, false, false),
        Ok(MreleaseEligible::NotDying)
    );
}

fn dying_group_keeplive() -> FreeMemFacts {
    FreeMemFacts {
        group_exit: true,
        thread_group_empty: false,
        pf_exiting: false,
        mm_users_at_most_one: true,
        ..FreeMemFacts::default()
    }
}

#[test]
fn msync_steps_apply_effects_in_linux_vma_order() {
    // mm/msync.c rejects unknown bits and MS_ASYNC together with MS_SYNC.
    assert!(msync_flags_valid(0));
    assert!(msync_flags_valid(MS_ASYNC));
    assert!(msync_flags_valid(MS_ASYNC | MS_INVALIDATE));
    assert!(msync_flags_valid(MS_SYNC | MS_INVALIDATE));
    assert!(!msync_flags_valid(8));
    assert!(!msync_flags_valid(MS_ASYNC | MS_SYNC));
    assert!(!msync_flags_valid(MS_SYNC | MS_INVALIDATE | 0x10));

    // A hole stops exact MS_ASYNC at once, but MS_ASYNC|MS_INVALIDATE keeps
    // walking so a later VM_LOCKED VMA can still produce -EBUSY.
    assert_eq!(
        msync_step(MS_ASYNC, true, false, false),
        MsyncStep::Hole { stop: true }
    );
    assert_eq!(
        msync_step(MS_ASYNC | MS_INVALIDATE, true, false, false),
        MsyncStep::Hole { stop: false }
    );

    // The MS_INVALIDATE rejection is tested per VMA, before that VMA's own
    // flush, so an earlier shared file VMA is already flushed when Linux
    // reports -EBUSY for a later locked one.
    assert_eq!(
        msync_step(MS_SYNC | MS_INVALIDATE, false, false, true),
        MsyncStep::Flush
    );
    assert_eq!(
        msync_step(MS_SYNC | MS_INVALIDATE, false, true, true),
        MsyncStep::Busy
    );
    // Only MS_SYNC flushes, and only a shared file mapping has a file range.
    assert_eq!(
        msync_step(MS_INVALIDATE, false, false, true),
        MsyncStep::Continue
    );
    assert_eq!(
        msync_step(MS_SYNC, false, false, false),
        MsyncStep::Continue
    );

    // error ?: unmapped_error
    assert_eq!(msync_result(false, false), MsyncResult::Ok);
    assert_eq!(msync_result(false, true), MsyncResult::NoMemory);
    assert_eq!(msync_result(true, false), MsyncResult::Busy);
    assert_eq!(msync_result(true, true), MsyncResult::Busy);
}
