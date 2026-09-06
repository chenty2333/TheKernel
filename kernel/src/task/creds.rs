use alloc::sync::Arc;
#[cfg(test)]
use core::cell::Cell;

#[cfg(test)]
extern crate std;

use axerrno::{AxError, AxResult};
#[cfg(test)]
pub(crate) use thekernel_linux_cred::CAPABILITY_VALID_MASK;
pub(crate) use thekernel_linux_cred::{
    CAPABILITY_WORDS, CredentialIds as Credentials, FsCredentialSnapshot as DacCredentialView,
    GroupInfo, SECBIT_KEEP_CAPS, SECBIT_NO_SETUID_FIXUP,
};
use thekernel_linux_cred::{CapabilitySets, Credential, CredentialTransitionEffects};

#[cfg(test)]
use super::security::test_frozen_registry;
use super::{
    cred_error,
    exec_cred::ExecCredentialDraft,
    process::UserNamespace,
    security::{
        CredentialMutationKind, CredentialSecurityState, CredentialStateTransition,
        FrozenSecurityRegistry, PendingCredentialPostCommit, capable, capable_for_setid,
    },
};
use crate::rcu::{
    CredentialRcuSlot, CredentialRetireReservation, ensure_current_cpu_registered,
    wake_credential_retire_worker,
};

pub(in crate::task) type CoreCred = Credential<UserNamespace>;

/// Kernel-owned composite credential. The module state field is intentionally
/// declared first so every free callback runs while the Linux credential core
/// is still alive. Readers and publication always move one outer `Arc<Cred>`.
pub(crate) struct Cred {
    security: CredentialSecurityState,
    core: Arc<CoreCred>,
}

impl Cred {
    fn try_from_parts(
        core: Arc<CoreCred>,
        security: CredentialSecurityState,
    ) -> AxResult<Arc<Self>> {
        Self::try_from_parts_with_allocator(core, security, |credential| {
            Arc::try_new(credential).map_err(|_| AxError::NoMemory)
        })
    }

    fn try_from_parts_with_allocator<F>(
        core: Arc<CoreCred>,
        security: CredentialSecurityState,
        allocate: F,
    ) -> AxResult<Arc<Self>>
    where
        F: FnOnce(Self) -> AxResult<Arc<Self>>,
    {
        allocate(Self { security, core })
    }

    pub(in crate::task) fn try_from_prepared_parts(
        core: Arc<CoreCred>,
        security: CredentialSecurityState,
    ) -> AxResult<Arc<Self>> {
        Self::try_from_parts(core, security)
    }

    #[cfg(test)]
    pub(in crate::task) fn try_from_prepared_parts_with_allocator<F>(
        core: Arc<CoreCred>,
        security: CredentialSecurityState,
        allocate: F,
    ) -> AxResult<Arc<Self>>
    where
        F: FnOnce(Self) -> AxResult<Arc<Self>>,
    {
        Self::try_from_parts_with_allocator(core, security, allocate)
    }

    pub(crate) fn try_root_with_registry(
        registry: FrozenSecurityRegistry,
        user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        let core = CoreCred::try_root(user_ns).map_err(cred_error)?;
        let security = registry.try_init_credential_state(&core)?;
        Self::try_from_parts(core, security)
    }

    /// Unit-test fixtures do not execute the architecture entry path. Their
    /// complete built-in registry is isolated from production publication.
    #[cfg(test)]
    pub(crate) fn try_root(user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_root_with_registry(test_frozen_registry(), user_ns)
    }

    pub(crate) fn try_prepare_clone_for_fork(old: &Arc<Self>) -> AxResult<Arc<Self>> {
        let registry = old.security.registry();
        let security = registry.try_prepare_credential_state(
            old.core(),
            &old.security,
            old.core(),
            CredentialStateTransition::Fork,
        )?;
        Self::try_from_parts(old.core.clone(), security)
    }

    pub(crate) fn try_prepare_with_user_namespace(
        old: &Arc<Self>,
        user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        let core = CoreCred::try_with_user_namespace(old.core(), user_ns).map_err(cred_error)?;
        Self::try_from_core_transition(old, core, CredentialStateTransition::UserNamespace)
    }

