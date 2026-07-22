use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use linux_raw_sys::general::{CAP_SETGID, CAP_SETPCAP, CAP_SETUID};
use thekernel_linux_cred::{
    CapsetAuthority, CapsetRequest, GroupIdAuthority, GroupIdTransitionInput,
    SECURE_ALL_UNPRIVILEGED, UserIdAuthority, UserIdTransitionInput, plan_capset,
    plan_group_id_transition, plan_user_id_transition,
};

use super::{
    Kgid, Kuid, Thread,
    creds::{
        CapabilityState, Cred, CredentialSlot, CredentialSnapshotGuard, CredentialUpdate,
        DacCredentialView, GroupInfo, PreparedCred, SECBIT_KEEP_CAPS,
    },
};
use crate::keyring;

/// One-shot proof that the exact credential currently installed in one slot
/// passed the typed setgroups capability hook before userspace input was read.
/// Publication revalidates immutable core authority and namespace policy, but
/// never dispatches the stacked hook a second time.
#[must_use = "setgroups admission must be consumed by the matching credential slot"]
pub(crate) struct SetgroupsAdmission {
    slot: Arc<CredentialSlot>,
    credential: Arc<Cred>,
}

impl SetgroupsAdmission {
    pub(in crate::task) fn try_new(slot: Arc<CredentialSlot>) -> AxResult<Self> {
        let credential = slot.current();
        if !credential.has_effective_capability_for_setid(CAP_SETGID)
            || !credential.user_ns().may_setgroups()
        {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(Self { slot, credential })
    }

    fn validate(&self, slot: &Arc<CredentialSlot>, current: &Arc<Cred>) -> AxResult<()> {
        if !Arc::ptr_eq(&self.slot, slot)
            || !Arc::ptr_eq(&self.credential, current)
            || !current
                .core()
                .has_effective_capability_in_own_user_ns(CAP_SETGID)
            || !current.user_ns().may_setgroups()
        {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }

    pub(crate) fn credential(&self) -> &Cred {
        &self.credential
    }

    #[cfg(test)]
    pub(in crate::task) fn validate_fixture(
        &self,
        slot: &Arc<CredentialSlot>,
        current: &Arc<Cred>,
    ) -> AxResult<()> {
        self.validate(slot, current)
    }
}

fn stage_user_id_update(
    update: &mut CredentialUpdate<'_>,
    input: UserIdTransitionInput,
) -> AxResult<(Kuid, bool)> {
    let authority = if update.old().has_effective_capability_for_setid(CAP_SETUID) {
        UserIdAuthority::CAP_SETUID
    } else {
        UserIdAuthority::UNPRIVILEGED
    };
    let (previous_fsuid, ids, capabilities, changed) = {
        let plan = plan_user_id_transition(update.old().core(), input, authority)
            .map_err(super::cred_error)?;
        (
            plan.previous_fsuid(),
            plan.ids(),
            plan.capabilities(),
            plan.changes_credential(),
        )
    };
    if changed {
        update.builder.ids = ids;
        update.builder.caps = capabilities;
    }
    Ok((previous_fsuid, changed))
}

fn stage_group_id_update(
    update: &mut CredentialUpdate<'_>,
    input: GroupIdTransitionInput,
) -> AxResult<(Kgid, bool)> {
    let authority = if update.old().has_effective_capability_for_setid(CAP_SETGID) {
        GroupIdAuthority::CAP_SETGID
    } else {
        GroupIdAuthority::UNPRIVILEGED
    };
    let (previous_fsgid, ids, changed) = {
        let plan = plan_group_id_transition(update.old().core(), input, authority)
            .map_err(super::cred_error)?;
        (plan.previous_fsgid(), plan.ids(), plan.changes_credential())
    };
    if changed {
        update.builder.ids = ids;
    }
    Ok((previous_fsgid, changed))
}

pub(in crate::task) fn prepare_user_id_update<'a>(
    credential: &'a CredentialSlot,
    input: UserIdTransitionInput,
) -> AxResult<(Kuid, Option<PreparedCred<'a>>)> {
    let mut update = credential.prepare();
    let (previous_fsuid, changed) = stage_user_id_update(&mut update, input)?;
    if !changed {
        return Ok((previous_fsuid, None));
    }
    Ok((previous_fsuid, Some(update.finish()?)))
}

pub(in crate::task) fn prepare_group_id_update<'a>(
    credential: &'a CredentialSlot,
    input: GroupIdTransitionInput,
) -> AxResult<(Kgid, Option<PreparedCred<'a>>)> {
    let mut update = credential.prepare();
    let (previous_fsgid, changed) = stage_group_id_update(&mut update, input)?;
    if !changed {
        return Ok((previous_fsgid, None));
    }
    Ok((previous_fsgid, Some(update.finish()?)))
}

