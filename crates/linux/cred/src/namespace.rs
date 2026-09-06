//! Concrete user-namespace topology and map-publication policy.
//!
//! This module owns Linux-visible namespace state, but deliberately owns no
//! lock, namespace registry, process link, signal account, procfs inode, or
//! resource-admission token. An embedding kernel places [`UserNamespaceMapState`]
//! behind its chosen short lock. Publication borrows a caller-owned replacement
//! and only clones it into a previously empty slot, so mutation never returns,
//! replaces, or drops `Arc` ownership while that lock is held.

use alloc::sync::Arc;

use crate::{CredError, IdMap, Kgid, Kuid, UserNamespaceView};

/// Largest parent level from which Linux permits another user namespace to be
/// created.
///
/// The resulting child has level 33. A level-33 namespace cannot create a
/// further child.
pub const USER_NAMESPACE_MAX_CREATION_PARENT_LEVEL: u32 = 32;

/// UID/GID emitted by Linux interfaces which explicitly munge an unmapped ID.
pub const USER_NAMESPACE_OVERFLOW_ID: u32 = 65_534;

/// Immutable hierarchy and ownership facts for one user namespace.
///
/// `N` is the embedding kernel's complete namespace wrapper. This core stores
/// only an `Arc<N>` parent and never reaches into process, signal, VFS, FD, or
/// MM state.
pub struct UserNamespaceDomain<N> {
    level: u32,
    parent: Option<Arc<N>>,
    owner: Kuid,
    group: Kgid,
    parent_could_setfcap: bool,
}

impl<N> UserNamespaceDomain<N> {
    /// Constructs the initial user-namespace topology.
    pub const fn initial() -> Self {
        Self {
            level: 0,
            parent: None,
            owner: Kuid::INITIAL_ROOT,
            group: Kgid::INITIAL_ROOT,
            parent_could_setfcap: true,
        }
    }

    /// Returns the parent wrapper, or `None` for the initial namespace.
    pub fn parent(&self) -> Option<Arc<N>> {
        self.parent.clone()
    }

    /// Returns the root-at-zero nesting level.
    pub const fn level(&self) -> u32 {
        self.level
    }

    /// Returns the kernel-global UID which owns this namespace in its parent.
    pub const fn owner_kuid(&self) -> Kuid {
        self.owner
    }

    /// Returns the creator's kernel-global GID recorded for this namespace.
    pub const fn owner_kgid(&self) -> Kgid {
        self.group
    }

    /// Returns whether the creator had `CAP_SETFCAP` in the parent namespace.
    pub const fn parent_could_setfcap(&self) -> bool {
        self.parent_could_setfcap
    }

    /// Reports whether this is the initial namespace.
    pub const fn is_initial(&self) -> bool {
        self.parent.is_none()
    }
}

impl<N: UserNamespaceView> UserNamespaceDomain<N> {
    /// Validates and constructs one direct child topology.
    ///
    /// `parent_uid_map` and `parent_gid_map` must be coherent snapshots from
    /// `parent`. Both creator IDs must be visible in those maps.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::Capacity`] when namespace nesting is exhausted, or
    /// [`CredError::NotPermitted`] when either creator ID is unmapped.
    pub fn try_child(
        parent: &Arc<N>,
        parent_uid_map: &IdMap,
        parent_gid_map: &IdMap,
        owner: Kuid,
        group: Kgid,
        parent_could_setfcap: bool,
    ) -> Result<Self, CredError> {
        let parent_level = parent.level();
        if parent_level > USER_NAMESPACE_MAX_CREATION_PARENT_LEVEL {
            return Err(CredError::Capacity);
        }
        let level = parent_level.checked_add(1).ok_or(CredError::Capacity)?;
        if parent_uid_map.kernel_uid_to_user(owner).is_none()
            || parent_gid_map.kernel_gid_to_user(group).is_none()
        {
            return Err(CredError::NotPermitted);
        }
        Ok(Self {
            level,
            parent: Some(parent.clone()),
            owner,
            group,
            parent_could_setfcap,
        })
    }
}

#[derive(Debug)]
enum UserNamespaceMapSlot {
    Fixed(Arc<IdMap>),
    PublishOnce {
        empty: Arc<IdMap>,
        published: Option<Arc<IdMap>>,
    },
}

impl UserNamespaceMapSlot {
    fn fixed(map: Arc<IdMap>) -> Self {
        Self::Fixed(map)
    }

    fn publish_once(empty: Arc<IdMap>) -> Self {
        Self::PublishOnce {
            empty,
            published: None,
        }
    }

    fn map(&self) -> Arc<IdMap> {
        match self {
            Self::Fixed(map) => map.clone(),
            Self::PublishOnce { empty, published } => published.as_ref().unwrap_or(empty).clone(),
        }
    }

