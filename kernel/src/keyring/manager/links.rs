//! Link, unlink, move, revoke, and clear operations over keyrings.

use super::*;

impl KeyManager {
    pub(super) fn remove_key_everywhere(&mut self, serial: i32) -> AxResult<()> {
        if !self.keys.contains_key(&serial) {
            return Ok(());
        }
        let parent_count = self
            .keys
            .iter()
            .filter(|(_, key)| key.links.contains(&serial))
            .count();
        let mut parents = Vec::new();
        parents
            .try_reserve_exact(parent_count)
            .map_err(|_| AxError::NoMemory)?;
        parents.extend(
            self.keys
                .iter()
                .filter_map(|(parent, key)| key.links.contains(&serial).then_some(*parent)),
        );
        if parents.len() != parent_count {
            return Err(AxError::BadState);
        }
        let removed_roots = self
            .thread_keyrings
            .values()
            .chain(self.process_keyrings.values())
            .chain(self.session_keyrings.values())
            .chain(
                self.namespaces
                    .values()
                    .flat_map(NamespaceRegistry::root_serials),
            )
            .filter(|linked| **linked == serial)
            .count();
        let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
        if key.root_refs != removed_roots || key.link_refs != parents.len() {
            return Err(AxError::BadState);
        }
        for parent in &parents {
            let parent = self.keys.get(parent).ok_or(AxError::BadState)?;
            if !parent.is_keyring() || !parent.links.contains(&serial) {
                return Err(AxError::BadState);
            }
        }
        for parent in parents {
            self.unlink_key_from_keyring(parent, serial)?;
        }
        if !self.keys.contains_key(&serial) {
            debug_assert_eq!(removed_roots, 0);
            return Ok(());
        }

        macro_rules! detach_roots {
            ($roots:expr) => {
                $roots.retain(|_, linked| *linked != serial);
            };
        }
        detach_roots!(self.thread_keyrings);
        detach_roots!(self.process_keyrings);
        detach_roots!(self.session_keyrings);
        for registry in self.namespaces.values_mut() {
            registry.detach_serial(serial);
        }

        self.keys
            .get_mut(&serial)
            .ok_or(AxError::BadState)?
            .root_refs = 0;
        self.collect_unreferenced(serial)
    }

