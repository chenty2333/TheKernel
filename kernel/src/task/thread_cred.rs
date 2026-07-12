use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use linux_raw_sys::general::{
    CAP_CHOWN, CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FOWNER, CAP_FSETID, CAP_LINUX_IMMUTABLE,
    CAP_MAC_OVERRIDE, CAP_MKNOD, CAP_SETGID, CAP_SETPCAP, CAP_SETUID,
};

use super::{
    Thread,
    creds::{
        CAPABILITY_WORDS, CapabilityState, Cred, CredentialSlot, Credentials, DacCredentialView,
        GroupInfo, PreparedCred, SECBIT_KEEP_CAPS, SECBIT_KEEP_CAPS_LOCKED,
        SECBIT_NO_CAP_AMBIENT_RAISE, SECBIT_NO_SETUID_FIXUP, SECURE_ALL_BITS, SECURE_ALL_LOCKS,
    },
    idmap::{Kgid, Kuid},
};

fn access_dac_credentials_for(cred: &Cred, effective: bool) -> DacCredentialView {
    let credentials = cred.ids();
    let capabilities = cred.capabilities();
    let root_kuid = cred.user_ns().root_kuid();
    let (uid, gid, capability_set) = if effective {
        (credentials.fsuid, credentials.fsgid, capabilities.effective)
    } else {
        let capability_set = if capabilities.securebits & SECBIT_NO_SETUID_FIXUP != 0 {
            capabilities.effective
        } else if root_kuid == Some(credentials.ruid) {
            capabilities.permitted
        } else {
            [0; CAPABILITY_WORDS]
        };
        (credentials.ruid, credentials.rgid, capability_set)
    };
    DacCredentialView::new(
        uid,
        gid,
        cred.groups().clone(),
        capability_set,
        cred.user_ns().is_initial(),
    )
}

fn prepare_setfsuid_update<'a>(
    credential: &'a CredentialSlot,
    fsuid: Kuid,
) -> AxResult<(Kuid, Option<PreparedCred<'a>>)> {
    let mut update = credential.prepare();
    let root_kuid = update.old().user_ns().root_kuid();
    let can_setuid = update
        .old()
        .has_effective_capability_in_own_user_ns(CAP_SETUID);
    let old_fsuid = update.builder.ids.fsuid;
    if can_setuid
        || fsuid == update.builder.ids.ruid
        || fsuid == update.builder.ids.euid
        || fsuid == update.builder.ids.suid
        || fsuid == update.builder.ids.fsuid
    {
        update.builder.ids.fsuid = fsuid;
    }
    if update.builder.ids.fsuid == old_fsuid {
        return Ok((old_fsuid, None));
    }
    Thread::fixup_capabilities_for_fsuid_change(
        root_kuid,
        old_fsuid,
        update.builder.ids.fsuid,
        &mut update.builder.caps,
    );
    Ok((old_fsuid, Some(update.finish()?)))
}

fn prepare_setfsgid_update<'a>(
    credential: &'a CredentialSlot,
    fsgid: Kgid,
) -> AxResult<(Kgid, Option<PreparedCred<'a>>)> {
    let mut update = credential.prepare();
    let can_setgid = update
        .old()
        .has_effective_capability_in_own_user_ns(CAP_SETGID);
    let old_fsgid = update.builder.ids.fsgid;
    if can_setgid
        || fsgid == update.builder.ids.rgid
        || fsgid == update.builder.ids.egid
        || fsgid == update.builder.ids.sgid
        || fsgid == update.builder.ids.fsgid
    {
        update.builder.ids.fsgid = fsgid;
    }
    if update.builder.ids.fsgid == old_fsgid {
        return Ok((old_fsgid, None));
    }
    Ok((old_fsgid, Some(update.finish()?)))
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
        update.finish()?.commit();
        Ok(())
    }
}

impl Thread {
    pub(crate) fn current_cred(&self) -> Arc<Cred> {
        self.credential.current()
    }