    const fn is_published(&self) -> bool {
        match self {
            Self::Fixed(_) => true,
            Self::PublishOnce { published, .. } => published.is_some(),
        }
    }

    fn try_publish(&mut self, replacement: &Arc<IdMap>) -> Result<(), CredError> {
        if replacement.is_empty() {
            return Err(CredError::InvalidInput);
        }
        let Self::PublishOnce { published, .. } = self else {
            return Err(CredError::NotPermitted);
        };
        if published.is_some() {
            return Err(CredError::NotPermitted);
        }
        *published = Some(replacement.clone());
        Ok(())
    }
}

/// UID/GID maps plus their one-write and `setgroups` publication state.
///
/// This value contains no synchronization. A consumer must serialize mutation
/// with a short guard. Map publication only clones a borrowed replacement into
/// an empty slot; it cannot release any map ownership under that guard.
#[derive(Debug)]
pub struct UserNamespaceMapState {
    uid_map: UserNamespaceMapSlot,
    gid_map: UserNamespaceMapSlot,
    setgroups_allowed: bool,
}

impl UserNamespaceMapState {
    /// Fallibly constructs the initial namespace's fully published identity
    /// maps and enabled `setgroups` policy.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::NoMemory`] if the identity map cannot be allocated.
    pub fn try_initial() -> Result<Self, CredError> {
        let identity = IdMap::try_identity()?;
        Ok(Self {
            uid_map: UserNamespaceMapSlot::fixed(identity.clone()),
            gid_map: UserNamespaceMapSlot::fixed(identity),
            setgroups_allowed: true,
        })
    }

    /// Fallibly constructs a child's empty maps while inheriting the parent's
    /// current `setgroups` policy.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::NoMemory`] if the shared empty map cannot be
    /// allocated.
    pub fn try_child(setgroups_allowed: bool) -> Result<Self, CredError> {
        let empty = IdMap::try_empty()?;
        Ok(Self {
            uid_map: UserNamespaceMapSlot::publish_once(empty.clone()),
            gid_map: UserNamespaceMapSlot::publish_once(empty),
            setgroups_allowed,
        })
    }

    /// Clones the current immutable UID map.
    pub fn uid_map(&self) -> Arc<IdMap> {
        self.uid_map.map()
    }

    /// Clones the current immutable GID map.
    pub fn gid_map(&self) -> Arc<IdMap> {
        self.gid_map.map()
    }

    /// Reports whether the UID map has already been published.
    pub const fn uid_map_written(&self) -> bool {
        self.uid_map.is_published()
    }

    /// Reports whether the GID map has already been published.
    pub const fn gid_map_written(&self) -> bool {
        self.gid_map.is_published()
    }

    /// Returns the current irreversible `setgroups` policy.
    pub const fn setgroups_allowed(&self) -> bool {
        self.setgroups_allowed
    }

    /// Returns whether a published GID map currently permits `setgroups`.
    pub const fn may_setgroups(&self) -> bool {
        self.gid_map.is_published() && self.setgroups_allowed
    }

    /// Publishes a non-empty UID map exactly once.
    ///
    /// The caller retains `replacement` on both success and failure. Success
    /// clones it into a previously empty slot while retaining the immutable
    /// empty fallback, so this operation cannot retire or drop an `Arc`.
    pub fn try_publish_uid_map(&mut self, replacement: &Arc<IdMap>) -> Result<(), CredError> {
        self.uid_map.try_publish(replacement)
    }

    /// Publishes a non-empty GID map exactly once.
    ///
    /// When `require_setgroups_denied` is true, the deny transition and map
    /// publication share this same serialized state. Ownership behavior matches
    /// [`Self::try_publish_uid_map`].
    pub fn try_publish_gid_map(
        &mut self,
        replacement: &Arc<IdMap>,
        require_setgroups_denied: bool,
    ) -> Result<(), CredError> {
        if replacement.is_empty() {
            return Err(CredError::InvalidInput);
        }
        if require_setgroups_denied && self.setgroups_allowed {
            return Err(CredError::NotPermitted);
        }
        self.gid_map.try_publish(replacement)
    }