pub(in crate::task) fn prepare_setfsuid_update(
    credential: &CredentialSlot,
    fsuid: Kuid,
) -> (Kuid, Option<PreparedCred<'_>>) {
    let mut update = credential.prepare();
    let previous_fsuid = update.old().ids().fsuid;
    let Ok((_, changed)) =
        stage_user_id_update(&mut update, UserIdTransitionInput::setfsuid(fsuid))
    else {
        return (previous_fsuid, None);
    };
    if !changed {
        return (previous_fsuid, None);
    }
    (previous_fsuid, update.finish().ok())
}

pub(in crate::task) fn prepare_setfsgid_update(
    credential: &CredentialSlot,
    fsgid: Kgid,
) -> (Kgid, Option<PreparedCred<'_>>) {
    let mut update = credential.prepare();
    let previous_fsgid = update.old().ids().fsgid;
    let Ok((_, changed)) =
        stage_group_id_update(&mut update, GroupIdTransitionInput::setfsgid(fsgid))
    else {
        return (previous_fsgid, None);
    };
    if !changed {
        return (previous_fsgid, None);
    }
    (previous_fsgid, update.finish().ok())
}

pub(in crate::task) fn prepare_capset_update(
    credential: &CredentialSlot,
    request: CapsetRequest,
) -> AxResult<Option<PreparedCred<'_>>> {
    let mut update = credential.prepare();
    let authority = if update
        .old()
        .has_effective_capability_in_own_user_ns(CAP_SETPCAP)
    {
        CapsetAuthority::CAP_SETPCAP
    } else {
        CapsetAuthority::RESTRICTED
    };
    let (capabilities, changed) = {
        let plan =
            plan_capset(update.old().core(), request, authority).map_err(super::cred_error)?;
        (plan.capabilities(), plan.changes_credential())
    };
    if !changed {
        return Ok(None);
    }
    update.builder.caps = capabilities;
    Ok(Some(update.finish()?))
}

pub(in crate::task) fn prepare_securebits_update(
    credential: &CredentialSlot,
    requested: u32,
) -> AxResult<Option<PreparedCred<'_>>> {
    let mut update = credential.prepare();
    let old = update.builder.caps;
    let mut proposed = old;
    proposed
        .try_set_securebits(requested)
        .map_err(super::cred_error)?;

    if !update
        .old()
        .has_effective_capability_in_own_user_ns(CAP_SETPCAP)
    {
        let changed = old.securebits() ^ requested;
        let unprivileged_mask = SECURE_ALL_UNPRIVILEGED | (SECURE_ALL_UNPRIVILEGED << 1);
        if changed == 0 || changed & !unprivileged_mask != 0 {
            return Err(AxError::OperationNotPermitted);
        }
    }
    if proposed == old {
        return Ok(None);
    }

    update.builder.caps = proposed;
    Ok(Some(update.finish()?))
}

impl Thread {
    pub fn no_new_privs(&self) -> bool {
        self.current_cred().no_new_privs()
    }

    pub fn set_no_new_privs(&self) -> AxResult<()> {
        let mut update = self.credential.prepare();
        if update.old().no_new_privs() {
            return Ok(());
        }
        update.builder.no_new_privs = true;
        self.commit_credential(update.finish()?)?;
        Ok(())
    }
}

impl Thread {
    pub(crate) fn current_cred(&self) -> Arc<Cred> {
        self.credential.current()
    }

