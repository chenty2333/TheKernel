//! Pure Linux key permission selection over one frozen credential snapshot.
//!
//! This module owns no key serial lookup, possession graph, keyring storage,
//! request-key authority, security-hook dispatch, or errno mapping. An
//! embedding kernel determines whether the actor possesses the exact key and
//! then passes that fact together with the key's decision-frozen owner IDs and
//! permission mask to [`KeyPermissionMask::allows`].

use core::ops::{BitOr, BitOrAssign};

use crate::{FsCredentialSnapshot, Kgid, Kuid};

/// Non-empty normalized access requested from a Linux key permission check.
///
/// The low six bits follow Linux's view, read, write, search, link, and
/// setattr ordering, but this remains a crate-owned typed value rather than a
/// raw `KEY_*` UAPI mask. Multiple rights may be combined when one operation
/// requires all of them.
///
/// ```compile_fail
/// use thekernel_linux_cred::KeyPermission;
///
/// // Raw tuple construction is not part of the public contract.
/// let _ = KeyPermission(1);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyPermission(u8);

impl KeyPermission {
    const VIEW_BIT: u8 = 1 << 0;
    const READ_BIT: u8 = 1 << 1;
    const WRITE_BIT: u8 = 1 << 2;
    const SEARCH_BIT: u8 = 1 << 3;
    const LINK_BIT: u8 = 1 << 4;
    const SETATTR_BIT: u8 = 1 << 5;
    const ALL_BITS: u8 = Self::VIEW_BIT
        | Self::READ_BIT
        | Self::WRITE_BIT
        | Self::SEARCH_BIT
        | Self::LINK_BIT
        | Self::SETATTR_BIT;

    /// View the key's attributes.
    pub const VIEW: Self = Self(Self::VIEW_BIT);
    /// Read the key payload or enumerate a keyring.
    pub const READ: Self = Self(Self::READ_BIT);
    /// Update the key payload or add a link to a keyring.
    pub const WRITE: Self = Self(Self::WRITE_BIT);
    /// Search for the key or traverse a keyring.
    pub const SEARCH: Self = Self(Self::SEARCH_BIT);
    /// Create a link to the key or keyring.
    pub const LINK: Self = Self(Self::LINK_BIT);
    /// Change the key's ownership, permission, timeout, or other attributes.
    pub const SETATTR: Self = Self(Self::SETATTR_BIT);
    /// Every access kind represented by this contract.
    pub const ALL: Self = Self(Self::ALL_BITS);

