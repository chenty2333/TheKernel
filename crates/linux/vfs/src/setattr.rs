use crate::dac::{DacCapability, DacCredentials, NodeKind, NodeMetadata, capable};

const MODE_MASK: u16 = 0o7777;
const SET_UID: u16 = 0o4000;
const SET_GID: u16 = 0o2000;
const GROUP_EXECUTE: u16 = 0o0010;

/// A normalized Linux `chmod` request.
///
/// Only the permission and special mode bits represented by `S_IALLUGO` are
/// retained. Inode-kind bits remain owned by the generic VFS object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChmodRequest {
    mode: u16,
}

impl ChmodRequest {
    /// Builds a request while discarding bits outside Linux `S_IALLUGO`.
    pub const fn new(mode: u16) -> Self {
        Self {
            mode: mode & MODE_MASK,
        }
    }

    /// Returns the normalized mode visible at the inode-setattr hook point.
    pub const fn mode(self) -> u16 {
        self.mode
    }
}

/// An omission-preserving Linux `chown` request.
///
/// `None` means that the corresponding field was not requested. It is not
/// equivalent to explicitly requesting the inode's current value: Linux uses
/// field presence when running `setattr_prepare()` authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChownRequest<U, G> {
    user: Option<U>,
    group: Option<G>,
}

impl<U: Copy, G: Copy> ChownRequest<U, G> {
    /// Builds a request from independently optional user and group IDs.
    pub const fn new(user: Option<U>, group: Option<G>) -> Self {
        Self { user, group }
    }

    /// Returns the requested user ID, preserving omission.
    pub const fn user(self) -> Option<U> {
        self.user
    }

    /// Returns the requested group ID, preserving omission.
    pub const fn group(self) -> Option<G> {
        self.group
    }

    /// Returns whether neither ownership field was requested.
    pub const fn is_fully_omitted(self) -> bool {
        self.user.is_none() && self.group.is_none()
    }
}

/// A Linux setattr-policy denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SetattrError {
    /// An explicit UID/GID change violates unprivileged `chown` rules.
    ChownDenied,
    /// A mode change requires inode ownership or `CAP_FOWNER`.
    ChmodDenied,
}

/// Backend-ready ownership and mode fields produced by Linux setattr policy.
///
/// Timestamps, storage publication, notifications, and security-hook dispatch
/// remain consumer responsibilities. The committed accessors are derived from
/// the exact metadata snapshot retained by the consumed plan, so a consumer
/// need not re-read the inode after successful publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedSetattr<U, G> {
    owner: Option<(U, G)>,
    mode: Option<u16>,
    committed_user: U,
    committed_group: G,
    committed_mode: u16,
}

impl<U: Copy, G: Copy> PreparedSetattr<U, G> {
    /// Returns an ownership update, or `None` when both fields were omitted.
    pub const fn owner(self) -> Option<(U, G)> {
        self.owner
    }

    /// Returns an implicit or explicit mode update.
    pub const fn mode(self) -> Option<u16> {
        self.mode
    }

    /// Returns the user ID after successful publication.
    pub const fn committed_user(self) -> U {
        self.committed_user
    }

    /// Returns the group ID after successful publication.
    pub const fn committed_group(self) -> G {
        self.committed_group
    }

    /// Returns the mode after successful publication.
    pub const fn committed_mode(self) -> u16 {
        self.committed_mode
    }
}

/// Move-only policy state spanning a `chmod` inode-setattr pre-hook.
///
/// The plan retains the exact old metadata, request, and credential view used
/// to construct the hook payload. Consuming it in [`Self::prepare`] prevents
/// the post-hook authorization phase from being paired with a different inode
/// or credential snapshot.
pub struct ChmodSetattrPlan<'a, C: DacCredentials> {
    node: NodeMetadata<'a, C::UserId, C::GroupId, C::UserNamespace>,
    request: ChmodRequest,
    credentials: C,
}

