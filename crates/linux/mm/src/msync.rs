//! `msync(2)` request validation and per-VMA effect ordering.
//!
//! Ground truth is Linux v7.2.3 `mm/msync.c:SYSCALL_DEFINE3(msync, ...)`,
//! whose single loop applies the `MS_INVALIDATE` rejection and the `MS_SYNC`
//! flush VMA by VMA in ascending address order:
//!
//! ```c
//! mmap_read_lock(mm);
//! vma = find_vma(mm, start);
//! for (;;) {
//! 	error = -ENOMEM;
//! 	if (!vma)
//! 		goto out_unlock;
//! 	if (start < vma->vm_start) {
//! 		if (flags == MS_ASYNC)
//! 			goto out_unlock;
//! 		start = vma->vm_start;
//! 		if (start >= end)
//! 			goto out_unlock;
//! 		unmapped_error = -ENOMEM;
//! 	}
//! 	if ((flags & MS_INVALIDATE) && (vma->vm_flags & VM_LOCKED)) {
//! 		error = -EBUSY;
//! 		goto out_unlock;
//! 	}
//! 	file = vma->vm_file;
//! 	fstart = (start - vma->vm_start) + ((loff_t)vma->vm_pgoff << PAGE_SHIFT);
//! 	fend = fstart + (min(end, vma->vm_end) - start) - 1;
//! 	start = vma->vm_end;
//! 	if ((flags & MS_SYNC) && file && (vma->vm_flags & VM_SHARED)) {
//! 		get_file(file);
//! 		mmap_read_unlock(mm);
//! 		error = vfs_fsync_range(file, fstart, fend, 1);
//! 		fput(file);
//! 		if (error || start >= end)
//! 			goto out;
//! 		mmap_read_lock(mm);
//! 		vma = find_vma(mm, start);
//! 	} else {
//! 		if (start >= end) {
//! 			error = 0;
//! 			goto out_unlock;
//! 		}
//! 		vma = find_vma(mm, vma->vm_end);
//! 	}
//! }
//! out_unlock:
//! 	mmap_read_unlock(mm);
//! out:
//! 	return error ? : unmapped_error;
//! ```
//!
//! The order matters: a whole-range `VM_LOCKED` preflight reports `-EBUSY`
//! for a range whose earlier shared file mappings Linux has already flushed
//! by the time it fails.

/// `MS_ASYNC`: no I/O is started; the flag is only a barrier hint.
pub const MS_ASYNC: u32 = 1;
/// `MS_INVALIDATE`: invalidate other mappings of the same file.
pub const MS_INVALIDATE: u32 = 2;
/// `MS_SYNC`: flush the file ranges backing shared mappings and wait.
pub const MS_SYNC: u32 = 4;

/// Linux's flag admission for `msync(2)`:
/// `if (flags & ~(MS_ASYNC | MS_INVALIDATE | MS_SYNC)) goto out;` followed by
/// `if ((flags & MS_ASYNC) && (flags & MS_SYNC)) goto out;`, both `-EINVAL`.
pub const fn msync_flags_valid(flags: u32) -> bool {
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0 {
        return false;
    }
    !(flags & MS_ASYNC != 0 && flags & MS_SYNC != 0)
}

/// The `-errno` Linux's `return error ?: unmapped_error` produces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MsyncResult {
    /// No VMA failed and no hole was skipped.
    Ok,
    /// The walk reached a `VM_LOCKED` VMA under `MS_INVALIDATE`.
    Busy,
    /// The walk skipped at least one hole.
    NoMemory,
}

/// Applies Linux's `error ? : unmapped_error` precedence: a hard `-EBUSY`
/// outranks the `-ENOMEM` recorded for skipped holes, and an `fsync` failure
/// is reported by the caller before this is consulted (`if (error || start >=
/// end) goto out`).
pub const fn msync_result(busy: bool, saw_hole: bool) -> MsyncResult {
    if busy {
        MsyncResult::Busy
    } else if saw_hole {
        MsyncResult::NoMemory
    } else {
        MsyncResult::Ok
    }
}

/// What Linux's loop does at one address, in the order the loop tests it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MsyncStep {
    /// The address is in a hole: `MS_ASYNC` alone returns at once, every
    /// other flag combination records `-ENOMEM` and keeps walking.
    Hole { stop: bool },
    /// `MS_INVALIDATE` met a `VM_LOCKED` VMA: `-EBUSY`, stop walking.
    Busy,
    /// `MS_SYNC` met a shared file mapping: flush it, then keep walking.
    Flush,
    /// No effect at this address; keep walking.
    Continue,
}

/// Classifies one VMA (or the hole in front of it) exactly as the loop body
/// above does.  `locked` is the VMA's `VM_LOCKED` state and `shared_file` is
/// `vma->vm_file && (vma->vm_flags & VM_SHARED)`.
pub const fn msync_step(flags: u32, hole: bool, locked: bool, shared_file: bool) -> MsyncStep {
    if hole {
        // `if (flags == MS_ASYNC) goto out_unlock;` — the comparison is for
        // equality, so `MS_ASYNC | MS_INVALIDATE` keeps walking.
        return MsyncStep::Hole {
            stop: flags == MS_ASYNC,
        };
    }
    if flags & MS_INVALIDATE != 0 && locked {
        return MsyncStep::Busy;
    }
    if flags & MS_SYNC != 0 && shared_file {
        return MsyncStep::Flush;
    }
    MsyncStep::Continue
}
