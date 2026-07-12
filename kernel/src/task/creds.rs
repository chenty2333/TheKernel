use alloc::{sync::Arc, vec::Vec};
use core::mem;

use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;
use linux_raw_sys::general::{CAP_LAST_CAP, NGROUPS_MAX};

use super::{
    idmap::{Kgid, Kuid},
    process::UserNamespace,
};

#[cfg(not(test))]
type CredentialUpdateMutex<T> = axsync::Mutex<T>;
#[cfg(test)]
type CredentialUpdateMutex<T> = spin::Mutex<T>;

#[cfg(not(test))]
type CredentialUpdateGuard<'a, T> = axsync::MutexGuard<'a, T>;
#[cfg(test)]
type CredentialUpdateGuard<'a, T> = spin::MutexGuard<'a, T>;

pub(in crate::task) const CAPABILITY_WORDS: usize = 2;

const fn capability_valid_mask_word(word: usize) -> u32 {
    let first_cap = word as u32 * u32::BITS;
    if CAP_LAST_CAP < first_cap {
        return 0;
    }

    let last_bit = CAP_LAST_CAP - first_cap;
    if last_bit >= u32::BITS - 1 {
        u32::MAX
    } else {
        (1u32 << (last_bit + 1)) - 1
    }
}

const CAPABILITY_VALID_MASK: [u32; CAPABILITY_WORDS] =
    [capability_valid_mask_word(0), capability_valid_mask_word(1)];

pub(in crate::task) const SECBIT_NO_SETUID_FIXUP: u32 = 1 << 2;
pub(in crate::task) const SECBIT_KEEP_CAPS: u32 = 1 << 4;
pub(in crate::task) const SECBIT_KEEP_CAPS_LOCKED: u32 = 1 << 5;
pub(in crate::task) const SECBIT_NO_CAP_AMBIENT_RAISE: u32 = 1 << 6;
pub(in crate::task) const SECURE_ALL_BITS: u32 =
    (1 << 0) | SECBIT_NO_SETUID_FIXUP | SECBIT_KEEP_CAPS | SECBIT_NO_CAP_AMBIENT_RAISE;
pub(in crate::task) const SECURE_ALL_LOCKS: u32 = SECURE_ALL_BITS << 1;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct Credentials {
    pub(crate) ruid: Kuid,
    pub(crate) euid: Kuid,
    pub(crate) suid: Kuid,
    pub(crate) fsuid: Kuid,
    pub(crate) rgid: Kgid,
    pub(crate) egid: Kgid,
    pub(crate) sgid: Kgid,
    pub(crate) fsgid: Kgid,
}

impl Credentials {
    pub(crate) const fn root() -> Self {
        Self {
            ruid: Kuid::INITIAL_ROOT,
            euid: Kuid::INITIAL_ROOT,
            suid: Kuid::INITIAL_ROOT,
            fsuid: Kuid::INITIAL_ROOT,
            rgid: Kgid::INITIAL_ROOT,
            egid: Kgid::INITIAL_ROOT,
            sgid: Kgid::INITIAL_ROOT,
            fsgid: Kgid::INITIAL_ROOT,
        }
    }
}

/// Immutable, sorted supplementary-group storage shared by credential snapshots.
///
/// The caller supplies an owned vector so sorting and deduplication need no
/// allocation. Only the final `Arc` allocation is fallible. This also lets DAC
/// views share group storage instead of cloning a `Vec` while resolving paths.
#[derive(Debug)]
pub(crate) struct GroupInfo {
    groups: Vec<Kgid>,
}

