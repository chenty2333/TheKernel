use core::ops::{BitOr, BitOrAssign};

/// A Linux DAC capability relevant to pathname and inode authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DacCapability {
    /// Bypass read, write, and eligible execute permission checks.
    Override,
    /// Bypass file-read and directory read/search checks.
    ReadSearch,
    /// Bypass ownership restrictions such as sticky-directory deletion.
    Fowner,
    /// Preserve set-ID mode bits while changing or creating an inode.
    Fsetid,
    /// Change inode ownership outside the unprivileged owner/group rules.
    Chown,
}

/// An immutable filesystem-credential view used by one VFS operation.
pub trait DacCredentials {
    /// Kernel user-ID type.
    type UserId: Copy + Eq;
    /// Kernel group-ID type.
    type GroupId: Copy + Eq;
    /// User-namespace ownership type.
    type UserNamespace: ?Sized;

    /// Returns the filesystem user ID.
    fn fs_user_id(&self) -> Self::UserId;

    /// Returns the filesystem group ID.
    fn fs_group_id(&self) -> Self::GroupId;

    /// Returns whether the filesystem credential is a member of `group`.
    fn is_in_group(&self, group: Self::GroupId) -> bool;

    /// Checks an effective capability in the object's owning user namespace.
    fn has_capability(&self, owner: &Self::UserNamespace, capability: DacCapability) -> bool;
}

/// Additional actor-namespace facts needed by Linux protected-hardlink policy.
///
/// Linux checks the source inode against the actor's own user namespace, not
/// merely the filesystem capability domain used by ordinary DAC fallback.
/// Keeping these facts explicit prevents an adapter from treating a capability
/// in an unrelated namespace as authority over the source inode.
pub trait HardlinkCredentials: DacCredentials {
    /// Returns whether `user` is mapped into the actor's own user namespace.
    fn user_id_is_mapped_in_own_namespace(&self, user: Self::UserId) -> bool;

    /// Checks an effective capability in the actor's own user namespace.
    fn has_capability_in_own_namespace(&self, capability: DacCapability) -> bool;
}

/// Portable inode kind needed by Linux DAC policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeKind {
    /// Filesystem-specific or currently unknown inode kind.
    Unknown,
    /// Regular file.
    Regular,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Named pipe.
    Fifo,
    /// Unix-domain socket inode.
    Socket,
    /// Character device.
    CharacterDevice,
    /// Block device.
    BlockDevice,
}

/// Read, write, and execute/search access requested from Linux DAC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Access(u8);

impl Access {
    /// No permission bits requested.
    pub const NONE: Self = Self(0);
    /// Read permission.
    pub const READ: Self = Self(0b100);
    /// Write permission.
    pub const WRITE: Self = Self(0b010);
    /// Execute permission for files or search permission for directories.
    pub const EXECUTE: Self = Self(0b001);

    /// Returns the low three Linux permission bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns whether every bit in `other` is requested.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns whether any bit in `other` is requested.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for Access {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Access {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Stable metadata snapshot used by one authorization decision.
#[derive(Debug, Clone, Copy)]
pub struct NodeMetadata<'a, U, G, N: ?Sized> {
    /// POSIX permission and special mode bits.
    pub mode: u16,
    /// Effective inode owner after mount-ID mapping.
    pub owner_user: U,
    /// Effective inode group after mount-ID mapping.
    pub owner_group: G,
    /// Inode kind.
    pub kind: NodeKind,
    /// User namespace that owns the inode/superblock capability domain.
    pub owner_user_namespace: &'a N,
    /// Whether both inode IDs are mapped into the actor's ID view.
    pub ids_mapped: bool,
}

/// A Linux DAC authorization failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DacError {
    /// The selected permission class and effective capabilities deny access.
    AccessDenied,
    /// Sticky-directory ownership rules deny a mutation.
    StickyDenied,
    /// The source inode owner or group is not representable through the mount.
    UnmappedId,
    /// Linux protected-hardlink policy denies pinning the source inode.
    HardlinkDenied,
}

/// Computes the exclusively selected owner, group, or other mode class.
fn granted_access<C: DacCredentials>(
    node: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    credentials: &C,
) -> u8 {
    if credentials.fs_user_id() == node.owner_user {
        ((node.mode >> 6) & 0o7) as u8
    } else if credentials.fs_group_id() == node.owner_group
        || credentials.is_in_group(node.owner_group)
    {
        ((node.mode >> 3) & 0o7) as u8
    } else {
        (node.mode & 0o7) as u8
    }
}

pub(crate) fn capable<C: DacCredentials>(
    node: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    credentials: &C,
    capability: DacCapability,
) -> bool {
    node.ids_mapped && credentials.has_capability(node.owner_user_namespace, capability)
}

