//! Key and keyring construction, resize, and per-task root slots.

use super::*;

impl KeyManager {
    #[cfg(test)]
    pub(super) fn insert_key(&mut self, key: AxResult<Key>) -> i32 {
        self.try_insert_key(key.unwrap(), QuotaAdmission::Enforced)
            .unwrap()
    }

    pub(super) fn try_insert_key(
        &mut self,
        mut key: Key,
        admission: QuotaAdmission,
    ) -> AxResult<i32> {
        // QUOTA_OVERRUN only relaxes creation admission. The object remains
        // charged, and all later growth and ownership transfers are enforced.
        key.in_owner_quota = admission != QuotaAdmission::Exempt;
        let owner = self.owners.plan_replace(
            key.quota_uid,
            admission,
            AbiQuotaCharge::ZERO,
            key.abi_charge,
        )?;
        let budget = self
            .budget
            .plan_replace(ResidentCharge::ZERO, key.resident_charge)?;
        let serial = self.alloc_serial()?;
        debug_assert!(!self.keys.contains_key(&serial));
        self.keys.insert(serial, key);
        self.owners.apply(owner);
        self.budget.apply(budget);
        Ok(serial)
    }

    #[cfg(test)]
    pub(super) fn create_keyring(
        &mut self,
        description: String,
        uid: Kuid,
        owner_gid: Kgid,
        permissions: KeyPermissionMask,
    ) -> i32 {
        self.insert_key(Key::keyring(description, uid, owner_gid, permissions))
    }

    pub(super) fn try_create_keyring(
        &mut self,
        description: String,
        uid: Kuid,
        owner_gid: Kgid,
        permissions: KeyPermissionMask,
        admission: QuotaAdmission,
    ) -> AxResult<i32> {
        self.try_insert_key(
            Key::keyring(description, uid, owner_gid, permissions)?,
            admission,
        )
    }

    pub(super) fn try_create_keyring_without_group(
        &mut self,
        description: String,
        uid: Kuid,
        permissions: KeyPermissionMask,
        admission: QuotaAdmission,
    ) -> AxResult<i32> {
        self.try_insert_key(
            Key::keyring_without_group(description, uid, permissions)?,
            admission,
        )
    }

    pub(super) fn try_create_rooted_keyring(
        &mut self,
        source: RootSource,
        description: String,
        uid: Kuid,
        owner_gid: Kgid,
        permissions: KeyPermissionMask,
        admission: QuotaAdmission,
    ) -> AxResult<i32> {
        let serial =
            self.try_create_keyring(description, uid, owner_gid, permissions, admission)?;
        self.install_new_root(source, serial)
    }

    pub(super) fn try_create_rooted_keyring_without_group(
        &mut self,
        source: RootSource,
        description: String,
        uid: Kuid,
        permissions: KeyPermissionMask,
        admission: QuotaAdmission,
    ) -> AxResult<i32> {
        let serial =
            self.try_create_keyring_without_group(description, uid, permissions, admission)?;
        self.install_new_root(source, serial)
    }

    pub(super) fn install_new_root(&mut self, source: RootSource, serial: i32) -> AxResult<i32> {
        if let Err(error) = self.install_root(source, serial) {
            self.discard_new_key(serial)?;
            return Err(error);
        }
        Ok(serial)
    }

    pub(super) fn plan_key_resize(
        &self,
        serial: i32,
        new_abi: AbiQuotaCharge,
        new_resident: ResidentCharge,
    ) -> AxResult<AccountingPlan> {
        let key = self
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        Ok(AccountingPlan {
            owner: self.owners.plan_replace(
                key.quota_uid,
                key.ongoing_quota_admission(),
                key.abi_charge,
                new_abi,
            )?,
            budget: self
                .budget
                .plan_replace(key.resident_charge, new_resident)?,
        })
    }