impl<C: DacCredentials> ChmodSetattrPlan<'_, C> {
    /// Returns the normalized request used for the pre-hook proposal.
    pub const fn request(&self) -> ChmodRequest {
        self.request
    }

    /// Runs the post-hook owner and set-group-ID authorization phase.
    pub fn prepare(self) -> Result<PreparedSetattr<C::UserId, C::GroupId>, SetattrError> {
        check_chmod_permission(&self.node, &self.credentials)?;
        let mut mode = self.request.mode();
        if !can_preserve_setgid(&self.node, self.node.owner_group, &self.credentials) {
            mode &= !SET_GID;
        }
        Ok(PreparedSetattr {
            owner: None,
            mode: Some(mode),
            committed_user: self.node.owner_user,
            committed_group: self.node.owner_group,
            committed_mode: mode,
        })
    }
}

/// Move-only policy state spanning a `chown` inode-setattr pre-hook.
///
/// Linux derives implicit set-ID removal before the inode hook, then runs
/// ownership, implicit-mode, and final-group SGID authorization afterwards.
/// This type retains the exact old metadata, omission-aware request, frozen
/// credential view, and hook mode across those two phases without owning any
/// hook registry.
pub struct ChownSetattrPlan<'a, C: DacCredentials> {
    node: NodeMetadata<'a, C::UserId, C::GroupId, C::UserNamespace>,
    request: ChownRequest<C::UserId, C::GroupId>,
    hook_mode: Option<u16>,
    credentials: C,
}

impl<C: DacCredentials> ChownSetattrPlan<'_, C> {
    /// Returns the omission-aware request visible to the pre-hook adapter.
    pub const fn request(&self) -> ChownRequest<C::UserId, C::GroupId> {
        self.request
    }

    /// Returns the implicit hook-point mode when set-ID bits must be removed.
    pub const fn hook_mode(&self) -> Option<u16> {
        self.hook_mode
    }

    /// Runs post-hook ownership and implicit-mode authorization.
    pub fn prepare(self) -> Result<PreparedSetattr<C::UserId, C::GroupId>, SetattrError> {
        check_chown_permission(&self.node, self.request, &self.credentials)?;

        let mode = if let Some(mut mode) = self.hook_mode {
            // notify_change() has converted KILL_SUID/KILL_SGID into an
            // implicit ATTR_MODE before the hook. setattr_prepare() therefore
            // applies the ordinary owner/CAP_FOWNER check afterwards.
            check_chmod_permission(&self.node, &self.credentials)?;

            // An explicit ATTR_GID changes the group against which the later
            // SGID preservation check runs, even though the hook saw a mode
            // derived from the old group.
            let final_group = self.request.group().unwrap_or(self.node.owner_group);
            if !can_preserve_setgid(&self.node, final_group, &self.credentials) {
                mode &= !SET_GID;
            }
            Some(mode)
        } else {
            None
        };

        let owner = if self.request.is_fully_omitted() {
            None
        } else {
            Some((
                self.request.user().unwrap_or(self.node.owner_user),
                self.request.group().unwrap_or(self.node.owner_group),
            ))
        };
        let committed_user = self.request.user().unwrap_or(self.node.owner_user);
        let committed_group = self.request.group().unwrap_or(self.node.owner_group);
        let committed_mode = mode.unwrap_or(self.node.mode);

        Ok(PreparedSetattr {
            owner,
            mode,
            committed_user,
            committed_group,
            committed_mode,
        })
    }
}

/// Starts a Linux `chmod` policy decision over one exact metadata snapshot.
pub fn plan_chmod<'a, C: DacCredentials>(
    node: &NodeMetadata<'a, C::UserId, C::GroupId, C::UserNamespace>,
    request: ChmodRequest,
    credentials: C,
) -> ChmodSetattrPlan<'a, C> {
    ChmodSetattrPlan {
        node: NodeMetadata {
            mode: node.mode,
            owner_user: node.owner_user,
            owner_group: node.owner_group,
            kind: node.kind,
            owner_user_namespace: node.owner_user_namespace,
            ids_mapped: node.ids_mapped,
        },
        request,
        credentials,
    }
}

