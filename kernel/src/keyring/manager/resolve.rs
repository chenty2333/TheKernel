//! Special-keyring resolution, possession, and permission checks.

use thekernel_linux_keyring::{
    KeyAvailability as LinuxKeyAvailability, KeyError as LinuxKeyError, KeyId as LinuxKeyId,
    KeyPermissions as LinuxKeyPermissions, MissingSessionPlan as LinuxMissingSessionPlan,
    PermissionInput as LinuxPermissionInput, PermissionLanes as LinuxPermissionLanes,
    SpecialKeyring as LinuxSpecialKeyring, TraversalNode as LinuxTraversalNode,
    availability as linux_availability, is_possessed as linux_is_possessed,
    permits as linux_permits, plan_missing_session as linux_plan_missing_session,
    resolve_special_keyring as linux_resolve_special_keyring,
};

use super::{links::KeyGraphView, *};

impl KeyManager {
    /// Bridges kernel-owned credentials and a validated UAPI permission mask
    /// into the ABI planner.  The manager retains object ownership only.
    pub(super) fn key_permission_allows(
        key: &Key,
        actor: &KeyActor,
        possessed: bool,
        permission: KeyPermission,
    ) -> bool {
        let raw = key.perm.into_raw();
        let lane = |shift: u32| LinuxKeyPermissions((raw >> shift) & 0x3f);
        linux_permits(
            LinuxPermissionLanes {
                possessor: Some(lane(24)),
                owner: Some(lane(16)),
                group: Some(lane(8)),
                other: Some(lane(0)),
            },
            LinuxPermissionInput {
                possessed,
                owner: actor.dac.uid() == key.uid,
                group: key.owner_gid.is_some_and(|gid| actor.in_group(gid)),
            },
            LinuxKeyPermissions(permission.bits() as u32),
        )
    }

    pub(super) fn special_keyring_in_namespace(
        &mut self,
        spec: i32,
        actor: &KeyActor,
        namespace: UserNamespaceId,
        create: bool,
    ) -> AxResult<i32> {
        match linux_resolve_special_keyring(spec).map_err(|_| AxError::from(LinuxError::ENOKEY))? {
            LinuxSpecialKeyring::Thread => {
                if let Some(id) = self.thread_keyrings.get(&actor.thread_owner) {
                    return Ok(*id);
                }
                if !create {
                    return Err(LinuxError::ENOKEY.into());
                }
                let id = self.try_create_rooted_keyring(
                    RootSource::Thread(actor.thread_owner),
                    format!("_tid.{}", actor.tid),
                    actor.real_uid(),
                    actor.real_gid(),
                    thread_process_keyring_permissions(),
                    QuotaAdmission::AllowOverrun,
                )?;
                Ok(id)
            }
            LinuxSpecialKeyring::Process => {
                if let Some(id) = self.process_keyrings.get(&actor.process_owner) {
                    return Ok(*id);
                }
                if !create {
                    return Err(LinuxError::ENOKEY.into());
                }
                let id = self.try_create_rooted_keyring(
                    RootSource::Process(actor.process_owner),
                    format!("_pid.{}", actor.pid),
                    actor.real_uid(),
                    actor.real_gid(),
                    thread_process_keyring_permissions(),
                    QuotaAdmission::AllowOverrun,
                )?;
                Ok(id)
            }
            LinuxSpecialKeyring::Session => {
                if let Some(id) = self.session_keyrings.get(&actor.thread_owner) {
                    return Ok(*id);
                }
                if linux_plan_missing_session(create) == LinuxMissingSessionPlan::InstallUserSession
                {
                    let id = self.special_keyring_in_namespace(
                        KEY_SPEC_USER_SESSION_KEYRING,
                        actor,
                        namespace,
                        true,
                    )?;
                    self.install_root(RootSource::Session(actor.thread_owner), id)?;
                    return Ok(id);
                }
                let id = self.try_create_rooted_keyring(
                    RootSource::Session(actor.thread_owner),
                    format!("_ses.{}", actor.pid),
                    actor.real_uid(),
                    actor.real_gid(),
                    anonymous_session_keyring_permissions(),
                    QuotaAdmission::AllowOverrun,
                )?;
                Ok(id)
            }
            LinuxSpecialKeyring::User => {
                let uid = actor.real_uid();
                if let Some(id) = self
                    .namespaces
                    .get(&namespace)
                    .and_then(|registry| registry.user_keyrings.get(&uid))
                {
                    return Ok(*id);
                }
                let id = self.try_create_rooted_keyring_without_group(
                    RootSource::User(namespace, uid),
                    format!("_uid.{}", actor.display_uid(uid)),
                    uid,
                    uid_keyring_permissions(),
                    QuotaAdmission::Enforced,
                )?;
                Ok(id)
            }
            LinuxSpecialKeyring::UserSession => {
                let uid = actor.real_uid();
                if let Some(id) = self
                    .namespaces
                    .get(&namespace)
                    .and_then(|registry| registry.user_session_keyrings.get(&uid))
                {
                    return Ok(*id);
                }
                let user_keyring = self.special_keyring_in_namespace(
                    KEY_SPEC_USER_KEYRING,
                    actor,
                    namespace,
                    true,
                )?;
                let id = self.try_create_keyring_without_group(
                    format!("_uid_ses.{}", actor.display_uid(uid)),
                    uid,
                    uid_keyring_permissions(),
                    QuotaAdmission::Enforced,
                )?;
                if let Err(error) = self.link_key_replace(id, user_keyring) {
                    self.discard_new_key(id)?;
                    return Err(error);
                }
                if let Err(error) = self.install_root(RootSource::UserSession(namespace, uid), id) {
                    self.discard_new_key(id)?;
                    return Err(error);
                }
                Ok(id)
            }
        }
    }