    /// Applies Linux's irreversible `allow -> deny` setgroups transition.
    ///
    /// Re-enabling a denied policy and disabling after GID-map publication are
    /// both rejected. Repeating the currently effective policy is idempotent.
    pub fn try_update_setgroups_policy(&mut self, allow: bool) -> Result<(), CredError> {
        if allow {
            if !self.setgroups_allowed {
                return Err(CredError::NotPermitted);
            }
        } else {
            if self.gid_map.is_published() {
                return Err(CredError::NotPermitted);
            }
            self.setgroups_allowed = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::Arc;

    use super::*;
    use crate::{UserGid, UserUid};

    struct MockNamespace {
        level: u32,
        parent: Option<Arc<Self>>,
        owner: Kuid,
    }

    impl MockNamespace {
        fn root() -> Arc<Self> {
            Arc::new(Self {
                level: 0,
                parent: None,
                owner: Kuid::INITIAL_ROOT,
            })
        }

        fn at_level(level: u32) -> Arc<Self> {
            Arc::new(Self {
                level,
                parent: None,
                owner: Kuid::INITIAL_ROOT,
            })
        }
    }

    impl UserNamespaceView for MockNamespace {
        fn parent(self: &Arc<Self>) -> Option<Arc<Self>> {
            self.parent.clone()
        }

        fn level(&self) -> u32 {
            self.level
        }

        fn owner_kuid(&self) -> Kuid {
            self.owner
        }

        fn root_kuid(&self) -> Option<Kuid> {
            Some(Kuid::INITIAL_ROOT)
        }

        fn is_initial(&self) -> bool {
            self.parent.is_none()
        }
    }

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    #[test]
    fn initial_domain_and_maps_are_explicit() {
        let domain = UserNamespaceDomain::<MockNamespace>::initial();
        let maps = UserNamespaceMapState::try_initial().unwrap();
        let uid_map = maps.uid_map();
        let gid_map = maps.gid_map();

        assert!(domain.is_initial());
        assert_eq!(domain.level(), 0);
        assert_eq!(domain.owner_kuid(), Kuid::INITIAL_ROOT);
        assert_eq!(domain.owner_kgid(), Kgid::INITIAL_ROOT);
        assert!(domain.parent_could_setfcap());
        assert!(domain.parent().is_none());
        assert!(maps.uid_map_written());
        assert!(maps.gid_map_written());
        assert!(maps.setgroups_allowed());
        assert!(maps.may_setgroups());
        assert!(Arc::ptr_eq(&uid_map, &gid_map));
        assert_eq!(
            uid_map.user_uid_to_kernel(UserUid::ROOT),
            Some(Kuid::INITIAL_ROOT)
        );
        assert_eq!(
            gid_map.user_gid_to_kernel(UserGid::ROOT),
            Some(Kgid::INITIAL_ROOT)
        );
    }

    #[test]
    fn child_domain_requires_mapped_owners_and_finite_depth() {
        let root = MockNamespace::root();
        let identity = IdMap::try_identity().unwrap();
        let empty = IdMap::try_empty().unwrap();

        let child = UserNamespaceDomain::try_child(
            &root,
            &identity,
            &identity,
            kuid(1000),
            kgid(100),
            false,
        )
        .unwrap();
        assert_eq!(child.level(), 1);
        assert_eq!(child.owner_kuid(), kuid(1000));
        assert_eq!(child.owner_kgid(), kgid(100));
        assert!(!child.parent_could_setfcap());
        assert!(Arc::ptr_eq(&child.parent().unwrap(), &root));

        assert!(matches!(
            UserNamespaceDomain::try_child(&root, &empty, &identity, kuid(1000), kgid(100), false,),
            Err(CredError::NotPermitted)
        ));
        assert!(matches!(
            UserNamespaceDomain::try_child(&root, &identity, &empty, kuid(1000), kgid(100), false,),
            Err(CredError::NotPermitted)
        ));

        let exhausted = MockNamespace::at_level(USER_NAMESPACE_MAX_CREATION_PARENT_LEVEL + 1);
        assert!(matches!(
            UserNamespaceDomain::try_child(
                &exhausted,
                &identity,
                &identity,
                kuid(1000),
                kgid(100),
                false,
            ),
            Err(CredError::Capacity)
        ));

        let last_parent = MockNamespace::at_level(USER_NAMESPACE_MAX_CREATION_PARENT_LEVEL);
        let last_child = UserNamespaceDomain::try_child(
            &last_parent,
            &identity,
            &identity,
            kuid(1000),
            kgid(100),
            false,
        )
        .unwrap();
        assert_eq!(
            last_child.level(),
            USER_NAMESPACE_MAX_CREATION_PARENT_LEVEL + 1
        );
    }

    #[test]
    fn child_map_slots_share_empty_fallback_and_keep_snapshots_immutable() {
        let mut state = UserNamespaceMapState::try_child(true).unwrap();
        let empty_uid = state.uid_map();
        let empty_gid = state.gid_map();
        assert!(Arc::ptr_eq(&empty_uid, &empty_gid));
        assert!(!state.uid_map_written());
        assert!(!state.gid_map_written());
        assert!(!state.may_setgroups());

        let empty_owners = Arc::strong_count(&empty_uid);
        let replacement = IdMap::try_identity().unwrap();
        let replacement_owners = Arc::strong_count(&replacement);
        state.try_publish_uid_map(&replacement).unwrap();

        assert_eq!(Arc::strong_count(&empty_uid), empty_owners);
        assert_eq!(Arc::strong_count(&replacement), replacement_owners + 1);
        assert!(empty_uid.is_empty());
        assert!(empty_gid.is_empty());
        assert!(Arc::ptr_eq(&state.uid_map(), &replacement));
        assert!(Arc::ptr_eq(&state.gid_map(), &empty_gid));
        assert!(state.uid_map_written());
        assert!(!state.gid_map_written());
    }

    #[test]
    fn map_publication_rejects_empty_duplicate_and_fixed_slots_without_taking_ownership() {
        let mut state = UserNamespaceMapState::try_child(true).unwrap();
        let empty = IdMap::try_empty().unwrap();
        let empty_owners = Arc::strong_count(&empty);
        assert_eq!(
            state.try_publish_uid_map(&empty),
            Err(CredError::InvalidInput)
        );
        assert_eq!(
            state.try_publish_gid_map(&empty, false),
            Err(CredError::InvalidInput)
        );
        assert_eq!(Arc::strong_count(&empty), empty_owners);

        let uid = IdMap::try_identity().unwrap();
        state.try_publish_uid_map(&uid).unwrap();
        let uid_owners = Arc::strong_count(&uid);
        assert_eq!(
            state.try_publish_uid_map(&uid),
            Err(CredError::NotPermitted)
        );
        assert_eq!(Arc::strong_count(&uid), uid_owners);

        let gid = IdMap::try_identity().unwrap();
        state.try_publish_gid_map(&gid, false).unwrap();
        let gid_owners = Arc::strong_count(&gid);
        assert_eq!(
            state.try_publish_gid_map(&gid, false),
            Err(CredError::NotPermitted)
        );
        assert_eq!(Arc::strong_count(&gid), gid_owners);

        let mut initial = UserNamespaceMapState::try_initial().unwrap();
        let rejected = IdMap::try_identity().unwrap();
        let rejected_owners = Arc::strong_count(&rejected);
        assert_eq!(
            initial.try_publish_uid_map(&rejected),
            Err(CredError::NotPermitted)
        );
        assert_eq!(
            initial.try_publish_gid_map(&rejected, false),
            Err(CredError::NotPermitted)
        );
        assert_eq!(Arc::strong_count(&rejected), rejected_owners);
    }

    #[test]
    fn setgroups_transition_matrix_is_serialized_with_gid_publication() {
        let inherited_denial = UserNamespaceMapState::try_child(false).unwrap();
        assert!(!inherited_denial.setgroups_allowed());
        assert!(!inherited_denial.may_setgroups());

        let mut denied = UserNamespaceMapState::try_child(true).unwrap();
        assert!(!denied.may_setgroups());
        denied.try_update_setgroups_policy(true).unwrap();
        denied.try_update_setgroups_policy(false).unwrap();
        denied.try_update_setgroups_policy(false).unwrap();
        assert!(!denied.setgroups_allowed());
        assert_eq!(
            denied.try_update_setgroups_policy(true),
            Err(CredError::NotPermitted)
        );

        let denied_gid = IdMap::try_identity().unwrap();
        denied.try_publish_gid_map(&denied_gid, true).unwrap();
        assert!(denied.gid_map_written());
        assert!(!denied.may_setgroups());
        assert_eq!(
            denied.try_update_setgroups_policy(false),
            Err(CredError::NotPermitted)
        );

        let mut allowed = UserNamespaceMapState::try_child(true).unwrap();
        let allowed_gid = IdMap::try_identity().unwrap();
        allowed.try_publish_gid_map(&allowed_gid, false).unwrap();
        assert!(allowed.may_setgroups());
        allowed.try_update_setgroups_policy(true).unwrap();
        allowed.try_update_setgroups_policy(true).unwrap();
        assert_eq!(
            allowed.try_update_setgroups_policy(false),
            Err(CredError::NotPermitted)
        );

        let mut gated = UserNamespaceMapState::try_child(true).unwrap();
        let gated_gid = IdMap::try_identity().unwrap();
        let gated_owners = Arc::strong_count(&gated_gid);
        assert_eq!(
            gated.try_publish_gid_map(&gated_gid, true),
            Err(CredError::NotPermitted)
        );
        assert_eq!(Arc::strong_count(&gated_gid), gated_owners);
        assert!(!gated.gid_map_written());
        gated.try_update_setgroups_policy(false).unwrap();
        gated.try_publish_gid_map(&gated_gid, true).unwrap();
        assert!(gated.gid_map_written());
        assert!(!gated.may_setgroups());
    }
}