impl GroupInfo {
    pub(crate) fn try_new(mut groups: Vec<Kgid>) -> AxResult<Arc<Self>> {
        if groups.len() > NGROUPS_MAX as usize {
            return Err(AxError::InvalidInput);
        }
        groups.sort_unstable();
        groups.dedup();
        Arc::try_new(Self { groups }).map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn as_slice(&self) -> &[Kgid] {
        &self.groups
    }

    pub(crate) fn contains(&self, gid: Kgid) -> bool {
        self.groups.binary_search(&gid).is_ok()
    }
}

/// The credential fields used by one discretionary-access-control operation.
///
/// This keeps a path operation from repeatedly sampling process state and
/// shares its immutable group storage without a fallible per-operation clone.
#[derive(Debug, Clone)]
pub(crate) struct DacCredentialView {
    uid: Kuid,
    gid: Kgid,
    groups: Arc<GroupInfo>,
    effective: [u32; CAPABILITY_WORDS],
    capabilities_apply_to_initial_user_ns: bool,
}

impl DacCredentialView {
    pub(crate) fn new(
        uid: Kuid,
        gid: Kgid,
        groups: Arc<GroupInfo>,
        effective: [u32; CAPABILITY_WORDS],
        capabilities_apply_to_initial_user_ns: bool,
    ) -> Self {
        Self {
            uid,
            gid,
            groups,
            effective,
            capabilities_apply_to_initial_user_ns,
        }
    }

    pub(crate) fn uid(&self) -> Kuid {
        self.uid
    }

    pub(crate) fn gid(&self) -> Kgid {
        self.gid
    }

    pub(crate) fn supplementary_groups(&self) -> &[Kgid] {
        self.groups.as_slice()
    }

    pub(crate) fn has_capability(&self, cap: u32) -> bool {
        if !self.capabilities_apply_to_initial_user_ns {
            return false;
        }
        let Some((word, mask)) = CapabilityState::cap_mask(cap) else {
            return false;
        };
        self.effective[word] & mask != 0
    }

    #[cfg(test)]
    pub(crate) fn selected_capability(&self, cap: u32) -> bool {
        let Some((word, mask)) = CapabilityState::cap_mask(cap) else {
            return false;
        };
        self.effective[word] & mask != 0
    }

    #[cfg(test)]
    pub(crate) fn try_for_test(
        uid: u32,
        gid: u32,
        source_groups: &[u32],
        effective: [u32; CAPABILITY_WORDS],
    ) -> AxResult<Self> {
        let uid = Kuid::from_raw(uid).ok_or(AxError::InvalidInput)?;
        let gid = Kgid::from_raw(gid).ok_or(AxError::InvalidInput)?;
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(source_groups.len())
            .map_err(|_| AxError::NoMemory)?;
        for group in source_groups {
            groups.push(Kgid::from_raw(*group).ok_or(AxError::InvalidInput)?);
        }
        Ok(Self::new(
            uid,
            gid,
            GroupInfo::try_new(groups)?,
            effective,
            true,
        ))
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct CapabilityState {
    pub(crate) effective: [u32; CAPABILITY_WORDS],
    pub(crate) permitted: [u32; CAPABILITY_WORDS],
    pub(crate) inheritable: [u32; CAPABILITY_WORDS],
    pub(crate) bounding: [u32; CAPABILITY_WORDS],
    pub(crate) ambient: [u32; CAPABILITY_WORDS],
    pub(crate) securebits: u32,
}

impl CapabilityState {
    pub(in crate::task) const fn full() -> Self {
        Self {
            effective: CAPABILITY_VALID_MASK,
            permitted: CAPABILITY_VALID_MASK,
            inheritable: [0; CAPABILITY_WORDS],
            bounding: CAPABILITY_VALID_MASK,
            ambient: [0; CAPABILITY_WORDS],
            securebits: 0,
        }
    }

    pub(in crate::task) fn cap_mask(cap: u32) -> Option<(usize, u32)> {
        if cap > CAP_LAST_CAP {
            return None;
        }
        let word = cap as usize / u32::BITS as usize;
        (word < CAPABILITY_WORDS).then_some((word, 1_u32 << (cap % u32::BITS)))
    }

    pub(crate) fn valid_mask(word: usize) -> u32 {
        CAPABILITY_VALID_MASK[word]
    }

    pub(crate) fn has_effective(self, cap: u32) -> bool {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return false;
        };
        self.effective[word] & mask != 0
    }

    pub(crate) fn bounding_contains(self, cap: u32) -> bool {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return false;
        };
        self.bounding[word] & mask != 0
    }

    pub(crate) fn ambient_contains(self, cap: u32) -> bool {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return false;
        };
        self.ambient[word] & mask != 0
    }

