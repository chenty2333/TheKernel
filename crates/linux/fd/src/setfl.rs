//! `fcntl(F_SETFL)` admission, mirroring Linux `fs/fcntl.c:setfl()`.
//!
//! Linux's `setfl()` is a short, strictly ordered rule set over the mutable
//! subset of `file->f_flags`:
//!
//! ```text
//! #define SETFL_MASK (O_APPEND | O_NONBLOCK | O_NDELAY | O_DIRECT | O_NOATIME)
//!
//! 	if (((arg ^ filp->f_flags) & O_APPEND) && IS_APPEND(inode))
//! 		return -EPERM;
//! 	if ((arg & O_NOATIME) && !(filp->f_flags & O_NOATIME))
//! 		if (!inode_owner_or_capable(file_mnt_idmap(filp), inode))
//! 			return -EPERM;
//! 	if (O_NONBLOCK != O_NDELAY)
//! 	       if (arg & O_NDELAY)
//! 		   arg |= O_NONBLOCK;
//! 	if (!S_ISFIFO(inode->i_mode) &&
//! 	    (arg & O_DIRECT) &&
//! 	    !(filp->f_mode & FMODE_CAN_ODIRECT))
//! 		return -EINVAL;
//! 	...
//! 	filp->f_flags = (arg & SETFL_MASK) | (filp->f_flags & ~SETFL_MASK);
//! ```
//!
//! The commit is a *replace* of the whole mutable subset: bits inside
//! `SETFL_MASK` that the caller omitted are cleared, and every bit outside it
//! is preserved. `O_NDELAY == O_NONBLOCK` on x86_64/asm-generic, so the SunOS
//! emulation block is behaviourally dead there and is not modelled.

/// `_IOC`-independent open-status bits from `include/uapi/asm-generic/fcntl.h`
/// that `F_SETFL` is allowed to change.
pub const O_APPEND: u32 = 0o2000;
/// `O_NONBLOCK`; numerically identical to `O_NDELAY` on x86_64.
pub const O_NONBLOCK: u32 = 0o4000;
/// `O_NDELAY` from `include/uapi/asm-generic/fcntl.h`. On x86_64 this is the
/// same bit as [`O_NONBLOCK`], which the static assertion below relies on.
pub const O_NDELAY: u32 = O_NONBLOCK;
/// `O_DIRECT` — on a FIFO this selects Linux's packetized pipe mode.
pub const O_DIRECT: u32 = 0o40000;
/// `O_NOATIME`.
pub const O_NOATIME: u32 = 0o1000000;

/// Linux's `SETFL_MASK` (defined in `fs/fcntl.c`, not in a public header).
///
/// Every bit in this mask is settable *and* clearable by `F_SETFL`; every
/// other bit of `f_flags` is preserved verbatim.
pub const SETFL_MASK: u32 = O_APPEND | O_NONBLOCK | O_NDELAY | O_DIRECT | O_NOATIME;

const _: () = assert!(O_NDELAY == O_NONBLOCK, "x86_64 O_NDELAY == O_NONBLOCK");

/// The ordered rejection reasons of `setfl()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetFlError {
    /// `-EPERM`: toggling `O_APPEND` on an append-only inode, or setting
    /// `O_NOATIME` without ownership/`CAP_FOWNER`.
    NotPermitted,
    /// `-EINVAL`: setting `O_DIRECT` on a non-FIFO descriptor that was not
    /// opened with `FMODE_CAN_ODIRECT`.
    InvalidInput,
}

/// Accepted `F_SETFL` transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetFlPlan {
    /// `arg & SETFL_MASK` — the mutable bits the caller selected.
    pub mutable: u32,
    /// The complete new `f_flags` value:
    /// `(arg & SETFL_MASK) | (current & !SETFL_MASK)`.
    pub flags: u32,
}

impl SetFlPlan {
    /// Whether the caller is turning `O_APPEND` on or off.
    pub const fn toggles_append(self) -> bool {
        self.mutable & O_APPEND != 0
    }
}

