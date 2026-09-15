//! Pure `memfd_create(2)` flag sanitising and `F_ADD_SEALS` arithmetic.
//!
//! Mirrors Linux v7.2.3 `mm/memfd.c:sanitize_flags()`,
//! `check_sysctl_memfd_noexec()`, `alloc_name()`, `memfd_alloc_file()` and
//! `memfd_add_seals()`.

use crate::MmError;

/// `MFD_CLOEXEC`
pub const MFD_CLOEXEC: u32 = 0x0001;
/// `MFD_ALLOW_SEALING`
pub const MFD_ALLOW_SEALING: u32 = 0x0002;
/// `MFD_HUGETLB`
pub const MFD_HUGETLB: u32 = 0x0004;
/// `MFD_NOEXEC_SEAL`
pub const MFD_NOEXEC_SEAL: u32 = 0x0008;
/// `MFD_EXEC`
pub const MFD_EXEC: u32 = 0x0010;
/// `MFD_HUGE_SHIFT`
pub const MFD_HUGE_SHIFT: u32 = 26;
/// `MFD_HUGE_MASK`
pub const MFD_HUGE_MASK: u32 = 0x3f;

/// `MFD_ALL_FLAGS`
pub const MFD_ALL_FLAGS: u32 =
    MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_HUGETLB | MFD_NOEXEC_SEAL | MFD_EXEC;

/// `vm.memfd_noexec` value 0: `MFD_EXEC` implied when neither bit is passed.
pub const MEMFD_NOEXEC_SCOPE_EXEC: u8 = 0;
/// `vm.memfd_noexec` value 1: `MFD_NOEXEC_SEAL` implied when neither bit is
/// passed.
pub const MEMFD_NOEXEC_SCOPE_NOEXEC_SEAL: u8 = 1;
/// `vm.memfd_noexec` value 2: as 1, and an explicit `MFD_EXEC` is rejected.
pub const MEMFD_NOEXEC_SCOPE_NOEXEC_ENFORCED: u8 = 2;

/// `NAME_MAX` on Linux.
pub const MEMFD_NAME_MAX: usize = 255;
/// `MFD_NAME_PREFIX_LEN` for `"memfd:"`.
pub const MFD_NAME_PREFIX_LEN: usize = 6;
/// `MFD_NAME_MAX_LEN`, the longest user-supplied name Linux accepts.
pub const MFD_NAME_MAX_LEN: usize = MEMFD_NAME_MAX - MFD_NAME_PREFIX_LEN;
/// `strncpy_from_user(..., MFD_NAME_MAX_LEN + 1)`: the terminating NUL must lie
/// inside this many bytes, which is exactly what makes `MFD_NAME_MAX_LEN` the
/// longest accepted name.
pub const MFD_NAME_COPY_LEN: usize = MFD_NAME_MAX_LEN + 1;

/// `F_SEAL_SEAL`
pub const F_SEAL_SEAL: u32 = 0x0001;
/// `F_SEAL_SHRINK`
pub const F_SEAL_SHRINK: u32 = 0x0002;
/// `F_SEAL_GROW`
pub const F_SEAL_GROW: u32 = 0x0004;
/// `F_SEAL_WRITE`
pub const F_SEAL_WRITE: u32 = 0x0008;
/// `F_SEAL_FUTURE_WRITE`
pub const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
/// `F_SEAL_EXEC`
pub const F_SEAL_EXEC: u32 = 0x0020;
/// `F_ALL_SEALS`
pub const F_ALL_SEALS: u32 =
    F_SEAL_SEAL | F_SEAL_EXEC | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;

/// Seals that deny writes to the file and to new writable mappings.
///
/// Linux v7.2.3 `mm/memfd.c:is_write_sealed()` and
/// `mm/shmem.c:shmem_write_begin()` both test `F_SEAL_WRITE |
/// F_SEAL_FUTURE_WRITE`; the difference between the two seals is only that
/// adding `F_SEAL_WRITE` must wait for existing writable shared mappings to go
/// away, while `F_SEAL_FUTURE_WRITE` deliberately leaves them writable.
pub const F_WRITE_SEALS: u32 = F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;

