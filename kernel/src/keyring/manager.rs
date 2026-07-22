use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::{String, ToString},
    vec::Vec,
};
#[cfg(test)]
use core::mem::size_of;

use axerrno::{AxError, AxResult, LinuxError};
use thekernel_linux_cred::{KeyPermission, KeyPermissionMask};

#[cfg(test)]
use super::accounting::{
    KEY_MAXBYTES_DEFAULT, KEY_MAXKEYS_DEFAULT, MANAGER_MAX_LINK_BYTES, ManagerBudgetLimits,
    ManagerBudgetUsage, OwnerUsage, validate_key_quota_limit,
};
#[cfg(test)]
use super::object::{KEY_RESIDENT_NODE_OVERHEAD, permission_mask};
use super::{
    accounting::{
        AbiQuotaCharge, AccountingPlan, ManagerBudget, OwnerLedger, QuotaAdmission, ResidentCharge,
        user_maxbytes, user_maxkeys,
    },
    contract::{KeyActor, KeyUserRecord, KeyctlCommand, KeyctlOutput, ReqKeyDefault},
    object::{
        BIG_KEY_ABI_PAYLOAD_CHARGE, KEY_LINK_CHARGE, Key, KeyState, KeyTypeKind,
        anonymous_session_keyring_permissions, named_session_keyring_permissions,
        persistent_keyring_permissions, thread_process_keyring_permissions,
        uid_keyring_permissions, wipe_key_bytes,
    },
};
#[cfg(test)]
use crate::task::{Credentials, DacCredentialView, UserNamespace};
use crate::{
    task::{Kgid, Kuid},
    time::wall_time,
};

const KEY_SPEC_THREAD_KEYRING: i32 = -1;
const KEY_SPEC_PROCESS_KEYRING: i32 = -2;
const KEY_SPEC_SESSION_KEYRING: i32 = -3;
const KEY_SPEC_USER_KEYRING: i32 = -4;
const KEY_SPEC_USER_SESSION_KEYRING: i32 = -5;