    pub(super) fn unlink_key_from_keyring(&mut self, keyring: i32, serial: i32) -> AxResult<()> {
        let keyring_key = self
            .keys
            .get(&keyring)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if !keyring_key.is_keyring() {
            return Err(AxError::InvalidInput);
        }
        let index = keyring_key
            .links
            .iter()
            .position(|linked| *linked == serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if self.keys.get(&serial).is_none_or(|key| key.link_refs == 0) {
            return Err(AxError::BadState);
        }
        let (new_abi, new_resident) =
            keyring_key.with_removed_link_charges(1, keyring_key.links.capacity())?;
        let plan = self.plan_key_resize(keyring, new_abi, new_resident)?;

        self.keys
            .get_mut(&keyring)
            .ok_or(AxError::BadState)?
            .links
            .remove(index);
        self.apply_key_resize(keyring, new_abi, new_resident, plan)?;
        let refs = self
            .keys
            .get(&serial)
            .ok_or(AxError::BadState)?
            .link_refs
            .checked_sub(1)
            .ok_or(AxError::BadState)?;
        self.keys
            .get_mut(&serial)
            .ok_or(AxError::BadState)?
            .link_refs = refs;
        self.collect_unreferenced(serial)
    }

    pub(super) fn link_key_replace(&mut self, keyring: i32, serial: i32) -> AxResult<()> {
        self.validate_keyring_link(keyring, serial)?;
        let (kind, description) = {
            let key = self
                .keys
                .get(&serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            (key.kind, key.description.clone())
        };
        let existing = self.find_linked_key(keyring, kind, &description);
        if existing == Some(serial) {
            return Ok(());
        }
        if let Some(existing) = existing {
            let new_refs = self
                .keys
                .get(&serial)
                .ok_or(AxError::BadState)?
                .link_refs
                .checked_add(1)
                .ok_or(AxError::NoMemory)?;
            let old_refs = self
                .keys
                .get(&existing)
                .ok_or(AxError::BadState)?
                .link_refs
                .checked_sub(1)
                .ok_or(AxError::BadState)?;
            let slot = self
                .keys
                .get_mut(&keyring)
                .ok_or(AxError::BadState)?
                .links
                .iter_mut()
                .find(|linked| **linked == existing)
                .ok_or(AxError::BadState)?;
            *slot = serial;
            self.keys
                .get_mut(&serial)
                .ok_or(AxError::BadState)?
                .link_refs = new_refs;
            self.keys
                .get_mut(&existing)
                .ok_or(AxError::BadState)?
                .link_refs = old_refs;
            self.collect_unreferenced(existing)?;
        } else {
            let (mut new_abi, mut new_resident, growth_capacity) = {
                let destination = self
                    .keys
                    .get(&keyring)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                let growth_capacity = destination.next_link_capacity()?;
                let new_capacity = growth_capacity.unwrap_or(destination.links.capacity());
                let (new_abi, new_resident) = destination.with_added_link_charges(new_capacity)?;
                (new_abi, new_resident, growth_capacity)
            };
            let mut plan = self.plan_key_resize(keyring, new_abi, new_resident)?;
            let new_refs = self
                .keys
                .get(&serial)
                .ok_or(AxError::BadState)?
                .link_refs
                .checked_add(1)
                .ok_or(AxError::NoMemory)?;
            let staged_links = if let Some(new_capacity) = growth_capacity {
                let transient_link_bytes = new_capacity
                    .checked_mul(KEY_LINK_CHARGE)
                    .ok_or(AxError::NoMemory)?;
                self.budget.check_transient(ResidentCharge {
                    objects: 0,
                    bytes: 0,
                    link_bytes: transient_link_bytes,
                })?;
                let staged = self
                    .keys
                    .get(&keyring)
                    .ok_or(AxError::BadState)?
                    .stage_link_push(serial, new_capacity)?;
                if staged.capacity() != new_capacity {
                    let actual_link_bytes = staged
                        .capacity()
                        .checked_mul(KEY_LINK_CHARGE)
                        .ok_or(AxError::NoMemory)?;
                    self.budget.check_transient(ResidentCharge {
                        objects: 0,
                        bytes: 0,
                        link_bytes: actual_link_bytes,
                    })?;
                    (new_abi, new_resident) = self
                        .keys
                        .get(&keyring)
                        .ok_or(AxError::BadState)?
                        .with_added_link_charges(staged.capacity())?;
                    plan = self.plan_key_resize(keyring, new_abi, new_resident)?;
                }
                Some(staged)
            } else {
                None
            };
            self.apply_key_resize(keyring, new_abi, new_resident, plan)?;
            self.keys
                .get_mut(&serial)
                .ok_or(AxError::BadState)?
                .link_refs = new_refs;
            let links = &mut self.keys.get_mut(&keyring).ok_or(AxError::BadState)?.links;
            if let Some(staged_links) = staged_links {
                *links = staged_links;
            } else {
                links.push(serial);
            }
        }
        Ok(())
    }

    pub(super) fn clear_keyring_links(&mut self, keyring: i32) -> AxResult<()> {
        let key = self
            .keys
            .get(&keyring)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if !key.is_keyring() {
            return Err(AxError::InvalidInput);
        }
        if key.links.is_empty() && key.links.capacity() == 0 {
            return Ok(());
        }
        for linked in &key.links {
            if self
                .keys
                .get(linked)
                .is_none_or(|linked_key| linked_key.link_refs == 0)
            {
                return Err(AxError::BadState);
            }
        }
        let link_count = key.links.len();
        let (new_abi, new_resident) = key.with_removed_link_charges(link_count, 0)?;
        let plan = self.plan_key_resize(keyring, new_abi, new_resident)?;
        let links =
            core::mem::take(&mut self.keys.get_mut(&keyring).ok_or(AxError::BadState)?.links);
        self.apply_key_resize(keyring, new_abi, new_resident, plan)?;
        for linked in links {
            let refs = self
                .keys
                .get(&linked)
                .ok_or(AxError::BadState)?
                .link_refs
                .checked_sub(1)
                .ok_or(AxError::BadState)?;
            self.keys
                .get_mut(&linked)
                .ok_or(AxError::BadState)?
                .link_refs = refs;
            self.collect_unreferenced(linked)?;
        }
        Ok(())
    }

    pub(super) fn revoke_key(&mut self, serial: i32) -> AxResult<()> {
        let key = self
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if key.state == KeyState::Revoked {
            return Ok(());
        }
        for linked in &key.links {
            if self
                .keys
                .get(linked)
                .is_none_or(|linked_key| linked_key.link_refs == 0)
            {
                return Err(AxError::BadState);
            }
        }
        let payload_abi = key.kind.abi_payload_charge(key.payload.len());
        let link_bytes = key
            .links
            .len()
            .checked_mul(KEY_LINK_CHARGE)
            .ok_or(AxError::BadState)?;
        let new_abi = AbiQuotaCharge {
            keys: key.abi_charge.keys,
            bytes: key
                .abi_charge
                .bytes
                .checked_sub(payload_abi)
                .and_then(|bytes| bytes.checked_sub(link_bytes))
                .ok_or(AxError::BadState)?,
        };
        let new_resident = ResidentCharge {
            objects: key.resident_charge.objects,
            bytes: key
                .resident_charge
                .bytes
                .checked_sub(key.payload.capacity())
                .ok_or(AxError::BadState)?,
            link_bytes: 0,
        };
        let plan = self.plan_key_resize(serial, new_abi, new_resident)?;
        let key = self.keys.get_mut(&serial).ok_or(AxError::BadState)?;
        let mut payload = core::mem::take(&mut key.payload);
        wipe_key_bytes(&mut payload);
        drop(payload);
        let links = core::mem::take(&mut key.links);
        key.state = KeyState::Revoked;
        self.apply_key_resize(serial, new_abi, new_resident, plan)?;
        for linked in links {
            let refs = self
                .keys
                .get(&linked)
                .ok_or(AxError::BadState)?
                .link_refs
                .checked_sub(1)
                .ok_or(AxError::BadState)?;
            self.keys
                .get_mut(&linked)
                .ok_or(AxError::BadState)?
                .link_refs = refs;
            self.collect_unreferenced(linked)?;
        }
        Ok(())
    }

    pub(super) fn validate_keyring_link(&self, destination: i32, serial: i32) -> AxResult<()> {
        self.check_link_destination(destination)?;

        let Some(key) = self.keys.get(&serial) else {
            return Err(LinuxError::ENOKEY.into());
        };
        if !key.is_keyring() {
            return Ok(());
        }

        let mut pending = Vec::from([(serial, 0)]);
        let mut visited = BTreeSet::new();
        while let Some((candidate, depth)) = pending.pop() {
            if candidate == destination {
                return Err(LinuxError::EDEADLK.into());
            }
            if !visited.insert(candidate) {
                continue;
            }
            let Some(candidate_key) = self.keys.get(&candidate) else {
                continue;
            };
            if !candidate_key.is_keyring() {
                continue;
            }
            if depth >= KEYRING_SEARCH_MAX_DEPTH
                && candidate_key
                    .links
                    .iter()
                    .any(|linked| self.keys.get(linked).is_some_and(Key::is_keyring))
            {
                return Err(LinuxError::ELOOP.into());
            }
            pending.extend(
                candidate_key
                    .links
                    .iter()
                    .copied()
                    .filter(|linked| self.keys.get(linked).is_some_and(Key::is_keyring))
                    .map(|linked| (linked, depth + 1)),
            );
        }
        Ok(())
    }

    pub(super) fn check_link_destination(&self, destination: i32) -> AxResult<()> {
        let destination_key = self
            .keys
            .get(&destination)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        if !destination_key.is_keyring() {
            return Err(AxError::InvalidInput);
        }
        if destination_key.restricted {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }

    pub(super) fn link_existing_key(
        &mut self,
        keyring: impl Into<ResolvedKey>,
        key: impl Into<ResolvedKey>,
        actor: &KeyActor,
        exclusive: bool,
    ) -> AxResult<()> {
        let keyring = keyring.into();
        let resolved = key.into();
        let key = self
            .keys
            .get(&resolved.serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        self.check_key_available(key, true)?;
        if !self.key_has_perm(resolved, actor, KeyPermission::LINK)? {
            return Err(LinuxError::EACCES.into());
        }
        let (kind, description) = (key.kind, key.description.clone());
        if !self.keyring_has_write(keyring, actor)? {
            return Err(LinuxError::EACCES.into());
        }
        if exclusive
            && self
                .find_linked_key(keyring.serial, kind, &description)
                .is_some()
        {
            return Err(LinuxError::EEXIST.into());
        }
        self.link_key_replace(keyring.serial, resolved.serial)
    }

    pub(super) fn move_key_link(
        &mut self,
        from: i32,
        to: i32,
        serial: i32,
        exclusive: bool,
    ) -> AxResult<()> {
        if from == to {
            return Ok(());
        }
        let (kind, description) = self
            .keys
            .get(&serial)
            .map(|key| (key.kind, key.description.clone()))
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        let source_index = self
            .keys
            .get(&from)
            .and_then(|keyring| keyring.links.iter().position(|linked| *linked == serial))
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        let replaced = self.find_linked_key(to, kind, &description);
        if exclusive && replaced.is_some() {
            return Err(LinuxError::EEXIST.into());
        }
        let replaced_index = replaced.and_then(|replaced| {
            self.keys
                .get(&to)
                .and_then(|keyring| keyring.links.iter().position(|linked| *linked == replaced))
        });
        if replaced.is_some() && replaced_index.is_none() {
            return Err(AxError::BadState);
        }
        self.validate_keyring_link(to, serial)?;

        if replaced.is_none() {
            let (
                old_destination,
                mut new_destination_abi,
                mut new_destination_resident,
                growth_capacity,
            ) = {
                let destination = self
                    .keys
                    .get(&to)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                let growth_capacity = destination.next_link_capacity()?;
                let new_capacity = growth_capacity.unwrap_or(destination.links.capacity());
                let (new_abi, new_resident) = destination.with_added_link_charges(new_capacity)?;
                (
                    (destination.abi_charge, destination.resident_charge),
                    new_abi,
                    new_resident,
                    growth_capacity,
                )
            };
            // Linux reserves the destination's +4-byte link charge before it
            // refunds the source, including for same-owner moves.
            let mut destination_plan =
                self.plan_key_resize(to, new_destination_abi, new_destination_resident)?;
            let staged_links = if let Some(new_capacity) = growth_capacity {
                let transient_link_bytes = new_capacity
                    .checked_mul(KEY_LINK_CHARGE)
                    .ok_or(AxError::NoMemory)?;
                self.budget.check_transient(ResidentCharge {
                    objects: 0,
                    bytes: 0,
                    link_bytes: transient_link_bytes,
                })?;
                let staged = self
                    .keys
                    .get(&to)
                    .ok_or(AxError::BadState)?
                    .stage_link_push(serial, new_capacity)?;
                if staged.capacity() != new_capacity {
                    let actual_link_bytes = staged
                        .capacity()
                        .checked_mul(KEY_LINK_CHARGE)
                        .ok_or(AxError::NoMemory)?;
                    self.budget.check_transient(ResidentCharge {
                        objects: 0,
                        bytes: 0,
                        link_bytes: actual_link_bytes,
                    })?;
                    (new_destination_abi, new_destination_resident) = self
                        .keys
                        .get(&to)
                        .ok_or(AxError::BadState)?
                        .with_added_link_charges(staged.capacity())?;
                    destination_plan =
                        self.plan_key_resize(to, new_destination_abi, new_destination_resident)?;
                }
                Some(staged)
            } else {
                None
            };
            self.apply_key_resize(
                to,
                new_destination_abi,
                new_destination_resident,
                destination_plan,
            )?;

            let source = self.keys.get(&from).ok_or(AxError::BadState)?;
            let (new_source_abi, new_source_resident) =
                source.with_removed_link_charges(1, source.links.capacity())?;
            let source_plan = match self.plan_key_resize(from, new_source_abi, new_source_resident)
            {
                Ok(plan) => plan,
                Err(error) => {
                    let rollback =
                        self.plan_key_resize(to, old_destination.0, old_destination.1)?;
                    self.apply_key_resize(to, old_destination.0, old_destination.1, rollback)?;
                    return Err(error);
                }
            };
            self.apply_key_resize(from, new_source_abi, new_source_resident, source_plan)?;
            self.keys
                .get_mut(&from)
                .ok_or(AxError::BadState)?
                .links
                .remove(source_index);
            let links = &mut self.keys.get_mut(&to).ok_or(AxError::BadState)?.links;
            if let Some(staged_links) = staged_links {
                *links = staged_links;
            } else {
                links.push(serial);
            }
            return Ok(());
        }

        let replaced = replaced.ok_or(AxError::BadState)?;
        let replaced_index = replaced_index.ok_or(AxError::BadState)?;
        let source = self.keys.get(&from).ok_or(AxError::BadState)?;
        let (new_source_abi, new_source_resident) =
            source.with_removed_link_charges(1, source.links.capacity())?;
        let source_plan = self.plan_key_resize(from, new_source_abi, new_source_resident)?;
        if self
            .keys
            .get(&replaced)
            .is_none_or(|key| key.link_refs == 0)
        {
            return Err(AxError::BadState);
        }
        self.apply_key_resize(from, new_source_abi, new_source_resident, source_plan)?;
        self.keys
            .get_mut(&from)
            .ok_or(AxError::BadState)?
            .links
            .remove(source_index);
        if replaced == serial {
            let refs = self
                .keys
                .get(&serial)
                .ok_or(AxError::BadState)?
                .link_refs
                .checked_sub(1)
                .ok_or(AxError::BadState)?;
            self.keys
                .get_mut(&serial)
                .ok_or(AxError::BadState)?
                .link_refs = refs;
        } else {
            self.keys.get_mut(&to).ok_or(AxError::BadState)?.links[replaced_index] = serial;
            let refs = self
                .keys
                .get(&replaced)
                .ok_or(AxError::BadState)?
                .link_refs
                .checked_sub(1)
                .ok_or(AxError::BadState)?;
            self.keys
                .get_mut(&replaced)
                .ok_or(AxError::BadState)?
                .link_refs = refs;
        }
        self.collect_unreferenced(replaced)
    }
}