    #[cfg(test)]
    pub(crate) fn try_clone_for_fork(old: &Arc<Self>) -> AxResult<Arc<Self>> {
        let credential = Self::try_prepare_clone_for_fork(old)?;
        credential.security.activate_fixture();
        Ok(credential)
    }

    #[cfg(test)]
    pub(crate) fn try_with_user_namespace(
        old: &Arc<Self>,
        user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        let credential = Self::try_prepare_with_user_namespace(old, user_ns)?;
        credential.security.activate_fixture();
        Ok(credential)
    }

    fn try_from_core_transition(
        old: &Arc<Self>,
        core: Arc<CoreCred>,
        transition: CredentialStateTransition,
    ) -> AxResult<Arc<Self>> {
        let security = Self::try_prepare_security_transition(old, &core, transition)?;
        Self::try_from_parts(core, security)
    }

    pub(in crate::task) fn try_prepare_security_transition(
        old: &Arc<Self>,
        proposed_core: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<CredentialSecurityState> {
        let registry = old.security.registry();
        registry.try_prepare_credential_state(old.core(), &old.security, proposed_core, transition)
    }

    pub(in crate::task) fn core(&self) -> &CoreCred {
        &self.core
    }

    pub(in crate::task) fn core_arc(&self) -> &Arc<CoreCred> {
        &self.core
    }

    pub(in crate::task) fn security(&self) -> &CredentialSecurityState {
        &self.security
    }

    pub(crate) fn ids(&self) -> Credentials {
        self.core.ids()
    }

    pub(crate) fn groups(&self) -> &Arc<GroupInfo> {
        self.core.groups()
    }

    pub(crate) fn capabilities(&self) -> CapabilitySets {
        self.core.capabilities()
    }

    pub(crate) fn no_new_privs(&self) -> bool {
        self.core.no_new_privs()
    }

    pub(crate) fn user_ns(&self) -> &Arc<UserNamespace> {
        self.core.user_ns()
    }

    pub(crate) fn fs_dac_credentials(&self) -> DacCredentialView {
        self.core.fs_credential_snapshot()
    }

    /// Builds the access(2) DAC projection from this exact pinned composite.
    /// AT_EACCESS callers should use `fs_dac_credentials()` and the live typed
    /// actor path; this helper preserves Linux's synthetic real-ID view.
    pub(crate) fn real_id_access_dac_credentials(&self) -> DacCredentialView {
        let credentials = self.ids();
        let capabilities = self.capabilities();
        let capability_set = if capabilities.securebits() & SECBIT_NO_SETUID_FIXUP != 0 {
            capabilities.effective()
        } else if self.user_ns().root_kuid() == Some(credentials.ruid) {
            capabilities.permitted()
        } else {
            [0; CAPABILITY_WORDS]
        };
        DacCredentialView::new(
            credentials.ruid,
            credentials.rgid,
            self.groups().clone(),
            capability_set,
            self.user_ns().is_initial(),
        )
    }

    pub(crate) fn has_effective_capability(&self, capability: u32) -> bool {
        self.user_ns().is_initial() && capable(self, self.user_ns(), capability)
    }

    pub(crate) fn has_effective_capability_in_own_user_ns(&self, capability: u32) -> bool {
        capable(self, self.user_ns(), capability)
    }

    /// Set-ID-family counterpart to the ordinary own-namespace check. This is
    /// intentionally not a generic operation-taking API: only setuid, setgid,
    /// setgroups, and their Linux variants may select `CAP_OPT_INSETID`.
    pub(crate) fn has_effective_capability_for_setid(&self, capability: u32) -> bool {
        capable_for_setid(self, self.user_ns(), capability)
    }

    /// Returns whether both composites wrap the same immutable Linux
    /// credential core. Fork may rebuild kernel-owned security state around a
    /// shared core, so outer `Arc` identity is intentionally too strict.
    pub(crate) fn same_linux_credential(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.core, &other.core)
    }

    pub(crate) fn is_initial_root_euid(&self) -> bool {
        self.core.is_initial_root_euid()
    }