    #[cfg(test)]
    pub(super) fn special_keyring(
        &mut self,
        spec: i32,
        actor: &KeyActor,
        create: bool,
    ) -> AxResult<i32> {
        let namespace = self.ensure_namespace_registry(actor)?;
        self.special_keyring_in_namespace(spec, actor, namespace, create)
    }

    pub(super) fn resolve_keyring_in_namespace(
        &mut self,
        id: i32,
        actor: &KeyActor,
        namespace: UserNamespaceId,
        create: bool,
    ) -> AxResult<ResolvedKey> {
        let resolved = self.resolve_key_in_namespace(id, actor, namespace, create)?;
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if key.is_keyring() {
            Ok(resolved)
        } else {
            Err(AxError::InvalidInput)
        }
    }

    #[cfg(test)]
    pub(super) fn resolve_keyring(
        &mut self,
        id: i32,
        actor: &KeyActor,
        create: bool,
    ) -> AxResult<ResolvedKey> {
        let namespace = self.ensure_namespace_registry(actor)?;
        self.resolve_keyring_in_namespace(id, actor, namespace, create)
    }

    pub(super) fn resolve_key_in_namespace(
        &mut self,
        id: i32,
        actor: &KeyActor,
        namespace: UserNamespaceId,
        create_special: bool,
    ) -> AxResult<ResolvedKey> {
        let resolved = if id < 0 {
            ResolvedKey::possessed(self.special_keyring_in_namespace(
                id,
                actor,
                namespace,
                create_special,
            )?)
        } else {
            ResolvedKey::numeric(id)
        };
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        Ok(resolved)
    }