/// Applies Linux `generic_permission()`-style DAC and capability fallback.
///
/// POSIX ACL evaluation, read-only/noexec mount policy, and typed security
/// hooks are separate stages. An adapter that supports ACLs must evaluate the
/// matching ACL class before calling this mode-bit fallback.
pub fn check_dac<C: DacCredentials>(
    node: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    requested: Access,
    credentials: &C,
) -> Result<(), DacError> {
    let requested_bits = requested.bits() & 0o7;
    if requested_bits == 0 || granted_access(node, credentials) & requested_bits == requested_bits {
        return Ok(());
    }

    if node.kind == NodeKind::Directory {
        if !requested.intersects(Access::WRITE)
            && capable(node, credentials, DacCapability::ReadSearch)
        {
            return Ok(());
        }
        if capable(node, credentials, DacCapability::Override) {
            return Ok(());
        }
        return Err(DacError::AccessDenied);
    }

    if requested == Access::READ && capable(node, credentials, DacCapability::ReadSearch) {
        return Ok(());
    }

    let execute_allowed = !requested.intersects(Access::EXECUTE) || node.mode & 0o111 != 0;
    if execute_allowed && capable(node, credentials, DacCapability::Override) {
        return Ok(());
    }

    Err(DacError::AccessDenied)
}

/// Requires write and search permission on a directory.
///
/// Read-only mount policy is intentionally a distinct check because the
/// generic VFS owns mount state while this crate owns Linux-visible ordering.
pub fn check_directory_mutation<C: DacCredentials>(
    directory: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    credentials: &C,
) -> Result<(), DacError> {
    check_dac(directory, Access::WRITE | Access::EXECUTE, credentials)
}

/// Applies Linux sticky-directory removal/rename ownership rules.
pub fn check_sticky_mutation<C: DacCredentials>(
    directory: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    target: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    credentials: &C,
) -> Result<(), DacError> {
    const STICKY: u16 = 0o1000;
    if directory.mode & STICKY == 0
        || credentials.fs_user_id() == directory.owner_user
        || credentials.fs_user_id() == target.owner_user
        || capable(directory, credentials, DacCapability::Fowner)
    {
        Ok(())
    } else {
        Err(DacError::StickyDenied)
    }
}

/// Applies Linux `may_linkat()` protected-hardlink policy to one source inode.
///
/// `source_access_allowed` is invoked exactly once, and only for a regular
/// source without set-user-ID or executable set-group-ID bits. A consumer uses
/// it to run the real source `READ | WRITE` DAC and inode-permission hook stack.
/// A denial makes the source unsafe but is deliberately not returned directly:
/// Linux still permits the source owner, or mapped own-namespace
/// `CAP_FOWNER`, to create the hard link.
pub fn check_hardlink_source<C, F>(
    source: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    credentials: &C,
    protected_hardlinks: bool,
    source_access_allowed: F,
) -> Result<(), DacError>
where
    C: HardlinkCredentials,
    F: FnOnce(Access) -> bool,
{
    const SET_UID: u16 = 0o4000;
    const SET_GID: u16 = 0o2000;
    const GROUP_EXECUTE: u16 = 0o0010;

    if !source.ids_mapped {
        return Err(DacError::UnmappedId);
    }
    if !protected_hardlinks {
        return Ok(());
    }

    let safe_shape = source.kind == NodeKind::Regular
        && source.mode & SET_UID == 0
        && source.mode & (SET_GID | GROUP_EXECUTE) != (SET_GID | GROUP_EXECUTE);
    if safe_shape && source_access_allowed(Access::READ | Access::WRITE) {
        return Ok(());
    }

    if credentials.fs_user_id() == source.owner_user
        || (credentials.user_id_is_mapped_in_own_namespace(source.owner_user)
            && credentials.has_capability_in_own_namespace(DacCapability::Fowner))
    {
        Ok(())
    } else {
        Err(DacError::HardlinkDenied)
    }
}

/// Owner and mode computed before publishing a newly named inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateAttributes<U, G> {
    /// New inode owner.
    pub user: U,
    /// New inode group.
    pub group: G,
    /// Permission and set-ID mode after parent inheritance and umask.
    pub mode: u16,
}