    pub(crate) fn raise_ambient(&mut self, cap: u32) -> AxResult<()> {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return Err(AxError::InvalidInput);
        };
        self.ambient[word] |= mask;
        Ok(())
    }

    pub(crate) fn lower_ambient(&mut self, cap: u32) -> AxResult<()> {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return Err(AxError::InvalidInput);
        };
        self.ambient[word] &= !mask;
        Ok(())
    }

    pub(crate) fn clear_ambient(&mut self) {
        self.ambient = [0; CAPABILITY_WORDS];
    }

    pub(crate) fn reconcile_ambient(&mut self) {
        for word in 0..CAPABILITY_WORDS {
            self.ambient[word] &= self.permitted[word] & self.inheritable[word];
        }
    }

    pub(crate) fn drop_bounding(&mut self, cap: u32) -> AxResult<()> {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return Err(AxError::InvalidInput);
        };
        self.bounding[word] &= !mask;
        Ok(())
    }
}

/// One immutable Linux security identity snapshot.
///
/// Every committed field is private and can only be changed by building a new
/// object through [`CredentialUpdate`]. Readers retain an `Arc`, so IDs,
/// groups, capabilities, securebits, `no_new_privs`, and the user namespace
/// always come from the same publication.
pub(crate) struct Cred {
    ids: Credentials,
    groups: Arc<GroupInfo>,
    caps: CapabilityState,
    no_new_privs: bool,
    user_ns: Arc<UserNamespace>,
}

