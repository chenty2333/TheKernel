//! Special-keyring resolution, possession, and permission checks.

use super::*;

impl KeyManager {
    pub(super) fn special_keyring_in_namespace(
        &mut self,
        spec: i32,
        actor: &KeyActor,
        namespace: UserNamespaceId,
        create: bool,
    ) -> AxResult<i32> {
        match spec {
            KEY_SPEC_THREAD_KEYRING => {
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
            KEY_SPEC_PROCESS_KEYRING => {
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
            KEY_SPEC_SESSION_KEYRING => {
                if let Some(id) = self.session_keyrings.get(&actor.thread_owner) {
                    return Ok(*id);
                }
                if !create {
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
            KEY_SPEC_USER_KEYRING => {
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
            KEY_SPEC_USER_SESSION_KEYRING => {
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
            _ => Err(LinuxError::ENOKEY.into()),
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
            KeyState::Revoked => return Err(LinuxError::EKEYREVOKED.into()),
            KeyState::Positive => {}
        }
        if key
            .expires_at
            .is_some_and(|expires_at| wall_time().as_secs() >= expires_at)
        {
            return Err(LinuxError::EKEYEXPIRED.into());
        }
        if !allow_keyring && key.is_keyring() {
            return Err(AxError::InvalidInput);
        }
        Ok(())
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
        let mut pending = self
            .possession_roots(actor)
            .into_iter()
            .map(|serial| (serial, 0))
            .collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        while let Some((serial, depth)) = pending.pop() {
            if !visited.insert(serial) {
                continue;
            }

            let Some(key) = self.keys.get(&serial) else {
                continue;
            };
            if serial == target {
                return self.check_key_available(key, true).is_ok()
                    && key.perm.allows(
                        key.uid,
                        key.owner_gid,
                        &actor.dac,
                        true,
                        KeyPermission::SEARCH,
                    );
            }
            if depth > KEYRING_SEARCH_MAX_DEPTH {
                continue;
            }
            if self.check_key_available(key, true).is_err()
                || !key.perm.allows(
                    key.uid,
                    key.owner_gid,
                    &actor.dac,
                    true,
                    KeyPermission::SEARCH,
                )
            {
                continue;
            }
            if key.is_keyring() {
                pending.extend(key.links.iter().copied().map(|serial| (serial, depth + 1)));
            }
        }
        false
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
        Ok(key
            .perm
            .allows(key.uid, key.owner_gid, &actor.dac, possessed, permission))
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
