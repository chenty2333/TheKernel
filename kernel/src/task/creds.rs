use alloc::sync::Arc;
use core::mem;

use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;
pub(crate) use thekernel_linux_cred::{
    CAPABILITY_VALID_MASK, CAPABILITY_WORDS, CredentialIds as Credentials,
    FsCredentialSnapshot as DacCredentialView, GroupInfo, SECBIT_KEEP_CAPS,
    SECBIT_KEEP_CAPS_LOCKED, SECBIT_NO_CAP_AMBIENT_RAISE, SECBIT_NO_SETUID_FIXUP, SECBIT_NOROOT,
    SECURE_ALL_BITS, SECURE_ALL_LOCKS,
};
use thekernel_linux_cred::{
    CapabilitySets, Credential, CredentialTransitionMode, credential_cap_is_subset,
};

use super::{cred_error, process::UserNamespace};

pub(crate) type Cred = Credential<UserNamespace>;

#[cfg(not(test))]
type CredentialUpdateMutex<T> = axsync::Mutex<T>;
#[cfg(test)]
type CredentialUpdateMutex<T> = spin::Mutex<T>;

#[cfg(not(test))]
type CredentialUpdateGuard<'a, T> = axsync::MutexGuard<'a, T>;
#[cfg(test)]
type CredentialUpdateGuard<'a, T> = spin::MutexGuard<'a, T>;

/// Kernel-local mutable capability draft used only while a credential writer
/// owns the sleepable update transaction. Committed capability state is always
/// the validated, field-private [`CapabilitySets`] value from the ABI crate.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct CapabilityDraft {
    pub(crate) effective: [u32; CAPABILITY_WORDS],
    pub(crate) permitted: [u32; CAPABILITY_WORDS],
    pub(crate) inheritable: [u32; CAPABILITY_WORDS],
    pub(crate) bounding: [u32; CAPABILITY_WORDS],
    pub(crate) ambient: [u32; CAPABILITY_WORDS],
    pub(crate) securebits: u32,
}

pub(crate) type CapabilityState = CapabilityDraft;

impl CapabilityDraft {
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
        CapabilitySets::cap_mask(cap)
    }

    pub(crate) fn valid_mask(word: usize) -> u32 {
        CAPABILITY_VALID_MASK[word]
    }

    pub(crate) fn has_effective(self, cap: u32) -> bool {
        Self::cap_mask(cap).is_some_and(|(word, mask)| self.effective[word] & mask != 0)
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

    pub(in crate::task) fn from_committed(caps: CapabilitySets) -> Self {
        Self {
            effective: caps.effective(),
            permitted: caps.permitted(),
            inheritable: caps.inheritable(),
            bounding: caps.bounding(),
            ambient: caps.ambient(),
            securebits: caps.securebits(),
        }
    }

    fn try_into_committed(self) -> AxResult<CapabilitySets> {
        CapabilitySets::try_new(
            self.effective,
            self.permitted,
            self.inheritable,
            self.bounding,
            self.ambient,
            self.securebits,
        )
        .map_err(cred_error)
    }
}

/// Mutable, unpublished copy of a credential transaction.
pub(crate) struct CredBuilder {
    pub(in crate::task) ids: Credentials,
    pub(in crate::task) groups: Arc<GroupInfo>,
    pub(in crate::task) caps: CapabilityState,
    pub(in crate::task) no_new_privs: bool,
}

impl CredBuilder {
    fn from_cred(cred: &Cred) -> Self {
        Self {
            ids: cred.ids(),
            groups: cred.groups().clone(),
            caps: CapabilityDraft::from_committed(cred.capabilities()),
            no_new_privs: cred.no_new_privs(),
        }
    }

    fn try_build(self, old: &Cred, exec_clears_keep_caps: bool) -> AxResult<Arc<Cred>> {
        let mode = if exec_clears_keep_caps {
            CredentialTransitionMode::ExecClearsKeepCaps
        } else {
            CredentialTransitionMode::Normal
        };
        let caps = self.caps.try_into_committed()?;
        Credential::try_from_transition(
            old,
            self.ids,
            self.groups,
            caps,
            self.no_new_privs,
            old.user_ns().clone(),
            mode,
        )
        .map_err(cred_error)
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

/// Pins one credential slot against publication while a security decision is
/// carried into a later composite operation. The sleepable update guard is
/// held for the lifetime of this value; the current-pointer spin lock is used
/// only to clone the immutable credential object.
pub(crate) struct CredentialSnapshotGuard<'a> {
    slot: &'a CredentialSlot,
    _update: CredentialUpdateGuard<'a, ()>,
    credential: Arc<Cred>,
}

impl CredentialSnapshotGuard<'_> {
    pub(crate) fn slot(&self) -> &CredentialSlot {
        self.slot
    }