    pub(crate) fn set_supplementary_groups(&self, groups: Vec<Kgid>) -> AxResult<()> {
        // Sort, deduplicate, validate the bound, and allocate the shared group
        // owner before entering the credential writer transaction.
        let groups = GroupInfo::try_new(groups)?;
        let mut update = self.credential.prepare();
        if !update.old().user_ns().may_setgroups()
            || !update
                .old()
                .has_effective_capability_in_own_user_ns(CAP_SETGID)
        {
            return Err(AxError::OperationNotPermitted);
        }
        if update.old().groups().as_slice() == groups.as_slice() {
            return Ok(());
        }
        update.builder.groups = groups;
        update.finish()?.commit();
        Ok(())
    }

    pub(crate) fn try_update_capability_state(
        &self,
        update_state: impl FnOnce(CapabilityState, &mut CapabilityState) -> AxResult<()>,
    ) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let old = update.builder.caps;
        update_state(old, &mut update.builder.caps)?;
        if update.builder.caps == old {
            return Ok(());
        }
        update.finish()?.commit();
        Ok(())
    }
}

impl Thread {
    /// Snapshot the filesystem identity and effective capabilities used by DAC.
    pub(crate) fn fs_dac_credentials(&self) -> DacCredentialView {
        self.current_cred().fs_dac_credentials()
    }

    /// Snapshot the credentials used by access(2)/faccessat2(2).
    ///
    /// Without AT_EACCESS Linux uses real IDs. Unless setuid fixups are
    /// disabled, a non-root real UID gets no capabilities while real UID 0
    /// checks with the permitted set. AT_EACCESS keeps the current filesystem
    /// IDs and effective capability set, matching Linux's normal VFS view.
    pub(crate) fn access_dac_credentials(&self, effective: bool) -> DacCredentialView {
        let cred = self.current_cred();
        access_dac_credentials_for(&cred, effective)
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
        update.builder.caps.drop_bounding(cap)?;
        update.finish()?.commit();
        Ok(())
    }

    pub fn ambient_capability_enabled(&self, cap: u32) -> AxResult<bool> {
        if CapabilityState::cap_mask(cap).is_none() {
            return Err(AxError::InvalidInput);
        }
        Ok(self.current_cred().capabilities().ambient_contains(cap))
    }

    pub fn raise_ambient_capability(&self, cap: u32) -> AxResult<()> {
        let Some((word, mask)) = CapabilityState::cap_mask(cap) else {
            return Err(AxError::InvalidInput);
        };
        let mut update = self.credential.prepare();
        if update.builder.caps.securebits & SECBIT_NO_CAP_AMBIENT_RAISE != 0
            || update.builder.caps.permitted[word] & mask == 0
            || update.builder.caps.inheritable[word] & mask == 0
        {
            return Err(AxError::OperationNotPermitted);
        }
        update.builder.caps.raise_ambient(cap)?;
        update.finish()?.commit();
        Ok(())
    }

    pub fn lower_ambient_capability(&self, cap: u32) -> AxResult<()> {
        let mut update = self.credential.prepare();
        update.builder.caps.lower_ambient(cap)?;
        update.finish()?.commit();
        Ok(())
    }

    pub fn clear_ambient_capabilities(&self) -> AxResult<()> {
        let mut update = self.credential.prepare();
        if update.builder.caps.ambient == [0; CAPABILITY_WORDS] {
            return Ok(());
        }
        update.builder.caps.clear_ambient();
        update.finish()?.commit();
        Ok(())
    }

    pub fn securebits(&self) -> u32 {
        self.current_cred().capabilities().securebits
    }