/// Starts a Linux `chown` policy decision and derives its pre-hook mode.
pub fn plan_chown<'a, C>(
    node: &NodeMetadata<'a, C::UserId, C::GroupId, C::UserNamespace>,
    request: ChownRequest<C::UserId, C::GroupId>,
    credentials: C,
) -> ChownSetattrPlan<'a, C>
where
    C: DacCredentials,
{
    let hook_mode = chown_hook_mode(node, &credentials);
    ChownSetattrPlan {
        node: NodeMetadata {
            mode: node.mode,
            owner_user: node.owner_user,
            owner_group: node.owner_group,
            kind: node.kind,
            owner_user_namespace: node.owner_user_namespace,
            ids_mapped: node.ids_mapped,
        },
        request,
        hook_mode,
        credentials,
    }
}

fn can_preserve_setgid<C: DacCredentials>(
    node: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    group: C::GroupId,
    credentials: &C,
) -> bool {
    credentials.fs_group_id() == group
        || credentials.is_in_group(group)
        || capable(node, credentials, DacCapability::Fsetid)
}

fn chown_hook_mode<C: DacCredentials>(
    node: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    credentials: &C,
) -> Option<u16> {
    if node.kind == NodeKind::Directory {
        return None;
    }
    let mut mode = node.mode;
    mode &= !SET_UID;
    if mode & GROUP_EXECUTE != 0 || !can_preserve_setgid(node, node.owner_group, credentials) {
        mode &= !SET_GID;
    }
    (mode != node.mode).then_some(mode)
}

fn check_chown_permission<C: DacCredentials>(
    node: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    request: ChownRequest<C::UserId, C::GroupId>,
    credentials: &C,
) -> Result<(), SetattrError> {
    if request.is_fully_omitted() || capable(node, credentials, DacCapability::Chown) {
        return Ok(());
    }
    if credentials.fs_user_id() != node.owner_user {
        return Err(SetattrError::ChownDenied);
    }
    if request.user().is_some_and(|user| user != node.owner_user) {
        return Err(SetattrError::ChownDenied);
    }
    if request.group().is_some_and(|group| {
        group != node.owner_group
            && group != credentials.fs_group_id()
            && !credentials.is_in_group(group)
    }) {
        return Err(SetattrError::ChownDenied);
    }
    Ok(())
}