/// Sanitised `memfd_create(2)` flags plus the file-creation plan they imply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemfdPlan {
    /// Flags after `check_sysctl_memfd_noexec()` has filled in the implied
    /// exec bit.
    pub flags: u32,
    /// Whether `F_SEAL_SEAL` must be cleared so the file can be sealed.
    pub may_seal: bool,
    /// Whether `F_SEAL_EXEC` is part of the file's initial seals.
    pub noexec_seal: bool,
    /// Whether the request needs a hugetlbfs inode.
    pub hugetlb: bool,
    /// The `MFD_HUGE_*` size log, valid only when `hugetlb` is set.
    pub huge_size_log: u32,
}

impl MemfdPlan {
    /// Whether the initial seal word is exactly `F_SEAL_EXEC` (the
    /// `MFD_NOEXEC_SEAL` case) or `0` (a sealable, executable file).
    pub const fn initial_seals(self) -> u32 {
        if self.noexec_seal { F_SEAL_EXEC } else { 0 }
    }
}

/// Linux v7.2.3 `mm/memfd.c:sanitize_flags()`:
///
/// ```c
/// static int sanitize_flags(unsigned int *flags_ptr)
/// {
/// 	unsigned int flags = *flags_ptr;
///
/// 	if (!(flags & MFD_HUGETLB)) {
/// 		if (flags & ~MFD_ALL_FLAGS)
/// 			return -EINVAL;
/// 	} else {
/// 		/* Allow huge page size encoding in flags. */
/// 		if (flags & ~(MFD_ALL_FLAGS |
/// 				(MFD_HUGE_MASK << MFD_HUGE_SHIFT)))
/// 			return -EINVAL;
/// 	}
///
/// 	/* Invalid if both EXEC and NOEXEC_SEAL are set.*/
/// 	if ((flags & MFD_EXEC) && (flags & MFD_NOEXEC_SEAL))
/// 		return -EINVAL;
///
/// 	return check_sysctl_memfd_noexec(flags_ptr);
/// }
/// ```
///
/// `memfd_noexec_scope` is Linux's `pidns_memfd_noexec_scope()` — the maximum
/// of `vm.memfd_noexec` over the task's pid namespace and all its ancestors.
/// A scope of 2 rejects an explicit `MFD_EXEC` with `-EACCES`; it does *not*
/// reject a request that named neither bit, because the implied
/// `MFD_NOEXEC_SEAL` is filled in before the enforcement test.
pub const fn sanitize_flags(flags: u32, memfd_noexec_scope: u8) -> Result<MemfdPlan, MmError> {
    let allowed = if flags & MFD_HUGETLB == 0 {
        MFD_ALL_FLAGS
    } else {
        MFD_ALL_FLAGS | (MFD_HUGE_MASK << MFD_HUGE_SHIFT)
    };
    if flags & !allowed != 0 {
        return Err(MmError::InvalidMemfdFlags);
    }
    if flags & MFD_EXEC != 0 && flags & MFD_NOEXEC_SEAL != 0 {
        return Err(MmError::InvalidMemfdFlags);
    }

    let mut effective = flags;
    if flags & (MFD_EXEC | MFD_NOEXEC_SEAL) == 0 {
        if memfd_noexec_scope >= MEMFD_NOEXEC_SCOPE_NOEXEC_SEAL {
            effective |= MFD_NOEXEC_SEAL;
        } else {
            effective |= MFD_EXEC;
        }
    }
    if effective & MFD_NOEXEC_SEAL == 0 && memfd_noexec_scope >= MEMFD_NOEXEC_SCOPE_NOEXEC_ENFORCED
    {
        return Err(MmError::MemfdNoexecEnforced);
    }

    let noexec_seal = effective & MFD_NOEXEC_SEAL != 0;
    Ok(MemfdPlan {
        flags: effective,
        // `memfd_alloc_file()`: MFD_NOEXEC_SEAL clears F_SEAL_SEAL as part of
        // installing F_SEAL_EXEC, so it implies sealability even without
        // MFD_ALLOW_SEALING.
        may_seal: noexec_seal || effective & MFD_ALLOW_SEALING != 0,
        noexec_seal,
        hugetlb: effective & MFD_HUGETLB != 0,
        huge_size_log: (effective >> MFD_HUGE_SHIFT) & MFD_HUGE_MASK,
    })
}