const KEYRING_SEARCH_MAX_DEPTH: usize = 6;
const PERSISTENT_KEYRING_TIMEOUT_SECS: u64 = 3 * 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PossessionContext {
    Recompute,
    Fixed(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedKey {
    serial: i32,
    possession: PossessionContext,
}

impl ResolvedKey {
    const fn numeric(serial: i32) -> Self {
        Self {
            serial,
            possession: PossessionContext::Recompute,
        }
    }

    const fn possessed(serial: i32) -> Self {
        Self {
            serial,
            possession: PossessionContext::Fixed(true),
        }
    }

    const fn with_possession(serial: i32, possessed: bool) -> Self {
        Self {
            serial,
            possession: PossessionContext::Fixed(possessed),
        }
    }
}

impl From<i32> for ResolvedKey {
    fn from(serial: i32) -> Self {
        Self::numeric(serial)
    }
}

pub(super) struct KeyManager {
    next_serial: i32,
    keys: BTreeMap<i32, Key>,
    owners: OwnerLedger,
    budget: ManagerBudget,
    thread_keyrings: BTreeMap<u32, i32>,
    process_keyrings: BTreeMap<u32, i32>,
    session_keyrings: BTreeMap<u32, i32>,
    user_keyrings: BTreeMap<Kuid, i32>,
    user_session_keyrings: BTreeMap<Kuid, i32>,
    persistent_keyrings: BTreeMap<Kuid, i32>,
    reqkey_defaults: BTreeMap<u32, i32>,
}

#[derive(Clone, Copy)]
enum RootSource {
    Thread(u32),
    Process(u32),
    Session(u32),
    User(Kuid),
    UserSession(Kuid),
    Persistent(Kuid),
}

impl KeyManager {
    pub(super) const fn new() -> Self {
        Self {
            next_serial: 1,
            keys: BTreeMap::new(),
            owners: OwnerLedger {
                usage: BTreeMap::new(),
            },
            budget: ManagerBudget::kernel_default(),
            thread_keyrings: BTreeMap::new(),
            process_keyrings: BTreeMap::new(),
            session_keyrings: BTreeMap::new(),
            user_keyrings: BTreeMap::new(),
            user_session_keyrings: BTreeMap::new(),
            persistent_keyrings: BTreeMap::new(),
            reqkey_defaults: BTreeMap::new(),
        }
    }

    fn alloc_serial(&mut self) -> AxResult<i32> {
        let start = self.next_serial.max(1);
        let mut serial = start;
        loop {
            if !self.keys.contains_key(&serial) {
                self.next_serial = serial.checked_add(1).unwrap_or(1);
                return Ok(serial);
            }
            serial = serial.checked_add(1).unwrap_or(1);
            if serial == start {
                return Err(LinuxError::ENOSPC.into());
            }
        }
    }

    #[cfg(test)]
    fn with_budget(limits: ManagerBudgetLimits) -> Self {
        Self {
            budget: ManagerBudget::new(limits),
            ..Self::new()
        }
    }

    #[cfg(test)]
    fn insert_key(&mut self, key: AxResult<Key>) -> i32 {
        self.try_insert_key(key.unwrap(), QuotaAdmission::Enforced)
            .unwrap()
    }

    fn try_insert_key(&mut self, mut key: Key, admission: QuotaAdmission) -> AxResult<i32> {
        // QUOTA_OVERRUN only relaxes creation admission. The object remains
        // charged, and all later growth and ownership transfers are enforced.
        key.in_owner_quota = admission != QuotaAdmission::Exempt;
        let owner =
            self.owners
                .plan_replace(key.uid, admission, AbiQuotaCharge::ZERO, key.abi_charge)?;
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
    fn create_keyring(
        &mut self,
        description: String,
        uid: Kuid,
        gid: Kgid,
        permissions: KeyPermissionMask,
    ) -> i32 {
        self.insert_key(Key::keyring(description, uid, gid, permissions))
    }

    fn try_create_keyring(
        &mut self,
        description: String,
        uid: Kuid,
        gid: Kgid,
        permissions: KeyPermissionMask,
        admission: QuotaAdmission,
    ) -> AxResult<i32> {
        self.try_insert_key(Key::keyring(description, uid, gid, permissions)?, admission)
    }

    fn try_create_rooted_keyring(
        &mut self,
        source: RootSource,
        description: String,
        uid: Kuid,
        gid: Kgid,
        permissions: KeyPermissionMask,
        admission: QuotaAdmission,
    ) -> AxResult<i32> {
        let serial = self.try_create_keyring(description, uid, gid, permissions, admission)?;
        if let Err(error) = self.install_root(source, serial) {
            self.discard_new_key(serial)?;
            return Err(error);
        }
        Ok(serial)
    }

    fn plan_key_resize(
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
                key.uid,
                key.ongoing_quota_admission(),
                key.abi_charge,
                new_abi,
            )?,
            budget: self
                .budget
                .plan_replace(key.resident_charge, new_resident)?,
        })
    }

    fn apply_key_resize(
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

    fn root_serial(&self, source: RootSource) -> Option<i32> {
        match source {
            RootSource::Thread(owner) => self.thread_keyrings.get(&owner).copied(),
            RootSource::Process(owner) => self.process_keyrings.get(&owner).copied(),
            RootSource::Session(owner) => self.session_keyrings.get(&owner).copied(),
            RootSource::User(uid) => self.user_keyrings.get(&uid).copied(),
            RootSource::UserSession(uid) => self.user_session_keyrings.get(&uid).copied(),
            RootSource::Persistent(uid) => self.persistent_keyrings.get(&uid).copied(),
        }
    }

    fn anonymous_session_admission(&self, process_owner: u32) -> QuotaAdmission {
        if self.session_keyrings.contains_key(&process_owner) {
            QuotaAdmission::Enforced
        } else {
            QuotaAdmission::AllowOverrun
        }
    }

    fn install_root(&mut self, source: RootSource, serial: i32) -> AxResult<()> {
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
            RootSource::User(uid) => self.user_keyrings.insert(uid, serial),
            RootSource::UserSession(uid) => self.user_session_keyrings.insert(uid, serial),
            RootSource::Persistent(uid) => self.persistent_keyrings.insert(uid, serial),
        };
        if let Some(old) = old {
            self.release_root_ref(old)?;
        }
        Ok(())
    }

    fn release_root_ref(&mut self, serial: i32) -> AxResult<()> {
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

    fn clear_gc_pending(&mut self, mut pending: Option<i32>) {
        while let Some(serial) = pending {
            let Some(key) = self.keys.get_mut(&serial) else {
                break;
            };
            pending = key.gc_next.take();
        }
    }

    fn collect_unreferenced(&mut self, serial: i32) -> AxResult<()> {
        let Some(key) = self.keys.get(&serial) else {
            return Ok(());
        };
        if key.has_references() {
            return Ok(());
        }
        if key.gc_next.is_some() {
            return Err(AxError::BadState);
        }

        let mut pending = Some(serial);
        let result = (|| -> AxResult<()> {
            while let Some(serial) = pending {
                let (next, uid, admission, abi_charge, resident_charge) = {
                    let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
                    if key.has_references() {
                        return Err(AxError::BadState);
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
                    (
                        key.gc_next,
                        key.uid,
                        key.ongoing_quota_admission(),
                        key.abi_charge,
                        key.resident_charge,
                    )
                };
                self.keys.get_mut(&serial).ok_or(AxError::BadState)?.gc_next = None;
                pending = next;
                let owner =
                    self.owners
                        .plan_replace(uid, admission, abi_charge, AbiQuotaCharge::ZERO)?;
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

    fn discard_new_key(&mut self, serial: i32) -> AxResult<()> {
        if self.keys.get(&serial).is_some_and(Key::has_references) {
            return Err(AxError::BadState);
        }
        self.collect_unreferenced(serial)
    }

    fn special_keyring(&mut self, spec: i32, actor: &KeyActor, create: bool) -> AxResult<i32> {
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
                    actor.owner_uid(),
                    actor.owner_gid(),
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
                    actor.owner_uid(),
                    actor.owner_gid(),
                    thread_process_keyring_permissions(),
                    QuotaAdmission::AllowOverrun,
                )?;
                Ok(id)
            }
            KEY_SPEC_SESSION_KEYRING => {
                if let Some(id) = self.session_keyrings.get(&actor.process_owner) {
                    return Ok(*id);
                }
                if !create {
                    let id = self.special_keyring(KEY_SPEC_USER_SESSION_KEYRING, actor, true)?;
                    self.install_root(RootSource::Session(actor.process_owner), id)?;
                    return Ok(id);
                }
                let id = self.try_create_rooted_keyring(
                    RootSource::Session(actor.process_owner),
                    format!("_ses.{}", actor.pid),
                    actor.owner_uid(),
                    actor.owner_gid(),
                    anonymous_session_keyring_permissions(),
                    QuotaAdmission::AllowOverrun,
                )?;
                Ok(id)
            }
            KEY_SPEC_USER_KEYRING => {
                let uid = actor.user_uid();
                if let Some(id) = self.user_keyrings.get(&uid) {
                    return Ok(*id);
                }
                let id = self.try_create_rooted_keyring(
                    RootSource::User(uid),
                    format!("_uid.{}", uid.into_raw()),
                    uid,
                    actor.owner_gid(),
                    uid_keyring_permissions(),
                    QuotaAdmission::Enforced,
                )?;
                Ok(id)
            }
            KEY_SPEC_USER_SESSION_KEYRING => {
                let uid = actor.user_uid();
                if let Some(id) = self.user_session_keyrings.get(&uid) {
                    return Ok(*id);
                }
                let user_keyring = self.special_keyring(KEY_SPEC_USER_KEYRING, actor, true)?;
                let id = self.try_create_keyring(
                    format!("_uid_ses.{}", uid.into_raw()),
                    uid,
                    actor.owner_gid(),
                    uid_keyring_permissions(),
                    QuotaAdmission::Enforced,
                )?;
                if let Err(error) = self.link_key_replace(id, user_keyring) {
                    self.discard_new_key(id)?;
                    return Err(error);
                }
                if let Err(error) = self.install_root(RootSource::UserSession(uid), id) {
                    self.discard_new_key(id)?;
                    return Err(error);
                }
                Ok(id)
            }
            _ => Err(LinuxError::ENOKEY.into()),
        }
    }

    fn resolve_keyring(
        &mut self,
        id: i32,
        actor: &KeyActor,
        create: bool,
    ) -> AxResult<ResolvedKey> {
        let resolved = self.resolve_key(id, actor, create)?;
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

    fn resolve_key(
        &mut self,
        id: i32,
        actor: &KeyActor,
        create_special: bool,
    ) -> AxResult<ResolvedKey> {
        let resolved = if id < 0 {
            ResolvedKey::possessed(self.special_keyring(id, actor, create_special)?)
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

    fn check_key_available(&self, key: &Key, allow_keyring: bool) -> AxResult<()> {
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

    fn possession_roots(&self, actor: &KeyActor) -> Vec<i32> {
        let mut roots = Vec::new();
        if let Some(id) = self.thread_keyrings.get(&actor.thread_owner) {
            roots.push(*id);
        }
        if let Some(id) = self.process_keyrings.get(&actor.process_owner) {
            roots.push(*id);
        }
        if let Some(id) = self.session_keyrings.get(&actor.process_owner) {
            roots.push(*id);
        } else if let Some(id) = self.user_session_keyrings.get(&actor.user_uid()) {
            roots.push(*id);
        }
        roots
    }

    fn is_possessed(&self, target: i32, actor: &KeyActor) -> bool {
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
                    && key
                        .perm
                        .allows(key.uid, key.gid, &actor.dac, true, KeyPermission::SEARCH);
            }
            if depth > KEYRING_SEARCH_MAX_DEPTH {
                continue;
            }
            if self.check_key_available(key, true).is_err()
                || !key
                    .perm
                    .allows(key.uid, key.gid, &actor.dac, true, KeyPermission::SEARCH)
            {
                continue;
            }
            if key.is_keyring() {
                pending.extend(key.links.iter().copied().map(|serial| (serial, depth + 1)));
            }
        }
        false
    }

    fn key_has_perm(
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
            .allows(key.uid, key.gid, &actor.dac, possessed, permission))
    }

    fn keyring_has_write(
        &self,
        keyring: impl Into<ResolvedKey>,
        actor: &KeyActor,
    ) -> AxResult<bool> {
        self.keyring_has_perm(keyring, actor, KeyPermission::WRITE)
    }

    fn keyring_has_perm(
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

    fn find_linked_key(&self, keyring: i32, kind: KeyTypeKind, description: &str) -> Option<i32> {
        let keyring = self.keys.get(&keyring)?;
        keyring.links.iter().copied().find(|serial| {
            self.keys
                .get(serial)
                .is_some_and(|key| key.kind == kind && key.description == description)
        })
    }

    fn remove_key_everywhere(&mut self, serial: i32) -> AxResult<()> {
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
            .chain(self.user_keyrings.values())
            .chain(self.user_session_keyrings.values())
            .chain(self.persistent_keyrings.values())
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
        detach_roots!(self.user_keyrings);
        detach_roots!(self.user_session_keyrings);
        detach_roots!(self.persistent_keyrings);

        self.keys
            .get_mut(&serial)
            .ok_or(AxError::BadState)?
            .root_refs = 0;
        self.collect_unreferenced(serial)
    }

    fn unlink_key_from_keyring(&mut self, keyring: i32, serial: i32) -> AxResult<()> {
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

    fn link_key_replace(&mut self, keyring: i32, serial: i32) -> AxResult<()> {
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

    fn clear_keyring_links(&mut self, keyring: i32) -> AxResult<()> {
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

    fn revoke_key(&mut self, serial: i32) -> AxResult<()> {
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

    fn validate_keyring_link(&self, destination: i32, serial: i32) -> AxResult<()> {
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

    fn check_link_destination(&self, destination: i32) -> AxResult<()> {
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

    fn link_existing_key(
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

    fn move_key_link(&mut self, from: i32, to: i32, serial: i32, exclusive: bool) -> AxResult<()> {
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

    fn search_keyring(
        &self,
        keyring: impl Into<ResolvedKey>,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: &str,
        visited: &mut BTreeSet<i32>,
    ) -> AxResult<Option<ResolvedKey>> {
        let keyring = keyring.into();
        let search_possessed = match keyring.possession {
            PossessionContext::Recompute => self.is_possessed(keyring.serial, actor),
            PossessionContext::Fixed(possessed) => possessed,
        };
        let mut pending = VecDeque::from([(keyring.serial, 0)]);
        let mut first_error = None;

        while let Some((ring_serial, depth)) = pending.pop_front() {
            if !visited.insert(ring_serial) {
                continue;
            }
            let Some(ring) = self.keys.get(&ring_serial) else {
                if depth == 0 {
                    return Err(LinuxError::ENOKEY.into());
                }
                continue;
            };
            if let Err(error) = self.check_key_available(ring, true) {
                if depth == 0 {
                    return Err(error);
                }
                continue;
            }
            if !ring.is_keyring() {
                if depth == 0 {
                    return Err(AxError::InvalidInput);
                }
                continue;
            }
            let ring_ref = ResolvedKey::with_possession(ring_serial, search_possessed);
            if !self.key_has_perm(ring_ref, actor, KeyPermission::SEARCH)? {
                if depth == 0 {
                    return Err(LinuxError::EACCES.into());
                }
                continue;
            }
            if depth == 0 && ring.kind == kind && ring.description == description {
                return Ok(Some(ResolvedKey::with_possession(
                    ring_serial,
                    search_possessed,
                )));
            }

            for serial in &ring.links {
                let Some(key) = self.keys.get(serial) else {
                    continue;
                };
                if key.kind != kind || key.description != description {
                    continue;
                }
                if let Err(error) = self.check_key_available(key, true) {
                    first_error.get_or_insert(error);
                    continue;
                }
                match self.key_has_perm(
                    ResolvedKey::with_possession(*serial, search_possessed),
                    actor,
                    KeyPermission::SEARCH,
                ) {
                    Ok(true) => {
                        return Ok(Some(ResolvedKey::with_possession(
                            *serial,
                            search_possessed,
                        )));
                    }
                    Ok(false) => {
                        first_error.get_or_insert(AxError::from(LinuxError::EACCES));
                    }
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }

            for serial in &ring.links {
                if !self.keys.get(serial).is_some_and(Key::is_keyring) {
                    continue;
                }
                if depth >= KEYRING_SEARCH_MAX_DEPTH {
                    continue;
                }
                pending.push_back((*serial, depth + 1));
            }
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(None)
        }
    }

    fn get_persistent_keyring(&mut self, uid: Kuid, actor: &KeyActor) -> AxResult<ResolvedKey> {
        if let Some(serial) = self.persistent_keyrings.get(&uid).copied() {
            let key = self.keys.get(&serial).ok_or(AxError::BadState)?;
            let now = wall_time().as_secs();
            if !key.expires_at.is_some_and(|expires_at| now >= expires_at) {
                return Ok(ResolvedKey::possessed(serial));
            }
            if key.root_refs == 0 {
                return Err(AxError::BadState);
            }
            self.persistent_keyrings.remove(&uid);
            self.release_root_ref(serial)?;
        }

        let serial = self.try_create_rooted_keyring(
            RootSource::Persistent(uid),
            format!("_persistent.{}", uid.into_raw()),
            uid,
            actor.owner_gid(),
            persistent_keyring_permissions(),
            QuotaAdmission::Exempt,
        )?;
        // UID/CAP_SETUID authorization above acquires this key reference. Keep
        // that possession through the link transaction instead of resolving
        // the persistent root as an unrelated numeric serial.
        Ok(ResolvedKey::possessed(serial))
    }

    fn link_persistent_keyring(
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

    fn current_search_keyrings(&self, actor: &KeyActor) -> Vec<ResolvedKey> {
        let mut keyrings = Vec::new();
        if let Some(id) = self.thread_keyrings.get(&actor.thread_owner) {
            keyrings.push(ResolvedKey::possessed(*id));
        }
        if let Some(id) = self.process_keyrings.get(&actor.process_owner) {
            keyrings.push(ResolvedKey::possessed(*id));
        }
        if let Some(id) = self.session_keyrings.get(&actor.process_owner) {
            keyrings.push(ResolvedKey::possessed(*id));
        } else if let Some(id) = self.user_session_keyrings.get(&actor.user_uid()) {
            keyrings.push(ResolvedKey::possessed(*id));
        }
        keyrings
    }

    fn search_current(
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

    fn link_existing_request_result(
        &mut self,
        dest: i32,
        key: impl Into<ResolvedKey>,
        actor: &KeyActor,
    ) -> AxResult<()> {
        if dest == 0 {
            return Ok(());
        }
        let destination = self.resolve_keyring(dest, actor, true)?;
        self.link_existing_key(destination, key, actor, false)
    }

    fn replace_payload(&mut self, serial: i32, payload: Vec<u8>) -> AxResult<()> {
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
                key.uid,
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

    pub(super) fn key_user_records(&self) -> Vec<KeyUserRecord> {
        let mut live_keys = BTreeMap::<Kuid, usize>::new();
        for key in self.keys.values() {
            *live_keys.entry(key.uid).or_default() += 1;
        }
        for uid in self.owners.usage.keys() {
            live_keys.entry(*uid).or_default();
        }
        live_keys
            .into_iter()
            .map(|(uid, keys)| {
                let quota = self.owners.usage(uid);
                KeyUserRecord {
                    uid: uid.into_raw(),
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
            .collect()
    }
}

impl KeyManager {
    pub(super) fn add_key(
        &mut self,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: String,
        payload: Vec<u8>,
        keyring: i32,
    ) -> AxResult<isize> {
        let manager = self;
        let keyring = manager.resolve_keyring(keyring, actor, true)?;
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

        manager.check_link_destination(keyring.serial)?;
        let key = Key::positive(
            kind,
            description,
            payload,
            actor.owner_uid(),
            actor.owner_gid(),
        );
        let serial = manager.try_insert_key(key?, QuotaAdmission::Enforced)?;
        if let Err(error) = manager.link_key_replace(keyring.serial, serial) {
            manager.discard_new_key(serial)?;
            return Err(error);
        }
        Ok(serial as isize)
    }

    pub(super) fn request_key(
        &mut self,
        actor: &KeyActor,
        kind: KeyTypeKind,
        description: &str,
        callout_present: bool,
        dest_keyring: i32,
    ) -> AxResult<isize> {
        let manager = self;
        if let Some(resolved) = manager.search_current(actor, kind, description)? {
            let key = manager
                .keys
                .get(&resolved.serial)
                .ok_or(AxError::from(LinuxError::ENOKEY))?;
            manager.check_key_available(key, true)?;
            manager.link_existing_request_result(dest_keyring, resolved, actor)?;
            return Ok(resolved.serial as isize);
        }

        if callout_present {
            // Construction requires a target-bound, one-shot request authority and
            // a userspace upcall. Neither exists yet, so do not fabricate a key or
            // a permanent negative cache entry.
            Err(LinuxError::EOPNOTSUPP.into())
        } else {
            Err(LinuxError::ENOKEY.into())
        }
    }

    pub(super) fn keyctl(
        &mut self,
        actor: &KeyActor,
        command: KeyctlCommand,
    ) -> AxResult<KeyctlOutput> {
        let manager = self;
        let value = match command {
            KeyctlCommand::GetKeyringId { keyring, create } => {
                let serial = manager.resolve_keyring(keyring, actor, create)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::SEARCH)? {
                    return Err(LinuxError::EACCES.into());
                }
                serial.serial as isize
            }
            KeyctlCommand::JoinSession { name } => {
                let serial = if let Some(name) = name.as_deref() {
                    if name.starts_with('.') {
                        return Err(AxError::OperationNotPermitted);
                    }
                    let existing = manager.keys.iter().find_map(|(serial, key)| {
                        (key.is_keyring()
                            && key.description == name
                            && manager
                                .key_has_perm(*serial, actor, KeyPermission::SEARCH)
                                .unwrap_or(false))
                        .then_some(*serial)
                    });
                    if let Some(serial) = existing {
                        serial
                    } else {
                        manager.try_create_keyring(
                            name.to_string(),
                            actor.owner_uid(),
                            actor.owner_gid(),
                            named_session_keyring_permissions(),
                            QuotaAdmission::Enforced,
                        )?
                    }
                } else {
                    let admission = manager.anonymous_session_admission(actor.process_owner);
                    manager.try_create_keyring(
                        format!("_ses.{}", actor.pid),
                        actor.owner_uid(),
                        actor.owner_gid(),
                        anonymous_session_keyring_permissions(),
                        admission,
                    )?
                };
                manager.install_root(RootSource::Session(actor.process_owner), serial)?;
                serial as isize
            }
            KeyctlCommand::Update { key, payload } => {
                let serial = manager.resolve_key(key, actor, false)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::WRITE)? {
                    return Err(LinuxError::EACCES.into());
                }
                manager.replace_payload(serial.serial, payload)?;
                0
            }
            KeyctlCommand::Revoke { key } => {
                let serial = manager.resolve_key(key, actor, false)?;
                let can_write = manager.key_has_perm(serial, actor, KeyPermission::WRITE)?;
                let can_setattr = manager.key_has_perm(serial, actor, KeyPermission::SETATTR)?;
                if !can_write && !can_setattr {
                    return Err(LinuxError::EACCES.into());
                }
                manager.revoke_key(serial.serial)?;
                0
            }
            KeyctlCommand::Chown { key, uid, gid } => {
                let serial = manager.resolve_key(key, actor, false)?;
                if !manager.key_has_perm(serial, actor, KeyPermission::SETATTR)? {
                    return Err(LinuxError::EACCES.into());
                }
                let (old_uid, old_gid, charge, admission) = manager
                    .keys
                    .get(&serial.serial)
                    .map(|key| {
                        (
                            key.uid,
                            key.gid,
                            key.abi_charge,
                            key.ongoing_quota_admission(),
                        )
                    })
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                let uid = uid.map(|uid| actor.map_user_uid(uid)).transpose()?;
                let gid = gid.map(|gid| actor.map_user_gid(gid)).transpose()?;
                if uid.is_some_and(|uid| uid != old_uid) && !actor.has_sys_admin {
                    return Err(AxError::OperationNotPermitted);
                }
                if gid.is_some_and(|gid| gid != old_gid)
                    && !actor.has_sys_admin
                    && !gid.is_some_and(|gid| actor.in_group(gid))
                {
                    return Err(AxError::OperationNotPermitted);
                }
                let owner_updates = uid
                    .filter(|uid| *uid != old_uid)
                    .map(|uid| {
                        manager
                            .owners
                            .plan_transfer(old_uid, uid, admission, charge)
                    })
                    .transpose()?
                    .unwrap_or([None, None]);
                let key = manager
                    .keys
                    .get_mut(&serial.serial)
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                if let Some(uid) = uid {
                    key.uid = uid;
                }
                if let Some(gid) = gid {
                    key.gid = gid;
                }
                manager.owners.apply(owner_updates[0]);
                manager.owners.apply(owner_updates[1]);
                0
            }
            KeyctlCommand::SetPerm { key, permissions } => {
                let serial = manager.resolve_key(key, actor, false)?;
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
                let serial = manager.resolve_key(key, actor, false)?;
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
                    actor.display_gid(key.gid),
                    key.perm.into_raw(),
                    key.description
                )
                .into_bytes();
                return Ok(KeyctlOutput::CountedBytes(description));
            }
            KeyctlCommand::Clear { keyring } => {
                let keyring = manager.resolve_keyring(keyring, actor, false)?;
                if !manager.keyring_has_write(keyring, actor)? {
                    return Err(LinuxError::EACCES.into());
                }
                manager.clear_keyring_links(keyring.serial)?;
                0
            }
            KeyctlCommand::Link { key, keyring } => {
                let serial = manager.resolve_key(key, actor, false)?;
                let keyring = manager.resolve_keyring(keyring, actor, true)?;
                manager.link_existing_key(keyring, serial, actor, false)?;
                0
            }
            KeyctlCommand::Unlink { serial, keyring } => {
                let keyring = manager.resolve_keyring(keyring, actor, false)?;
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
                let keyring = manager.resolve_keyring(keyring, actor, false)?;
                let kind = KeyTypeKind::from_name(&type_name).ok_or(AxError::NoSuchDevice)?;
                let serial = manager
                    .search_keyring(keyring, actor, kind, &description, &mut BTreeSet::new())?
                    .ok_or(AxError::from(LinuxError::ENOKEY))?;
                if let Some(destination) = destination {
                    let dest = manager.resolve_keyring(destination, actor, true)?;
                    manager.link_existing_key(dest, serial, actor, false)?;
                }
                serial.serial as isize
            }
            KeyctlCommand::Read { key, copy_limit } => {
                let serial = manager.resolve_key(key, actor, false)?;
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
                let bytes = copy_limit
                    .map(|limit| key.payload[..full_len.min(limit)].to_vec())
                    .unwrap_or_default();
                return Ok(KeyctlOutput::Payload { full_len, bytes });
            }
            KeyctlCommand::SetReqKeyring { setting } => {
                let old_setting = manager
                    .reqkey_defaults
                    .get(&actor.process_owner)
                    .copied()
                    .unwrap_or(ReqKeyDefault::Default as i32);
                match setting {
                    ReqKeyDefault::NoChange => {
                        return Ok(KeyctlOutput::Value(old_setting as isize));
                    }
                    ReqKeyDefault::Thread => {
                        manager.special_keyring(KEY_SPEC_THREAD_KEYRING, actor, true)?;
                        manager
                            .reqkey_defaults
                            .insert(actor.process_owner, setting as i32);
                    }
                    ReqKeyDefault::Process => {
                        manager.special_keyring(KEY_SPEC_PROCESS_KEYRING, actor, true)?;
                        manager
                            .reqkey_defaults
                            .insert(actor.process_owner, setting as i32);
                    }
                    ReqKeyDefault::Default => {
                        manager.reqkey_defaults.remove(&actor.process_owner);
                    }
                    ReqKeyDefault::Session | ReqKeyDefault::User | ReqKeyDefault::UserSession => {
                        manager
                            .reqkey_defaults
                            .insert(actor.process_owner, setting as i32);
                    }
                }
                old_setting as isize
            }
            KeyctlCommand::SetTimeout { key, seconds } => {
                let serial = manager.resolve_key(key, actor, false)?;
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
                let serial = manager.resolve_key(key, actor, false)?;
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
                let dest = manager.resolve_keyring(destination, actor, true)?;
                if !manager.keyring_has_write(dest, actor)? {
                    return Err(LinuxError::EACCES.into());
                }
                let persistent = manager.get_persistent_keyring(uid, actor)?;
                manager.link_persistent_keyring(dest, persistent, actor)?;
                persistent.serial as isize
            }
            KeyctlCommand::Restrict { keyring } => {
                let serial = manager.resolve_keyring(keyring, actor, false)?;
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
                0
            }
            KeyctlCommand::Move {
                key,
                from,
                to,
                exclusive,
            } => {
                let serial = manager.resolve_key(key, actor, false)?;
                let from = manager.resolve_keyring(from, actor, false)?;
                let to = manager.resolve_keyring(to, actor, true)?;
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

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec};

    use thekernel_linux_cred::{CAPABILITY_WORDS, GroupInfo};

    use super::*;

    fn actor(tid: u32, pid: u32, uid: u32, gid: u32) -> KeyActor {
        let uid = Kuid::from_raw(uid).unwrap();
        let gid = Kgid::from_raw(gid).unwrap();
        let user_ns = UserNamespace::try_new_root().unwrap();
        let groups = GroupInfo::try_new(Vec::new()).unwrap();
        KeyActor {
            tid,
            pid,
            thread_owner: tid,
            process_owner: pid,
            ids: Credentials {
                ruid: uid,
                euid: uid,
                suid: uid,
                fsuid: uid,
                rgid: gid,
                egid: gid,
                sgid: gid,
                fsgid: gid,
            },
            dac: DacCredentialView::new(uid, gid, groups, [0; CAPABILITY_WORDS], true),
            user_ns,
            has_sys_admin: false,
            has_setuid: false,
        }
    }

    fn assert_accounting_consistent(manager: &KeyManager) {
        let mut owners = BTreeMap::<Kuid, OwnerUsage>::new();
        let mut budget = ManagerBudgetUsage::default();
        let mut root_refs = BTreeMap::<i32, usize>::new();
        let mut link_refs = BTreeMap::<i32, usize>::new();

        for (serial, key) in &manager.keys {
            if key.in_owner_quota {
                let usage = owners.entry(key.uid).or_default();
                usage.keys += key.abi_charge.keys;
                usage.bytes += key.abi_charge.bytes;
            }
            budget.objects += key.resident_charge.objects;
            budget.bytes += key.resident_charge.bytes;
            budget.link_bytes += key.resident_charge.link_bytes;
            for linked in &key.links {
                *link_refs.entry(*linked).or_default() += 1;
            }
            root_refs.entry(*serial).or_default();
            link_refs.entry(*serial).or_default();
        }

        for serial in manager
            .thread_keyrings
            .values()
            .chain(manager.process_keyrings.values())
            .chain(manager.session_keyrings.values())
            .chain(manager.user_keyrings.values())
            .chain(manager.user_session_keyrings.values())
            .chain(manager.persistent_keyrings.values())
        {
            *root_refs.entry(*serial).or_default() += 1;
        }

        assert_eq!(manager.owners.usage, owners);
        assert_eq!(manager.budget.used, budget);
        for (serial, key) in &manager.keys {
            assert_eq!(key.root_refs, root_refs[serial]);
            assert_eq!(key.link_refs, link_refs[serial]);
            assert_eq!(key.gc_next, None);
            assert_eq!(
                key.resident_charge.link_bytes,
                key.links.capacity() * KEY_LINK_CHARGE
            );
        }
    }

    #[test]
    fn quota_sysctl_limits_match_linux_int_range() {
        assert_eq!(validate_key_quota_limit(1), Ok(1));
        assert_eq!(
            validate_key_quota_limit(i32::MAX as usize),
            Ok(i32::MAX as usize)
        );
        assert_eq!(validate_key_quota_limit(0), Err(AxError::InvalidInput));
        assert_eq!(
            validate_key_quota_limit(i32::MAX as usize + 1),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn only_the_first_anonymous_session_install_allows_quota_overrun() {
        let owner = actor(1, 1, 1000, 1000);
        let mut manager = KeyManager::new();
        assert_eq!(
            manager.anonymous_session_admission(owner.process_owner),
            QuotaAdmission::AllowOverrun
        );
        let session = manager
            .try_create_rooted_keyring(
                RootSource::Session(owner.process_owner),
                "_ses.1".to_string(),
                owner.owner_uid(),
                owner.owner_gid(),
                anonymous_session_keyring_permissions(),
                QuotaAdmission::AllowOverrun,
            )
            .unwrap();
        assert_eq!(
            manager.anonymous_session_admission(owner.process_owner),
            QuotaAdmission::Enforced
        );
        assert_eq!(manager.session_keyrings[&owner.process_owner], session);
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn session_lookup_without_create_installs_the_user_session_keyring() {
        let owner = actor(2, 2, 1000, 1000);
        let mut manager = KeyManager::new();

        let session = manager
            .special_keyring(KEY_SPEC_SESSION_KEYRING, &owner, false)
            .unwrap();

        assert_eq!(manager.session_keyrings[&owner.process_owner], session);
        assert_eq!(manager.user_session_keyrings[&owner.user_uid()], session);
        assert_eq!(manager.keys[&session].root_refs, 2);
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn persistent_lookup_carries_possession_into_link_and_refreshes_expiry() {
        let owner = actor(3, 3, 1000, 1000);
        let mut manager = KeyManager::new();
        let destination = manager
            .special_keyring(KEY_SPEC_PROCESS_KEYRING, &owner, true)
            .unwrap();

        let persistent = manager
            .get_persistent_keyring(owner.user_uid(), &owner)
            .unwrap();
        assert_eq!(persistent.possession, PossessionContext::Fixed(true));
        assert!(!manager.is_possessed(persistent.serial, &owner));
        assert_eq!(manager.keys[&persistent.serial].expires_at, None);

        manager.budget.limits.link_bytes = 0;
        assert_eq!(
            manager.link_persistent_keyring(ResolvedKey::numeric(destination), persistent, &owner,),
            Err(AxError::NoMemory)
        );
        assert_eq!(manager.keys[&persistent.serial].expires_at, None);
        assert!(manager.keys[&destination].links.is_empty());

        manager.budget.limits.link_bytes = MANAGER_MAX_LINK_BYTES;
        manager
            .link_persistent_keyring(ResolvedKey::numeric(destination), persistent, &owner)
            .unwrap();
        let first_expiry = manager.keys[&persistent.serial].expires_at.unwrap();
        assert!(first_expiry > wall_time().as_secs());
        assert_eq!(manager.keys[&destination].links, [persistent.serial]);
        assert_eq!(manager.keys[&persistent.serial].root_refs, 1);
        assert_eq!(manager.keys[&persistent.serial].link_refs, 1);

        let reused = manager
            .get_persistent_keyring(owner.user_uid(), &owner)
            .unwrap();
        assert_eq!(reused.serial, persistent.serial);
        assert!(manager.keys[&persistent.serial].expires_at.unwrap() >= first_expiry);

        manager.keys.get_mut(&persistent.serial).unwrap().expires_at = Some(0);
        let replacement = manager
            .get_persistent_keyring(owner.user_uid(), &owner)
            .unwrap();
        assert_ne!(replacement.serial, persistent.serial);
        assert_eq!(manager.keys[&persistent.serial].root_refs, 0);
        assert_eq!(manager.keys[&persistent.serial].link_refs, 1);
        assert_eq!(manager.keys[&replacement.serial].expires_at, None);
        manager
            .link_persistent_keyring(ResolvedKey::numeric(destination), replacement, &owner)
            .unwrap();
        assert!(!manager.keys.contains_key(&persistent.serial));
        assert_eq!(manager.keys[&destination].links, [replacement.serial]);
        assert!(manager.keys[&replacement.serial].expires_at.is_some());
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn layer_two_rejects_invalid_type_payloads() {
        let owner = actor(1, 1, 1000, 1000);
        assert!(matches!(
            Key::positive(
                KeyTypeKind::BigKey,
                "empty-big-key".to_string(),
                Vec::new(),
                owner.owner_uid(),
                owner.owner_gid(),
            ),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            Key::positive(
                KeyTypeKind::User,
                "empty-user".to_string(),
                Vec::new(),
                owner.owner_uid(),
                owner.owner_gid(),
            ),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            Key::positive(
                KeyTypeKind::Logon,
                "service:empty".to_string(),
                Vec::new(),
                owner.owner_uid(),
                owner.owner_gid(),
            ),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            Key::positive(
                KeyTypeKind::Logon,
                "missing-separator".to_string(),
                vec![1],
                owner.owner_uid(),
                owner.owner_gid(),
            ),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            Key::positive(
                KeyTypeKind::Logon,
                ":missing-prefix".to_string(),
                vec![1],
                owner.owner_uid(),
                owner.owner_gid(),
            ),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            Key::new(
                KeyTypeKind::Keyring,
                "ring".to_string(),
                vec![1],
                owner.owner_uid(),
                owner.owner_gid(),
                thread_process_keyring_permissions(),
            ),
            Err(AxError::InvalidInput)
        ));
    }

    #[test]
    fn staged_link_growth_is_geometric() {
        const LINKS: i32 = 4_096;

        let owner = actor(3, 3, 0, 0);
        let mut ring = Key::keyring(
            "wide".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        )
        .unwrap();
        let mut reallocations = 0;
        for serial in 1..=LINKS {
            if let Some(new_capacity) = ring.next_link_capacity().unwrap() {
                ring.links = ring.stage_link_push(serial, new_capacity).unwrap();
                reallocations += 1;
            } else {
                ring.links.push(serial);
            }
        }

        assert_eq!(ring.links.len(), LINKS as usize);
        assert!(ring.links.capacity() >= LINKS as usize);
        assert!(reallocations <= 11, "reallocated {reallocations} times");
    }

    #[test]
    fn payload_update_rejects_empty_user_and_logon_data() {
        let owner = actor(4, 4, 1000, 1000);
        let mut manager = KeyManager::new();
        for (kind, description) in [
            (KeyTypeKind::User, "user"),
            (KeyTypeKind::Logon, "service:secret"),
        ] {
            let serial = manager.insert_key(Key::positive(
                kind,
                description.to_string(),
                vec![0xa5],
                owner.owner_uid(),
                owner.owner_gid(),
            ));
            assert_eq!(
                manager.replace_payload(serial, Vec::new()),
                Err(AxError::InvalidInput)
            );
            assert_eq!(manager.keys[&serial].payload, vec![0xa5]);
        }
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn guessed_serial_does_not_receive_possessor_permissions() {
        let owner = actor(1, 1, 1000, 1000);
        let outsider = actor(2, 2, 2000, 2000);
        let same_uid = actor(3, 3, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "secret".to_string(),
            Vec::from([1, 2, 3]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, serial).unwrap();

        assert!(
            manager
                .key_has_perm(serial, &owner, KeyPermission::READ)
                .unwrap()
        );
        assert!(
            !manager
                .key_has_perm(serial, &same_uid, KeyPermission::READ)
                .unwrap()
        );
        assert!(
            !manager
                .key_has_perm(serial, &outsider, KeyPermission::READ)
                .unwrap()
        );
    }

    #[test]
    fn possessor_lane_requires_a_searchable_path_to_the_exact_key() {
        let owner = actor(11, 11, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "possessor-only".to_string(),
            vec![0],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.keys.get_mut(&serial).unwrap().perm =
            KeyPermissionMask::try_from_raw(0x0a00_0000).unwrap();
        manager.link_key_replace(root, serial).unwrap();

        assert!(manager.is_possessed(serial, &owner));
        assert!(
            manager
                .key_has_perm(serial, &owner, KeyPermission::READ)
                .unwrap()
        );

        manager.keys.get_mut(&serial).unwrap().perm =
            KeyPermissionMask::try_from_raw(0x0200_0000).unwrap();
        assert!(!manager.is_possessed(serial, &owner));
        assert!(
            !manager
                .key_has_perm(serial, &owner, KeyPermission::READ)
                .unwrap()
        );
    }

    #[test]
    fn keyring_cycles_are_rejected_and_traversal_depth_is_bounded() {
        let owner = actor(21, 21, 1000, 1000);
        let mut manager = KeyManager::new();

        let cycle_a = manager.create_keyring(
            "cycle-a".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let cycle_b = manager.create_keyring(
            "cycle-b".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        manager.link_key_replace(cycle_a, cycle_b).unwrap();
        assert_eq!(
            manager.link_key_replace(cycle_b, cycle_a),
            Err(LinuxError::EDEADLK.into())
        );

        let mut rings = Vec::new();
        for index in 0..=KEYRING_SEARCH_MAX_DEPTH + 1 {
            rings.push(manager.create_keyring(
                format!("ring-{index}"),
                owner.owner_uid(),
                owner.owner_gid(),
                thread_process_keyring_permissions(),
            ));
        }
        manager.thread_keyrings.insert(owner.thread_owner, rings[0]);
        for pair in rings.windows(2) {
            manager.link_key_replace(pair[0], pair[1]).unwrap();
        }
        let beyond_search_depth = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "beyond-search-depth".to_string(),
            vec![0],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager
            .link_key_replace(*rings.last().unwrap(), beyond_search_depth)
            .unwrap();

        assert!(manager.is_possessed(*rings.last().unwrap(), &owner));
        assert!(!manager.is_possessed(beyond_search_depth, &owner));
        assert!(
            manager
                .search_keyring(
                    rings[0],
                    &owner,
                    KeyTypeKind::User,
                    "beyond-search-depth",
                    &mut BTreeSet::new(),
                )
                .unwrap()
                .is_none()
        );
        let destination = manager.create_keyring(
            "depth-destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        assert!(manager.link_key_replace(destination, rings[0]) == Err(LinuxError::ELOOP.into()));
    }

    #[test]
    fn link_depth_counts_only_nested_keyrings() {
        let owner = actor(26, 26, 1000, 1000);
        let mut manager = KeyManager::new();
        let mut rings = Vec::new();
        for index in 0..=KEYRING_SEARCH_MAX_DEPTH {
            rings.push(manager.create_keyring(
                format!("bounded-ring-{index}"),
                owner.owner_uid(),
                owner.owner_gid(),
                thread_process_keyring_permissions(),
            ));
        }
        for pair in rings.windows(2) {
            manager.link_key_replace(pair[0], pair[1]).unwrap();
        }
        let ordinary_key = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "ordinary-leaf".to_string(),
            vec![0],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager
            .link_key_replace(*rings.last().unwrap(), ordinary_key)
            .unwrap();

        let valid_destination = manager.create_keyring(
            "valid-depth-destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        manager
            .link_key_replace(valid_destination, rings[0])
            .unwrap();

        let nested_keyring = manager.create_keyring(
            "one-keyring-too-deep".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        manager
            .link_key_replace(*rings.last().unwrap(), nested_keyring)
            .unwrap();
        let invalid_destination = manager.create_keyring(
            "invalid-depth-destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        assert_eq!(
            manager.link_key_replace(invalid_destination, rings[0]),
            Err(LinuxError::ELOOP.into())
        );
    }

    #[test]
    fn an_existing_request_result_links_only_to_an_explicit_destination() {
        let owner = actor(31, 31, 1000, 1000);
        let mut manager = KeyManager::new();
        let search_root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let destination = manager.create_keyring(
            "request-destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "existing".to_string(),
            vec![0],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(search_root, destination).unwrap();
        manager.link_key_replace(search_root, serial).unwrap();

        manager
            .link_existing_request_result(0, serial, &owner)
            .unwrap();
        assert!(!manager.keys[&destination].links.contains(&serial));

        manager
            .link_existing_request_result(destination, serial, &owner)
            .unwrap();
        assert!(manager.keys[&destination].links.contains(&serial));
    }

    #[test]
    fn logon_payload_is_not_userspace_readable_even_when_possessed() {
        let owner = actor(41, 41, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::Logon,
            "service:secret".to_string(),
            Vec::from([1, 2, 3]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, serial).unwrap();

        assert!(!KeyTypeKind::Logon.userspace_readable());
        assert!(manager.is_possessed(serial, &owner));
        assert!(
            !manager
                .key_has_perm(serial, &owner, KeyPermission::READ)
                .unwrap()
        );
    }

    #[test]
    fn visible_tid_rebinding_keeps_the_immutable_thread_owner() {
        let first = actor(51, 51, 1000, 1000);
        let mut manager = KeyManager::new();
        let thread_ring = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &first, true)
            .unwrap();

        let mut rebound = first.clone();
        rebound.tid = 1;
        assert_eq!(
            manager.special_keyring(KEY_SPEC_THREAD_KEYRING, &rebound, false),
            Ok(thread_ring)
        );
        assert!(manager.is_possessed(thread_ring, &rebound));
    }

    #[test]
    fn restriction_and_failed_move_preserve_the_source_link() {
        let owner = actor(61, 61, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let source = manager.create_keyring(
            "source".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let destination = manager.create_keyring(
            "destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "movable".to_string(),
            vec![0],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, source).unwrap();
        manager.link_key_replace(root, destination).unwrap();
        manager.link_key_replace(source, serial).unwrap();
        manager.keys.get_mut(&destination).unwrap().restricted = true;

        assert_eq!(
            manager.move_key_link(source, destination, serial, false),
            Err(AxError::OperationNotPermitted)
        );
        assert!(manager.keys[&source].links.contains(&serial));
        assert!(!manager.keys[&destination].links.contains(&serial));
    }

    #[test]
    fn move_replaces_an_existing_match_without_growing_the_destination() {
        let owner = actor(66, 66, 1000, 1000);
        let mut manager = KeyManager::new();
        let source = manager.create_keyring(
            "source".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let destination = manager.create_keyring(
            "destination".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let moved = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "same-description".to_string(),
            Vec::from([1]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        let replaced = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "same-description".to_string(),
            Vec::from([2]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(source, moved).unwrap();
        manager.link_key_replace(destination, replaced).unwrap();
        let destination_len = manager.keys[&destination].links.len();

        manager
            .move_key_link(source, destination, moved, false)
            .unwrap();

        assert!(!manager.keys[&source].links.contains(&moved));
        assert_eq!(manager.keys[&destination].links.len(), destination_len);
        assert!(manager.keys[&destination].links.contains(&moved));
        assert!(!manager.keys[&destination].links.contains(&replaced));
    }

    #[test]
    fn payload_quota_failure_is_non_mutating() {
        let owner = actor(71, 71, 1000, 1000);
        let mut manager = KeyManager::new();
        let fixed_charge = "quota".len() + 1;
        let original_len = KEY_MAXBYTES_DEFAULT - fixed_charge - 1;
        let original = vec![0x5a; original_len];
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "quota".to_string(),
            original.clone(),
            owner.owner_uid(),
            owner.owner_gid(),
        ));

        assert_eq!(
            manager.replace_payload(serial, vec![0xa5; original_len + 2]),
            Err(LinuxError::EDQUOT.into())
        );
        assert_eq!(manager.keys[&serial].payload, original);
    }

    #[test]
    fn big_key_update_checks_transient_raw_payload_quota() {
        let owner = actor(72, 72, 1000, 1000);
        let mut manager = KeyManager::new();
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::BigKey,
            "big".to_string(),
            vec![0x11],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        let current = manager.owners.usage(owner.owner_uid()).bytes;
        let filler_fixed_charge = "filler".len() + 1;
        let spare = 32;
        let filler_len = KEY_MAXBYTES_DEFAULT - current - filler_fixed_charge - spare;
        manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "filler".to_string(),
            vec![0; filler_len],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        let before = manager.owners.usage(owner.owner_uid());

        assert_eq!(
            manager.replace_payload(serial, vec![0x22; 64]),
            Err(LinuxError::EDQUOT.into())
        );
        assert_eq!(manager.keys[&serial].payload, vec![0x11]);
        assert_eq!(manager.owners.usage(owner.owner_uid()), before);
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn link_quota_failure_is_non_mutating() {
        let owner = actor(73, 73, 1000, 1000);
        let mut manager = KeyManager::new();
        let destination = manager.create_keyring(
            "quota-ring".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "link-target".to_string(),
            vec![0],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        let filler_fixed_charge = "filler".len() + 1;
        let current_charge = manager.owners.usage(owner.owner_uid()).bytes;
        let filler_len =
            KEY_MAXBYTES_DEFAULT - current_charge - filler_fixed_charge - (KEY_LINK_CHARGE - 1);
        manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "filler".to_string(),
            vec![0; filler_len],
            owner.owner_uid(),
            owner.owner_gid(),
        ));

        assert_eq!(
            manager.link_key_replace(destination, serial),
            Err(LinuxError::EDQUOT.into())
        );
        assert!(manager.keys[&destination].links.is_empty());
    }

    #[test]
    fn abi_quota_and_resident_budget_use_distinct_charges() {
        let owner = actor(74, 74, 1000, 1000);
        let mut description = String::with_capacity(128);
        description.push_str("large");
        let mut payload = Vec::with_capacity(4096);
        payload.resize(1024, 0x5a);
        let key = Key::positive(
            KeyTypeKind::BigKey,
            description,
            payload,
            owner.owner_uid(),
            owner.owner_gid(),
        )
        .unwrap();

        assert_eq!(KeyTypeKind::BigKey.payload_limit(), 1 << 20);
        assert_eq!(
            key.abi_charge,
            AbiQuotaCharge {
                keys: 1,
                bytes: "large".len() + 1 + BIG_KEY_ABI_PAYLOAD_CHARGE,
            }
        );
        assert_eq!(
            key.resident_charge.bytes,
            size_of::<Key>() + KEY_RESIDENT_NODE_OVERHEAD + 128 + 4096
        );
        assert!(key.resident_charge.bytes > key.abi_charge.bytes);
    }

    #[test]
    fn quota_overrun_only_relaxes_creation_admission() {
        let owner = actor(75, 75, 1000, 1000);
        let mut manager = KeyManager::new();
        let oversized = "x".repeat(KEY_MAXBYTES_DEFAULT + 1);
        let ring = manager
            .try_insert_key(
                Key::keyring(
                    oversized,
                    owner.owner_uid(),
                    owner.owner_gid(),
                    thread_process_keyring_permissions(),
                )
                .unwrap(),
                QuotaAdmission::AllowOverrun,
            )
            .unwrap();
        let target = manager
            .try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    "exempt-target".to_string(),
                    vec![0],
                    owner.owner_uid(),
                    owner.owner_gid(),
                )
                .unwrap(),
                QuotaAdmission::Exempt,
            )
            .unwrap();
        let usage_before = manager.owners.usage(owner.owner_uid());
        let budget_before = manager.budget.used;

        assert!(usage_before.bytes > KEY_MAXBYTES_DEFAULT);
        assert_eq!(
            manager.link_key_replace(ring, target),
            Err(LinuxError::EDQUOT.into())
        );
        assert_eq!(manager.owners.usage(owner.owner_uid()), usage_before);
        assert_eq!(manager.budget.used, budget_before);
        assert!(manager.keys[&ring].links.is_empty());
        assert_eq!(manager.keys[&target].link_refs, 0);
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn quota_refunds_succeed_while_usage_remains_over_limit() {
        let owner = actor(76, 76, 1000, 1000);
        let mut manager = KeyManager::new();
        let oversized = manager
            .try_insert_key(
                Key::keyring(
                    "x".repeat(KEY_MAXBYTES_DEFAULT + 1),
                    owner.owner_uid(),
                    owner.owner_gid(),
                    thread_process_keyring_permissions(),
                )
                .unwrap(),
                QuotaAdmission::AllowOverrun,
            )
            .unwrap();
        let removable = manager
            .try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    "removable".to_string(),
                    vec![1],
                    owner.owner_uid(),
                    owner.owner_gid(),
                )
                .unwrap(),
                QuotaAdmission::AllowOverrun,
            )
            .unwrap();

        manager.discard_new_key(removable).unwrap();
        assert!(manager.keys.contains_key(&oversized));
        assert!(!manager.keys.contains_key(&removable));
        assert!(manager.owners.usage(owner.owner_uid()).bytes > KEY_MAXBYTES_DEFAULT);
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn manager_budget_failure_preserves_serial_and_ledgers() {
        let owner = actor(77, 77, 1000, 1000);
        let mut manager = KeyManager::with_budget(ManagerBudgetLimits {
            objects: 0,
            bytes: usize::MAX,
            link_bytes: usize::MAX,
        });

        assert_eq!(
            manager.try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    "budget".to_string(),
                    vec![1, 2, 3],
                    owner.owner_uid(),
                    owner.owner_gid(),
                )
                .unwrap(),
                QuotaAdmission::Enforced,
            ),
            Err(AxError::NoMemory)
        );
        assert_eq!(manager.next_serial, 1);
        assert!(manager.keys.is_empty());
        assert!(manager.owners.usage.is_empty());
        assert_eq!(manager.budget.used, ManagerBudgetUsage::default());
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn manager_budget_accounts_staged_link_growth_at_peak() {
        let limits = ManagerBudgetLimits {
            objects: usize::MAX,
            bytes: usize::MAX,
            link_bytes: 40,
        };
        let mut budget = ManagerBudget::new(limits);
        budget.used.link_bytes = 16;

        assert_eq!(
            budget.plan_replace(
                ResidentCharge {
                    objects: 0,
                    bytes: 0,
                    link_bytes: 16,
                },
                ResidentCharge {
                    objects: 0,
                    bytes: 0,
                    link_bytes: 32,
                },
            ),
            Ok(ManagerBudgetUsage {
                objects: 0,
                bytes: 0,
                link_bytes: 32,
            })
        );
        assert_eq!(
            budget.check_transient(ResidentCharge {
                objects: 0,
                bytes: 0,
                link_bytes: 32,
            }),
            Err(AxError::NoMemory)
        );
        assert_eq!(budget.used.link_bytes, 16);
    }

    #[test]
    fn key_user_records_separate_live_and_quota_counts() {
        let owner = actor(77, 77, 1000, 1000);
        let mut manager = KeyManager::new();
        manager
            .try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    "charged".to_string(),
                    vec![1],
                    owner.owner_uid(),
                    owner.owner_gid(),
                )
                .unwrap(),
                QuotaAdmission::Enforced,
            )
            .unwrap();
        manager
            .try_insert_key(
                Key::keyring(
                    "exempt".to_string(),
                    owner.owner_uid(),
                    owner.owner_gid(),
                    persistent_keyring_permissions(),
                )
                .unwrap(),
                QuotaAdmission::Exempt,
            )
            .unwrap();

        let records = manager.key_user_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].uid, 1000);
        assert_eq!(records[0].usage, 2);
        assert_eq!(records[0].keys, 2);
        assert_eq!(records[0].instantiated_keys, 2);
        assert_eq!(records[0].quota_keys, 1);
        assert_eq!(records[0].quota_bytes, "charged".len() + 1 + 1);
    }

    #[test]
    fn owner_quota_is_per_kuid_not_a_global_object_limit() {
        let nonroot = actor(78, 78, 1000, 1000);
        let root = actor(79, 79, 0, 0);
        let mut manager = KeyManager::new();

        for index in 0..KEY_MAXKEYS_DEFAULT {
            manager
                .try_insert_key(
                    Key::positive(
                        KeyTypeKind::User,
                        format!("nonroot-{index}"),
                        vec![0],
                        nonroot.owner_uid(),
                        nonroot.owner_gid(),
                    )
                    .unwrap(),
                    QuotaAdmission::Enforced,
                )
                .unwrap();
        }
        assert_eq!(
            manager.try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    "nonroot-over-limit".to_string(),
                    vec![1],
                    nonroot.owner_uid(),
                    nonroot.owner_gid(),
                )
                .unwrap(),
                QuotaAdmission::Enforced,
            ),
            Err(LinuxError::EDQUOT.into())
        );
        for description in ["root-a", "root-b"] {
            manager
                .try_insert_key(
                    Key::positive(
                        KeyTypeKind::User,
                        description.to_string(),
                        vec![0],
                        root.owner_uid(),
                        root.owner_gid(),
                    )
                    .unwrap(),
                    QuotaAdmission::Enforced,
                )
                .unwrap();
        }
        assert_eq!(
            manager.owners.usage(nonroot.owner_uid()).keys,
            KEY_MAXKEYS_DEFAULT
        );
        assert_eq!(manager.owners.usage(root.owner_uid()).keys, 2);
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn unlink_collects_only_after_the_last_reference() {
        let owner = actor(80, 80, 1000, 1000);
        let mut manager = KeyManager::new();
        let first = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let second = manager
            .special_keyring(KEY_SPEC_PROCESS_KEYRING, &owner, true)
            .unwrap();
        let serial = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "shared".to_string(),
            vec![0x5a; 32],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(first, serial).unwrap();
        manager.link_key_replace(second, serial).unwrap();
        assert_eq!(manager.keys[&serial].link_refs, 2);

        manager.unlink_key_from_keyring(first, serial).unwrap();
        assert!(manager.keys.contains_key(&serial));
        assert_eq!(manager.keys[&serial].link_refs, 1);
        manager.unlink_key_from_keyring(second, serial).unwrap();
        assert!(!manager.keys.contains_key(&serial));
        assert_accounting_consistent(&manager);

        assert!(manager.keys[&first].links.capacity() != 0);
        assert!(manager.budget.used.link_bytes != 0);
        manager.clear_keyring_links(first).unwrap();
        manager.clear_keyring_links(second).unwrap();
        assert_eq!(manager.keys[&first].links.capacity(), 0);
        assert_eq!(manager.keys[&second].links.capacity(), 0);
        assert_eq!(manager.budget.used.link_bytes, 0);
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn iterative_gc_retires_a_deep_keyring_chain_without_stack_growth() {
        const DEPTH: usize = 2_048;

        let owner = actor(80, 80, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .try_insert_key(
                Key::keyring(
                    "r0".to_string(),
                    owner.owner_uid(),
                    owner.owner_gid(),
                    thread_process_keyring_permissions(),
                )
                .unwrap(),
                QuotaAdmission::Exempt,
            )
            .unwrap();
        manager.install_root(RootSource::Thread(80), root).unwrap();
        let mut parent = root;
        for index in 1..DEPTH {
            let child = manager
                .try_insert_key(
                    Key::keyring(
                        format!("r{index}"),
                        owner.owner_uid(),
                        owner.owner_gid(),
                        thread_process_keyring_permissions(),
                    )
                    .unwrap(),
                    QuotaAdmission::Exempt,
                )
                .unwrap();
            manager.link_key_replace(parent, child).unwrap();
            parent = child;
        }

        assert_eq!(manager.thread_keyrings.remove(&80), Some(root));
        manager.release_root_ref(root).unwrap();
        assert!(manager.keys.is_empty());
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn invalidate_validates_root_accounting_before_detaching_roots() {
        let owner = actor(81, 81, 1000, 1000);
        let mut manager = KeyManager::new();
        let ring = manager.insert_key(Key::keyring(
            "multi-root".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        ));
        manager.install_root(RootSource::Thread(81), ring).unwrap();
        manager.install_root(RootSource::Session(81), ring).unwrap();
        let parent = manager.insert_key(Key::keyring(
            "parent".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        ));
        manager
            .install_root(RootSource::Process(81), parent)
            .unwrap();
        manager.link_key_replace(parent, ring).unwrap();
        manager.keys.get_mut(&ring).unwrap().root_refs = 1;
        let thread_roots = manager.thread_keyrings.clone();
        let session_roots = manager.session_keyrings.clone();
        let parent_links = manager.keys[&parent].links.clone();

        assert_eq!(manager.remove_key_everywhere(ring), Err(AxError::BadState));
        assert_eq!(manager.thread_keyrings, thread_roots);
        assert_eq!(manager.session_keyrings, session_roots);
        assert_eq!(manager.keys[&parent].links, parent_links);
        assert!(manager.keys.contains_key(&ring));
    }

    #[test]
    fn revoke_releases_payload_links_and_recursive_children() {
        let owner = actor(82, 82, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let ring = manager.insert_key(Key::keyring(
            "revoked-ring".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        ));
        let child = manager.insert_key(Key::positive(
            KeyTypeKind::Logon,
            "service:secret".to_string(),
            vec![0xa5; 64],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, ring).unwrap();
        manager.link_key_replace(ring, child).unwrap();
        let ring_base_bytes = manager.keys[&ring].abi_charge.bytes - KEY_LINK_CHARGE;

        manager.revoke_key(ring).unwrap();
        assert_eq!(manager.keys[&ring].state, KeyState::Revoked);
        assert!(manager.keys[&ring].links.is_empty());
        assert_eq!(manager.keys[&ring].abi_charge.bytes, ring_base_bytes);
        assert!(!manager.keys.contains_key(&child));
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn same_owner_move_reserves_destination_quota_before_source_refund() {
        let owner = actor(84, 84, 1000, 1000);
        let mut manager = KeyManager::new();
        let source = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let destination = manager
            .special_keyring(KEY_SPEC_PROCESS_KEYRING, &owner, true)
            .unwrap();
        let moved = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "moved".to_string(),
            vec![0],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(source, moved).unwrap();

        let current = manager.owners.usage(owner.owner_uid()).bytes;
        let filler_base = "move-filler".len() + 1;
        let filler_len = KEY_MAXBYTES_DEFAULT - current - filler_base;
        manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "move-filler".to_string(),
            vec![0; filler_len],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        assert_eq!(
            manager.owners.usage(owner.owner_uid()).bytes,
            KEY_MAXBYTES_DEFAULT
        );
        let owners_before = manager.owners.usage.clone();
        let budget_before = manager.budget.used;

        assert_eq!(
            manager.move_key_link(source, destination, moved, false),
            Err(LinuxError::EDQUOT.into())
        );
        assert!(manager.keys[&source].links.contains(&moved));
        assert!(!manager.keys[&destination].links.contains(&moved));
        assert_eq!(manager.keys[&moved].link_refs, 1);
        assert_eq!(manager.owners.usage, owners_before);
        assert_eq!(manager.budget.used, budget_before);
        assert_accounting_consistent(&manager);
    }

    #[test]
    fn owner_quota_overflow_is_edquot() {
        let owner = actor(85, 85, 1000, 1000);
        let uid = owner.owner_uid();
        let mut ledger = OwnerLedger::default();
        ledger.usage.insert(
            uid,
            OwnerUsage {
                keys: usize::MAX,
                bytes: usize::MAX,
            },
        );
        assert_eq!(
            ledger.plan_replace(
                uid,
                QuotaAdmission::Enforced,
                AbiQuotaCharge::ZERO,
                AbiQuotaCharge { keys: 1, bytes: 1 },
            ),
            Err(LinuxError::EDQUOT.into())
        );
    }

    #[test]
    fn serial_wrap_finds_a_hole_and_never_overwrites_a_key() {
        let owner = actor(76, 76, 1000, 1000);
        let mut manager = KeyManager::new();
        let first = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "first".to_string(),
            vec![0],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        assert_eq!(first, 1);

        manager.next_serial = i32::MAX;
        let last = manager
            .try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    "last".to_string(),
                    vec![0],
                    owner.owner_uid(),
                    owner.owner_gid(),
                )
                .unwrap(),
                QuotaAdmission::AllowOverrun,
            )
            .unwrap();
        assert_eq!(last, i32::MAX);

        let wrapped = manager
            .try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    "wrapped".to_string(),
                    vec![0],
                    owner.owner_uid(),
                    owner.owner_gid(),
                )
                .unwrap(),
                QuotaAdmission::AllowOverrun,
            )
            .unwrap();
        assert_eq!(wrapped, 2);
        assert_eq!(manager.keys[&first].description, "first");
        assert_eq!(manager.keys[&last].description, "last");
        assert_eq!(manager.keys[&wrapped].description, "wrapped");
    }

    #[test]
    fn search_is_breadth_first_across_keyring_links() {
        let owner = actor(81, 81, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let branch = manager.create_keyring(
            "deep-branch".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let shallow_branch = manager.create_keyring(
            "shallow-branch".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let deeper = manager.create_keyring(
            "deeper".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let deep_match = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "target".to_string(),
            Vec::from([1]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        let shallow_match = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "target".to_string(),
            Vec::from([2]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, branch).unwrap();
        manager.link_key_replace(root, shallow_branch).unwrap();
        manager.link_key_replace(branch, deeper).unwrap();
        manager.link_key_replace(deeper, deep_match).unwrap();
        manager
            .link_key_replace(shallow_branch, shallow_match)
            .unwrap();

        assert_eq!(
            manager
                .search_keyring(
                    root,
                    &owner,
                    KeyTypeKind::User,
                    "target",
                    &mut BTreeSet::new(),
                )
                .unwrap()
                .map(|key| key.serial),
            Some(shallow_match)
        );
    }

    #[test]
    fn revoked_nested_keyring_does_not_poison_an_unrelated_miss() {
        let owner = actor(91, 91, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let revoked_child = manager.create_keyring(
            "revoked-child".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        manager.link_key_replace(root, revoked_child).unwrap();
        manager.keys.get_mut(&revoked_child).unwrap().state = KeyState::Revoked;

        assert_eq!(
            manager.search_keyring(
                root,
                &owner,
                KeyTypeKind::User,
                "missing",
                &mut BTreeSet::new(),
            ),
            Ok(None)
        );
    }

    #[test]
    fn inaccessible_match_reports_eacces_but_does_not_hide_a_later_match() {
        let owner = actor(101, 101, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let denied = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "permission-target".to_string(),
            Vec::from([1]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.keys.get_mut(&denied).unwrap().perm = KeyPermissionMask::try_from_raw(0).unwrap();
        manager.link_key_replace(root, denied).unwrap();

        assert_eq!(
            manager.search_keyring(
                root,
                &owner,
                KeyTypeKind::User,
                "permission-target",
                &mut BTreeSet::new(),
            ),
            Err(LinuxError::EACCES.into())
        );

        let branch = manager.create_keyring(
            "valid-branch".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        );
        let allowed = manager.insert_key(Key::positive(
            KeyTypeKind::User,
            "permission-target".to_string(),
            Vec::from([2]),
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        manager.link_key_replace(root, branch).unwrap();
        manager.link_key_replace(branch, allowed).unwrap();

        assert_eq!(
            manager.search_keyring(
                root,
                &owner,
                KeyTypeKind::User,
                "permission-target",
                &mut BTreeSet::new(),
            ),
            Ok(Some(ResolvedKey::possessed(allowed)))
        );
    }

    #[test]
    fn basal_keyring_is_considered_before_its_children() {
        let owner = actor(111, 111, 1000, 1000);
        let mut manager = KeyManager::new();
        let root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();

        assert_eq!(
            manager.search_keyring(
                ResolvedKey::possessed(root),
                &owner,
                KeyTypeKind::Keyring,
                "_tid.111",
                &mut BTreeSet::new(),
            ),
            Ok(Some(ResolvedKey::possessed(root)))
        );
    }

    #[test]
    fn direct_special_lookup_does_not_grant_numeric_possession() {
        let mut shifted = actor(121, 121, 1000, 1000);
        let groups = GroupInfo::try_new(Vec::new()).unwrap();
        shifted.dac = DacCredentialView::new(
            Kuid::INITIAL_ROOT,
            Kgid::INITIAL_ROOT,
            groups,
            [0; CAPABILITY_WORDS],
            true,
        );
        let mut manager = KeyManager::new();
        let direct = manager
            .resolve_keyring(KEY_SPEC_USER_KEYRING, &shifted, true)
            .unwrap();

        assert!(
            manager
                .key_has_perm(direct, &shifted, KeyPermission::WRITE)
                .unwrap()
        );
        assert!(
            !manager
                .key_has_perm(
                    ResolvedKey::numeric(direct.serial),
                    &shifted,
                    KeyPermission::WRITE,
                )
                .unwrap()
        );
    }

    #[test]
    fn explicit_unpossessed_search_does_not_borrow_an_independent_possession() {
        let owner = actor(131, 131, 1000, 1000);
        let mut manager = KeyManager::new();
        let credential_root = manager
            .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
            .unwrap();
        let independently_possessed = manager.create_keyring(
            "independently-possessed".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            permission_mask(KeyPermission::SEARCH, KeyPermission::VIEW),
        );
        manager
            .link_key_replace(credential_root, independently_possessed)
            .unwrap();

        let explicit_source = manager.create_keyring(
            "explicit-source".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            permission_mask(KeyPermission::SEARCH, KeyPermission::SEARCH),
        );
        manager
            .link_key_replace(explicit_source, independently_possessed)
            .unwrap();

        assert_eq!(
            manager.search_keyring(
                ResolvedKey::with_possession(explicit_source, false),
                &owner,
                KeyTypeKind::Keyring,
                "independently-possessed",
                &mut BTreeSet::new(),
            ),
            Err(LinuxError::EACCES.into())
        );
    }

    #[test]
    fn encrypted_key_type_is_not_advertised_without_a_real_type_backend() {
        assert_eq!(KeyTypeKind::from_name("encrypted"), None);
    }
}