    pub(crate) fn credential(&self) -> &Arc<Cred> {
        &self.credential
    }
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

    pub(crate) fn lock_snapshot(&self) -> CredentialSnapshotGuard<'_> {
        let update = self.update.lock();
        let credential = self.current();
        CredentialSnapshotGuard {
            slot: self,
            _update: update,
            credential,
        }
    }

    /// Attempts to pin this slot without sleeping.
    ///
    /// Composite graph/lifecycle transactions use this form when waiting for
    /// a concurrent credential writer would invert their outer lock order.
    pub(crate) fn try_lock_snapshot(&self) -> Option<CredentialSnapshotGuard<'_>> {
        let update = self.update.try_lock()?;
        let credential = self.current();
        Some(CredentialSnapshotGuard {
            slot: self,
            _update: update,
            credential,
        })
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
    pub(in crate::task) fn old(&self) -> &Cred {
        &self.old
    }

    /// Linux lowers process dumpability and clears the parent-death signal
    /// when an effective/filesystem ID changes or the proposed permitted
    /// authority is not contained by the old credential. The process layer
    /// consumes this before publication so readers cannot observe stronger
    /// authority with stale, more permissive image state.
    pub(crate) fn requires_dumpability_drop(&self) -> bool {
        let old = self.old.ids();
        let proposed = self.proposed.ids();
        old.euid != proposed.euid
            || old.egid != proposed.egid
            || old.fsuid != proposed.fsuid
            || old.fsgid != proposed.fsgid
            || !credential_cap_is_subset(&self.old, &self.proposed)
    }

    pub(in crate::task) fn proposed(&self) -> &Cred {
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
    use crate::task::{Kgid, Kuid};

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
            executor.current().capabilities().securebits() & SECBIT_KEEP_CAPS,
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
    fn process_access_credential_snapshot_guard_pins_authorized_object() {
        let slot = slot();
        let original = slot.current();
        let guard = slot.lock_snapshot();
        assert!(core::ptr::eq(guard.slot(), &*slot));
        assert!(Arc::ptr_eq(guard.credential(), &original));
        drop(guard);

        let mut update = slot.prepare();
        update.builder.ids.ruid = kuid(1000);
        update.finish().unwrap().commit();
        assert!(!Arc::ptr_eq(&slot.current(), &original));
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

        let publish = |slot: &CredentialSlot, first: bool| {
            let mut update = slot.prepare();
            let (id, group, cap, securebits) = if first {
                (kuid(1000), kgid(100), CAP_CHOWN, 0)
            } else {
                (kuid(2000), kgid(200), CAP_DAC_OVERRIDE, SECBIT_KEEP_CAPS)
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
            update.finish().unwrap().commit();
        };
        publish(&slot, true);

        let start = Arc::new(Barrier::new(READERS + 1));
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let slot = slot.clone();
                let start = start.clone();
                let root_ns = root_ns.clone();
                thread::spawn(move || {
                    start.wait();
                    for _ in 0..WRITES {
                        let cred = slot.current();
                        let ids = cred.ids();
                        let caps = cred.capabilities();
                        let first = ids.ruid == kuid(1000)
                            && ids.rgid == kgid(100)
                            && cred.groups().as_slice() == [kgid(100)]
                            && caps.effective()[0] == 1 << CAP_CHOWN
                            && caps.permitted()[0] == 1 << CAP_CHOWN
                            && caps.securebits() == 0
                            && Arc::ptr_eq(cred.user_ns(), &root_ns);
                        let second = ids.ruid == kuid(2000)
                            && ids.rgid == kgid(200)
                            && cred.groups().as_slice() == [kgid(200)]
                            && caps.effective()[0] == 1 << CAP_DAC_OVERRIDE
                            && caps.permitted()[0] == 1 << CAP_DAC_OVERRIDE
                            && caps.securebits() == SECBIT_KEEP_CAPS
                            && Arc::ptr_eq(cred.user_ns(), &root_ns);
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