/// `close_on_exec` for the resulting descriptor.
pub const fn close_on_exec(plan: MemfdPlan) -> bool {
    plan.flags & MFD_CLOEXEC != 0
}

/// Linux `alloc_name()`'s length rule: `strncpy_from_user()` returns the name
/// length without its terminator, and `len > MFD_NAME_MAX_LEN` is `-EINVAL`.
/// A NUL therefore has to appear within `MFD_NAME_COPY_LEN` bytes.  An empty
/// name is accepted (the dentry is just `"memfd:"`).
pub const fn name_within_limit(len: usize) -> bool {
    len <= MFD_NAME_MAX_LEN
}

/// The requested inode mode of a `memfd_create(2)` file.
///
/// Linux `mm/shmem.c:__shmem_file_setup()` creates `S_IFREG | S_IRWXUGO`
/// without applying the umask, and `mm/memfd.c:memfd_alloc_file()` then clears
/// the execute bits for `MFD_NOEXEC_SEAL`.  The mode is user-visible through
/// `fstat(2)`, so it is part of the ABI.
pub const fn inode_mode(plan: MemfdPlan) -> u16 {
    if plan.noexec_seal { 0o666 } else { 0o777 }
}

/// Outcome of Linux `mm/memfd.c:memfd_add_seals()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddSeals {
    /// Publish the additional seal bits.
    Publish(u32),
    /// The descriptor is not open for writing.
    ReadOnly,
    /// The request contains bits outside `F_ALL_SEALS`.
    UnknownSeal,
    /// The file is not a shmem/hugetlbfs inode.
    NotSealable,
    /// `F_SEAL_SEAL` is already set.
    AlreadySealed,
}

/// Linux v7.2.3 `mm/memfd.c:memfd_add_seals()`, in its exact order:
///
/// ```c
/// if (!(file->f_mode & FMODE_WRITE))
/// 	return -EPERM;
/// if (seals & ~(unsigned int)F_ALL_SEALS)
/// 	return -EINVAL;
/// ...
/// file_seals = memfd_file_seals_ptr(file);
/// if (!file_seals) {
/// 	error = -EINVAL;
/// 	goto unlock;
/// }
/// if (*file_seals & F_SEAL_SEAL) {
/// 	error = -EPERM;
/// 	goto unlock;
/// }
///
/// /*
///  * SEAL_EXEC implies SEAL_WRITE, making W^X from the start.
///  */
/// if (seals & F_SEAL_EXEC && inode->i_mode & 0111)
/// 	seals |= F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_WRITE|F_SEAL_FUTURE_WRITE;
/// ```
///
/// `F_SEAL_WRITE`'s `mapping_deny_writable()`/`memfd_wait_for_pins()` step is
/// reported separately by [`write_seal_needs_quiescence`] because it needs live
/// mapping state rather than the seal word alone.
pub const fn plan_add_seals(
    current: u32,
    requested: u32,
    writable_fd: bool,
    sealable_file: bool,
    inode_has_exec_bits: bool,
) -> AddSeals {
    if !writable_fd {
        return AddSeals::ReadOnly;
    }
    if requested & !F_ALL_SEALS != 0 {
        return AddSeals::UnknownSeal;
    }
    if !sealable_file {
        return AddSeals::NotSealable;
    }
    if current & F_SEAL_SEAL != 0 {
        return AddSeals::AlreadySealed;
    }
    let mut add = requested;
    if add & F_SEAL_EXEC != 0 && inode_has_exec_bits {
        add |= F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;
    }
    AddSeals::Publish(add)
}

/// Whether publishing this seal must first deny and drain writable shared
/// mappings (`mapping_deny_writable()` + `memfd_wait_for_pins()`), which Linux
/// answers with `-EBUSY` when the range is pinned or still writable.
pub const fn write_seal_needs_quiescence(current: u32, add: u32) -> bool {
    add & F_SEAL_WRITE != 0 && current & F_SEAL_WRITE == 0
}