impl Cred {
    pub(crate) fn try_root(user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let mut groups = Vec::new();
        groups.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        groups.push(Kgid::INITIAL_ROOT);
        let groups = GroupInfo::try_new(groups)?;
        Arc::try_new(Self {
            ids: Credentials::root(),
            groups,
            caps: CapabilityState::full(),
            no_new_privs: false,
            user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn ids(&self) -> Credentials {
        self.ids
    }

    /// Creates the unpublished credential installed when a clone child enters
    /// a newly created user namespace. Linux keeps kernel-global IDs and group
    /// membership but resets namespace-relative capability state to a full,
    /// unbounded set with default securebits.
    pub(crate) fn try_with_user_ns(
        current: &Arc<Self>,
        user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            ids: current.ids,
            groups: current.groups.clone(),
            caps: CapabilityState::full(),
            no_new_privs: current.no_new_privs,
            user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn groups(&self) -> &Arc<GroupInfo> {
        &self.groups
    }

    pub(crate) fn capabilities(&self) -> CapabilityState {
        self.caps
    }

    pub(crate) fn no_new_privs(&self) -> bool {
        self.no_new_privs
    }

    pub(crate) fn user_ns(&self) -> &Arc<UserNamespace> {
        &self.user_ns
    }

    /// Snapshot the filesystem identity and effective capabilities used by DAC.
    ///
    /// Keeping this on the immutable credential lets early boot and other
    /// explicitly credentialed kernel operations avoid pretending that a
    /// kernel task is a Linux userspace thread.
    pub(crate) fn fs_dac_credentials(&self) -> DacCredentialView {
        DacCredentialView::new(
            self.ids.fsuid,
            self.ids.fsgid,
            self.groups.clone(),
            self.caps.effective,
            self.user_ns.is_initial(),
        )
    }

    /// Returns whether this credential can use `cap` over a resource which has
    /// not yet been assigned an owning user namespace. Such legacy/global
    /// resources belong to the initial namespace; child-userns capability
    /// bits must never silently become host authority.
    pub(crate) fn has_effective_capability(&self, cap: u32) -> bool {
        self.user_ns.is_initial() && self.has_effective_capability_in_own_user_ns(cap)
    }

    /// Preserves legacy UID-0 authority for global objects which do not yet
    /// carry an owning user namespace. A child namespace may map its root to
    /// kernel ID 0, so the typed ID comparison alone is not sufficient.
    pub(crate) fn is_initial_root_euid(&self) -> bool {
        self.user_ns.is_initial() && self.ids.euid == Kuid::INITIAL_ROOT
    }

    /// Real-UID counterpart of [`Self::is_initial_root_euid`].
    pub(crate) fn is_initial_root_ruid(&self) -> bool {
        self.user_ns.is_initial() && self.ids.ruid == Kuid::INITIAL_ROOT
    }

    /// Tests the capability set relative to this credential's own user
    /// namespace. Callers must either operate on credential-local state or
    /// have already selected the target namespace through `ns_capable()`.
    pub(crate) fn has_effective_capability_in_own_user_ns(&self, cap: u32) -> bool {
        self.caps.has_effective(cap)
    }
}

/// Mutable, unpublished copy of a credential transaction.
pub(crate) struct CredBuilder {
    pub(in crate::task) ids: Credentials,
    pub(in crate::task) groups: Arc<GroupInfo>,
    pub(in crate::task) caps: CapabilityState,
    pub(in crate::task) no_new_privs: bool,
    pub(in crate::task) user_ns: Arc<UserNamespace>,
}

impl CredBuilder {
    fn from_cred(cred: &Cred) -> Self {
        Self {
            ids: cred.ids,
            groups: cred.groups.clone(),
            caps: cred.caps,
            no_new_privs: cred.no_new_privs,
            user_ns: cred.user_ns.clone(),
        }
    }

    fn validate_capabilities(&self) -> AxResult<()> {
        for word in 0..CAPABILITY_WORDS {
            let valid = CapabilityState::valid_mask(word);
            let all_sets = self.caps.effective[word]
                | self.caps.permitted[word]
                | self.caps.inheritable[word]
                | self.caps.bounding[word]
                | self.caps.ambient[word];
            if all_sets & !valid != 0
                || self.caps.effective[word] & !self.caps.permitted[word] != 0
                || self.caps.ambient[word]
                    & !(self.caps.permitted[word] & self.caps.inheritable[word])
                    != 0
            {
                return Err(AxError::OperationNotPermitted);
            }
        }
        Ok(())
    }

    fn validate_transition(&self, old: &Cred, exec_clears_keep_caps: bool) -> AxResult<()> {
        self.validate_capabilities()?;
        if old.no_new_privs && !self.no_new_privs {
            return Err(AxError::OperationNotPermitted);
        }
        let mut changed_locked_bits = ((old.caps.securebits & SECURE_ALL_LOCKS) >> 1)
            & (old.caps.securebits ^ self.caps.securebits);
        if exec_clears_keep_caps
            && old.caps.securebits & (SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED)
                == (SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED)
            && self.caps.securebits & SECBIT_KEEP_CAPS == 0
        {
            // Linux clears KEEP_CAPS on exec even when user changes to the
            // bit are locked. The lock itself remains set, so this narrow
            // transition cannot be reused to unlock securebits.
            changed_locked_bits &= !SECBIT_KEEP_CAPS;
        }
        let cleared_locks = old.caps.securebits & SECURE_ALL_LOCKS & !self.caps.securebits;
        if changed_locked_bits != 0
            || cleared_locks != 0
            || self.caps.securebits & !(SECURE_ALL_BITS | SECURE_ALL_LOCKS) != 0
        {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }

    fn try_build(self, old: &Cred, exec_clears_keep_caps: bool) -> AxResult<Arc<Cred>> {
        self.validate_transition(old, exec_clears_keep_caps)?;
        Arc::try_new(Cred {
            ids: self.ids,
            groups: self.groups,
            caps: self.caps,
            no_new_privs: self.no_new_privs,
            user_ns: self.user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }
}

/// The single publication point for one task-identity owner.
///
/// The slot has no `ProcessData` dependency and is embedded exactly once in
/// `Thread`. Clone and fork construct a new slot from one caller snapshot;
/// subsequent commits therefore affect only the owning task.
pub(crate) struct CredentialSlot {
    update: CredentialUpdateMutex<()>,
    current: SpinNoIrq<Arc<Cred>>,
}

impl CredentialSlot {
    pub(crate) fn new(initial: Arc<Cred>) -> Self {
        Self {
            update: CredentialUpdateMutex::new(()),
            current: SpinNoIrq::new(initial),
        }
    }

    pub(crate) fn try_new(initial: Arc<Cred>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self::new(initial)).map_err(|_| AxError::NoMemory)
    }

    /// Takes a coherent reader reference while holding only the short
    /// publication spin lock. No allocation or destruction occurs here.
    pub(crate) fn current(&self) -> Arc<Cred> {
        self.current.lock().clone()
    }

    /// Serializes a complete prepare/authorize/commit transaction.
    pub(crate) fn prepare(&self) -> CredentialUpdate<'_> {
        let guard = self.update.lock();
        let old = self.current();
        let builder = CredBuilder::from_cred(&old);
        CredentialUpdate {
            slot: self,
            guard,
            old,
            builder,
        }
    }
}

pub(crate) struct CredentialUpdate<'a> {
    slot: &'a CredentialSlot,
    guard: CredentialUpdateGuard<'a, ()>,
    old: Arc<Cred>,
    pub(in crate::task) builder: CredBuilder,
}

impl<'a> CredentialUpdate<'a> {
    pub(crate) fn old(&self) -> &Cred {
        &self.old
    }

    /// Finalizes all invariants and performs the only fallible allocation for
    /// the replacement object. Dropping the returned value aborts cleanly.
    fn finish_inner(self, exec_clears_keep_caps: bool) -> AxResult<PreparedCred<'a>> {
        let CredentialUpdate {
            slot,
            guard,
            old,
            builder,
        } = self;
        let proposed = builder.try_build(&old, exec_clears_keep_caps)?;
        Ok(PreparedCred {
            slot,
            guard,
            old,
            proposed,
        })
    }

    pub(crate) fn finish(self) -> AxResult<PreparedCred<'a>> {
        self.finish_inner(false)
    }

    pub(in crate::task) fn finish_exec_keep_caps_clear(self) -> AxResult<PreparedCred<'a>> {
        self.finish_inner(true)
    }
}

/// A fully built credential that has not yet become observable.
pub(crate) struct PreparedCred<'a> {
    slot: &'a CredentialSlot,
    guard: CredentialUpdateGuard<'a, ()>,
    old: Arc<Cred>,
    proposed: Arc<Cred>,
}