    pub(crate) fn is_initial_root_ruid(&self) -> bool {
        self.core.is_initial_root_ruid()
    }
}

#[cfg(not(test))]
type CredentialUpdateMutex<T> = axsync::Mutex<T>;
#[cfg(test)]
struct CredentialUpdateMutex<T>(spin::Mutex<T>);

#[cfg(not(test))]
type CredentialUpdateGuard<'a, T> = axsync::MutexGuard<'a, T>;
#[cfg(test)]
struct CredentialUpdateGuard<'a, T> {
    _guard: spin::MutexGuard<'a, T>,
}

#[cfg(test)]
impl<T> CredentialUpdateMutex<T> {
    fn new(value: T) -> Self {
        Self(spin::Mutex::new(value))
    }

    fn lock(&self) -> CredentialUpdateGuard<'_, T> {
        let guard = self.0.lock();
        CREDENTIAL_WRITER_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        CredentialUpdateGuard { _guard: guard }
    }

    fn try_lock(&self) -> Option<CredentialUpdateGuard<'_, T>> {
        let guard = self.0.try_lock()?;
        CREDENTIAL_WRITER_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Some(CredentialUpdateGuard { _guard: guard })
    }
}

#[cfg(test)]
impl<T> Drop for CredentialUpdateGuard<'_, T> {
    fn drop(&mut self) {
        CREDENTIAL_WRITER_LOCK_DEPTH.with(|depth| {
            let held = depth.get();
            debug_assert!(held != 0);
            depth.set(held - 1);
        });
    }
}

#[cfg(test)]
std::thread_local! {
    static CREDENTIAL_WRITER_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
    static CREDENTIAL_PUBLICATION_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
pub(in crate::task) fn credential_writer_lock_held() -> bool {
    CREDENTIAL_WRITER_LOCK_DEPTH.with(|depth| depth.get() != 0)
}

#[cfg(test)]
pub(in crate::task) fn credential_publication_lock_held() -> bool {
    CREDENTIAL_PUBLICATION_LOCK_DEPTH.with(|depth| depth.get() != 0)
}

/// Kernel compatibility name for the field-private capability value owned by
/// the reusable credential crate. The kernel does not keep a second mutable
/// representation or capability-policy implementation.
pub(crate) type CapabilityState = CapabilitySets;

#[cfg(test)]
pub(in crate::task) fn capability_state_for_test(
    effective: [u32; CAPABILITY_WORDS],
    permitted: [u32; CAPABILITY_WORDS],
    inheritable: [u32; CAPABILITY_WORDS],
    bounding: [u32; CAPABILITY_WORDS],
    ambient: [u32; CAPABILITY_WORDS],
    securebits: u32,
) -> CapabilityState {
    CapabilitySets::try_new(
        effective,
        permitted,
        inheritable,
        bounding,
        ambient,
        securebits,
    )
    .expect("test capability state must satisfy credential invariants")
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
            caps: cred.capabilities(),
            no_new_privs: cred.no_new_privs(),
        }
    }

    fn try_build(
        self,
        old: &Arc<Cred>,
    ) -> AxResult<(
        Arc<Cred>,
        CredentialTransitionEffects,
        CredentialStateTransition,
    )> {
        let prepared_core = CoreCred::try_prepare_transition(
            old.core_arc(),
            self.ids,
            self.groups,
            self.caps,
            self.no_new_privs,
        )
        .map_err(cred_error)?;
        let effects = prepared_core.effects();
        let transition = CredentialStateTransition::Mutation(CredentialMutationKind::between(
            old.core(),
            prepared_core.proposed(),
        ));
        let security =
            Cred::try_prepare_security_transition(old, prepared_core.proposed(), transition)?;
        let core = prepared_core
            .try_into_proposed(old.core_arc())
            .map_err(cred_error)?;
        let proposed = Cred::try_from_prepared_parts(core, security)?;
        Ok((proposed, effects, transition))
    }
}

/// The single publication point for one task-identity owner.
///
/// The slot has no `ProcessData` dependency and is embedded exactly once in
/// `Thread`. Clone and fork construct a new slot from one caller snapshot;
/// subsequent commits therefore affect only the owning task.
pub(crate) struct CredentialSlot {
    update: CredentialUpdateMutex<()>,
    current: CredentialRcuSlot<Cred>,
}

