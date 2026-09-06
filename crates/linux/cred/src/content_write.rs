//! Pure set-ID cleanup planning for regular-file content mutations.
//!
//! The planner consumes only a normalized inode mode and the embedding
//! kernel's typed `CAP_FSETID` decision. It owns no VFS object, xattr provider,
//! metadata transaction, security-hook registry, or errno mapping.

/// Normalized permission and special-mode bits observed before a content
/// mutation.
///
/// This value contains only the low `0o7777` bits. The embedding VFS remains
/// responsible for proving that the target is a regular file and preserving
/// any filesystem-internal file-type bits.
///
/// ```compile_fail
/// use thekernel_linux_cred::ContentWriteMode;
///
/// // Raw tuple construction is not part of the public contract.
/// let _ = ContentWriteMode(0o6755);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContentWriteMode(u16);

impl ContentWriteMode {
    const ALL_BITS: u16 = 0o7777;

    /// Constructs a mode from normalized permission and special bits.
    ///
    /// Mode zero is valid. File-type and all other unknown bits are rejected.
    pub const fn try_from_bits(bits: u16) -> Option<Self> {
        if bits & !Self::ALL_BITS != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    /// Returns the normalized permission and special-mode bits.
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// The consumer's `CAP_FSETID` decision for one content mutation.
///
/// Select [`Self::CAP_FSETID`] only after the embedding kernel has completed
/// its typed set-ID capability hook against the exact actor and filesystem-
/// owner user namespace frozen for the operation. Keeping the result explicit
/// prevents this policy leaf from consulting a current task or assuming that
/// authority in the actor's own user namespace applies to the filesystem.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContentWriteSetIdAuthority {
    preserves_setid: bool,
}

impl ContentWriteSetIdAuthority {
    /// No successful `CAP_FSETID` decision; Linux set-ID cleanup applies.
    pub const UNPRIVILEGED: Self = Self {
        preserves_setid: false,
    };

    /// The exact operation passed its consumer-owned `CAP_FSETID` hook.
    pub const CAP_FSETID: Self = Self {
        preserves_setid: true,
    };

    const fn preserves_setid(self) -> bool {
        self.preserves_setid
    }
}

/// Exact set-ID mode effect selected for a content mutation.
///
/// This effect describes only bits which were present and will be cleared. A
/// consumer separately owns `security.capability` discovery and removal.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ContentWriteSetIdCleanup {
    /// Keep the mode unchanged.
    Preserve,
    /// Clear only `S_ISUID`.
    ClearSetUserId,
    /// Clear only `S_ISGID`.
    ClearSetGroupId,
    /// Clear both `S_ISUID` and `S_ISGID`.
    ClearSetUserAndGroupId,
}

/// Allocation-free set-ID cleanup plan for one regular-file content mutation.
///
/// The plan retains its checked input and typed capability result so adapters
/// can audit the exact decision before applying [`Self::next_mode`] under
/// their own metadata transaction.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[must_use = "a content-write set-ID plan must be applied or explicitly discarded"]
pub struct ContentWriteSetIdPlan {
    mode: ContentWriteMode,
    authority: ContentWriteSetIdAuthority,
    cleanup: ContentWriteSetIdCleanup,
    next_mode: ContentWriteMode,
}

impl ContentWriteSetIdPlan {
    /// Returns the exact checked mode passed to the planner.
    pub const fn mode(self) -> ContentWriteMode {
        self.mode
    }

    /// Returns the typed `CAP_FSETID` result passed to the planner.
    pub const fn authority(self) -> ContentWriteSetIdAuthority {
        self.authority
    }

    /// Returns the exact set-ID cleanup effect.
    pub const fn cleanup(self) -> ContentWriteSetIdCleanup {
        self.cleanup
    }

    /// Returns the complete normalized mode after the planned cleanup.
    pub const fn next_mode(self) -> ContentWriteMode {
        self.next_mode
    }

