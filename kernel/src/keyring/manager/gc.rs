//! Bounded, transactional garbage collection over the key graph.
//!
//! Retirement is planned as a complete transaction — roots, reachable
//! closure, accounting — before anything is mutated, so a planning failure
//! aborts with the manager untouched.

use super::*;

impl KeyManager {
    pub(super) fn next_gc_epoch(&mut self) -> AxResult<u64> {
        let epoch = self.next_gc_epoch;
        self.next_gc_epoch = epoch
            .checked_add(1)
            .ok_or(AxError::from(LinuxError::ENOSPC))?;
        Ok(epoch)
    }

    pub(super) fn gc_touch_key(
        keys: &mut BTreeMap<i32, Key>,
        build: &mut GcTxnBuild,
        serial: i32,
    ) -> AxResult<()> {
        let key = keys.get_mut(&serial).ok_or(AxError::BadState)?;
        if key.gc_plan.epoch == build.epoch {
            return Ok(());
        }
        if !key.gc_plan.is_idle() || key.gc_next.is_some() {
            return Err(AxError::BadState);
        }
        let touched_count = build
            .touched_count
            .checked_add(1)
            .ok_or(AxError::BadState)?;
        key.gc_plan = GcPlanScratch {
            epoch: build.epoch,
            root_drops: 0,
            link_drops: 0,
            state: Some(GcPlanState::Touched),
            touched_next: build.touched_head,
            work_next: None,
        };
        build.touched_head = Some(serial);
        build.touched_count = touched_count;
        Ok(())
    }

    pub(super) fn gc_add_root_drop(
        keys: &mut BTreeMap<i32, Key>,
        build: &mut GcTxnBuild,
        serial: i32,
    ) -> AxResult<()> {
        Self::gc_touch_key(keys, build, serial)?;
        let key = keys.get_mut(&serial).ok_or(AxError::BadState)?;
        if !key.is_keyring() {
            return Err(AxError::BadState);
        }
        key.gc_plan.root_drops = key
            .gc_plan
            .root_drops
            .checked_add(1)
            .ok_or(AxError::BadState)?;
        if key.gc_plan.root_drops > key.root_refs {
            return Err(AxError::BadState);
        }
        Ok(())
    }

    pub(super) fn gc_add_link_drop(
        keys: &mut BTreeMap<i32, Key>,
        build: &mut GcTxnBuild,
        serial: i32,
    ) -> AxResult<()> {
        Self::gc_touch_key(keys, build, serial)?;
        let key = keys.get_mut(&serial).ok_or(AxError::BadState)?;
        key.gc_plan.link_drops = key
            .gc_plan
            .link_drops
            .checked_add(1)
            .ok_or(AxError::BadState)?;
        if key.gc_plan.link_drops > key.link_refs {
            return Err(AxError::BadState);
        }
        Ok(())
    }

    pub(super) fn gc_maybe_queue(
        keys: &mut BTreeMap<i32, Key>,
        build: &mut GcTxnBuild,
        serial: i32,
    ) -> AxResult<()> {
        let key = keys.get_mut(&serial).ok_or(AxError::BadState)?;
        if key.gc_plan.epoch != build.epoch {
            return Err(AxError::BadState);
        }
        let roots_left = key
            .root_refs
            .checked_sub(key.gc_plan.root_drops)
            .ok_or(AxError::BadState)?;
        let links_left = key
            .link_refs
            .checked_sub(key.gc_plan.link_drops)
            .ok_or(AxError::BadState)?;
        if roots_left != 0 || links_left != 0 {
            return Ok(());
        }
        match key.gc_plan.state {
            Some(GcPlanState::Touched) => {
                key.gc_plan.state = Some(GcPlanState::Queued);
                key.gc_plan.work_next = build.work_head;
                build.work_head = Some(serial);
                Ok(())
            }
            Some(GcPlanState::Queued) | Some(GcPlanState::Retire) => Ok(()),
            None => Err(AxError::BadState),
        }
    }