/// Pins one credential slot against publication while a security decision is
/// carried into a later composite operation. The sleepable update guard is
/// held for the lifetime of this value; the bounded RCU slot is used only to
/// clone the immutable credential object.
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
        ensure_current_cpu_registered()
            .unwrap_or_else(|error| panic!("credential RCU CPU registration failed: {error}"));
        Self {
            update: CredentialUpdateMutex::new(()),
            current: crate::rcu::credential_slot(initial),
        }
    }

    pub(crate) fn try_new(initial: Arc<Cred>) -> AxResult<Arc<Self>> {
        ensure_current_cpu_registered()?;
        Arc::try_new(Self {
            update: CredentialUpdateMutex::new(()),
            current: crate::rcu::credential_slot(initial),
        })
        .map_err(|_| AxError::NoMemory)
    }

    /// Takes a coherent immutable reader reference through the bounded RCU
    /// slot. The allocation-free read side only clones the existing Arc.
    pub(crate) fn current(&self) -> Arc<Cred> {
        self.current.load()
    }

    /// Runs a read-only credential query from an owned immutable snapshot.
    ///
    /// The RCU section is intentionally limited to `current()`, which bumps
    /// one Arc strong count and then releases the preemption pin. Callers may
    /// therefore perform arbitrarily long pure evaluation (including helper
    /// calls that can reschedule) without holding a non-preemptible RCU
    /// section. Publication/clear still linearize at the slot's atomic swap;
    /// the returned Arc keeps the exact committed composite alive.
    pub(crate) fn with_current<R>(&self, operation: impl for<'a> FnOnce(&'a Cred) -> R) -> R {
        let credential = self.current();
        operation(&credential)
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

    fn reserve_retire(&self) -> AxResult<CredentialRetireReservation> {
        match self.current.reserve_retire() {
            Ok(reservation) => Ok(reservation),
            // The caller still holds the credential writer mutex. Draining
            // here could run a retired `Cred` destructor (and its security
            // free callback) under that lock, violating the publication
            // lock order and permitting callback re-entry. The policy worker
            // owns reclamation; report bounded pressure and leave the exact
            // transaction uncommitted instead.
            Err(_) => Err(AxError::ResourceBusy),
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_fs_ids_for_test(
        &self,
        fsuid: thekernel_linux_cred::Kuid,
        fsgid: thekernel_linux_cred::Kgid,
    ) -> AxResult<Arc<Cred>> {
        let mut update = self.prepare();
        update.builder.ids.fsuid = fsuid;
        update.builder.ids.fsgid = fsgid;
        Ok(update.finish()?.commit())
    }

    #[cfg(test)]
    pub(crate) fn replace_capabilities_for_test(
        &self,
        permitted: &[u32],
        effective: &[u32],
    ) -> AxResult<Arc<Cred>> {
        fn insert_capability(set: &mut [u32; CAPABILITY_WORDS], capability: u32) -> AxResult<()> {
            let (word, mask) =
                CapabilityState::cap_mask(capability).ok_or(AxError::InvalidInput)?;
            set[word] |= mask;
            Ok(())
        }

        let mut update = self.prepare();
        let old = update.builder.caps;
        let mut permitted_set = [0; CAPABILITY_WORDS];
        let mut effective_set = [0; CAPABILITY_WORDS];
        for &capability in permitted {
            insert_capability(&mut permitted_set, capability)?;
        }
        for &capability in effective {
            insert_capability(&mut effective_set, capability)?;
        }
        update.builder.caps = capability_state_for_test(
            effective_set,
            permitted_set,
            [0; CAPABILITY_WORDS],
            old.bounding(),
            [0; CAPABILITY_WORDS],
            old.securebits(),
        );
        Ok(update.finish()?.commit())
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

    pub(in crate::task) fn old_arc(&self) -> &Arc<Cred> {
        &self.old
    }

    /// Finalizes all invariants and performs the only fallible allocation for
    /// the replacement object. Dropping the returned value aborts cleanly.
    fn finish_inner(self) -> AxResult<PreparedCred<'a>> {
        let CredentialUpdate {
            slot,
            guard,
            old,
            builder,
        } = self;
        let (proposed, effects, transition) = builder.try_build(&old)?;
        let post_commit = PendingCredentialPostCommit::try_new(&old, &proposed, transition)?;
        let retire = slot.reserve_retire()?;
        Ok(PreparedCred {
            slot,
            guard,
            old,
            proposed,
            post_commit,
            retire,
            requires_dumpability_drop: effects.requires_dumpability_drop(),
        })
    }

    pub(crate) fn finish(self) -> AxResult<PreparedCred<'a>> {
        self.finish_inner()
    }

    /// Builds a user-namespace credential transition from the exact slot
    /// snapshot guarded by this transaction.  `setns(CLONE_NEWUSER)` cannot
    /// reuse the generic mutable builder because the core credential changes
    /// namespace identity as one operation.
    pub(crate) fn finish_user_namespace(
        self,
        user_ns: Arc<UserNamespace>,
    ) -> AxResult<PreparedCred<'a>> {
        let CredentialUpdate {
            slot,
            guard,
            old,
            builder,
        } = self;
        let proposed = Cred::try_prepare_with_user_namespace(&old, user_ns)?;
        let post_commit = PendingCredentialPostCommit::try_new(
            &old,
            &proposed,
            CredentialStateTransition::UserNamespace,
        )?;
        let retire = slot.reserve_retire()?;
        drop(builder);
        Ok(PreparedCred {
            slot,
            guard,
            old,
            proposed,
            post_commit,
            retire,
            requires_dumpability_drop: true,
        })
    }

    /// Accepts only an external exec proposal derived from this transaction's
    /// exact old `Arc`. Equal-looking credentials from another slot or writer
    /// snapshot are rejected before they can reach the publication token.
    pub(in crate::task) fn finish_exec_draft(
        self,
        draft: ExecCredentialDraft,
    ) -> AxResult<PreparedCred<'a>> {
        let CredentialUpdate {
            slot,
            guard,
            old,
            builder,
        } = self;
        let requires_dumpability_drop = draft.proposal().effects().clear_pdeath_signal();
        let (proposed_core, proposed_security) = draft.try_into_parts(&old)?;
        let proposed = Cred::try_from_prepared_parts(proposed_core, proposed_security)?;
        let post_commit =
            PendingCredentialPostCommit::try_new(&old, &proposed, CredentialStateTransition::Exec)?;
        let retire = slot.reserve_retire()?;
        drop(builder);
        Ok(PreparedCred {
            slot,
            guard,
            old,
            proposed,
            post_commit,
            retire,
            requires_dumpability_drop,
        })
    }
}

