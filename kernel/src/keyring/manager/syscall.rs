//! The `add_key`, `request_key`, and `keyctl` entry points.

use super::*;

enum ConstructionResult {
    Positive(Vec<u8>),
    Negative(i32, u64),
}

/// `keyctl_reject_key()`'s error-code filter.
///
/// The negative-key error must be a real errno: positive, below `MAX_ERRNO`,
/// and not one of the restart-block codes `keyctl.c` lists explicitly.
pub(super) fn valid_rejection_error(error: i32) -> bool {
    const MAX_ERRNO: i32 = 4095;
    const ERESTARTSYS: i32 = 512;
    const ERESTARTNOINTR: i32 = 513;
    const ERESTARTNOHAND: i32 = 514;
    const ERESTART_RESTARTBLOCK: i32 = 516;
    error > 0
        && error < MAX_ERRNO
        && error != ERESTARTSYS
        && error != ERESTARTNOINTR
        && error != ERESTARTNOHAND
        && error != ERESTART_RESTARTBLOCK
}

/// The key type's own payload rule, re-applied when a pending key is
/// instantiated.
///
/// `key_instantiate_and_link()` fills in `prep.orig_description` and calls
/// `key->type->preparse()` again, so `KEYCTL_INSTANTIATE` enforces exactly the
/// same limits as `add_key()`: a keyring takes no payload, and `user`/`logon`
/// take 1..=32767 bytes.
pub(super) fn validate_instantiated_payload(kind: KeyTypeKind, payload: &[u8]) -> AxResult<()> {
    match kind {
        KeyTypeKind::Keyring => {
            if !payload.is_empty() {
                return Err(AxError::InvalidInput);
            }
        }
        KeyTypeKind::User | KeyTypeKind::Logon | KeyTypeKind::BigKey => {
            if payload.is_empty() || payload.len() > kind.payload_limit() {
                return Err(AxError::InvalidInput);
            }
        }
    }
    Ok(())
}

impl KeyManager {
    fn complete_construction(
        &mut self,
        actor: &KeyActor,
        serial: i32,
        result: ConstructionResult,
        destination: i32,
    ) -> AxResult<()> {
        if self
            .construction_authorities
            .get(&actor.thread_owner)
            .copied()
            != Some(serial)
        {
            return Err(LinuxError::EACCES.into());
        }
        let namespace = self.ensure_namespace_registry(actor)?;
        let pending = self
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if pending.state != KeyState::Pending
            || pending.construction_owner != Some(actor.thread_owner)
        {
            return Err(LinuxError::EACCES.into());
        }
        // Resolve and authorize the destination before consuming construction
        // authority or publishing the key state. A bad destination leaves the
        // pending key available for a corrected helper completion.
        let destination = if destination != 0 {
            let destination =
                self.resolve_keyring_in_namespace(destination, actor, namespace, true)?;
            if !self.keyring_has_write(destination, actor)? {
                return Err(LinuxError::EACCES.into());
            }
            Some(destination)
        } else {
            None
        };
        match result {
            ConstructionResult::Positive(payload) => {
                let kind = self.keys.get(&serial).ok_or(AxError::BadState)?.kind;
                validate_instantiated_payload(kind, &payload)?;
                self.replace_payload(serial, payload)?;
                self.keys.get_mut(&serial).ok_or(AxError::BadState)?.state = KeyState::Positive;
            }
            ConstructionResult::Negative(error, timeout) => {
                let key = self.keys.get_mut(&serial).ok_or(AxError::BadState)?;
                wipe_key_bytes(&mut key.payload);
                key.payload.clear();
                key.state = KeyState::Negative(error);
                key.expires_at =
                    (timeout != 0).then(|| wall_time().as_secs().saturating_add(timeout));
            }
        }
        if let Some(destination) = destination {
            self.link_key_replace(destination.serial, serial)?;
        }
        let key = self.keys.get_mut(&serial).ok_or(AxError::BadState)?;
        key.construction_owner = None;
        self.construction_authorities.remove(&actor.thread_owner);
        self.remove_pending_construction(serial);
        crate::keyring::service::notify_request_key_waiters();
        Ok(())
    }
    pub(in crate::keyring) fn add_key(
        &mut self,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: String,
        payload: Vec<u8>,
        keyring: i32,
    ) -> AxResult<isize> {
        let namespace = self.ensure_namespace_registry(actor)?;
        let manager = self;
        let keyring = manager.resolve_keyring_in_namespace(keyring, actor, namespace, true)?;
        if !manager.keyring_has_write(keyring, actor)? {
            return Err(LinuxError::EACCES.into());
        }
        if kind.supports_payload_update()
            && let Some(serial) = manager.find_linked_key(keyring.serial, kind, &description)
        {
            if !manager.key_has_perm(serial, actor, KeyPermission::WRITE)? {
                return Err(LinuxError::EACCES.into());
            }
            manager.replace_payload(serial, payload)?;
            return Ok(serial as isize);
        }

        let publish_name = kind == KeyTypeKind::Keyring
            && !description.is_empty()
            && !description.starts_with('.');

        manager.check_link_destination(keyring.serial)?;
        let key = Key::positive(
            kind,
            description,
            payload,
            actor.owner_uid(),
            actor.owner_gid(),
        );
        let serial = manager.try_insert_key(key?, QuotaAdmission::Enforced)?;
        let publication = if publish_name {
            match manager.plan_name_publication(serial, namespace) {
                Ok(publication) => Some(publication),
                Err(error) => {
                    manager.discard_new_key(serial)?;
                    return Err(error);
                }
            }
        } else {
            None
        };
        if let Err(error) = manager.link_key_replace(keyring.serial, serial) {
            manager.discard_new_key(serial)?;
            return Err(error);
        }
        if let Some((publication, next)) = publication {
            manager.commit_name_publication(serial, publication, next);
        }
        Ok(serial as isize)
    }