    /// Constructs a non-empty combination from crate-local normalized bits.
    ///
    /// Returns `None` for an empty request or when any unknown bit is present.
    pub const fn try_from_bits(bits: u8) -> Option<Self> {
        if bits == 0 || bits & !Self::ALL_BITS != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    /// Returns the crate-local normalized bit representation.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Reports whether every access kind in `other` is present in `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Reports whether any access kind in `other` is present in `self`.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Combines two non-empty access requests.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOr for KeyPermission {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl BitOrAssign for KeyPermission {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

/// Validated Linux `key_perm_t` permission lanes.
///
/// Linux stores identical six-bit permission sets for other, group, user, and
/// possessor in byte lanes 0 through 3. The top two bits of every lane are
/// reserved and rejected here. A zero mask is valid and denies every access.
///
/// ```compile_fail
/// use thekernel_linux_cred::KeyPermissionMask;
///
/// // Raw tuple construction is not part of the public contract.
/// let _ = KeyPermissionMask(0x3f00_0000);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyPermissionMask(u32);

impl KeyPermissionMask {
    const LANE_BITS: u32 = KeyPermission::ALL.bits() as u32;
    const OTHER_SHIFT: u32 = 0;
    const GROUP_SHIFT: u32 = 8;
    const USER_SHIFT: u32 = 16;
    const POSSESSOR_SHIFT: u32 = 24;
    const VALID_BITS: u32 = Self::LANE_BITS << Self::OTHER_SHIFT
        | Self::LANE_BITS << Self::GROUP_SHIFT
        | Self::LANE_BITS << Self::USER_SHIFT
        | Self::LANE_BITS << Self::POSSESSOR_SHIFT;

    /// Constructs a mask from typed possessor, user, group, and other lanes.
    ///
    /// `None` represents an empty lane. This is infallible because every
    /// nonempty lane is already a validated [`KeyPermission`].
    pub const fn from_lanes(
        possessor: Option<KeyPermission>,
        user: Option<KeyPermission>,
        group: Option<KeyPermission>,
        other: Option<KeyPermission>,
    ) -> Self {
        Self(
            Self::lane_raw(possessor, Self::POSSESSOR_SHIFT)
                | Self::lane_raw(user, Self::USER_SHIFT)
                | Self::lane_raw(group, Self::GROUP_SHIFT)
                | Self::lane_raw(other, Self::OTHER_SHIFT),
        )
    }

    /// Constructs a validated permission mask from Linux's raw layout.
    ///
    /// Returns `None` when a reserved bit in any permission lane is set.
    pub const fn try_from_raw(raw: u32) -> Option<Self> {
        if raw & !Self::VALID_BITS != 0 {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// Returns Linux's raw `key_perm_t` representation.
    pub const fn into_raw(self) -> u32 {
        self.0
    }

    /// Tests the Linux key permission-bit gate for one actor and key.
    ///
    /// Exactly one identity lane is selected: user when the actor's filesystem
    /// UID owns the key, otherwise group when the key has a valid group owner,
    /// that lane is nonempty, and either the filesystem GID or a supplementary
    /// GID matches, otherwise other. `None` represents Linux's `INVALID_GID`
    /// and can never select the group lane. When the embedding kernel has
    /// proven possession of the exact key, possessor rights are added to that
    /// one selected identity lane. Final security-module arbitration remains
    /// consumer-owned.
    pub fn allows(
        self,
        owner_uid: Kuid,
        owner_gid: Option<Kgid>,
        actor: &FsCredentialSnapshot,
        possessed: bool,
        requested: KeyPermission,
    ) -> bool {
        let identity_shift = if actor.uid() == owner_uid {
            Self::USER_SHIFT
        } else if self.lane(Self::GROUP_SHIFT) != 0
            && owner_gid.is_some_and(|owner_gid| {
                actor.gid() == owner_gid
                    || actor
                        .supplementary_groups()
                        .binary_search(&owner_gid)
                        .is_ok()
            })
        {
            Self::GROUP_SHIFT
        } else {
            Self::OTHER_SHIFT
        };
        let mut granted = self.lane(identity_shift);
        if possessed {
            granted |= self.lane(Self::POSSESSOR_SHIFT);
        }
        granted & requested.bits() == requested.bits()
    }

    fn lane(self, shift: u32) -> u8 {
        ((self.0 >> shift) & Self::LANE_BITS) as u8
    }

    const fn lane_raw(permission: Option<KeyPermission>, shift: u32) -> u32 {
        match permission {
            Some(permission) => (permission.bits() as u32) << shift,
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CAPABILITY_WORDS, GroupInfo};

    const OTHER: u32 = 0;
    const GROUP: u32 = 8;
    const USER: u32 = 16;
    const POSSESSOR: u32 = 24;

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    fn actor(uid: u32, gid: u32, groups: &[u32]) -> FsCredentialSnapshot {
        let groups = groups.iter().copied().map(kgid).collect();
        FsCredentialSnapshot::new(
            kuid(uid),
            kgid(gid),
            GroupInfo::try_new(groups).unwrap(),
            [0; CAPABILITY_WORDS],
            true,
        )
    }

    fn mask(lanes: &[(u32, KeyPermission)]) -> KeyPermissionMask {
        let raw = lanes.iter().fold(0, |raw, (shift, permission)| {
            raw | u32::from(permission.bits()) << shift
        });
        KeyPermissionMask::try_from_raw(raw).unwrap()
    }

    #[test]
    fn requested_permission_is_nonempty_known_and_composable() {
        assert_eq!(KeyPermission::try_from_bits(0), None);
        assert_eq!(KeyPermission::try_from_bits(1 << 6), None);
        assert_eq!(
            KeyPermission::try_from_bits(KeyPermission::ALL.bits()),
            Some(KeyPermission::ALL)
        );

        let mut requested = KeyPermission::VIEW | KeyPermission::READ;
        assert!(requested.contains(KeyPermission::VIEW));
        assert!(requested.intersects(KeyPermission::READ));
        assert!(!requested.intersects(KeyPermission::WRITE));
        requested |= KeyPermission::WRITE;
        assert!(requested.contains(KeyPermission::VIEW | KeyPermission::WRITE));
    }

    #[test]
    fn raw_mask_accepts_only_four_six_bit_lanes() {
        assert_eq!(KeyPermissionMask::try_from_raw(0).unwrap().into_raw(), 0);
        assert_eq!(
            KeyPermissionMask::try_from_raw(0x3f3f_3f3f)
                .unwrap()
                .into_raw(),
            0x3f3f_3f3f
        );
        for invalid in [0x0000_0040, 0x0000_8000, 0x0040_0000, 0x8000_0000] {
            assert_eq!(KeyPermissionMask::try_from_raw(invalid), None);
        }
    }

    #[test]
    fn typed_lanes_construct_the_linux_layout_without_reserved_bits() {
        let permissions = KeyPermissionMask::from_lanes(
            Some(KeyPermission::VIEW | KeyPermission::READ),
            Some(KeyPermission::WRITE),
            None,
            Some(KeyPermission::SEARCH),
        );

        assert_eq!(permissions.into_raw(), 0x0304_0008);
    }

    #[test]
    fn user_group_and_other_selection_is_mutually_exclusive() {
        let owner_uid = kuid(1000);
        let owner_gid = Some(kgid(2000));
        let permissions = mask(&[
            (USER, KeyPermission::VIEW),
            (GROUP, KeyPermission::READ),
            (OTHER, KeyPermission::WRITE),
        ]);

        let owner = actor(1000, 2000, &[]);
        assert!(permissions.allows(owner_uid, owner_gid, &owner, false, KeyPermission::VIEW));
        assert!(!permissions.allows(owner_uid, owner_gid, &owner, false, KeyPermission::READ));

        let primary_group_member = actor(1001, 2000, &[]);
        assert!(permissions.allows(
            owner_uid,
            owner_gid,
            &primary_group_member,
            false,
            KeyPermission::READ
        ));
        assert!(!permissions.allows(
            owner_uid,
            owner_gid,
            &primary_group_member,
            false,
            KeyPermission::WRITE
        ));

        let unrelated = actor(1001, 2001, &[]);
        assert!(permissions.allows(
            owner_uid,
            owner_gid,
            &unrelated,
            false,
            KeyPermission::WRITE
        ));
        assert!(!permissions.allows(owner_uid, owner_gid, &unrelated, false, KeyPermission::READ));
    }

    #[test]
    fn supplementary_group_selects_group_lane() {
        let owner_uid = kuid(1000);
        let owner_gid = Some(kgid(2000));
        let permissions = mask(&[(GROUP, KeyPermission::SEARCH), (OTHER, KeyPermission::VIEW)]);
        let supplementary_member = actor(1001, 2001, &[1999, 2000, 2002]);

        assert!(permissions.allows(
            owner_uid,
            owner_gid,
            &supplementary_member,
            false,
            KeyPermission::SEARCH
        ));
        assert!(!permissions.allows(
            owner_uid,
            owner_gid,
            &supplementary_member,
            false,
            KeyPermission::VIEW
        ));
    }

    #[test]
    fn empty_group_lane_falls_back_to_other_even_for_a_group_member() {
        let owner_uid = kuid(1000);
        let owner_gid = Some(kgid(2000));
        let permissions = mask(&[(OTHER, KeyPermission::VIEW)]);

        for group_member in [actor(1001, 2000, &[]), actor(1001, 2001, &[2000])] {
            assert!(permissions.allows(
                owner_uid,
                owner_gid,
                &group_member,
                false,
                KeyPermission::VIEW
            ));
        }
    }

    #[test]
    fn absent_owner_group_never_selects_group_but_preserves_other_and_possessor() {
        let owner_uid = kuid(1000);
        let permissions = mask(&[
            (USER, KeyPermission::WRITE),
            (GROUP, KeyPermission::READ),
            (OTHER, KeyPermission::VIEW),
            (POSSESSOR, KeyPermission::SEARCH),
        ]);

        for nonowner in [actor(1001, 2000, &[]), actor(1001, 2001, &[2000])] {
            assert!(!permissions.allows(owner_uid, None, &nonowner, false, KeyPermission::READ));
            assert!(permissions.allows(owner_uid, None, &nonowner, false, KeyPermission::VIEW));
            assert!(permissions.allows(
                owner_uid,
                None,
                &nonowner,
                true,
                KeyPermission::VIEW | KeyPermission::SEARCH
            ));
        }

        let owner = actor(1000, 2000, &[]);
        assert!(permissions.allows(owner_uid, None, &owner, false, KeyPermission::WRITE));
        assert!(!permissions.allows(owner_uid, None, &owner, false, KeyPermission::VIEW));
    }

    #[test]
    fn possessor_rights_are_cumulative_with_selected_identity_lane() {
        let owner_uid = kuid(1000);
        let owner_gid = Some(kgid(2000));
        let permissions = mask(&[
            (USER, KeyPermission::VIEW),
            (POSSESSOR, KeyPermission::READ),
        ]);
        let owner = actor(1000, 2000, &[]);
        let both = KeyPermission::VIEW | KeyPermission::READ;

        assert!(!permissions.allows(owner_uid, owner_gid, &owner, false, both));
        assert!(permissions.allows(owner_uid, owner_gid, &owner, true, both));
        assert!(!permissions.allows(
            owner_uid,
            owner_gid,
            &owner,
            true,
            both | KeyPermission::WRITE
        ));
    }

    #[test]
    fn zero_mask_denies_every_nonempty_request() {
        let permissions = KeyPermissionMask::try_from_raw(0).unwrap();
        let actor = actor(1000, 2000, &[]);

        for requested in [
            KeyPermission::VIEW,
            KeyPermission::READ,
            KeyPermission::WRITE,
            KeyPermission::SEARCH,
            KeyPermission::LINK,
            KeyPermission::SETATTR,
            KeyPermission::ALL,
        ] {
            assert!(!permissions.allows(kuid(1000), Some(kgid(2000)), &actor, true, requested));
        }
    }
}
