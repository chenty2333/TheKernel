//! Pure overcommit-reservation derivation for one prospective mapping.
//!
//! Mirrors Linux v7.2.3 `mm/mmap.c:do_mmap()`'s `VM_NORESERVE` derivation and
//! `mm/vma.c:accountable_mapping()`, which together decide whether
//! `__mmap_setup()` calls `security_vm_enough_memory_mm()`.

use crate::MappingKind;

/// `VM_NORESERVE` derivation inputs for one `mmap`-class request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReserveRequest {
    /// Mapping class (`MAP_PRIVATE`/`MAP_SHARED` against an anonymous or file
    /// object).
    pub kind: MappingKind,
    /// Whether the new VMA carries `VM_WRITE`.
    pub writable: bool,
    /// Whether userspace passed `MAP_NORESERVE`.
    pub map_noreserve: bool,
    /// Whether the backing file is a hugetlbfs inode (`is_file_hugepages()`).
    pub file_is_hugepages: bool,
    /// Whether it is also `MAP_DROPPABLE`, which forces `VM_NORESERVE`.
    pub droppable: bool,
}

/// Outcome of the derivation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReservePlan {
    /// The value of `VM_NORESERVE` for the new VMA.
    pub noreserve: bool,
    /// Whether `__mmap_setup()` charges the mapping against the commit limit.
    pub accountable: bool,
}

/// Linux v7.2.3 `mm/mmap.c:do_mmap()`:
///
/// ```c
/// if (flags & MAP_NORESERVE) {
/// 	/* We honor MAP_NORESERVE if allowed to overcommit */
/// 	if (sysctl_overcommit_memory != OVERCOMMIT_NEVER)
/// 		vm_flags |= VM_NORESERVE;
///
/// 	/* hugetlb applies strict overcommit unless MAP_NORESERVE */
/// 	if (file && is_file_hugepages(file))
/// 		vm_flags |= VM_NORESERVE;
/// }
/// ```
///
/// plus the `MAP_DROPPABLE` path above it, which sets
/// `VM_NORESERVE` unconditionally because droppable pages can vanish at any
/// time.  `overcommit_never` is `sysctl_overcommit_memory == OVERCOMMIT_NEVER`
/// (value 2), the mode in which `MAP_NORESERVE` is deliberately ignored.
pub const fn derive_noreserve(request: ReserveRequest, overcommit_never: bool) -> bool {
    if request.droppable {
        return true;
    }
    if request.map_noreserve {
        if !overcommit_never {
            return true;
        }
        if request.file_is_hugepages {
            return true;
        }
    }
    false
}

/// Linux v7.2.3 `mm/vma.c:accountable_mapping()`:
///
/// ```c
/// /*
///  * We account for memory if it's a private writeable mapping,
///  * not hugepages and VM_NORESERVE wasn't set.
///  */
/// static bool accountable_mapping(struct mmap_state *map)
/// {
/// 	const struct file *file = map->file;
///
/// 	/*
/// 	 * hugetlb has its own accounting separate from the core VM
/// 	 * VM_HUGETLB may not be set yet so we cannot check for that flag.
/// 	 */
/// 	if (file && is_file_hugepages(file))
/// 		return false;
///
/// 	return vma_flags_test(&map->vma_flags, VMA_WRITE_BIT) &&
/// 		!vma_flags_test_any(&map->vma_flags, VMA_NORESERVE_BIT,
/// 				    VMA_SHARED_BIT);
/// }
/// ```
///
/// A private writable file mapping is accounted exactly like a private
/// writable anonymous mapping; only `VM_SHARED` and `VM_NORESERVE` exempt it.
pub const fn derive_accountable(request: ReserveRequest, noreserve: bool) -> bool {
    if request.file_is_hugepages {
        return false;
    }
    if !request.writable {
        return false;
    }
    if noreserve {
        return false;
    }
    !matches!(
        request.kind,
        MappingKind::AnonymousShared | MappingKind::FileShared
    )
}

/// Derives both `VM_NORESERVE` and the commit-accounting decision.
pub const fn plan_reserve(request: ReserveRequest, overcommit_never: bool) -> ReservePlan {
    let noreserve = derive_noreserve(request, overcommit_never);
    ReservePlan {
        noreserve,
        accountable: derive_accountable(request, noreserve),
    }
}

/// Whether a mapping class is shared for the purposes of `accountable_mapping`.
pub const fn mapping_kind_is_shared(kind: MappingKind) -> bool {
    matches!(kind, MappingKind::AnonymousShared | MappingKind::FileShared)
}