/// Applies Linux's ordered `setfl()` rules to a requested flag word.
///
/// * `arg` is `(int)arg` from the syscall, widened to the platform's flag
///   word.
/// * `current` is the descriptor's live `f_flags` (`F_GETFL`).
/// * `append_only` is `IS_APPEND(inode)`.
/// * `noatime_owner_or_capable` is `inode_owner_or_capable(mnt_idmap, inode)`.
/// * `can_odirect` is `FMODE_CAN_ODIRECT`.
/// * `is_fifo` is `S_ISFIFO(inode->i_mode)`; FIFOs are exempt from the
///   `O_DIRECT` admission because `O_DIRECT` means packetized mode there.
pub const fn plan_setfl(
    arg: u32,
    current: u32,
    append_only: bool,
    noatime_owner_or_capable: bool,
    can_odirect: bool,
    is_fifo: bool,
) -> Result<SetFlPlan, SetFlError> {
    // O_APPEND cannot be toggled on an append-only inode. Note this fires for
    // a *clear* as well as a set, exactly like Linux.
    if (arg ^ current) & O_APPEND != 0 && append_only {
        return Err(SetFlError::NotPermitted);
    }
    // O_NOATIME can only be *set* by the owner or a CAP_FOWNER holder;
    // clearing it is always allowed.
    if (arg & O_NOATIME) != 0 && (current & O_NOATIME) == 0 && !noatime_owner_or_capable {
        return Err(SetFlError::NotPermitted);
    }
    // Pipe packetized mode is controlled by the O_DIRECT flag, so FIFOs are
    // exempt from the FMODE_CAN_ODIRECT requirement.
    if !is_fifo && (arg & O_DIRECT) != 0 && !can_odirect {
        return Err(SetFlError::InvalidInput);
    }
    let mutable = arg & SETFL_MASK;
    Ok(SetFlPlan {
        mutable,
        flags: mutable | (current & !SETFL_MASK),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMMUTABLE: u32 = 0o100000 | 0o2000000 | 0o4000_0000; // O_LARGEFILE|O_CLOEXEC|O_PATH-ish

    fn allowed(arg: u32, current: u32) -> SetFlPlan {
        plan_setfl(arg, current, false, true, true, false).expect("admitted")
    }

    #[test]
    fn mask_matches_linux_setfl_mask() {
        assert_eq!(SETFL_MASK, 0o2000 | 0o4000 | 0o40000 | 0o1000000);
        // O_NDELAY is a spelling of O_NONBLOCK, so it adds no bit.
        assert_eq!(SETFL_MASK.count_ones(), 4);
    }

    #[test]
    fn commit_replaces_the_mutable_subset_and_preserves_everything_else() {
        let current = O_APPEND | O_NONBLOCK | IMMUTABLE;
        // Omitting O_APPEND from arg clears it; immutable bits survive.
        let plan = allowed(O_DIRECT, current);
        assert_eq!(plan.flags, O_DIRECT | IMMUTABLE);
        assert!(!plan.toggles_append());
        // O_NOATIME is now mutable, unlike the previous partial mask.
        let plan = allowed(O_APPEND | O_NOATIME, current);
        assert_eq!(plan.flags, O_APPEND | O_NOATIME | IMMUTABLE);
        assert!(plan.toggles_append());
        // O_NDELAY alone sets O_NONBLOCK.
        let plan = allowed(O_NDELAY, 0);
        assert_eq!(plan.flags & O_NONBLOCK, O_NONBLOCK);
        // Bits outside SETFL_MASK in arg are ignored, not copied.
        assert_eq!(allowed(u32::MAX, 0).flags, SETFL_MASK);
    }

    #[test]
    fn append_only_inode_rejects_both_directions() {
        // Setting O_APPEND on an append-only inode.
        assert_eq!(
            plan_setfl(O_APPEND, 0, true, true, true, false),
            Err(SetFlError::NotPermitted)
        );
        // Clearing it is rejected too: `(arg ^ f_flags) & O_APPEND` is set.
        assert_eq!(
            plan_setfl(0, O_APPEND, true, true, true, false),
            Err(SetFlError::NotPermitted)
        );
        // Re-asserting the bit that is already there is not a toggle.
        assert!(plan_setfl(O_APPEND, O_APPEND, true, true, true, false).is_ok());
        // An unrelated transition is fine even though O_APPEND stays clear.
        assert!(plan_setfl(O_NONBLOCK, O_NONBLOCK, true, true, true, false).is_ok());
    }

    #[test]
    fn noatime_is_set_only_and_needs_ownership_or_cap_fowner() {
        assert_eq!(
            plan_setfl(O_NOATIME, 0, false, false, true, false),
            Err(SetFlError::NotPermitted)
        );
        // Already set: no permission check, and clearing is always allowed.
        assert!(plan_setfl(O_NOATIME, O_NOATIME, false, false, true, false).is_ok());
        assert!(plan_setfl(0, O_NOATIME, false, false, true, false).is_ok());
        // Owner or CAP_FOWNER admits the set.
        assert_eq!(
            allowed(O_NOATIME, 0).flags,
            O_NOATIME,
            "owner/CAP_FOWNER path must commit the bit"
        );
    }

    #[test]
    fn odirect_requires_fmode_can_odirect_except_on_fifos() {
        assert_eq!(
            plan_setfl(O_DIRECT, 0, false, true, false, false),
            Err(SetFlError::InvalidInput)
        );
        // A FIFO always admits O_DIRECT (packetized mode).
        assert!(plan_setfl(O_DIRECT, 0, false, true, false, true).is_ok());
        // `setfl()` re-tests FMODE_CAN_ODIRECT on every request that carries
        // O_DIRECT, whether or not the flag is already set, so a descriptor
        // that merely *claims* the bit is not a substitute for the mode bit.
        assert_eq!(
            plan_setfl(O_DIRECT, O_DIRECT, false, true, false, false),
            Err(SetFlError::InvalidInput)
        );
        // Clearing O_DIRECT never needs the capability.
        assert!(plan_setfl(0, O_DIRECT, false, true, false, false).is_ok());
    }

    #[test]
    fn rejection_order_is_append_then_noatime_then_odirect() {
        // All three rules violated: Linux reports the O_APPEND EPERM first.
        assert_eq!(
            plan_setfl(O_APPEND | O_NOATIME | O_DIRECT, 0, true, false, false, false),
            Err(SetFlError::NotPermitted)
        );
        // Without the append-only inode, the O_NOATIME EPERM wins over O_DIRECT.
        assert_eq!(
            plan_setfl(O_NOATIME | O_DIRECT, 0, false, false, false, false),
            Err(SetFlError::NotPermitted)
        );
        // With both permissions, only O_DIRECT remains.
        assert_eq!(
            plan_setfl(O_NOATIME | O_DIRECT, 0, false, true, false, false),
            Err(SetFlError::InvalidInput)
        );
    }
}