    pub(super) fn check_key_available(&self, key: &Key, allow_keyring: bool) -> AxResult<()> {
        match key.state {
            KeyState::Pending => return Err(LinuxError::EINPROGRESS.into()),
            KeyState::Negative(error) => {
                return Err(match error {
                    error if error == LinuxError::EACCES as i32 => LinuxError::EACCES.into(),
                    error if error == LinuxError::EKEYREVOKED as i32 => {
                        LinuxError::EKEYREVOKED.into()
                    }
                    error if error == LinuxError::EKEYEXPIRED as i32 => {
                        LinuxError::EKEYEXPIRED.into()
                    }
                    _ => LinuxError::ENOKEY.into(),
                });
            }
            KeyState::Positive | KeyState::Revoked => {}
        }
        let expired = key
            .expires_at
            .is_some_and(|expires_at| wall_time().as_secs() >= expires_at);
        match linux_availability(
            key.state == KeyState::Revoked,
            expired,
            key.is_keyring(),
            allow_keyring,
        ) {
            LinuxKeyAvailability::Available => Ok(()),
            LinuxKeyAvailability::Revoked => Err(LinuxError::EKEYREVOKED.into()),
            LinuxKeyAvailability::Expired => Err(LinuxError::EKEYEXPIRED.into()),
            LinuxKeyAvailability::WrongKind => Err(AxError::InvalidInput),
        }
    }

    pub(super) fn possession_roots(&self, actor: &KeyActor) -> Vec<i32> {
        let mut roots = Vec::new();
        if let Some(id) = self.thread_keyrings.get(&actor.thread_owner) {
            roots.push(*id);
        }
        if let Some(id) = self.process_keyrings.get(&actor.process_owner) {
            roots.push(*id);
        }
        if let Some(id) = self.session_keyrings.get(&actor.thread_owner) {
            roots.push(*id);
        } else if let Some(id) = self
            .namespaces
            .get(&actor.user_ns.identity())
            .and_then(|registry| registry.user_session_keyrings.get(&actor.real_uid()))
        {
            roots.push(*id);
        }
        roots
    }

    pub(super) fn is_possessed(&self, target: i32, actor: &KeyActor) -> bool {
        let roots = self
            .possession_roots(actor)
            .into_iter()
            .map(|serial| LinuxKeyId::new(serial as u32))
            .collect::<Option<Vec<_>>>();
        let Some(roots) = roots else {
            return false;
        };
        let Some(target) = LinuxKeyId::new(target as u32) else {
            return false;
        };
        linux_is_possessed(
            &KeyGraphView::new(&self.keys),
            &roots,
            target,
            self.keys.len().saturating_add(1),
            KEYRING_SEARCH_MAX_DEPTH,
            |id| {
                let key = self
                    .keys
                    .get(&(id.get() as i32))
                    .ok_or(LinuxKeyError::NotFound)?;
                Ok(LinuxTraversalNode {
                    available: self.check_key_available(key, true).is_ok(),
                    searchable: Self::key_permission_allows(
                        key,
                        actor,
                        true,
                        KeyPermission::SEARCH,
                    ),
                    keyring: key.is_keyring(),
                })
            },
        )
        .unwrap_or(false)
    }

    pub(super) fn key_has_perm(
        &self,
        key: impl Into<ResolvedKey>,
        actor: &KeyActor,
        permission: KeyPermission,
    ) -> AxResult<bool> {
        let resolved = key.into();
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        let possessed = match resolved.possession {
            PossessionContext::Recompute => self.is_possessed(resolved.serial, actor),
            PossessionContext::Fixed(possessed) => possessed,
        };
        Ok(Self::key_permission_allows(
            key, actor, possessed, permission,
        ))
    }

    pub(super) fn keyring_has_write(
        &self,
        keyring: impl Into<ResolvedKey>,
        actor: &KeyActor,
    ) -> AxResult<bool> {
        self.keyring_has_perm(keyring, actor, KeyPermission::WRITE)
    }

    pub(super) fn keyring_has_perm(
        &self,
        keyring: impl Into<ResolvedKey>,
        actor: &KeyActor,
        permission: KeyPermission,
    ) -> AxResult<bool> {
        let resolved = keyring.into();
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        Ok(key.is_keyring() && self.key_has_perm(resolved, actor, permission)?)
    }

    pub(super) fn find_linked_key(
        &self,
        keyring: i32,
        kind: KeyTypeKind,
        description: &str,
    ) -> Option<i32> {
        let keyring = self.keys.get(&keyring)?;
        keyring.links.iter().copied().find(|serial| {
            self.keys
                .get(serial)
                .is_some_and(|key| key.kind == kind && key.description == description)
        })
    }
}