    /// Pins this task's exact immutable credential across a composite
    /// authorization/publication operation.  Callers must acquire this before
    /// ptrace/image spin gates so credential publication keeps its established
    /// writer -> image lock order.
    pub(crate) fn lock_credential_snapshot(&self) -> CredentialSnapshotGuard<'_> {
        self.credential.lock_snapshot()
    }

    /// Nonblocking counterpart used while a ptrace publication already owns
    /// lifecycle/task-parent gates.  A busy writer makes the caller release
    /// those outer gates and retry instead of inverting the lock order.
    pub(crate) fn try_lock_credential_snapshot(&self) -> Option<CredentialSnapshotGuard<'_>> {
        self.credential.try_lock_snapshot()
    }

    fn commit_credential<'a>(&self, prepared: PreparedCred<'a>) -> AxResult<Arc<Cred>> {
        let old_ids = prepared.old_arc().ids();
        let new_ids = prepared.proposed_arc().ids();
        if old_ids.fsuid != new_ids.fsuid || old_ids.fsgid != new_ids.fsgid {
            keyring::credential_fsids_precommit(self.kernel_tid(), new_ids.fsuid, new_ids.fsgid)?;
        }
        Ok(self
            .proc_data
            .publish_credential(prepared, self.pdeath_signal_state()))
    }

    pub(crate) fn admit_setgroups(&self) -> AxResult<SetgroupsAdmission> {
        SetgroupsAdmission::try_new(self.credential_slot())
    }

    pub(crate) fn set_supplementary_groups(
        &self,
        admission: SetgroupsAdmission,
        groups: Vec<Kgid>,
    ) -> AxResult<()> {
        // Sort, deduplicate, validate the bound, and allocate the shared group
        // owner before entering the credential writer transaction.
        let groups = GroupInfo::try_new(groups).map_err(super::cred_error)?;
        let slot = self.credential_slot();
        let mut update = slot.prepare();
        admission.validate(&slot, update.old_arc())?;
        if update.old().groups().as_slice() == groups.as_slice() {
            return Ok(());
        }
        update.builder.groups = groups;
        self.commit_credential(update.finish()?)?;
        Ok(())
    }

    pub(crate) fn apply_capset(&self, request: CapsetRequest) -> AxResult<()> {
        if let Some(update) = prepare_capset_update(&self.credential, request)? {
            self.commit_credential(update)?;
        }
        Ok(())
    }
}

impl Thread {
    /// Snapshot the filesystem identity and effective capabilities used by DAC.
    pub(crate) fn fs_dac_credentials(&self) -> DacCredentialView {
        self.current_cred().fs_dac_credentials()
    }

    pub fn has_effective_capability(&self, cap: u32) -> bool {
        self.current_cred().has_effective_capability(cap)
    }

    pub fn bounding_capability_enabled(&self, cap: u32) -> AxResult<bool> {
        if CapabilityState::cap_mask(cap).is_none() {
            return Err(AxError::InvalidInput);
        }
        Ok(self.current_cred().capabilities().bounding_contains(cap))
    }

    pub fn drop_bounding_capability(&self, cap: u32) -> AxResult<()> {
        let mut update = self.credential.prepare();
        if !update
            .old()
            .has_effective_capability_in_own_user_ns(CAP_SETPCAP)
        {
            return Err(AxError::OperationNotPermitted);
        }
        update
            .builder
            .caps
            .try_drop_bounding(cap)
            .map_err(super::cred_error)?;
        self.commit_credential(update.finish()?)?;
        Ok(())
    }

    pub fn ambient_capability_enabled(&self, cap: u32) -> AxResult<bool> {
        if CapabilityState::cap_mask(cap).is_none() {
            return Err(AxError::InvalidInput);
        }
        Ok(self.current_cred().capabilities().ambient_contains(cap))
    }

    pub fn raise_ambient_capability(&self, cap: u32) -> AxResult<()> {
        let mut update = self.credential.prepare();
        update
            .builder
            .caps
            .try_raise_ambient(cap)
            .map_err(super::cred_error)?;
        self.commit_credential(update.finish()?)?;
        Ok(())
    }