/// A fully built credential that has not yet become observable.
pub(crate) struct PreparedCred<'a> {
    slot: &'a CredentialSlot,
    guard: CredentialUpdateGuard<'a, ()>,
    old: Arc<Cred>,
    proposed: Arc<Cred>,
    post_commit: PendingCredentialPostCommit,
    retire: CredentialRetireReservation,
    requires_dumpability_drop: bool,
}

/// A completed pointer publication whose mandatory post-commit notification
/// has not run yet. Callers must first release every enclosing process/image/
/// alias lock and then consume this value with `complete_post_commit`.
#[must_use = "published credentials must complete their post-commit notification"]
pub(crate) struct CredentialPublication<'a> {
    guard: Option<CredentialUpdateGuard<'a, ()>>,
    proposed: Option<Arc<Cred>>,
    published: Option<Arc<Cred>>,
    old: Option<Arc<Cred>>,
    post_commit: Option<PendingCredentialPostCommit>,
}

/// Exact old ownership retained after notification until the caller reaches
/// its destruction-safe boundary. Exec carries this value past the hardware
/// page-table-root switch; ordinary transitions may drop it immediately.
#[must_use = "retired credential ownership must reach its destruction-safe boundary"]
pub(crate) struct CredentialRetirement {
    _published: Arc<Cred>,
    _old: Arc<Cred>,
}