/// Computes Linux-style initial ownership, SGID inheritance, and umask.
pub fn initial_create_attributes<C: DacCredentials>(
    parent: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    child_kind: NodeKind,
    requested_mode: u16,
    umask: u16,
    credentials: &C,
) -> CreateAttributes<C::UserId, C::GroupId> {
    const SET_GID: u16 = 0o2000;
    const GROUP_EXECUTE: u16 = 0o0010;

    let parent_is_sgid = parent.mode & SET_GID != 0;
    let in_parent_group = credentials.fs_group_id() == parent.owner_group
        || credentials.is_in_group(parent.owner_group);
    let executable_sgid = requested_mode & (SET_GID | GROUP_EXECUTE) == (SET_GID | GROUP_EXECUTE);

    let mut mode = requested_mode;
    if child_kind != NodeKind::Directory
        && parent_is_sgid
        && executable_sgid
        && !in_parent_group
        && !capable(parent, credentials, DacCapability::Fsetid)
    {
        mode &= !SET_GID;
    }
    mode &= !umask;

    if child_kind == NodeKind::Directory && parent_is_sgid {
        mode |= SET_GID;
    }

    CreateAttributes {
        user: credentials.fs_user_id(),
        group: if parent_is_sgid {
            parent.owner_group
        } else {
            credentials.fs_group_id()
        },
        mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Credentials {
        uid: u32,
        gid: u32,
        groups: [u32; 2],
        group_count: usize,
        caps: u8,
        own_namespace_caps: u8,
        own_namespace_mapped_user: Option<u32>,
    }

    impl DacCredentials for Credentials {
        type UserId = u32;
        type GroupId = u32;
        type UserNamespace = u8;

        fn fs_user_id(&self) -> Self::UserId {
            self.uid
        }

        fn fs_group_id(&self) -> Self::GroupId {
            self.gid
        }

        fn is_in_group(&self, group: Self::GroupId) -> bool {
            self.groups[..self.group_count].contains(&group)
        }

        fn has_capability(&self, _owner: &Self::UserNamespace, capability: DacCapability) -> bool {
            let bit = match capability {
                DacCapability::Override => 1,
                DacCapability::ReadSearch => 2,
                DacCapability::Fowner => 4,
                DacCapability::Fsetid => 8,
                DacCapability::Chown => 16,
            };
            self.caps & bit != 0
        }
    }

    impl HardlinkCredentials for Credentials {
        fn user_id_is_mapped_in_own_namespace(&self, user: Self::UserId) -> bool {
            self.own_namespace_mapped_user == Some(user)
        }

        fn has_capability_in_own_namespace(&self, capability: DacCapability) -> bool {
            capability == DacCapability::Fowner && self.own_namespace_caps & 4 != 0
        }
    }

    const USER_NS: u8 = 1;

    fn credentials(uid: u32, gid: u32, caps: u8) -> Credentials {
        Credentials {
            uid,
            gid,
            groups: [0; 2],
            group_count: 0,
            caps,
            own_namespace_caps: 0,
            own_namespace_mapped_user: None,
        }
    }

    fn node(mode: u16, uid: u32, gid: u32, kind: NodeKind) -> NodeMetadata<'static, u32, u32, u8> {
        NodeMetadata {
            mode,
            owner_user: uid,
            owner_group: gid,
            kind,
            owner_user_namespace: &USER_NS,
            ids_mapped: true,
        }
    }

    #[test]
    fn permission_classes_are_exclusive() {
        let owner = credentials(1000, 7, 0);
        assert_eq!(
            check_dac(
                &node(0o004, 1000, 9, NodeKind::Regular),
                Access::READ,
                &owner,
            ),
            Err(DacError::AccessDenied)
        );

        let group = credentials(1001, 9, 0);
        assert_eq!(
            check_dac(
                &node(0o004, 1000, 9, NodeKind::Regular),
                Access::READ,
                &group,
            ),
            Err(DacError::AccessDenied)
        );
    }

    #[test]
    fn uid_zero_is_not_implicitly_privileged() {
        assert_eq!(
            check_dac(
                &node(0, 1000, 9, NodeKind::Directory),
                Access::EXECUTE,
                &credentials(0, 0, 0),
            ),
            Err(DacError::AccessDenied)
        );
    }

    #[test]
    fn capability_fallback_matches_linux_file_and_directory_rules() {
        let read_search = credentials(0, 0, 2);
        assert!(
            check_dac(
                &node(0, 1000, 9, NodeKind::Regular),
                Access::READ,
                &read_search,
            )
            .is_ok()
        );
        assert_eq!(
            check_dac(
                &node(0, 1000, 9, NodeKind::Regular),
                Access::EXECUTE,
                &read_search,
            ),
            Err(DacError::AccessDenied)
        );
        assert!(
            check_dac(
                &node(0, 1000, 9, NodeKind::Directory),
                Access::READ | Access::EXECUTE,
                &read_search,
            )
            .is_ok()
        );
        assert_eq!(
            check_dac(
                &node(0, 1000, 9, NodeKind::Directory),
                Access::WRITE,
                &read_search,
            ),
            Err(DacError::AccessDenied)
        );

        let override_cap = credentials(0, 0, 1);
        assert_eq!(
            check_dac(
                &node(0, 1000, 9, NodeKind::Regular),
                Access::EXECUTE,
                &override_cap,
            ),
            Err(DacError::AccessDenied)
        );
        assert!(
            check_dac(
                &node(0o001, 1000, 9, NodeKind::Regular),
                Access::EXECUTE,
                &override_cap,
            )
            .is_ok()
        );
    }

    #[test]
    fn unmapped_ids_disable_capability_bypass() {
        let mut target = node(0, 1000, 9, NodeKind::Directory);
        target.ids_mapped = false;
        assert_eq!(
            check_dac(&target, Access::EXECUTE, &credentials(0, 0, 3)),
            Err(DacError::AccessDenied)
        );
    }

    #[test]
    fn sticky_directory_checks_both_owners_and_fowner() {
        let directory = node(0o1777, 10, 10, NodeKind::Directory);
        let target = node(0o600, 20, 20, NodeKind::Regular);
        assert_eq!(
            check_sticky_mutation(&directory, &target, &credentials(30, 30, 0)),
            Err(DacError::StickyDenied)
        );
        assert!(check_sticky_mutation(&directory, &target, &credentials(20, 30, 0)).is_ok());
        assert!(check_sticky_mutation(&directory, &target, &credentials(30, 30, 4)).is_ok());
    }

    #[test]
    fn protected_hardlinks_preserve_safe_source_and_owner_capability_order() {
        let source = node(0o600, 1000, 100, NodeKind::Regular);
        let actor = credentials(2000, 200, 0);
        let mut calls = 0;
        check_hardlink_source(&source, &actor, true, |requested| {
            calls += 1;
            assert_eq!(requested, Access::READ | Access::WRITE);
            true
        })
        .unwrap();
        assert_eq!(calls, 1);

        let mut owner = credentials(1000, 200, 0);
        let mut calls = 0;
        check_hardlink_source(&source, &owner, true, |_| {
            calls += 1;
            false
        })
        .unwrap();
        assert_eq!(
            calls, 1,
            "safe shape checks source access before owner fallback"
        );

        owner.uid = 2000;
        owner.own_namespace_mapped_user = Some(1000);
        owner.own_namespace_caps = 4;
        check_hardlink_source(&source, &owner, true, |_| false).unwrap();
        owner.own_namespace_mapped_user = None;
        assert_eq!(
            check_hardlink_source(&source, &owner, true, |_| false),
            Err(DacError::HardlinkDenied)
        );
    }

    #[test]
    fn protected_hardlinks_reject_unsafe_shapes_without_source_access_probe() {
        let actor = credentials(2000, 200, 0);
        for source in [
            node(0o4600, 1000, 100, NodeKind::Regular),
            node(0o2610, 1000, 100, NodeKind::Regular),
            node(0o600, 1000, 100, NodeKind::Fifo),
        ] {
            let mut called = false;
            assert_eq!(
                check_hardlink_source(&source, &actor, true, |_| {
                    called = true;
                    true
                }),
                Err(DacError::HardlinkDenied)
            );
            assert!(!called);
        }

        let non_executable_sgid = node(0o2600, 1000, 100, NodeKind::Regular);
        let mut called = false;
        check_hardlink_source(&non_executable_sgid, &actor, true, |_| {
            called = true;
            true
        })
        .unwrap();
        assert!(called);
    }

    #[test]
    fn hardlink_mapping_precedes_the_protected_policy_toggle() {
        let actor = credentials(2000, 200, 0);
        let mut source = node(0o600, 1000, 100, NodeKind::Regular);
        source.ids_mapped = false;
        let mut called = false;
        assert_eq!(
            check_hardlink_source(&source, &actor, false, |_| {
                called = true;
                true
            }),
            Err(DacError::UnmappedId)
        );
        assert!(!called);

        source.ids_mapped = true;
        check_hardlink_source(&source, &actor, false, |_| {
            called = true;
            false
        })
        .unwrap();
        assert!(!called);
    }

    #[test]
    fn create_attributes_apply_sgid_before_umask() {
        let parent = node(0o2770, 10, 200, NodeKind::Directory);
        let attrs = initial_create_attributes(
            &parent,
            NodeKind::Regular,
            0o2670,
            0o020,
            &credentials(1000, 100, 0),
        );
        assert_eq!(attrs.user, 1000);
        assert_eq!(attrs.group, 200);
        assert_eq!(attrs.mode, 0o650);

        let directory = initial_create_attributes(
            &parent,
            NodeKind::Directory,
            0o770,
            0o027,
            &credentials(1000, 100, 0),
        );
        assert_eq!(directory.mode, 0o2750);
    }
}
