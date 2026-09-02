//! Keyring search, persistent keyrings, payload replacement, and
//! per-user accounting records.

use core::cell::Cell;

use thekernel_linux_keyring::{
    BfsRequest as LinuxBfsRequest, KeyError as LinuxKeyError, KeyId as LinuxKeyId,
    TraversalNode as LinuxTraversalNode, bfs as linux_bfs,
};

use super::{links::KeyGraphView, *};

fn map_linux_search_error(error: LinuxKeyError) -> AxError {
    match error {
        LinuxKeyError::NotFound => LinuxError::ENOKEY.into(),
        LinuxKeyError::Limit => LinuxError::ELOOP.into(),
        LinuxKeyError::Permission => LinuxError::EACCES.into(),
        LinuxKeyError::Invalid => AxError::InvalidInput,
        LinuxKeyError::Overflow | LinuxKeyError::State => AxError::BadState,
        LinuxKeyError::Exists | LinuxKeyError::Quota | LinuxKeyError::Cycle => {
            AxError::InvalidInput
        }
    }
}

impl KeyManager {
    pub(super) fn search_keyring(
        &self,
        keyring: impl Into<ResolvedKey>,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: &str,
        _visited: &mut BTreeSet<i32>,
    ) -> AxResult<Option<ResolvedKey>> {
        let keyring = keyring.into();
        let search_possessed = match keyring.possession {
            PossessionContext::Recompute => self.is_possessed(keyring.serial, actor),
            PossessionContext::Fixed(possessed) => possessed,
        };
        let root = self
            .keys
            .get(&keyring.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(root, true)?;
        if !root.is_keyring() {
            return Err(AxError::InvalidInput);
        }
        if !self.key_has_perm(keyring, actor, KeyPermission::SEARCH)? {
            return Err(LinuxError::EACCES.into());
        }

        let root = LinuxKeyId::new(keyring.serial as u32).ok_or(AxError::BadState)?;
        let first_error = Cell::new(None);
        let found = linux_bfs(
            &KeyGraphView::new(&self.keys),
            LinuxBfsRequest {
                roots: &[root],
                max_visits: self.keys.len().saturating_add(1),
                max_depth: KEYRING_SEARCH_MAX_DEPTH,
            },
            |id| {
                let serial = id.get() as i32;
                let key = self.keys.get(&serial).ok_or(LinuxKeyError::NotFound)?;
                let available = match self.check_key_available(key, true) {
                    Ok(()) => true,
                    Err(error) => {
                        if key.kind == kind
                            && key.description == description
                            && first_error.get().is_none()
                        {
                            first_error.set(Some(error));
                        }
                        false
                    }
                };
                let searchable = available
                    && Self::key_permission_allows(
                        key,
                        actor,
                        search_possessed,
                        KeyPermission::SEARCH,
                    );
                // The generic BFS planner only calls its match callback for
                // searchable nodes.  Linux key searches must still remember a
                // matching key that was reached but denied, so a later
                // accessible match wins while an otherwise empty search
                // reports EACCES rather than ENOKEY.
                if available
                    && !searchable
                    && key.kind == kind
                    && key.description == description
                    && first_error.get().is_none()
                {
                    first_error.set(Some(AxError::from(LinuxError::EACCES)));
                }
                Ok(LinuxTraversalNode {
                    available,
                    searchable,
                    keyring: key.is_keyring(),
                })
            },
            |id, _depth| {
                let key = self
                    .keys
                    .get(&(id.get() as i32))
                    .expect("planner key vanished");
                if key.kind != kind || key.description != description {
                    return false;
                }
                if Self::key_permission_allows(key, actor, search_possessed, KeyPermission::SEARCH)
                {
                    true
                } else {
                    if first_error.get().is_none() {
                        first_error.set(Some(AxError::from(LinuxError::EACCES)));
                    }
                    false
                }
            },
        )
        .map_err(map_linux_search_error)?;
        if let Some(found) = found {
            Ok(Some(ResolvedKey::with_possession(
                found.get() as i32,
                search_possessed,
            )))
        } else if let Some(error) = first_error.get() {
            Err(error)
        } else {
            Ok(None)
        }
    }

    pub(super) fn get_persistent_keyring_in_namespace(
        &mut self,
        uid: Kuid,
        actor: &KeyActor,
        namespace: UserNamespaceId,
    ) -> AxResult<ResolvedKey> {
        if let Some(serial) = self
            .namespaces
            .get(&namespace)
            .and_then(|registry| registry.persistent_keyrings.get(&uid))
            .copied()
        {
            let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
            let now = wall_time().as_secs();
            if key.expires_at.is_none_or(|expires_at| now < expires_at) {
                return Ok(ResolvedKey::possessed(serial));
            }
            if key.root_refs == 0 {
                return Err(AxError::BadState);
            }
            self.namespaces
                .get_mut(&namespace)
                .ok_or(AxError::BadState)?
                .persistent_keyrings
                .remove(&uid);
            self.release_root_ref(serial)?;
        }

        let serial = self.try_create_rooted_keyring_without_group(
            RootSource::Persistent(namespace, uid),
            format!("_persistent.{}", actor.display_uid(uid)),
            uid,
            persistent_keyring_permissions(),
            QuotaAdmission::Exempt,
        )?;
        // UID/CAP_SETUID authorization above acquires this key reference. Keep
        // that possession through the link transaction instead of resolving
        // the persistent root as an unrelated numeric serial.
        Ok(ResolvedKey::possessed(serial))
    }

    #[cfg(test)]
    pub(super) fn get_persistent_keyring(
        &mut self,
        uid: Kuid,
        actor: &KeyActor,
    ) -> AxResult<ResolvedKey> {
        let namespace = self.ensure_namespace_registry(actor)?;
        self.get_persistent_keyring_in_namespace(uid, actor, namespace)
    }

    pub(super) fn link_persistent_keyring(
        &mut self,
        destination: ResolvedKey,
        persistent: ResolvedKey,
        actor: &KeyActor,
    ) -> AxResult<()> {
        self.link_existing_key(destination, persistent, actor, false)?;
        self.keys
            .get_mut(&persistent.serial)
            .ok_or(AxError::BadState)?
            .expires_at = Some(
            wall_time()
                .as_secs()
                .saturating_add(PERSISTENT_KEYRING_TIMEOUT_SECS),
        );
        Ok(())
    }

    pub(super) fn current_search_keyrings(&self, actor: &KeyActor) -> Vec<ResolvedKey> {
        let mut keyrings = Vec::new();
        if let Some(id) = self.thread_keyrings.get(&actor.thread_owner) {
            keyrings.push(ResolvedKey::possessed(*id));
        }
        if let Some(id) = self.process_keyrings.get(&actor.process_owner) {
            keyrings.push(ResolvedKey::possessed(*id));
        }
        if let Some(id) = self.session_keyrings.get(&actor.thread_owner) {
            keyrings.push(ResolvedKey::possessed(*id));
        } else if let Some(id) = self
            .namespaces
            .get(&actor.user_ns.identity())
            .and_then(|registry| registry.user_session_keyrings.get(&actor.real_uid()))
        {
            keyrings.push(ResolvedKey::possessed(*id));
        }
        keyrings
    }

    pub(super) fn search_current(
        &self,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: &str,
    ) -> AxResult<Option<ResolvedKey>> {
        let mut first_error = None;
        for keyring in self.current_search_keyrings(actor) {
            match self.search_keyring(keyring, actor, kind, description, &mut BTreeSet::new()) {
                Ok(Some(serial)) => return Ok(Some(serial)),
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(None)
        }
    }

    pub(super) fn link_existing_request_result(
        &mut self,
        dest: i32,
        key: impl Into<ResolvedKey>,
        actor: &KeyActor,
        namespace: UserNamespaceId,
    ) -> AxResult<()> {
        if dest == 0 {
            return Ok(());
        }
        let destination = self.resolve_keyring_in_namespace(dest, actor, namespace, true)?;
        self.link_existing_key(destination, key, actor, false)
    }

    pub(super) fn replace_payload(&mut self, serial: i32, payload: Vec<u8>) -> AxResult<()> {
        let key = self
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        let kind = key.kind;
        if !kind.supports_payload_update() {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        if payload.len() > kind.payload_limit()
            || matches!(
                kind,
                KeyTypeKind::User | KeyTypeKind::Logon | KeyTypeKind::BigKey
            ) && payload.is_empty()
        {
            return Err(AxError::InvalidInput);
        }
        if kind == KeyTypeKind::BigKey {
            // Linux first reserves the raw input length, then commits the
            // type's 16-byte steady-state quota charge.
            let transient_abi = AbiQuotaCharge {
                keys: key.abi_charge.keys,
                bytes: key
                    .abi_charge
                    .bytes
                    .checked_sub(BIG_KEY_ABI_PAYLOAD_CHARGE)
                    .and_then(|bytes| bytes.checked_add(payload.len()))
                    .ok_or(AxError::NoMemory)?,
            };
            let _ = self.owners.plan_replace(
                key.quota_uid,
                key.ongoing_quota_admission(),
                key.abi_charge,
                transient_abi,
            )?;
        }
        let (new_abi, new_resident) = key.payload_charges(&payload)?;
        let plan = self.plan_key_resize(serial, new_abi, new_resident)?;
        let old_payload = core::mem::replace(
            &mut self
                .keys
                .get_mut(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?
                .payload,
            payload,
        );
        let apply_result = self.apply_key_resize(serial, new_abi, new_resident, plan);
        let mut old_payload = old_payload;
        wipe_key_bytes(&mut old_payload);
        drop(old_payload);
        apply_result?;
        Ok(())
    }

    pub(in crate::keyring) fn key_user_records(&mut self) -> AxResult<Vec<KeyUserRecord>> {
        self.prune_dead_namespaces()?;
        let mut live_keys = BTreeMap::<Kuid, usize>::new();
        for key in self.keys.values() {
            *live_keys.entry(key.quota_uid).or_default() += 1;
        }
        for uid in self.owners.usage.keys() {
            live_keys.entry(*uid).or_default();
        }
        Ok(live_keys
            .into_iter()
            .map(|(uid, keys)| {
                let quota = self.owners.usage(uid);
                KeyUserRecord {
                    uid,
                    // TheKernel has no transient key_user references: every
                    // live key owns exactly one record reference.
                    usage: keys,
                    keys,
                    // All currently supported key types are instantiated at
                    // publication and remain so through revocation.
                    instantiated_keys: keys,
                    quota_keys: quota.keys,
                    max_keys: user_maxkeys(uid),
                    quota_bytes: quota.bytes,
                    max_bytes: user_maxbytes(uid),
                }
            })
            .collect())
    }
}