impl<'a> CredentialPublication<'a> {
    /// Releases the sleepable writer mutex, runs the infallible ordered
    /// notification while exact old/new composites are alive, and returns a
    /// separate retirement owner. Every publication/process/image/alias lock
    /// must already be absent when this method is called.
    pub(crate) fn complete_post_commit(mut self) -> (Arc<Cred>, CredentialRetirement) {
        let guard = self.guard.take().expect("credential writer guard is live");
        let proposed = self
            .proposed
            .take()
            .expect("published proposed credential is live");
        let published = self
            .published
            .take()
            .expect("retired slot credential is live");
        let old = self.old.take().expect("transaction old credential is live");
        let post_commit = self
            .post_commit
            .take()
            .expect("post-commit notification is pending");
        drop(guard);
        post_commit.notify();
        (
            proposed,
            CredentialRetirement {
                _published: published,
                _old: old,
            },
        )
    }
}

impl Drop for CredentialPublication<'_> {
    fn drop(&mut self) {
        assert!(
            self.post_commit.is_none(),
            "published credential dropped without post-commit notification"
        );
    }
}

impl<'a> PreparedCred<'a> {
    /// Linux lowers process dumpability and clears the parent-death signal
    /// when an effective/filesystem ID changes or the proposed permitted
    /// authority is not contained by the old credential. The process layer
    /// consumes this before publication so readers cannot observe stronger
    /// authority with stale, more permissive image state.
    pub(crate) fn requires_dumpability_drop(&self) -> bool {
        self.requires_dumpability_drop
    }

    pub(in crate::task) fn proposed(&self) -> &Cred {
        &self.proposed
    }

    pub(in crate::task) fn old_arc(&self) -> &Arc<Cred> {
        &self.old
    }