fn check_chmod_permission<C: DacCredentials>(
    node: &NodeMetadata<'_, C::UserId, C::GroupId, C::UserNamespace>,
    credentials: &C,
) -> Result<(), SetattrError> {
    if credentials.fs_user_id() == node.owner_user
        || capable(node, credentials, DacCapability::Fowner)
    {
        Ok(())
    } else {
        Err(SetattrError::ChmodDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_NS: u8 = 1;

    #[derive(Debug, Clone)]
    struct Credentials {
        uid: u32,
        gid: u32,
        groups: [u32; 2],
        group_count: usize,
        caps: u8,
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
                DacCapability::Fowner => 1,
                DacCapability::Fsetid => 2,
                DacCapability::Chown => 4,
                _ => 0,
            };
            self.caps & bit != 0
        }
    }

    fn credentials(uid: u32, gid: u32, groups: &[u32], caps: u8) -> Credentials {
        let mut stored_groups = [0; 2];
        stored_groups[..groups.len()].copy_from_slice(groups);
        Credentials {
            uid,
            gid,
            groups: stored_groups,
            group_count: groups.len(),
            caps,
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
    fn chmod_preserves_the_requested_hook_mode_but_strips_unauthorized_sgid_later() {
        let inode = node(0o755, 1000, 200, NodeKind::Regular);
        let request = ChmodRequest::new(0o102755);
        let plan = plan_chmod(&inode, request, credentials(1000, 100, &[], 0));
        assert_eq!(plan.request().mode(), 0o2755);

        let prepared = plan.prepare().unwrap();
        assert_eq!(prepared.mode(), Some(0o755));
        assert_eq!(prepared.committed_mode(), 0o755);
        assert_eq!(prepared.owner(), None);
    }

    #[test]
    fn chmod_requires_owner_or_fowner_and_respects_group_membership() {
        let inode = node(0o755, 2000, 200, NodeKind::Regular);
        assert_eq!(
            plan_chmod(
                &inode,
                ChmodRequest::new(0o2755),
                credentials(1000, 100, &[], 0),
            )
            .prepare(),
            Err(SetattrError::ChmodDenied)
        );
        assert_eq!(
            plan_chmod(
                &inode,
                ChmodRequest::new(0o2755),
                credentials(1000, 100, &[200], 1),
            )
            .prepare()
            .unwrap()
            .mode(),
            Some(0o2755)
        );
    }

    #[test]
    fn chown_hook_mode_is_derived_before_post_hook_authorization() {
        let inode = node(0o6755, 1000, 100, NodeKind::Regular);
        let actor = credentials(2000, 200, &[], 0);
        let plan = plan_chown(&inode, ChownRequest::new(None, None), actor.clone());
        assert_eq!(plan.request(), ChownRequest::new(None, None));
        assert_eq!(plan.hook_mode(), Some(0o755));
        assert_eq!(plan.prepare(), Err(SetattrError::ChmodDenied));

        let directory = node(0o6755, 1000, 100, NodeKind::Directory);
        assert_eq!(
            plan_chown(&directory, ChownRequest::new(None, None), actor).hook_mode(),
            None
        );
    }

    #[test]
    fn fully_omitted_chown_keeps_owner_absent_without_synthetic_chown_authority() {
        let inode = node(0o600, 2000, 200, NodeKind::Regular);
        let actor = credentials(1000, 100, &[], 0);
        let prepared = plan_chown(&inode, ChownRequest::new(None, None), actor)
            .prepare()
            .unwrap();
        assert_eq!(prepared.owner(), None);
        assert_eq!(prepared.mode(), None);
        assert_eq!(prepared.committed_user(), 2000);
        assert_eq!(prepared.committed_group(), 200);
    }

    #[test]
    fn chown_preserves_each_omission_and_applies_unprivileged_group_rules() {
        let inode = node(0o644, 1000, 100, NodeKind::Regular);
        let actor = credentials(1000, 100, &[200], 0);
        let prepared = plan_chown(&inode, ChownRequest::new(None, Some(200)), actor.clone())
            .prepare()
            .unwrap();
        assert_eq!(prepared.owner(), Some((1000, 200)));
        assert_eq!(prepared.committed_user(), 1000);
        assert_eq!(prepared.committed_group(), 200);

        assert_eq!(
            plan_chown(&inode, ChownRequest::new(Some(2000), None), actor).prepare(),
            Err(SetattrError::ChownDenied)
        );
    }

    #[test]
    fn cap_chown_does_not_replace_fowner_for_an_implicit_mode() {
        let inode = node(0o4755, 2000, 200, NodeKind::Regular);
        let chown_only = credentials(1000, 100, &[], 4);
        assert_eq!(
            plan_chown(&inode, ChownRequest::new(Some(2000), None), chown_only).prepare(),
            Err(SetattrError::ChmodDenied)
        );

        let chown_and_fowner = credentials(1000, 100, &[], 5);
        assert!(
            plan_chown(
                &inode,
                ChownRequest::new(Some(2000), None),
                chown_and_fowner,
            )
            .prepare()
            .is_ok()
        );
    }

    #[test]
    fn chown_rechecks_sgid_against_the_explicit_final_group() {
        let inode = node(0o2644, 1000, 100, NodeKind::Regular);
        let actor = credentials(1000, 100, &[], 4);
        let plan = plan_chown(&inode, ChownRequest::new(None, Some(200)), actor.clone());
        assert_eq!(plan.hook_mode(), None);

        // With no implicit mode there is no SGID update to recheck. Adding
        // set-user-ID forces ATTR_MODE, after which the requested GID controls
        // the second SGID preservation decision.
        let inode = node(0o6644, 1000, 100, NodeKind::Regular);
        let plan = plan_chown(&inode, ChownRequest::new(None, Some(200)), actor);
        assert_eq!(plan.hook_mode(), Some(0o2644));
        let prepared = plan.prepare().unwrap();
        assert_eq!(prepared.owner(), Some((1000, 200)));
        assert_eq!(prepared.mode(), Some(0o644));
    }
}