    pub fn set_securebits(&self, securebits: u32) -> AxResult<()> {
        let mut update = self.credential.prepare();
        if !update
            .old()
            .has_effective_capability_in_own_user_ns(CAP_SETPCAP)
        {
            return Err(AxError::OperationNotPermitted);
        }
        if update.builder.caps.securebits == securebits {
            return Ok(());
        }
        if (((update.builder.caps.securebits & SECURE_ALL_LOCKS) >> 1)
            & (update.builder.caps.securebits ^ securebits))
            != 0
            || (update.builder.caps.securebits & SECURE_ALL_LOCKS & !securebits) != 0
            || (securebits & !(SECURE_ALL_LOCKS | SECURE_ALL_BITS)) != 0
        {
            return Err(AxError::OperationNotPermitted);
        }
        update.builder.caps.securebits = securebits;
        update.finish()?.commit();
        Ok(())
    }

    pub fn keep_caps(&self) -> bool {
        self.current_cred().capabilities().securebits & SECBIT_KEEP_CAPS != 0
    }

    pub fn set_keep_caps(&self, enabled: bool) -> AxResult<()> {
        let mut update = self.credential.prepare();
        if update.builder.caps.securebits & SECBIT_KEEP_CAPS_LOCKED != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        if (update.builder.caps.securebits & SECBIT_KEEP_CAPS != 0) == enabled {
            return Ok(());
        }
        if enabled {
            update.builder.caps.securebits |= SECBIT_KEEP_CAPS;
        } else {
            update.builder.caps.securebits &= !SECBIT_KEEP_CAPS;
        }
        update.finish()?.commit();
        Ok(())
    }