    /// Reports whether applying the plan changes the inode mode.
    pub const fn changes_mode(self) -> bool {
        !matches!(self.cleanup, ContentWriteSetIdCleanup::Preserve)
    }
}

/// Plans Linux set-ID cleanup for a regular-file content mutation.
///
/// Successful `CAP_FSETID` authority preserves both special bits. Otherwise
/// `S_ISUID` is removed whenever present, while `S_ISGID` is removed only when
/// `S_IXGRP` is also present. The function is infallible because both inputs
/// are already normalized typed values.
pub const fn plan_content_write_setid_cleanup(
    mode: ContentWriteMode,
    authority: ContentWriteSetIdAuthority,
) -> ContentWriteSetIdPlan {
    const S_ISUID: u16 = 0o4000;
    const S_ISGID: u16 = 0o2000;
    const S_IXGRP: u16 = 0o0010;

    let bits = mode.bits();
    let clear_user = !authority.preserves_setid() && bits & S_ISUID != 0;
    let clear_group =
        !authority.preserves_setid() && bits & (S_ISGID | S_IXGRP) == (S_ISGID | S_IXGRP);
    let cleared = (if clear_user { S_ISUID } else { 0 }) | (if clear_group { S_ISGID } else { 0 });
    let cleanup = match (clear_user, clear_group) {
        (false, false) => ContentWriteSetIdCleanup::Preserve,
        (true, false) => ContentWriteSetIdCleanup::ClearSetUserId,
        (false, true) => ContentWriteSetIdCleanup::ClearSetGroupId,
        (true, true) => ContentWriteSetIdCleanup::ClearSetUserAndGroupId,
    };

    ContentWriteSetIdPlan {
        mode,
        authority,
        cleanup,
        next_mode: ContentWriteMode(bits & !cleared),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(bits: u16) -> ContentWriteMode {
        ContentWriteMode::try_from_bits(bits).unwrap()
    }

    #[test]
    fn checked_mode_rejects_file_type_and_unknown_bits() {
        assert_eq!(ContentWriteMode::try_from_bits(0), Some(mode(0)));
        assert_eq!(ContentWriteMode::try_from_bits(0o7777), Some(mode(0o7777)));
        assert_eq!(ContentWriteMode::try_from_bits(0o100000 | 0o6755), None);
        assert_eq!(ContentWriteMode::try_from_bits(u16::MAX), None);
    }

    #[test]
    fn cap_fsetid_preserves_every_mode() {
        for bits in [0o6755, 0o4755, 0o2755, 0o2644, 0o0755] {
            let plan = plan_content_write_setid_cleanup(
                mode(bits),
                ContentWriteSetIdAuthority::CAP_FSETID,
            );
            assert_eq!(plan.mode(), mode(bits));
            assert_eq!(plan.authority(), ContentWriteSetIdAuthority::CAP_FSETID);
            assert_eq!(plan.cleanup(), ContentWriteSetIdCleanup::Preserve);
            assert_eq!(plan.next_mode(), mode(bits));
            assert!(!plan.changes_mode());
        }
    }

    #[test]
    fn unprivileged_cleanup_reports_exact_effect_and_next_mode() {
        let cases = [
            (
                0o6755,
                ContentWriteSetIdCleanup::ClearSetUserAndGroupId,
                0o0755,
            ),
            (0o4644, ContentWriteSetIdCleanup::ClearSetUserId, 0o0644),
            (0o2755, ContentWriteSetIdCleanup::ClearSetGroupId, 0o0755),
            (0o2644, ContentWriteSetIdCleanup::Preserve, 0o2644),
            (0o0755, ContentWriteSetIdCleanup::Preserve, 0o0755),
        ];

        for (before, cleanup, after) in cases {
            let plan = plan_content_write_setid_cleanup(
                mode(before),
                ContentWriteSetIdAuthority::UNPRIVILEGED,
            );
            assert_eq!(plan.cleanup(), cleanup);
            assert_eq!(plan.next_mode(), mode(after));
            assert_eq!(plan.changes_mode(), before != after);
        }
    }
}