    pub fn lower_ambient_capability(&self, cap: u32) -> AxResult<()> {
        let mut update = self.credential.prepare();
        update
            .builder
            .caps
            .try_lower_ambient(cap)
            .map_err(super::cred_error)?;
        self.commit_credential(update.finish()?)?;
        Ok(())
    }

    pub fn clear_ambient_capabilities(&self) -> AxResult<()> {
        let mut update = self.credential.prepare();
        if update.builder.caps.ambient() == [0; thekernel_linux_cred::CAPABILITY_WORDS] {
            return Ok(());
        }
        update.builder.caps.clear_ambient();
        self.commit_credential(update.finish()?)?;
        Ok(())
    }

    pub fn securebits(&self) -> u32 {
        self.current_cred().capabilities().securebits()
    }

    pub fn set_securebits(&self, securebits: u32) -> AxResult<()> {
        if let Some(update) = prepare_securebits_update(&self.credential, securebits)? {
            self.commit_credential(update)?;
        }
        Ok(())
    }

    pub fn keep_caps(&self) -> bool {
        self.current_cred().capabilities().securebits() & SECBIT_KEEP_CAPS != 0
    }

    pub fn set_keep_caps(&self, enabled: bool) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let old = update.builder.caps;
        update
            .builder
            .caps
            .try_set_keep_caps(enabled)
            .map_err(super::cred_error)?;
        if update.builder.caps == old {
            return Ok(());
        }
        self.commit_credential(update.finish()?)?;
        Ok(())
    }

    pub(crate) fn setuid(&self, uid: Kuid) -> AxResult<()> {
        let (_, update) =
            prepare_user_id_update(&self.credential, UserIdTransitionInput::setuid(uid))?;
        if let Some(update) = update {
            self.commit_credential(update)?;
        }
        Ok(())
    }

    pub(crate) fn setgid(&self, gid: Kgid) -> AxResult<()> {
        let (_, update) =
            prepare_group_id_update(&self.credential, GroupIdTransitionInput::setgid(gid))?;
        if let Some(update) = update {
            self.commit_credential(update)?;
        }
        Ok(())
    }

    pub(crate) fn setreuid(&self, ruid: Option<Kuid>, euid: Option<Kuid>) -> AxResult<()> {
        let (_, update) = prepare_user_id_update(
            &self.credential,
            UserIdTransitionInput::setreuid(ruid, euid),
        )?;
        if let Some(update) = update {
            self.commit_credential(update)?;
        }
        Ok(())
    }

    pub(crate) fn setregid(&self, rgid: Option<Kgid>, egid: Option<Kgid>) -> AxResult<()> {
        let (_, update) = prepare_group_id_update(
            &self.credential,
            GroupIdTransitionInput::setregid(rgid, egid),
        )?;
        if let Some(update) = update {
            self.commit_credential(update)?;
        }
        Ok(())
    }

    pub(crate) fn setresuid(
        &self,
        ruid: Option<Kuid>,
        euid: Option<Kuid>,
        suid: Option<Kuid>,
    ) -> AxResult<()> {
        let (_, update) = prepare_user_id_update(
            &self.credential,
            UserIdTransitionInput::setresuid(ruid, euid, suid),
        )?;
        if let Some(update) = update {
            self.commit_credential(update)?;
        }
        Ok(())
    }

    pub(crate) fn setresgid(
        &self,
        rgid: Option<Kgid>,
        egid: Option<Kgid>,
        sgid: Option<Kgid>,
    ) -> AxResult<()> {
        let (_, update) = prepare_group_id_update(
            &self.credential,
            GroupIdTransitionInput::setresgid(rgid, egid, sgid),
        )?;
        if let Some(update) = update {
            self.commit_credential(update)?;
        }
        Ok(())
    }

    pub(crate) fn setfsuid(&self, fsuid: Kuid) -> AxResult<Kuid> {
        let (old_fsuid, update) = prepare_setfsuid_update(&self.credential, fsuid);
        if let Some(update) = update {
            self.commit_credential(update)?;
        }
        Ok(old_fsuid)
    }

    pub(crate) fn setfsgid(&self, fsgid: Kgid) -> AxResult<Kgid> {
        let (old_fsgid, update) = prepare_setfsgid_update(&self.credential, fsgid);
        if let Some(update) = update {
            self.commit_credential(update)?;
        }
        Ok(old_fsgid)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec};

    use linux_raw_sys::general::{CAP_CHOWN, CAP_KILL, CAP_SETPCAP};

    use super::*;
    use crate::task::{
        Credentials, IdMapInputExtent, UserNamespace,
        creds::{CAPABILITY_WORDS, capability_state_for_test},
    };

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    fn ids(uid: Kuid, gid: Kgid) -> Credentials {
        Credentials {
            ruid: uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
        }
    }

    fn mapped_child_credential() -> (Arc<UserNamespace>, Arc<Cred>) {
        let initial = UserNamespace::try_new_root().unwrap();
        let initial_cred = Cred::try_root(initial.clone()).unwrap();
        let child = initial
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        child
            .publish_uid_map(
                child
                    .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 2)])
                    .unwrap(),
            )
            .unwrap();
        child
            .publish_gid_map(
                child
                    .try_build_gid_map(vec![IdMapInputExtent::new(0, 100, 2)])
                    .unwrap(),
                false,
            )
            .unwrap();

        let slot = CredentialSlot::new(
            Cred::try_with_user_namespace(&initial_cred, child.clone()).unwrap(),
        );
        let mut update = slot.prepare();
        update.builder.ids = ids(kuid(1000), kgid(100));
        let cred = update.finish().unwrap().commit();
        (child, cred)
    }

    #[test]
    fn child_user_namespace_root_selects_real_id_permitted_capabilities() {
        let (child, root_cred) = mapped_child_credential();
        assert_eq!(child.root_kuid(), Some(kuid(1000)));
        assert_eq!(child.root_kgid(), Some(kgid(100)));

        let root_view = root_cred.real_id_access_dac_credentials();
        assert!(root_view.selected_capability(CAP_CHOWN));

        let slot = CredentialSlot::new(root_cred);
        let mut update = slot.prepare();
        update.builder.ids.ruid = kuid(1001);
        let nonroot_cred = update.finish().unwrap().commit();
        let nonroot_view = nonroot_cred.real_id_access_dac_credentials();
        assert!(!nonroot_view.selected_capability(CAP_CHOWN));
    }

    #[test]
    fn child_user_namespace_uid_fixup_uses_mapped_root_not_global_zero() {
        let (child, root_cred) = mapped_child_credential();
        assert_eq!(child.root_kuid(), Some(kuid(1000)));
        let slot = CredentialSlot::new(root_cred);

        let (_, update) =
            prepare_user_id_update(&slot, UserIdTransitionInput::setuid(kuid(1001))).unwrap();
        let updated = update.unwrap().commit();

        assert_eq!(updated.ids().euid, kuid(1001));
        assert_eq!(updated.capabilities().effective(), [0; CAPABILITY_WORDS]);
        assert_eq!(updated.capabilities().permitted(), [0; CAPABILITY_WORDS]);
    }

    #[test]
    fn child_user_namespace_fsuid_fixup_uses_mapped_root() {
        let (_, root_cred) = mapped_child_credential();
        let slot = CredentialSlot::new(root_cred);
        let (old_fsuid, update) = prepare_setfsuid_update(&slot, kuid(1001));
        let updated = update.unwrap().commit();

        assert_eq!(old_fsuid, kuid(1000));
        assert!(!updated.capabilities().has_effective(CAP_CHOWN));
        assert!(updated.capabilities().has_effective(CAP_KILL));
    }

    #[test]
    fn setid_admission_uses_writer_locked_old_credential() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::new(Cred::try_root(namespace).unwrap());
        let stale_root = slot.current();
        let mut lower = slot.prepare();
        lower.builder.ids = ids(kuid(1000), kgid(100));
        let old_caps = lower.builder.caps;
        lower.builder.caps = capability_state_for_test(
            [0; CAPABILITY_WORDS],
            [0; CAPABILITY_WORDS],
            [0; CAPABILITY_WORDS],
            old_caps.bounding(),
            [0; CAPABILITY_WORDS],
            old_caps.securebits(),
        );
        lower.finish().unwrap().commit();

        assert!(stale_root.has_effective_capability_for_setid(CAP_SETUID));
        let before = slot.current();
        let error = prepare_user_id_update(&slot, UserIdTransitionInput::setuid(kuid(2000)))
            .err()
            .unwrap();
        assert_eq!(error, AxError::OperationNotPermitted);
        assert!(Arc::ptr_eq(&before, &slot.current()));
    }

    #[test]
    fn capset_with_setpcap_still_enforces_old_bounding_set() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::new(Cred::try_root(namespace).unwrap());
        let mut restrict = slot.prepare();
        restrict.builder.caps.try_drop_bounding(CAP_CHOWN).unwrap();
        restrict.finish().unwrap().commit();

        let before = slot.current();
        assert!(before.has_effective_capability_in_own_user_ns(CAP_SETPCAP));
        let capabilities = before.capabilities();
        let mut inheritable = capabilities.inheritable();
        let (word, mask) = CapabilityState::cap_mask(CAP_CHOWN).unwrap();
        inheritable[word] |= mask;
        let request = CapsetRequest::try_new(
            capabilities.effective(),
            capabilities.permitted(),
            inheritable,
        )
        .unwrap();

        let error = prepare_capset_update(&slot, request).err().unwrap();
        assert_eq!(error, AxError::OperationNotPermitted);
        assert!(Arc::ptr_eq(&before, &slot.current()));
    }

    #[test]
    fn securebits_without_setpcap_only_changes_unprivileged_bits() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::new(Cred::try_root(namespace).unwrap());
        let mut restrict = slot.prepare();
        let caps = restrict.builder.caps;
        let (word, mask) = CapabilityState::cap_mask(CAP_SETPCAP).unwrap();
        let mut effective = caps.effective();
        let mut permitted = caps.permitted();
        effective[word] &= !mask;
        permitted[word] &= !mask;
        restrict.builder.caps = capability_state_for_test(
            effective,
            permitted,
            caps.inheritable(),
            caps.bounding(),
            caps.ambient(),
            caps.securebits(),
        );
        restrict.finish().unwrap().commit();

        let requested = thekernel_linux_cred::SECBIT_EXEC_RESTRICT_FILE;
        let update = prepare_securebits_update(&slot, requested)
            .unwrap()
            .unwrap();
        let updated = update.commit();
        assert_eq!(updated.capabilities().securebits(), requested);

        let before_noop = slot.current();
        let error = prepare_securebits_update(&slot, requested).err().unwrap();
        assert_eq!(error, AxError::OperationNotPermitted);
        assert!(Arc::ptr_eq(&before_noop, &slot.current()));

        let error = prepare_securebits_update(&slot, requested | SECBIT_KEEP_CAPS)
            .err()
            .unwrap();
        assert_eq!(error, AxError::OperationNotPermitted);
        assert!(Arc::ptr_eq(&before_noop, &slot.current()));
    }

    #[test]
    fn unauthorized_setfsid_requests_return_old_without_publication() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::new(Cred::try_root(namespace).unwrap());
        let mut lower = slot.prepare();
        lower.builder.ids = ids(kuid(1000), kgid(100));
        let old_caps = lower.builder.caps;
        lower.builder.caps = capability_state_for_test(
            [0; CAPABILITY_WORDS],
            [0; CAPABILITY_WORDS],
            [0; CAPABILITY_WORDS],
            old_caps.bounding(),
            [0; CAPABILITY_WORDS],
            old_caps.securebits(),
        );
        lower.finish().unwrap().commit();

        let before_uid = slot.current();
        let (old_uid, uid_update) = prepare_setfsuid_update(&slot, kuid(2000));
        assert_eq!(old_uid, kuid(1000));
        assert!(uid_update.is_none());
        assert!(Arc::ptr_eq(&before_uid, &slot.current()));

        let before_gid = slot.current();
        let (old_gid, gid_update) = prepare_setfsgid_update(&slot, kgid(200));
        assert_eq!(old_gid, kgid(100));
        assert!(gid_update.is_none());
        assert!(Arc::ptr_eq(&before_gid, &slot.current()));
    }
}