    pub(crate) fn prepare_clear_keep_caps_on_exec(&self) -> AxResult<Option<PreparedCred<'_>>> {
        let mut update = self.credential.prepare();
        if update.builder.caps.securebits & SECBIT_KEEP_CAPS == 0 {
            return Ok(None);
        }
        update.builder.caps.securebits &= !SECBIT_KEEP_CAPS;
        Ok(Some(update.finish_exec_keep_caps_clear()?))
    }

    fn fixup_capabilities_for_uid_change(
        root_kuid: Option<Kuid>,
        old: Credentials,
        new: Credentials,
        caps: &mut CapabilityState,
    ) {
        if old.ruid == new.ruid && old.euid == new.euid && old.suid == new.suid {
            return;
        }

        if caps.securebits & SECBIT_NO_SETUID_FIXUP != 0 {
            return;
        }
        let old_had_root = [old.ruid, old.euid, old.suid]
            .into_iter()
            .any(|id| root_kuid == Some(id));
        let new_has_root = [new.ruid, new.euid, new.suid]
            .into_iter()
            .any(|id| root_kuid == Some(id));
        if old_had_root && !new_has_root {
            if caps.securebits & SECBIT_KEEP_CAPS == 0 {
                caps.permitted = [0; CAPABILITY_WORDS];
                caps.effective = [0; CAPABILITY_WORDS];
            }
            caps.clear_ambient();
        }
        if root_kuid == Some(old.euid) && root_kuid != Some(new.euid) {
            caps.effective = [0; CAPABILITY_WORDS];
        }
        if root_kuid != Some(old.euid) && root_kuid == Some(new.euid) {
            caps.effective = caps.permitted;
        }
    }

    fn fixup_capabilities_for_fsuid_change(
        root_kuid: Option<Kuid>,
        old_fsuid: Kuid,
        new_fsuid: Kuid,
        caps: &mut CapabilityState,
    ) {
        if old_fsuid == new_fsuid {
            return;
        }

        const FS_CAPS: [u32; 8] = [
            CAP_CHOWN,
            CAP_MKNOD,
            CAP_DAC_OVERRIDE,
            CAP_DAC_READ_SEARCH,
            CAP_FOWNER,
            CAP_FSETID,
            CAP_MAC_OVERRIDE,
            CAP_LINUX_IMMUTABLE,
        ];

        if caps.securebits & SECBIT_NO_SETUID_FIXUP != 0 {
            return;
        }
        for cap in FS_CAPS {
            let Some((word, mask)) = CapabilityState::cap_mask(cap) else {
                continue;
            };
            if root_kuid == Some(old_fsuid) && root_kuid != Some(new_fsuid) {
                caps.effective[word] &= !mask;
            } else if root_kuid != Some(old_fsuid)
                && root_kuid == Some(new_fsuid)
                && caps.permitted[word] & mask != 0
            {
                caps.effective[word] |= mask;
            }
        }
    }

    pub(crate) fn setuid(&self, uid: Kuid) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let root_kuid = update.old().user_ns().root_kuid();
        let can_setuid = update
            .old()
            .has_effective_capability_in_own_user_ns(CAP_SETUID);
        let old = update.builder.ids;
        if can_setuid {
            update.builder.ids.ruid = uid;
            update.builder.ids.euid = uid;
            update.builder.ids.suid = uid;
            update.builder.ids.fsuid = uid;
            Self::fixup_capabilities_for_uid_change(
                root_kuid,
                old,
                update.builder.ids,
                &mut update.builder.caps,
            );
            update.finish()?.commit();
            return Ok(());
        }
        if uid == old.ruid || uid == old.suid {
            update.builder.ids.euid = uid;
            update.builder.ids.fsuid = uid;
            Self::fixup_capabilities_for_uid_change(
                root_kuid,
                old,
                update.builder.ids,
                &mut update.builder.caps,
            );
            update.finish()?.commit();
            return Ok(());
        }
        Err(AxError::OperationNotPermitted)
    }

    pub(crate) fn setgid(&self, gid: Kgid) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let can_setgid = update
            .old()
            .has_effective_capability_in_own_user_ns(CAP_SETGID);
        let old = update.builder.ids;
        if can_setgid {
            update.builder.ids.rgid = gid;
            update.builder.ids.egid = gid;
            update.builder.ids.sgid = gid;
            update.builder.ids.fsgid = gid;
            update.finish()?.commit();
            return Ok(());
        }
        if gid == old.rgid || gid == old.sgid {
            update.builder.ids.egid = gid;
            update.builder.ids.fsgid = gid;
            update.finish()?.commit();
            return Ok(());
        }
        Err(AxError::OperationNotPermitted)
    }

    pub(crate) fn setreuid(&self, ruid: Option<Kuid>, euid: Option<Kuid>) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let root_kuid = update.old().user_ns().root_kuid();
        let can_setuid = update
            .old()
            .has_effective_capability_in_own_user_ns(CAP_SETUID);
        let old = update.builder.ids;
        if !can_setuid {
            if let Some(id) = ruid
                && id != old.ruid
                && id != old.euid
            {
                return Err(AxError::OperationNotPermitted);
            }
            if let Some(id) = euid
                && id != old.ruid
                && id != old.euid
                && id != old.suid
            {
                return Err(AxError::OperationNotPermitted);
            }
        }

        let new_ruid = ruid.unwrap_or(old.ruid);
        let new_euid = euid.unwrap_or(old.euid);
        update.builder.ids.ruid = new_ruid;
        update.builder.ids.euid = new_euid;
        update.builder.ids.fsuid = new_euid;
        if ruid.is_some() || euid.is_some_and(|id| id != old.ruid) {
            update.builder.ids.suid = new_euid;
        }
        Self::fixup_capabilities_for_uid_change(
            root_kuid,
            old,
            update.builder.ids,
            &mut update.builder.caps,
        );
        update.finish()?.commit();
        Ok(())
    }

    pub(crate) fn setregid(&self, rgid: Option<Kgid>, egid: Option<Kgid>) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let can_setgid = update
            .old()
            .has_effective_capability_in_own_user_ns(CAP_SETGID);
        let old = update.builder.ids;
        if !can_setgid {
            if let Some(id) = rgid
                && id != old.rgid
                && id != old.egid
            {
                return Err(AxError::OperationNotPermitted);
            }
            if let Some(id) = egid
                && id != old.rgid
                && id != old.egid
                && id != old.sgid
            {
                return Err(AxError::OperationNotPermitted);
            }
        }

        let new_rgid = rgid.unwrap_or(old.rgid);
        let new_egid = egid.unwrap_or(old.egid);
        update.builder.ids.rgid = new_rgid;
        update.builder.ids.egid = new_egid;
        update.builder.ids.fsgid = new_egid;
        if rgid.is_some() || egid.is_some_and(|id| id != old.rgid) {
            update.builder.ids.sgid = new_egid;
        }
        update.finish()?.commit();
        Ok(())
    }

    pub(crate) fn setresuid(
        &self,
        ruid: Option<Kuid>,
        euid: Option<Kuid>,
        suid: Option<Kuid>,
    ) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let root_kuid = update.old().user_ns().root_kuid();
        let can_setuid = update
            .old()
            .has_effective_capability_in_own_user_ns(CAP_SETUID);
        let old = update.builder.ids;
        if !can_setuid {
            for id in [ruid, euid, suid].into_iter().flatten() {
                if id != old.ruid && id != old.euid && id != old.suid {
                    return Err(AxError::OperationNotPermitted);
                }
            }
        }

        if let Some(id) = ruid {
            update.builder.ids.ruid = id;
        }
        if let Some(id) = euid {
            update.builder.ids.euid = id;
        }
        if let Some(id) = suid {
            update.builder.ids.suid = id;
        }
        update.builder.ids.fsuid = update.builder.ids.euid;
        Self::fixup_capabilities_for_uid_change(
            root_kuid,
            old,
            update.builder.ids,
            &mut update.builder.caps,
        );
        update.finish()?.commit();
        Ok(())
    }

    pub(crate) fn setresgid(
        &self,
        rgid: Option<Kgid>,
        egid: Option<Kgid>,
        sgid: Option<Kgid>,
    ) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let can_setgid = update
            .old()
            .has_effective_capability_in_own_user_ns(CAP_SETGID);
        let old = update.builder.ids;
        if !can_setgid {
            for id in [rgid, egid, sgid].into_iter().flatten() {
                if id != old.rgid && id != old.egid && id != old.sgid {
                    return Err(AxError::OperationNotPermitted);
                }
            }
        }

        if let Some(id) = rgid {
            update.builder.ids.rgid = id;
        }
        if let Some(id) = egid {
            update.builder.ids.egid = id;
        }
        if let Some(id) = sgid {
            update.builder.ids.sgid = id;
        }
        update.builder.ids.fsgid = update.builder.ids.egid;
        update.finish()?.commit();
        Ok(())
    }

    pub(crate) fn setfsuid(&self, fsuid: Kuid) -> AxResult<Kuid> {
        let (old_fsuid, update) = prepare_setfsuid_update(&self.credential, fsuid)?;
        if let Some(update) = update {
            update.commit();
        }
        Ok(old_fsuid)
    }

    pub(crate) fn setfsgid(&self, fsgid: Kgid) -> AxResult<Kgid> {
        let (old_fsgid, update) = prepare_setfsgid_update(&self.credential, fsgid)?;
        if let Some(update) = update {
            update.commit();
        }
        Ok(old_fsgid)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec};

    use linux_raw_sys::general::{CAP_CHOWN, CAP_KILL};

    use super::*;
    use crate::task::{IdMapInputExtent, UserNamespace};

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
        let child = initial.try_fork(kuid(1000), kgid(100), false).unwrap();
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

        let slot =
            CredentialSlot::new(Cred::try_with_user_ns(&initial_cred, child.clone()).unwrap());
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

        let root_view = access_dac_credentials_for(&root_cred, false);
        assert!(root_view.selected_capability(CAP_CHOWN));

        let slot = CredentialSlot::new(root_cred);
        let mut update = slot.prepare();
        update.builder.ids.ruid = kuid(1001);
        let nonroot_cred = update.finish().unwrap().commit();
        let nonroot_view = access_dac_credentials_for(&nonroot_cred, false);
        assert!(!nonroot_view.selected_capability(CAP_CHOWN));
    }

    #[test]
    fn child_user_namespace_uid_fixup_uses_mapped_root_not_global_zero() {
        let root_kuid = kuid(1000);
        let old = ids(root_kuid, kgid(100));
        let new = ids(kuid(1001), kgid(101));

        let mut mapped_root_caps = CapabilityState::full();
        Thread::fixup_capabilities_for_uid_change(Some(root_kuid), old, new, &mut mapped_root_caps);
        assert_eq!(mapped_root_caps.effective, [0; CAPABILITY_WORDS]);
        assert_eq!(mapped_root_caps.permitted, [0; CAPABILITY_WORDS]);

        let mut wrong_global_root_caps = CapabilityState::full();
        Thread::fixup_capabilities_for_uid_change(
            Some(Kuid::INITIAL_ROOT),
            old,
            new,
            &mut wrong_global_root_caps,
        );
        assert_eq!(wrong_global_root_caps, CapabilityState::full());
    }

    #[test]
    fn uid_fixup_drops_capabilities_when_any_old_id_was_namespace_root() {
        let root = kuid(1000);
        let user = kuid(1001);
        let mut old = ids(user, kgid(100));
        old.euid = root;
        let new = ids(user, kgid(100));
        let mut caps = CapabilityState::full();
        caps.inheritable[0] = 1;
        caps.ambient[0] = 1;

        Thread::fixup_capabilities_for_uid_change(Some(root), old, new, &mut caps);

        assert_eq!(caps.permitted, [0; CAPABILITY_WORDS]);
        assert_eq!(caps.effective, [0; CAPABILITY_WORDS]);
        assert_eq!(caps.ambient, [0; CAPABILITY_WORDS]);
    }

    #[test]
    fn uid_fixup_keep_caps_still_clears_ambient_when_last_root_id_is_lost() {
        let root = kuid(1000);
        let user = kuid(1001);
        let mut old = ids(user, kgid(100));
        old.ruid = root;
        let new = ids(user, kgid(100));
        let mut caps = CapabilityState::full();
        caps.securebits |= SECBIT_KEEP_CAPS;
        caps.inheritable[0] = 1;
        caps.ambient[0] = 1;
        let expected_permitted = caps.permitted;
        let expected_effective = caps.effective;

        Thread::fixup_capabilities_for_uid_change(Some(root), old, new, &mut caps);

        assert_eq!(caps.permitted, expected_permitted);
        assert_eq!(caps.effective, expected_effective);
        assert_eq!(caps.ambient, [0; CAPABILITY_WORDS]);
    }

    #[test]
    fn child_user_namespace_fsuid_fixup_uses_mapped_root() {
        let mut caps = CapabilityState::full();
        Thread::fixup_capabilities_for_fsuid_change(
            Some(kuid(1000)),
            kuid(1000),
            kuid(1001),
            &mut caps,
        );

        assert!(!caps.has_effective(CAP_CHOWN));
        assert!(caps.has_effective(CAP_KILL));
    }

    #[test]
    fn unauthorized_setfsid_requests_return_old_without_publication() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::new(Cred::try_root(namespace).unwrap());
        let mut lower = slot.prepare();
        lower.builder.ids = ids(kuid(1000), kgid(100));
        lower.builder.caps.effective = [0; CAPABILITY_WORDS];
        lower.builder.caps.permitted = [0; CAPABILITY_WORDS];
        lower.builder.caps.clear_ambient();
        lower.finish().unwrap().commit();

        let before_uid = slot.current();
        let (old_uid, uid_update) = prepare_setfsuid_update(&slot, kuid(2000)).unwrap();
        assert_eq!(old_uid, kuid(1000));
        assert!(uid_update.is_none());
        assert!(Arc::ptr_eq(&before_uid, &slot.current()));

        let before_gid = slot.current();
        let (old_gid, gid_update) = prepare_setfsgid_update(&slot, kgid(200)).unwrap();
        assert_eq!(old_gid, kgid(100));
        assert!(gid_update.is_none());
        assert!(Arc::ptr_eq(&before_gid, &slot.current()));
    }
}