    pub(in crate::task) fn proposed_arc(&self) -> &Arc<Cred> {
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
            post_commit,
            retire,
            requires_dumpability_drop: _,
        } = self;
        post_commit.activate();
        let published = slot
            .current
            .publish(proposed.clone(), &old, retire)
            .unwrap_or_else(|_| panic!("credential publication lost its exact old composite"));
        wake_credential_retire_worker();
        CredentialPublication {
            guard: Some(guard),
            proposed: Some(proposed),
            published: Some(published),
            old: Some(old),
            post_commit: Some(post_commit),
        }
    }

    /// Atomically publishes the proposed pointer. Both the old slot ownership
    /// and the transaction snapshot are released after the spin lock and the
    /// writer mutex have been dropped.
    pub(crate) fn commit(self) -> Arc<Cred> {
        let publication = self.publish();
        let (proposed, retirement) = publication.complete_post_commit();
        drop(retirement);
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
        // Host tests have no background retire worker. Drain completed
        // publications from earlier tests before reserving fresh updates.
        crate::rcu::drain_credential_retire(crate::rcu::CREDENTIAL_RETIRE_CAPACITY);
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
            let caps = update.builder.caps;
            update.builder.caps = capability_state_for_test(
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                caps.inheritable(),
                caps.bounding(),
                [0; CAPABILITY_WORDS],
                caps.securebits(),
            );
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

        loop {
            let mut update = first.prepare();
            let caps = update.builder.caps;
            let mut effective = caps.effective();
            let mut permitted = caps.permitted();
            effective[word] &= !mask;
            permitted[word] &= !mask;
            update.builder.caps = capability_state_for_test(
                effective,
                permitted,
                caps.inheritable(),
                caps.bounding(),
                caps.ambient(),
                caps.securebits(),
            );
            match update.finish() {
                Ok(prepared) => {
                    prepared.commit();
                    break;
                }
                Err(AxError::ResourceBusy) => {
                    crate::rcu::drain_credential_retire(crate::rcu::CREDENTIAL_RETIRE_CAPACITY);
                    thread::yield_now();
                }
                Err(error) => panic!("credential update failed: {error:?}"),
            }
        }

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
    fn process_fork_clones_outer_state_while_sharing_immutable_core() {
        let slot = slot();
        let parent = slot.current();
        let child = Cred::try_clone_for_fork(&parent).unwrap();

        assert!(!Arc::ptr_eq(&parent, &child));
        assert!(Arc::ptr_eq(parent.core_arc(), child.core_arc()));
        assert!(parent.same_linux_credential(&child));
        assert!(
            parent
                .security()
                .registry()
                .same_registry(child.security().registry())
        );
        assert_eq!(parent.ids(), child.ids());
    }

    #[test]
    fn non_leader_exec_commit_stays_bound_to_executing_task_slot() {
        let leader = slot();
        let executor = inherited_slot(&leader);
        publish_raw_setuid(&executor, 1000);
        publish_raw_setuid(&leader, 2000);

        let mut enable_keep_caps = executor.prepare();
        enable_keep_caps
            .builder
            .caps
            .try_set_keep_caps(true)
            .unwrap();
        enable_keep_caps.finish().unwrap().commit();

        let exec = executor.prepare();
        let input = thekernel_linux_cred::ExecCredentialInput::new(
            0,
            Some(thekernel_linux_cred::ExecFileOwner::new(
                Kuid::INITIAL_ROOT,
                Kgid::INITIAL_ROOT,
            )),
            thekernel_linux_cred::ExecMountPrivilege::Honor,
            thekernel_linux_cred::ExecTraceState::NotSuppressingPrivilege,
            thekernel_linux_cred::ExecImageReadability::Readable,
            None,
        );
        let source = crate::task::ExecFileSecurityObject::new(
            crate::task::ExecFileIdentity::new(1, 2),
            exec.old().user_ns().clone(),
            Some(crate::task::ExecFileOwner::new(
                Kuid::INITIAL_ROOT,
                Kgid::INITIAL_ROOT,
            )),
            0o755,
            true,
            crate::task::ExecExecutableRole::Requested,
        );
        let draft = ExecCredentialDraft::try_new(exec.old_arc(), input, source).unwrap();
        let prepared = exec.finish_exec_draft(draft).unwrap();
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
                    loop {
                        let mut update = slot.prepare();
                        update.builder.ids.ruid = kuid(value);
                        match update.finish() {
                            Ok(prepared) => {
                                prepared.commit();
                                break;
                            }
                            Err(AxError::ResourceBusy) => {
                                // The writer guard has been dropped with the
                                // consumed update. Reclaim only at this
                                // outer retry boundary; production uses the
                                // policy worker for the same lock-safe drain.
                                crate::rcu::drain_credential_retire(
                                    crate::rcu::CREDENTIAL_RETIRE_CAPACITY,
                                );
                                thread::yield_now();
                            }
                            Err(error) => panic!("credential update failed: {error:?}"),
                        }
                    }
                    finish.wait();
                }
            })
        };
        let gid_writer = {
            let slot = slot.clone();
            thread::spawn(move || {
                for value in 1..=UPDATES {
                    round.wait();
                    loop {
                        let mut update = slot.prepare();
                        update.builder.ids.rgid = kgid(10_000 + value);
                        match update.finish() {
                            Ok(prepared) => {
                                prepared.commit();
                                break;
                            }
                            Err(AxError::ResourceBusy) => {
                                crate::rcu::drain_credential_retire(
                                    crate::rcu::CREDENTIAL_RETIRE_CAPACITY,
                                );
                                thread::yield_now();
                            }
                            Err(error) => panic!("credential update failed: {error:?}"),
                        }
                    }
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

        let publish = |slot: &CredentialSlot, first: bool| loop {
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
            update.builder.caps = capability_state_for_test(
                [1 << cap, 0],
                [1 << cap, 0],
                [0; CAPABILITY_WORDS],
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                securebits,
            );
            match update.finish() {
                Ok(prepared) => {
                    prepared.commit();
                    break;
                }
                Err(AxError::ResourceBusy) => {
                    // `finish` has released the credential writer guard.  The
                    // test fixture has no policy worker, so make bounded RCU
                    // progress here without ever running destructors under
                    // the writer lock.
                    crate::rcu::drain_credential_retire(crate::rcu::CREDENTIAL_RETIRE_CAPACITY);
                    thread::yield_now();
                }
                Err(error) => panic!("credential update failed: {error:?}"),
            }
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