    pub(super) fn apply_key_resize(
        &mut self,
        serial: i32,
        new_abi: AbiQuotaCharge,
        new_resident: ResidentCharge,
        plan: AccountingPlan,
    ) -> AxResult<()> {
        let key = self
            .keys
            .get_mut(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?;
        key.abi_charge = new_abi;
        key.resident_charge = new_resident;
        self.owners.apply(plan.owner);
        self.budget.apply(plan.budget);
        Ok(())
    }

    pub(super) fn root_serial(&self, source: RootSource) -> Option<i32> {
        match source {
            RootSource::Thread(owner) => self.thread_keyrings.get(&owner).copied(),
            RootSource::Process(owner) => self.process_keyrings.get(&owner).copied(),
            RootSource::Session(owner) => self.session_keyrings.get(&owner).copied(),
            RootSource::User(namespace, uid) => self
                .namespaces
                .get(&namespace)
                .and_then(|registry| registry.user_keyrings.get(&uid))
                .copied(),
            RootSource::UserSession(namespace, uid) => self
                .namespaces
                .get(&namespace)
                .and_then(|registry| registry.user_session_keyrings.get(&uid))
                .copied(),
            RootSource::Persistent(namespace, uid) => self
                .namespaces
                .get(&namespace)
                .and_then(|registry| registry.persistent_keyrings.get(&uid))
                .copied(),
        }
    }

    pub(super) fn anonymous_session_admission(&self, thread_owner: u32) -> QuotaAdmission {
        if self.session_keyrings.contains_key(&thread_owner) {
            QuotaAdmission::Enforced
        } else {
            QuotaAdmission::AllowOverrun
        }
    }

    pub(super) fn install_root(&mut self, source: RootSource, serial: i32) -> AxResult<()> {
        let namespace = match source {
            RootSource::User(namespace, _)
            | RootSource::UserSession(namespace, _)
            | RootSource::Persistent(namespace, _) => Some(namespace),
            RootSource::Thread(_) | RootSource::Process(_) | RootSource::Session(_) => None,
        };
        if namespace.is_some_and(|namespace| !self.namespaces.contains_key(&namespace)) {
            return Err(AxError::BadState);
        }
        let old = self.root_serial(source);
        if old == Some(serial) {
            return Ok(());
        }
        let new_refs = self
            .keys
            .get(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?
            .root_refs
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        self.keys
            .get_mut(&serial)
            .ok_or(AxError::from(LinuxError::ENOKEY))?
            .root_refs = new_refs;
        match source {
            RootSource::Thread(owner) => self.thread_keyrings.insert(owner, serial),
            RootSource::Process(owner) => self.process_keyrings.insert(owner, serial),
            RootSource::Session(owner) => self.session_keyrings.insert(owner, serial),
            RootSource::User(namespace, uid) => self
                .namespaces
                .get_mut(&namespace)
                .ok_or(AxError::BadState)?
                .user_keyrings
                .insert(uid, serial),
            RootSource::UserSession(namespace, uid) => self
                .namespaces
                .get_mut(&namespace)
                .ok_or(AxError::BadState)?
                .user_session_keyrings
                .insert(uid, serial),
            RootSource::Persistent(namespace, uid) => self
                .namespaces
                .get_mut(&namespace)
                .ok_or(AxError::BadState)?
                .persistent_keyrings
                .insert(uid, serial),
        };
        if let Some(old) = old {
            self.release_root_ref(old)?;
        }
        Ok(())
    }

    pub(super) fn release_root_ref(&mut self, serial: i32) -> AxResult<()> {
        let refs = self
            .keys
            .get(&serial)
            .ok_or(AxError::BadState)?
            .root_refs
            .checked_sub(1)
            .ok_or(AxError::BadState)?;
        self.keys
            .get_mut(&serial)
            .ok_or(AxError::BadState)?
            .root_refs = refs;
        self.collect_unreferenced(serial)
    }

    pub(super) fn task_root_serial(&self, source: RootSource) -> AxResult<Option<i32>> {
        match source {
            RootSource::Thread(owner) => Ok(self.thread_keyrings.get(&owner).copied()),
            RootSource::Process(owner) => Ok(self.process_keyrings.get(&owner).copied()),
            RootSource::Session(owner) => Ok(self.session_keyrings.get(&owner).copied()),
            RootSource::User(..) | RootSource::UserSession(..) | RootSource::Persistent(..) => {
                Err(AxError::BadState)
            }
        }
    }

    pub(super) fn current_task_root(
        &self,
        source: RootSource,
    ) -> AxResult<Option<ExpectedTaskRoot>> {
        Ok(self
            .task_root_serial(source)?
            .map(|serial| ExpectedTaskRoot { source, serial }))
    }

    /// Detaches a bounded set of exact task roots after validating the complete
    /// mutation. No namespace registry work or dynamic planning allocation is
    /// permitted on credential lifecycle paths.
    pub(super) fn detach_expected_task_roots(
        &mut self,
        expected: [Option<ExpectedTaskRoot>; 3],
        allow_already_absent: bool,
    ) -> AxResult<()> {
        let mut present = [None; 3];
        for (index, entry) in expected.into_iter().enumerate() {
            let Some(entry) = entry else {
                continue;
            };
            if expected[..index]
                .iter()
                .flatten()
                .any(|prior| prior.source == entry.source)
            {
                return Err(AxError::BadState);
            }
            match self.task_root_serial(entry.source)? {
                Some(serial) if serial == entry.serial => present[index] = Some(entry),
                None if allow_already_absent => {}
                Some(_) | None => return Err(AxError::BadState),
            }
        }

        self.run_gc_txn(PreparedGcRoots::Task(present))
    }

    pub(super) fn validate_lifecycle_root(&self, source: RootSource) -> AxResult<Option<i32>> {
        let Some(serial) = self.task_root_serial(source)? else {
            return Ok(None);
        };
        let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
        if !key.is_keyring()
            || key.root_refs == 0
            || !key.gc_plan.is_idle()
            || key.gc_next.is_some()
        {
            return Err(AxError::BadState);
        }
        Ok(Some(serial))
    }
}