    pub(in crate::keyring) fn begin_request_key(
        &mut self,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: &str,
        callout: Option<&str>,
        dest_keyring: i32,
    ) -> AxResult<RequestKeyBegin> {
        let namespace = self.ensure_namespace_registry(actor)?;
        if let Some(resolved) = self.search_current(actor, kind, description)? {
            let key = self
                .keys
                .get(&resolved.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            self.check_key_available(key, true)?;
            self.link_existing_request_result(dest_keyring, resolved, actor, namespace)?;
            return Ok(RequestKeyBegin::Resolved(resolved.serial as isize));
        }

        // `request_key_and_link()` only consults `/sbin/request-key` when
        // `callout_info` was supplied.  A miss with a NULL callout is
        // immediately -ENOKEY, and a keyring is never constructed by an
        // upcall at all (`construct_key_and_link()` returns -EPERM).
        let Some(callout) = callout else {
            return Err(LinuxError::ENOKEY.into());
        };
        if kind == KeyTypeKind::Keyring {
            return Err(AxError::OperationNotPermitted);
        }
        let callout = callout.to_string();
        let pending_id = PendingConstructionKey::new(namespace, kind, description);
        if let Some(serial) = self.pending_constructions.get(&pending_id).copied() {
            if self
                .keys
                .get(&serial)
                .is_some_and(|key| key.state == KeyState::Pending)
            {
                return Ok(RequestKeyBegin::Pending(serial));
            }
            // Terminal transitions erase their index entry before releasing
            // the manager lock.  A stale entry therefore denotes an older
            // interrupted cleanup and is safe to discard before retrying.
            self.pending_constructions.remove(&pending_id);
        }
        // The key is deliberately ownerless until the helper task exists.
        // This prevents the requester from assuming authority between pending
        // key publication and helper process publication.
        let mut key = Key::pending(
            kind,
            description.to_string(),
            actor.owner_uid(),
            actor.owner_gid(),
        )?;
        key.resident_charge.bytes = key
            .resident_charge
            .bytes
            .checked_add(callout.capacity())
            .ok_or(AxError::NoMemory)?;
        key.construction_callout = Some(callout);
        let serial = self.try_insert_key(key, QuotaAdmission::Enforced)?;
        self.pending_constructions.insert(pending_id, serial);
        let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
        Ok(RequestKeyBegin::Construction(RequestKeyConstruction {
            serial,
            kind,
            description: key.description.clone(),
            callout: key
                .construction_callout
                .as_ref()
                .ok_or(AxError::BadState)?
                .clone(),
        }))
    }

    /// Installs the only construction authority after the helper's task
    /// identity is published. The target task never inherits the requester's
    /// ordinary key roots; it receives just this serial and it is consumed by
    /// one instantiate/negate/reject transition or task exit.
    pub(in crate::keyring) fn install_construction_authority(
        &mut self,
        serial: i32,
        helper_thread_owner: u32,
    ) -> AxResult<()> {
        if self
            .construction_authorities
            .contains_key(&helper_thread_owner)
        {
            return Err(LinuxError::EACCES.into());
        }
        let key = self
            .keys
            .get_mut(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if key.state != KeyState::Pending || key.construction_owner.is_some() {
            return Err(LinuxError::EACCES.into());
        }
        key.construction_owner = Some(helper_thread_owner);
        self.construction_authorities
            .insert(helper_thread_owner, serial);
        Ok(())
    }

    /// Rechecks a completed construction under the requester's key namespace
    /// and performs the delayed destination-keyring link. Pending stays
    /// `EINPROGRESS` only inside the service wait loop; it is never returned
    /// to userspace by request_key(2).
    pub(in crate::keyring) fn finish_request_key(
        &mut self,
        actor: &KeyActor,
        serial: i32,
        dest_keyring: i32,
    ) -> AxResult<isize> {
        let namespace = self.ensure_namespace_registry(actor)?;
        let key = self
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        self.link_existing_request_result(
            dest_keyring,
            ResolvedKey::numeric(serial),
            actor,
            namespace,
        )?;
        Ok(serial as isize)
    }

    pub(in crate::keyring) fn abort_request_key(&mut self, serial: i32) -> AxResult<()> {
        let key = self
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if key.state != KeyState::Pending {
            return Ok(());
        }
        let authority_owner = key.construction_owner;
        // Validate the reverse authority edge before retiring the key.  The
        // actual removal follows the successful object retirement so an
        // allocation failure in `remove_key_everywhere` cannot leave a live
        // pending key ownerless; once retirement wins, no stale authority may
        // outlive the serial.
        if let Some(owner) = authority_owner
            && self.construction_authorities.get(&owner).copied() != Some(serial)
        {
            return Err(AxError::BadState);
        }
        self.remove_key_everywhere(serial)?;
        if let Some(owner) = authority_owner {
            self.construction_authorities.remove(&owner);
        }
        self.remove_pending_construction(serial);
        Ok(())
    }

    /// Resolve a request if its helper completed; otherwise remove the
    /// pending construction while retaining the manager mutex.  This is the
    /// deadline/interrupt linearization point: a helper completion which got
    /// the mutex first always wins, while a still-pending construction is
    /// fully detached (including its authority) before another helper can
    /// observe it.
    pub(in crate::keyring) fn finish_or_abort_request_key(
        &mut self,
        actor: &KeyActor,
        serial: i32,
        dest_keyring: i32,
    ) -> AxResult<isize> {
        match self.finish_request_key(actor, serial, dest_keyring) {
            Err(error) if error == LinuxError::EINPROGRESS.into() => {
                self.abort_request_key(serial)?;
                Err(LinuxError::ENOKEY.into())
            }
            result => result,
        }
    }

    pub(super) fn remove_pending_construction(&mut self, serial: i32) {
        self.pending_constructions
            .retain(|_, indexed| *indexed != serial);
    }

    // Keep the manager-local legacy test surface while the service owns the
    // sleepable helper lifecycle. Production callers use begin_request_key.
    #[cfg(test)]
    pub(in crate::keyring) fn request_key(
        &mut self,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: &str,
        callout: Option<&str>,
        dest_keyring: i32,
    ) -> AxResult<isize> {
        match self.begin_request_key(actor, kind, description, callout, dest_keyring)? {
            RequestKeyBegin::Resolved(serial) => Ok(serial),
            RequestKeyBegin::Pending(_) => Err(LinuxError::EINPROGRESS.into()),
            RequestKeyBegin::Construction(_) => Err(LinuxError::EINPROGRESS.into()),
        }
    }

    pub(in crate::keyring) fn keyctl(
        &mut self,
        actor: &KeyActor,
        command: KeyctlCommand,
    ) -> AxResult<KeyctlOutput> {
        let mapped_chown = match &command {
            KeyctlCommand::Chown { uid, gid, .. } => {
                if uid.is_none() && gid.is_none() {
                    return Ok(KeyctlOutput::Value(0));
                }
                Some((
                    uid.map(|uid| actor.map_user_uid(uid)).transpose()?,
                    gid.map(|gid| actor.map_user_gid(gid)).transpose()?,
                ))
            }
            _ => None,
        };
        let namespace = self.ensure_namespace_registry(actor)?;
        let manager = self;
        let value = match command {
            KeyctlCommand::AssumeAuthority { key } => {
                // `keyctl_assume_authority()`: every `KEY_SPEC_*` negative
                // serial, including -1, is rejected before the lookup.
                if key < 0 {
                    return Err(AxError::InvalidInput);
                }
                let serial = if key == 0 {
                    manager
                        .construction_authorities
                        .get(&actor.thread_owner)
                        .copied()
                } else {
                    Some(key)
                }
                .ok_or(AxError::from(LinuxError::EACCES))?;
                let pending = manager
                    .keys
                    .get(&serial)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                if pending.state != KeyState::Pending
                    || pending.construction_owner != Some(actor.thread_owner)
                {
                    return Err(LinuxError::EACCES.into());
                }
                manager
                    .construction_authorities
                    .insert(actor.thread_owner, serial);
                0
            }
            KeyctlCommand::Instantiate {
                key,
                payload,
                destination,
            } => {
                manager.complete_construction(
                    actor,
                    key,
                    ConstructionResult::Positive(payload),
                    destination,
                )?;
                0
            }
            KeyctlCommand::Negate {
                key,
                timeout,
                destination,
            } => {
                manager.complete_construction(
                    actor,
                    key,
                    ConstructionResult::Negative(LinuxError::ENOKEY as i32, timeout),
                    destination,
                )?;
                0
            }
            KeyctlCommand::Reject {
                key,
                timeout,
                error,
                destination,
            } => {
                // `keyctl_reject_key()` validates the error code before it
                // looks at the assumed authority: it must be a positive errno
                // below MAX_ERRNO and outside the restart-block magic range.
                if !valid_rejection_error(error) {
                    return Err(AxError::InvalidInput);
                }
                manager.complete_construction(
                    actor,
                    key,
                    ConstructionResult::Negative(error, timeout),
                    destination,
                )?;
                0
            }
            KeyctlCommand::GetKeyringId { keyring, create } => {
                let serial =
                    manager.resolve_keyring_in_namespace(keyring, actor, namespace, create)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::SEARCH)? {
                    return Err(LinuxError::EACCES.into());
                }
                serial.serial as isize
            }
            KeyctlCommand::JoinSession { name } => {
                let (serial, publication, created) = if let Some(name) = name.as_deref() {
                    if name.starts_with('.') {
                        return Err(AxError::OperationNotPermitted);
                    }
                    if name.is_empty() {
                        // `keyring_alloc("")` reaches `key_alloc()`, which
                        // rejects an empty description with -EINVAL.
                        return Err(AxError::InvalidInput);
                    } else {
                        let existing = manager
                            .keys
                            .iter()
                            .filter_map(|(serial, key)| {
                                let publication = key.published_name?;
                                (publication.namespace == namespace
                                    && key.is_keyring()
                                    && key.description == name
                                    && actor.user_ns.kernel_uid_to_user(key.quota_uid).is_some()
                                    && manager.check_key_available(key, true).is_ok()
                                    && KeyManager::key_permission_allows(
                                        key,
                                        actor,
                                        false,
                                        KeyPermission::SEARCH,
                                    ))
                                .then_some((publication.order, *serial))
                            })
                            .min_by_key(|(order, _)| *order)
                            .map(|(_, serial)| serial);
                        if let Some(serial) = existing {
                            (serial, None, false)
                        } else {
                            let serial = manager.try_create_keyring(
                                name.to_string(),
                                actor.real_uid(),
                                actor.real_gid(),
                                named_session_keyring_permissions(),
                                QuotaAdmission::Enforced,
                            )?;
                            let publication = match manager.plan_name_publication(serial, namespace)
                            {
                                Ok(publication) => publication,
                                Err(error) => {
                                    manager.discard_new_key(serial)?;
                                    return Err(error);
                                }
                            };
                            (serial, Some(publication), true)
                        }
                    }
                } else {
                    let admission = manager.anonymous_session_admission(actor.thread_owner);
                    (
                        manager.try_create_keyring(
                            format!("_ses.{}", actor.pid),
                            actor.real_uid(),
                            actor.real_gid(),
                            anonymous_session_keyring_permissions(),
                            admission,
                        )?,
                        None,
                        true,
                    )
                };
                if let Err(error) =
                    manager.install_root(RootSource::Session(actor.thread_owner), serial)
                {
                    if created {
                        manager.discard_new_key(serial)?;
                    }
                    return Err(error);
                }
                if let Some((publication, next)) = publication {
                    manager.commit_name_publication(serial, publication, next);
                }
                serial as isize
            }
            KeyctlCommand::Update { key, payload } => {
                let serial = manager.resolve_key_in_namespace(key, actor, namespace, false)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::WRITE)? {
                    return Err(LinuxError::EACCES.into());
                }
                manager.replace_payload(serial.serial, payload)?;
                0
            }
            KeyctlCommand::Revoke { key } => {
                let serial = manager.resolve_key_in_namespace(key, actor, namespace, false)?;
                let can_write = manager.key_has_perm(serial, actor, KeyPermission::WRITE)?;
                let can_setattr = manager.key_has_perm(serial, actor, KeyPermission::SETATTR)?;
                if !can_write && !can_setattr {
                    return Err(LinuxError::EACCES.into());
                }
                manager.revoke_key(serial.serial)?;
                0
            }
            KeyctlCommand::Chown { key, .. } => {
                let (uid, gid) = mapped_chown.ok_or(AxError::BadState)?;
                let serial = manager.resolve_key_in_namespace(key, actor, namespace, false)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::SETATTR)? {
                    return Err(LinuxError::EACCES.into());
                }
                let (old_uid, old_quota_uid, old_gid, charge, admission) = manager
                    .keys
                    .get(&serial.serial)
                    .map(|key| {
                        (
                            key.uid,
                            key.quota_uid,
                            key.owner_gid,
                            key.abi_charge,
                            key.ongoing_quota_admission(),
                        )
                    })
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                if uid.is_some_and(|uid| uid != old_uid) && !actor.has_sys_admin {
                    return Err(AxError::OperationNotPermitted);
                }
                if gid.is_some_and(|gid| Some(gid) != old_gid)
                    && !actor.has_sys_admin
                    && !gid.is_some_and(|gid| actor.in_group(gid))
                {
                    return Err(AxError::OperationNotPermitted);
                }
                let uid = uid.filter(|uid| *uid != old_uid);
                let owner_updates = uid
                    .filter(|uid| *uid != old_quota_uid)
                    .map(|uid| {
                        manager
                            .owners
                            .plan_transfer(old_quota_uid, uid, admission, charge)
                    })
                    .transpose()?
                    .unwrap_or([None, None]);
                let key = manager
                    .keys
                    .get_mut(&serial.serial)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                if let Some(uid) = uid {
                    key.uid = uid;
                    key.quota_uid = uid;
                }
                if let Some(gid) = gid {
                    key.owner_gid = Some(gid);
                }
                manager.owners.apply(owner_updates[0]);
                manager.owners.apply(owner_updates[1]);
                0
            }
            KeyctlCommand::SetPerm { key, permissions } => {
                let serial = manager.resolve_key_in_namespace(key, actor, namespace, false)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::SETATTR)? {
                    return Err(LinuxError::EACCES.into());
                }
                let owner = manager
                    .keys
                    .get(&serial.serial)
                    .map(|key| key.uid)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                if !actor.has_sys_admin && owner != actor.owner_uid() {
                    return Err(AxError::OperationNotPermitted);
                }
                manager
                    .keys
                    .get_mut(&serial.serial)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?
                    .perm = permissions;
                0
            }
            KeyctlCommand::Describe { key } => {
                let serial = manager.resolve_key_in_namespace(key, actor, namespace, false)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::VIEW)? {
                    return Err(LinuxError::EACCES.into());
                }
                let key = manager
                    .keys
                    .get(&serial.serial)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                let description = format!(
                    "{};{};{};{:08x};{}\0",
                    key.kind.name(),
                    actor.display_uid(key.uid),
                    actor.display_key_gid(key.owner_gid),
                    key.perm.into_raw(),
                    key.description
                )
                .into_bytes();
                return Ok(KeyctlOutput::CountedBytes(description));
            }
            KeyctlCommand::Clear { keyring } => {
                let keyring =
                    manager.resolve_keyring_in_namespace(keyring, actor, namespace, false)?;
                if !manager.keyring_has_write(keyring, actor)? {
                    return Err(LinuxError::EACCES.into());
                }
                manager.clear_keyring_links(keyring.serial)?;
                0
            }
            KeyctlCommand::Link { key, keyring } => {
                let serial = manager.resolve_key_in_namespace(key, actor, namespace, false)?;
                let keyring =
                    manager.resolve_keyring_in_namespace(keyring, actor, namespace, true)?;
                manager.link_existing_key(keyring, serial, actor, false)?;
                0
            }
            KeyctlCommand::Unlink { serial, keyring } => {
                let keyring =
                    manager.resolve_keyring_in_namespace(keyring, actor, namespace, false)?;
                if !manager.keyring_has_write(keyring, actor)? {
                    return Err(LinuxError::EACCES.into());
                }
                manager.unlink_key_from_keyring(keyring.serial, serial)?;
                0
            }
            KeyctlCommand::Search {
                keyring,
                type_name,
                description,
                destination,
            } => {
                let keyring =
                    manager.resolve_keyring_in_namespace(keyring, actor, namespace, false)?;
                // `keyctl_keyring_search()` propagates `key_type_lookup()`
                // unchanged, so an unregistered type is -ENOKEY here.
                let kind = KeyTypeKind::from_name(&type_name)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                let serial = manager
                    .search_keyring(keyring, actor, kind, &description, &mut BTreeSet::new())?
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                if let Some(destination) = destination {
                    let dest = manager.resolve_keyring_in_namespace(
                        destination,
                        actor,
                        namespace,
                        true,
                    )?;
                    manager.link_existing_key(dest, serial, actor, false)?;
                }
                serial.serial as isize
            }
            KeyctlCommand::Read { key, copy_limit } => {
                // `keyctl_read_key()` reports a failed lookup as -ENOKEY: a
                // nonexistent serial, an unusable `KEY_SPEC_*` id and an
                // out-of-range id are all collapsed into it before the key's
                // own state is inspected.
                let serial = manager
                    .resolve_key_in_namespace(key, actor, namespace, false)
                    .map_err(|error| match LinuxError::from(error) {
                        LinuxError::ENOKEY | LinuxError::EINVAL => {
                            AxError::from(LinuxError::ENOKEY)
                        }
                        other => AxError::from(other),
                    })?;
                if !manager.key_has_perm(serial, actor, KeyPermission::READ)? {
                    return Err(LinuxError::EACCES.into());
                }
                let key = manager
                    .keys
                    .get(&serial.serial)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                manager.check_key_available(key, true)?;
                if key.is_keyring() {
                    return Ok(KeyctlOutput::KeyringIds(key.links.clone()));
                }
                if !key.kind.userspace_readable() {
                    return Err(LinuxError::EOPNOTSUPP.into());
                }
                let full_len = key.payload.len();
                // `keyctl_read_key()` answers a short buffer with the full
                // length and no data at all: the payload only reaches
                // userspace when it fits whole.
                let bytes = if copy_limit.is_some_and(|limit| limit >= full_len) {
                    key.payload.clone()
                } else {
                    Vec::new()
                };
                return Ok(KeyctlOutput::Payload { full_len, bytes });
            }
            KeyctlCommand::SetReqKeyring { setting } => {
                let old_setting = manager
                    .reqkey_defaults
                    .get(&actor.thread_owner)
                    .copied()
                    .unwrap_or(ReqKeyDefault::Default as i32);
                match setting {
                    ReqKeyDefault::NoChange => {
                        return Ok(KeyctlOutput::Value(old_setting as isize));
                    }
                    ReqKeyDefault::Thread => {
                        manager.special_keyring_in_namespace(
                            KEY_SPEC_THREAD_KEYRING,
                            actor,
                            namespace,
                            true,
                        )?;
                        manager
                            .reqkey_defaults
                            .insert(actor.thread_owner, setting as i32);
                    }
                    ReqKeyDefault::Process => {
                        manager.special_keyring_in_namespace(
                            KEY_SPEC_PROCESS_KEYRING,
                            actor,
                            namespace,
                            true,
                        )?;
                        manager
                            .reqkey_defaults
                            .insert(actor.thread_owner, setting as i32);
                    }
                    ReqKeyDefault::Default => {
                        manager.reqkey_defaults.remove(&actor.thread_owner);
                    }
                    ReqKeyDefault::Session | ReqKeyDefault::User | ReqKeyDefault::UserSession => {
                        manager
                            .reqkey_defaults
                            .insert(actor.thread_owner, setting as i32);
                    }
                }
                old_setting as isize
            }
            KeyctlCommand::SetTimeout { key, seconds } => {
                let serial = manager.resolve_key_in_namespace(key, actor, namespace, false)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::SETATTR)? {
                    return Err(LinuxError::EACCES.into());
                }
                manager
                    .keys
                    .get_mut(&serial.serial)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?
                    .expires_at =
                    (seconds != 0).then(|| wall_time().as_secs().saturating_add(seconds));
                0
            }
            KeyctlCommand::Invalidate { key } => {
                let serial = manager.resolve_key_in_namespace(key, actor, namespace, false)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::SEARCH)? {
                    return Err(LinuxError::EACCES.into());
                }
                manager.remove_key_everywhere(serial.serial)?;
                0
            }
            KeyctlCommand::GetPersistent { uid, destination } => {
                let uid = uid
                    .map(|uid| actor.map_user_uid(uid))
                    .transpose()?
                    .unwrap_or(actor.ids.ruid);
                if uid != actor.ids.ruid && uid != actor.ids.euid && !actor.has_setuid {
                    return Err(AxError::OperationNotPermitted);
                }
                let dest =
                    manager.resolve_keyring_in_namespace(destination, actor, namespace, true)?;
                if !manager.keyring_has_write(dest, actor)? {
                    return Err(LinuxError::EACCES.into());
                }
                let persistent =
                    manager.get_persistent_keyring_in_namespace(uid, actor, namespace)?;
                manager.link_persistent_keyring(dest, persistent, actor)?;
                persistent.serial as isize
            }
            KeyctlCommand::Restrict { keyring, kind } => {
                let serial =
                    manager.resolve_keyring_in_namespace(keyring, actor, namespace, false)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::SETATTR)? {
                    return Err(LinuxError::EACCES.into());
                }
                let key = manager
                    .keys
                    .get_mut(&serial.serial)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                if key.restricted {
                    return Err(LinuxError::EEXIST.into());
                }
                key.restricted = true;
                key.restriction_kind = kind;
                0
            }
            KeyctlCommand::Move {
                key,
                from,
                to,
                exclusive,
            } => {
                let serial = manager.resolve_key_in_namespace(key, actor, namespace, false)?;
                let from = manager.resolve_keyring_in_namespace(from, actor, namespace, false)?;
                let to = manager.resolve_keyring_in_namespace(to, actor, namespace, true)?;
                if !manager.keyring_has_write(from, actor)?
                    || !manager.keyring_has_write(to, actor)?
                    || !manager.key_has_perm(serial, actor, KeyPermission::LINK)?
                {
                    return Err(LinuxError::EACCES.into());
                }
                if from.serial == to.serial {
                    return Ok(KeyctlOutput::Value(0));
                }
                manager.move_key_link(from.serial, to.serial, serial.serial, exclusive)?;
                0
            }
        };
        Ok(KeyctlOutput::Value(value))
    }
}