/// A completed pointer publication whose retired references still need to be
/// destroyed.  Keeping those references in an explicit value lets composite
/// operations (notably non-leader exec) publish a task credential and switch
/// the process's group-leader binding under one short lock, while deferring
/// potentially cascading `Arc` destruction until after that lock is released.
pub(crate) struct CredentialPublication<'a> {
    _guard: CredentialUpdateGuard<'a, ()>,
    proposed: Arc<Cred>,
    _published: Arc<Cred>,
    _old: Arc<Cred>,
}

impl CredentialPublication<'_> {
    pub(crate) fn proposed(&self) -> Arc<Cred> {
        self.proposed.clone()
    }
}

impl<'a> PreparedCred<'a> {
    #[cfg(test)]
    pub(crate) fn proposed(&self) -> &Cred {
        &self.proposed
    }

    /// Atomically publishes the proposed pointer while returning ownership of
    /// all retired references to the caller for deferred destruction.
    pub(crate) fn publish(self) -> CredentialPublication<'a> {
        let PreparedCred {
            slot,
            guard,
            old,
            proposed,
        } = self;
        let published = {
            let mut current = slot.current.lock();
            mem::replace(&mut *current, proposed.clone())
        };
        debug_assert!(Arc::ptr_eq(&published, &old));
        CredentialPublication {
            _guard: guard,
            proposed,
            _published: published,
            _old: old,
        }
    }

    /// Atomically publishes the proposed pointer. Both the old slot ownership
    /// and the transaction snapshot are released after the spin lock and the
    /// writer mutex have been dropped.
    pub(crate) fn commit(self) -> Arc<Cred> {
        let publication = self.publish();
        let proposed = publication.proposed();
        drop(publication);
        proposed
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{sync::Arc, vec};
    use std::{sync::Barrier, thread, vec::Vec};

    use linux_raw_sys::general::{CAP_CHOWN, CAP_DAC_OVERRIDE};

    use super::*;

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    fn slot() -> Arc<CredentialSlot> {
        let namespace = UserNamespace::try_new_root().unwrap();
        Arc::new(CredentialSlot::new(Cred::try_root(namespace).unwrap()))
    }

    fn inherited_slot(parent: &CredentialSlot) -> Arc<CredentialSlot> {
        Arc::new(CredentialSlot::new(parent.current()))
    }

    fn publish_raw_setuid(slot: &CredentialSlot, uid: u32) {
        let kernel_uid = kuid(uid);
        let mut update = slot.prepare();
        update.builder.ids.ruid = kernel_uid;
        update.builder.ids.euid = kernel_uid;
        update.builder.ids.suid = kernel_uid;
        update.builder.ids.fsuid = kernel_uid;
        if uid != 0 {
            update.builder.caps.effective = [0; CAPABILITY_WORDS];
            update.builder.caps.permitted = [0; CAPABILITY_WORDS];
            update.builder.caps.clear_ambient();
        }
        update.finish().unwrap().commit();
    }

    #[test]
    fn sibling_task_slots_diverge_after_raw_setuid() {
        let first = slot();
        let second = inherited_slot(&first);

        publish_raw_setuid(&first, 1000);

        assert_eq!(first.current().ids().euid, kuid(1000));
        assert_eq!(second.current().ids().euid, Kuid::INITIAL_ROOT);
    }

    #[test]
    fn no_new_privs_is_task_local_after_clone_thread() {
        let first = slot();
        let second = inherited_slot(&first);

        let mut update = first.prepare();
        update.builder.no_new_privs = true;
        update.finish().unwrap().commit();

        assert!(first.current().no_new_privs());
        assert!(!second.current().no_new_privs());
    }

    #[test]
    fn capability_commits_are_task_local() {
        let first = slot();
        let second = inherited_slot(&first);
        let (word, mask) = CapabilityState::cap_mask(CAP_CHOWN).unwrap();

        let mut update = first.prepare();
        update.builder.caps.effective[word] &= !mask;
        update.builder.caps.permitted[word] &= !mask;
        update.finish().unwrap().commit();

        assert!(!first.current().has_effective_capability(CAP_CHOWN));
        assert!(second.current().has_effective_capability(CAP_CHOWN));
    }

    #[test]
    fn fork_inherits_calling_task_snapshot_into_independent_slot() {
        let caller = slot();
        let unrelated_sibling = inherited_slot(&caller);
        publish_raw_setuid(&caller, 1000);

        let child = inherited_slot(&caller);
        assert_eq!(child.current().ids().euid, kuid(1000));
        assert_eq!(unrelated_sibling.current().ids().euid, Kuid::INITIAL_ROOT);

        publish_raw_setuid(&caller, 2000);
        assert_eq!(caller.current().ids().euid, kuid(2000));
        assert_eq!(child.current().ids().euid, kuid(1000));
    }

    #[test]
    fn non_leader_exec_commit_stays_bound_to_executing_task_slot() {
        let leader = slot();
        let executor = inherited_slot(&leader);
        publish_raw_setuid(&executor, 1000);
        publish_raw_setuid(&leader, 2000);

        let mut enable_keep_caps = executor.prepare();
        enable_keep_caps.builder.caps.securebits |= SECBIT_KEEP_CAPS;
        enable_keep_caps.finish().unwrap().commit();

        let mut exec = executor.prepare();
        exec.builder.caps.securebits &= !SECBIT_KEEP_CAPS;
        let prepared = exec.finish_exec_keep_caps_clear().unwrap();
        // Exec de-threading may rebind the visible TID, but it never replaces
        // the executing Thread or the slot captured by this transaction.
        prepared.commit();

        assert_eq!(executor.current().ids().euid, kuid(1000));
        assert_eq!(
            executor.current().capabilities().securebits & SECBIT_KEEP_CAPS,
            0
        );
        assert_eq!(leader.current().ids().euid, kuid(2000));
    }

    #[test]
    fn dropping_prepared_credential_does_not_publish() {
        let slot = slot();
        let original = slot.current();
        let mut update = slot.prepare();
        update.builder.ids.ruid = kuid(1000);
        let prepared = update.finish().unwrap();
        assert_eq!(prepared.proposed().ids().ruid, kuid(1000));
        drop(prepared);

        let observed = slot.current();
        assert!(Arc::ptr_eq(&original, &observed));
        assert_eq!(observed.ids().ruid, Kuid::INITIAL_ROOT);
    }

    #[test]
    fn no_new_privs_and_locked_securebits_are_monotonic() {
        let slot = slot();
        let mut update = slot.prepare();
        update.builder.no_new_privs = true;
        update.builder.caps.securebits = SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED;
        update.finish().unwrap().commit();

        let mut lower = slot.prepare();
        lower.builder.no_new_privs = false;
        assert_eq!(lower.finish().err(), Some(AxError::OperationNotPermitted));

        let mut unlock = slot.prepare();
        unlock.builder.caps.securebits = 0;
        assert_eq!(unlock.finish().err(), Some(AxError::OperationNotPermitted));
    }

    #[test]
    fn exec_can_only_clear_locked_keep_caps_bit() {
        let slot = slot();
        let mut lock = slot.prepare();
        lock.builder.caps.securebits = SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED;
        lock.finish().unwrap().commit();

        let mut exec = slot.prepare();
        exec.builder.caps.securebits &= !SECBIT_KEEP_CAPS;
        exec.finish_exec_keep_caps_clear().unwrap().commit();
        assert_eq!(
            slot.current().capabilities().securebits,
            SECBIT_KEEP_CAPS_LOCKED
        );
    }

    #[test]
    fn capability_invariants_reject_mixed_authority() {
        let slot = slot();
        let mut effective_without_permitted = slot.prepare();
        effective_without_permitted.builder.caps.effective = [1, 0];
        effective_without_permitted.builder.caps.permitted = [0, 0];
        assert_eq!(
            effective_without_permitted.finish().err(),
            Some(AxError::OperationNotPermitted)
        );

        let mut ambient_without_inheritable = slot.prepare();
        ambient_without_inheritable.builder.caps.ambient = [1, 0];
        ambient_without_inheritable.builder.caps.inheritable = [0, 0];
        assert_eq!(
            ambient_without_inheritable.finish().err(),
            Some(AxError::OperationNotPermitted)
        );

        let mut out_of_range = slot.prepare();
        out_of_range.builder.caps.permitted[1] = u32::MAX;
        assert_eq!(
            out_of_range.finish().err(),
            Some(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn group_info_is_sorted_deduplicated_and_bounded() {
        let groups = GroupInfo::try_new(vec![kgid(300), kgid(100), kgid(300), kgid(200)]).unwrap();
        assert_eq!(groups.as_slice(), [kgid(100), kgid(200), kgid(300)]);

        let mut too_many = Vec::new();
        too_many
            .try_reserve_exact(NGROUPS_MAX as usize + 1)
            .unwrap();
        too_many.resize(NGROUPS_MAX as usize + 1, kgid(1));
        assert_eq!(
            GroupInfo::try_new(too_many).err(),
            Some(AxError::InvalidInput)
        );
    }

    #[test]
    fn entering_new_user_namespace_resets_namespace_relative_capabilities() {
        let root_ns = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::new(Cred::try_root(root_ns.clone()).unwrap());
        assert!(slot.current().is_initial_root_euid());
        assert!(slot.current().is_initial_root_ruid());
        let mut update = slot.prepare();
        update.builder.caps.effective = [0; CAPABILITY_WORDS];
        update.builder.caps.permitted = [0; CAPABILITY_WORDS];
        update.builder.caps.bounding = [0; CAPABILITY_WORDS];
        update.builder.no_new_privs = true;
        let old = update.finish().unwrap().commit();

        let child_ns = root_ns
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let child = Cred::try_with_user_ns(&old, child_ns.clone()).unwrap();
        let caps = child.capabilities();
        assert!(Arc::ptr_eq(child.user_ns(), &child_ns));
        assert_eq!(child.ids(), old.ids());
        assert!(child.no_new_privs());
        assert_eq!(caps.effective, CAPABILITY_VALID_MASK);
        assert_eq!(caps.permitted, CAPABILITY_VALID_MASK);
        assert_eq!(caps.bounding, CAPABILITY_VALID_MASK);
        assert_eq!(caps.inheritable, [0; CAPABILITY_WORDS]);
        assert_eq!(caps.ambient, [0; CAPABILITY_WORDS]);
        assert_eq!(caps.securebits, 0);
        assert!(child.has_effective_capability_in_own_user_ns(CAP_DAC_OVERRIDE));
        assert!(!child.has_effective_capability(CAP_DAC_OVERRIDE));
        assert!(!child.is_initial_root_euid());
        assert!(!child.is_initial_root_ruid());

        let ids = child.ids();
        let dac = DacCredentialView::new(
            ids.fsuid,
            ids.fsgid,
            child.groups().clone(),
            caps.effective,
            child.user_ns().is_initial(),
        );
        assert!(!dac.has_capability(CAP_DAC_OVERRIDE));
    }

    #[test]
    fn concurrent_writers_preserve_each_others_fields() {
        const UPDATES: u32 = 500;

        let slot = slot();
        let round = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let uid_writer = {
            let slot = slot.clone();
            let round = round.clone();
            let finish = finish.clone();
            thread::spawn(move || {
                for value in 1..=UPDATES {
                    round.wait();
                    let mut update = slot.prepare();
                    update.builder.ids.ruid = kuid(value);
                    update.finish().unwrap().commit();
                    finish.wait();
                }
            })
        };
        let gid_writer = {
            let slot = slot.clone();
            thread::spawn(move || {
                for value in 1..=UPDATES {
                    round.wait();
                    let mut update = slot.prepare();
                    update.builder.ids.rgid = kgid(10_000 + value);
                    update.finish().unwrap().commit();
                    finish.wait();
                }
            })
        };

        uid_writer.join().unwrap();
        gid_writer.join().unwrap();
        let ids = slot.current().ids();
        assert_eq!(ids.ruid, kuid(UPDATES));
        assert_eq!(ids.rgid, kgid(10_000 + UPDATES));
    }

    #[test]
    fn concurrent_readers_never_mix_committed_snapshots() {
        const READERS: usize = 8;
        const WRITES: usize = 2_000;

        let slot = slot();
        let root_ns = slot.current().user_ns().clone();
        let child_ns = root_ns.try_fork(kuid(1000), kgid(100), false).unwrap();

        let publish = |slot: &CredentialSlot, first: bool| {
            let mut update = slot.prepare();
            let (id, group, cap, securebits, namespace) = if first {
                (kuid(1000), kgid(100), CAP_CHOWN, 0, root_ns.clone())
            } else {
                (
                    kuid(2000),
                    kgid(200),
                    CAP_DAC_OVERRIDE,
                    SECBIT_KEEP_CAPS,
                    child_ns.clone(),
                )
            };
            update.builder.ids = Credentials {
                ruid: id,
                euid: id,
                suid: id,
                fsuid: id,
                rgid: group,
                egid: group,
                sgid: group,
                fsgid: group,
            };
            update.builder.groups = GroupInfo::try_new(vec![group]).unwrap();
            update.builder.caps = CapabilityState {
                effective: [1 << cap, 0],
                permitted: [1 << cap, 0],
                inheritable: [0; CAPABILITY_WORDS],
                bounding: CAPABILITY_VALID_MASK,
                ambient: [0; CAPABILITY_WORDS],
                securebits,
            };
            update.builder.user_ns = namespace;
            update.finish().unwrap().commit();
        };
        publish(&slot, true);

        let start = Arc::new(Barrier::new(READERS + 1));
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let slot = slot.clone();
                let start = start.clone();
                let root_ns = root_ns.clone();
                let child_ns = child_ns.clone();
                thread::spawn(move || {
                    start.wait();
                    for _ in 0..WRITES {
                        let cred = slot.current();
                        let ids = cred.ids();
                        let caps = cred.capabilities();
                        let first = ids.ruid == kuid(1000)
                            && ids.rgid == kgid(100)
                            && cred.groups().as_slice() == [kgid(100)]
                            && caps.effective[0] == 1 << CAP_CHOWN
                            && caps.permitted[0] == 1 << CAP_CHOWN
                            && caps.securebits == 0
                            && Arc::ptr_eq(cred.user_ns(), &root_ns);
                        let second = ids.ruid == kuid(2000)
                            && ids.rgid == kgid(200)
                            && cred.groups().as_slice() == [kgid(200)]
                            && caps.effective[0] == 1 << CAP_DAC_OVERRIDE
                            && caps.permitted[0] == 1 << CAP_DAC_OVERRIDE
                            && caps.securebits == SECBIT_KEEP_CAPS
                            && Arc::ptr_eq(cred.user_ns(), &child_ns);
                        assert!(first || second);
                    }
                })
            })
            .collect();

        start.wait();
        for index in 0..WRITES {
            publish(&slot, index % 2 == 0);
        }
        for reader in readers {
            reader.join().unwrap();
        }
    }
}