    pub(super) fn gc_plan_roots(&mut self, build: &mut GcTxnBuild) -> AxResult<()> {
        match build.roots {
            PreparedGcRoots::Namespace(identity) => {
                let (namespaces, keys) = (&self.namespaces, &mut self.keys);
                let registry = namespaces.get(&identity).ok_or(AxError::BadState)?;
                if registry.namespace.strong_count() != 0 {
                    return Err(AxError::BadState);
                }
                for serial in registry.root_serials() {
                    Self::gc_add_root_drop(keys, build, *serial)?;
                }
                for serial in registry.root_serials() {
                    Self::gc_maybe_queue(keys, build, *serial)?;
                }
            }
            PreparedGcRoots::Task(roots) => {
                for root in roots.into_iter().flatten() {
                    Self::gc_add_root_drop(&mut self.keys, build, root.serial)?;
                }
                for root in roots.into_iter().flatten() {
                    Self::gc_maybe_queue(&mut self.keys, build, root.serial)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn gc_discover_closure(&mut self, build: &mut GcTxnBuild) -> AxResult<()> {
        while let Some(serial) = build.work_head {
            let link_count = {
                let key = self.keys.get_mut(&serial).ok_or(AxError::BadState)?;
                if key.gc_plan.epoch != build.epoch
                    || key.gc_plan.state != Some(GcPlanState::Queued)
                {
                    return Err(AxError::BadState);
                }
                build.work_head = key.gc_plan.work_next.take();
                key.gc_plan.state = Some(GcPlanState::Retire);
                if key.root_refs != key.gc_plan.root_drops
                    || key.link_refs != key.gc_plan.link_drops
                    || !key.is_keyring() && !key.links.is_empty()
                {
                    return Err(AxError::BadState);
                }
                key.links.len()
            };
            for index in 0..link_count {
                let linked = self
                    .keys
                    .get(&serial)
                    .and_then(|key| key.links.get(index))
                    .copied()
                    .ok_or(AxError::BadState)?;
                Self::gc_add_link_drop(&mut self.keys, build, linked)?;
                Self::gc_maybe_queue(&mut self.keys, build, linked)?;
            }
        }
        Ok(())
    }

    pub(super) fn gc_plan_accounting(
        &mut self,
        build: &mut GcTxnBuild,
    ) -> AxResult<crate::keyring::accounting::ManagerBudgetUsage> {
        let mut current = build.touched_head;
        for _ in 0..build.touched_count {
            let serial = current.ok_or(AxError::BadState)?;
            let (next, state, root_refs, link_refs, root_drops, link_drops, resident, owner) = {
                let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
                if key.gc_plan.epoch != build.epoch || key.gc_plan.work_next.is_some() {
                    return Err(AxError::BadState);
                }
                (
                    key.gc_plan.touched_next,
                    key.gc_plan.state,
                    key.root_refs,
                    key.link_refs,
                    key.gc_plan.root_drops,
                    key.gc_plan.link_drops,
                    key.resident_charge,
                    key.in_owner_quota
                        .then_some((key.quota_uid, key.abi_charge)),
                )
            };
            let roots_left = root_refs.checked_sub(root_drops).ok_or(AxError::BadState)?;
            let links_left = link_refs.checked_sub(link_drops).ok_or(AxError::BadState)?;
            match state {
                Some(GcPlanState::Retire) => {
                    if roots_left != 0 || links_left != 0 {
                        return Err(AxError::BadState);
                    }
                    build.retired = ResidentCharge {
                        objects: build
                            .retired
                            .objects
                            .checked_add(resident.objects)
                            .ok_or(AxError::BadState)?,
                        bytes: build
                            .retired
                            .bytes
                            .checked_add(resident.bytes)
                            .ok_or(AxError::BadState)?,
                        link_bytes: build
                            .retired
                            .link_bytes
                            .checked_add(resident.link_bytes)
                            .ok_or(AxError::BadState)?,
                    };
                    if let Some((uid, charge)) = owner
                        && self.owners.plan_gc_retire(
                            uid,
                            charge,
                            build.epoch,
                            &mut build.owner_head,
                        )?
                    {
                        build.owner_count = build
                            .owner_count
                            .checked_add(1)
                            .expect("GC owner chain cannot exceed the owner ledger");
                    }
                }
                Some(GcPlanState::Touched) => {
                    if roots_left == 0 && links_left == 0 {
                        return Err(AxError::BadState);
                    }
                }
                Some(GcPlanState::Queued) | None => return Err(AxError::BadState),
            }
            current = next;
        }
        if current.is_some() {
            return Err(AxError::BadState);
        }
        self.budget
            .plan_replace(build.retired, ResidentCharge::ZERO)
    }

    pub(super) fn abort_gc_build(&mut self, build: &GcTxnBuild) {
        self.owners
            .abort_gc(build.epoch, build.owner_head, build.owner_count);
        let mut current = build.touched_head;
        for _ in 0..build.touched_count {
            let serial = current.expect("prepared key chain ended early");
            let key = self
                .keys
                .get_mut(&serial)
                .expect("prepared key disappeared before abort");
            assert_eq!(key.gc_plan.epoch, build.epoch, "foreign key GC scratch");
            current = key.gc_plan.touched_next;
            key.gc_plan = GcPlanScratch::IDLE;
        }
        assert!(current.is_none(), "prepared key chain exceeded its count");
    }

    /// Plans and commits one GC closure without allowing prepared scratch to
    /// escape this call. Epochs are issued before any scratch is written and
    /// never reused, so an equal epoch can only belong to this transaction
    /// while the manager mutex excludes interposition.
    pub(super) fn run_gc_txn(&mut self, roots: PreparedGcRoots) -> AxResult<()> {
        let epoch = self.next_gc_epoch()?;
        let mut build = GcTxnBuild {
            epoch,
            roots,
            touched_head: None,
            touched_count: 0,
            work_head: None,
            owner_head: None,
            owner_count: 0,
            retired: ResidentCharge::ZERO,
        };
        let result = (|| {
            self.gc_plan_roots(&mut build)?;
            self.gc_discover_closure(&mut build)?;
            self.gc_plan_accounting(&mut build)
        })();
        match result {
            Ok(budget_after) => {
                self.commit_gc_txn(PreparedGcTxn {
                    epoch,
                    roots,
                    touched_head: build.touched_head,
                    touched_count: build.touched_count,
                    owner_head: build.owner_head,
                    owner_count: build.owner_count,
                    budget_after,
                });
                Ok(())
            }
            Err(error) => {
                self.abort_gc_build(&build);
                Err(error)
            }
        }
    }

    pub(super) fn commit_gc_txn(&mut self, plan: PreparedGcTxn) {
        match plan.roots {
            PreparedGcRoots::Namespace(identity) => {
                self.namespaces
                    .remove(&identity)
                    .expect("prepared namespace disappeared before GC commit");
            }
            PreparedGcRoots::Task(roots) => {
                for root in roots.into_iter().flatten() {
                    let removed = match root.source {
                        RootSource::Thread(owner) => self.thread_keyrings.remove(&owner),
                        RootSource::Process(owner) => self.process_keyrings.remove(&owner),
                        RootSource::Session(owner) => self.session_keyrings.remove(&owner),
                        RootSource::User(..)
                        | RootSource::UserSession(..)
                        | RootSource::Persistent(..) => {
                            panic!("namespace root in prepared task GC")
                        }
                    };
                    assert_eq!(removed, Some(root.serial), "prepared task root changed");
                }
            }
        }

        self.owners
            .commit_gc(plan.epoch, plan.owner_head, plan.owner_count);
        self.budget.apply(plan.budget_after);
        let mut current = plan.touched_head;
        for _ in 0..plan.touched_count {
            let serial = current.expect("prepared key chain ended early");
            let (next, scratch) = {
                let key = self
                    .keys
                    .get(&serial)
                    .expect("prepared key disappeared before GC commit");
                assert_eq!(key.gc_plan.epoch, plan.epoch, "foreign key GC scratch");
                (key.gc_plan.touched_next, key.gc_plan)
            };
            if scratch.state == Some(GcPlanState::Retire) {
                self.keys
                    .remove(&serial)
                    .expect("prepared key disappeared during GC commit");
            } else {
                let key = self
                    .keys
                    .get_mut(&serial)
                    .expect("prepared survivor disappeared during GC commit");
                key.root_refs = key
                    .root_refs
                    .checked_sub(scratch.root_drops)
                    .expect("prepared root decrement became invalid");
                key.link_refs = key
                    .link_refs
                    .checked_sub(scratch.link_drops)
                    .expect("prepared link decrement became invalid");
                key.gc_plan = GcPlanScratch::IDLE;
            }
            current = next;
        }
        assert!(current.is_none(), "prepared key chain exceeded its count");
    }

    /// Drops namespace-owned roots after the namespace's final strong
    /// reference disappears. The complete transitive retirement closure is
    /// validated before the registry or any reference/accounting state moves.
    pub(super) fn prune_dead_namespaces(&mut self) -> AxResult<()> {
        let mut cursor = None;
        loop {
            let next = match cursor {
                Some(identity) => self
                    .namespaces
                    .range((Excluded(identity), Unbounded))
                    .next(),
                None => self.namespaces.iter().next(),
            }
            .map(|(identity, registry)| (*identity, registry.namespace.strong_count() == 0));
            let Some((identity, dead)) = next else {
                break;
            };
            cursor = Some(identity);
            #[cfg(test)]
            {
                self.namespace_prune_candidates += 1;
            }
            if !dead {
                continue;
            }
            self.run_gc_txn(PreparedGcRoots::Namespace(identity))?;
        }
        Ok(())
    }

    pub(super) fn ensure_namespace_registry(
        &mut self,
        actor: &KeyActor,
    ) -> AxResult<UserNamespaceId> {
        #[cfg(test)]
        {
            self.namespace_ensure_calls += 1;
        }
        self.prune_dead_namespaces()?;
        let identity = actor.user_ns.identity();
        if let Some(registry) = self.namespaces.get(&identity) {
            let namespace = registry.namespace.upgrade().ok_or(AxError::BadState)?;
            if !Arc::ptr_eq(&namespace, &actor.user_ns) {
                return Err(AxError::BadState);
            }
            return Ok(identity);
        }
        self.namespaces
            .insert(identity, NamespaceRegistry::new(&actor.user_ns));
        Ok(identity)
    }
}

impl KeyManager {
    pub(super) fn clear_gc_pending(&mut self, mut pending: Option<i32>) {
        while let Some(serial) = pending {
            let Some(key) = self.keys.get_mut(&serial) else {
                break;
            };
            pending = key.gc_next.take();
        }
    }

    pub(super) fn collect_unreferenced(&mut self, serial: i32) -> AxResult<()> {
        let Some(key) = self.keys.get(&serial) else {
            return Ok(());
        };
        if key.has_references() {
            return Ok(());
        }
        if !key.gc_plan.is_idle() || key.gc_next.is_some() {
            return Err(AxError::BadState);
        }

        let mut pending = Some(serial);
        let result = (|| -> AxResult<()> {
            while let Some(serial) = pending {
                let (next, quota_uid, admission, abi_charge, resident_charge) = {
                    let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
                    if key.has_references() || !key.gc_plan.is_idle() {
                        return Err(AxError::BadState);
                    }
                    for linked in &key.links {
                        if self.keys.get(linked).is_none_or(|linked_key| {
                            linked_key.link_refs == 0 || !linked_key.gc_plan.is_idle()
                        }) {
                            return Err(AxError::BadState);
                        }
                    }
                    (
                        key.gc_next,
                        key.quota_uid,
                        key.ongoing_quota_admission(),
                        key.abi_charge,
                        key.resident_charge,
                    )
                };
                self.keys.get_mut(&serial).ok_or(AxError::BadState)?.gc_next = None;
                pending = next;
                let owner = self.owners.plan_replace(
                    quota_uid,
                    admission,
                    abi_charge,
                    AbiQuotaCharge::ZERO,
                )?;
                let budget = self
                    .budget
                    .plan_replace(resident_charge, ResidentCharge::ZERO)?;
                let mut key = self.keys.remove(&serial).ok_or(AxError::BadState)?;
                let links = core::mem::take(&mut key.links);
                self.owners.apply(owner);
                self.budget.apply(budget);

                for linked in links {
                    let linked_key = self.keys.get_mut(&linked).ok_or(AxError::BadState)?;
                    let refs = linked_key
                        .link_refs
                        .checked_sub(1)
                        .ok_or(AxError::BadState)?;
                    linked_key.link_refs = refs;
                    if refs == 0 {
                        if linked_key.gc_next.is_some() {
                            return Err(AxError::BadState);
                        }
                        linked_key.gc_next = pending;
                        pending = Some(linked);
                    }
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.clear_gc_pending(pending);
        }
        result
    }

    pub(super) fn discard_new_key(&mut self, serial: i32) -> AxResult<()> {
        if self.keys.get(&serial).is_some_and(Key::has_references) {
            return Err(AxError::BadState);
        }
        self.collect_unreferenced(serial)
    }
}
