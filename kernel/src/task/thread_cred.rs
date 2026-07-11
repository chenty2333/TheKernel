use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use linux_raw_sys::general::{
    CAP_CHOWN, CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FOWNER, CAP_FSETID, CAP_LINUX_IMMUTABLE,
    CAP_MAC_OVERRIDE, CAP_MKNOD, CAP_SETGID, CAP_SETPCAP, CAP_SETUID,
};

use super::{
    Thread,
    creds::{
        CAPABILITY_WORDS, CapabilityState, Cred, Credentials, DacCredentialView, GroupInfo,
        PreparedCred, SECBIT_KEEP_CAPS, SECBIT_KEEP_CAPS_LOCKED, SECBIT_NO_CAP_AMBIENT_RAISE,
        SECBIT_NO_SETUID_FIXUP, SECURE_ALL_BITS, SECURE_ALL_LOCKS,
    },
};

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

    pub fn set_supplementary_groups(&self, groups: Vec<u32>) -> AxResult<()> {
        // Sort, deduplicate, validate the bound, and allocate the shared group
        // owner before entering the credential writer transaction.
        let groups = GroupInfo::try_new(groups)?;
        let mut update = self.credential.prepare();
        if !update.old().has_effective_capability(CAP_SETGID) {
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
    pub fn uid(&self) -> u32 {
        self.current_cred().ids().ruid
    }

    pub fn euid(&self) -> u32 {
        self.current_cred().ids().euid
    }

    pub fn gid(&self) -> u32 {
        self.current_cred().ids().rgid
    }

    pub fn egid(&self) -> u32 {
        self.current_cred().ids().egid
    }

    pub fn suid(&self) -> u32 {
        self.current_cred().ids().suid
    }

    pub fn fsuid(&self) -> u32 {
        self.current_cred().ids().fsuid
    }

    pub fn sgid(&self) -> u32 {
        self.current_cred().ids().sgid
    }

    pub fn fsgid(&self) -> u32 {
        self.current_cred().ids().fsgid
    }

    pub fn is_in_group(&self, gid: u32) -> bool {
        let cred = self.current_cred();
        cred.ids().egid == gid || cred.groups().contains(gid)
    }

    pub fn is_in_fs_group(&self, gid: u32) -> bool {
        let cred = self.current_cred();
        cred.ids().fsgid == gid || cred.groups().contains(gid)
    }

    /// Snapshot the filesystem identity and effective capabilities used by DAC.
    pub(crate) fn fs_dac_credentials(&self) -> DacCredentialView {
        let cred = self.current_cred();
        let credentials = cred.ids();
        let capabilities = cred.capabilities();
        DacCredentialView::new(
            credentials.fsuid,
            credentials.fsgid,
            cred.groups().clone(),
            capabilities.effective,
        )
    }

    /// Snapshot the credentials used by access(2)/faccessat2(2).
    ///
    /// Without AT_EACCESS Linux uses real IDs. Unless setuid fixups are
    /// disabled, a non-root real UID gets no capabilities while real UID 0
    /// checks with the permitted set. AT_EACCESS keeps the current filesystem
    /// IDs and effective capability set, matching Linux's normal VFS view.
    pub(crate) fn access_dac_credentials(&self, effective: bool) -> DacCredentialView {
        let cred = self.current_cred();
        let credentials = cred.ids();
        let capabilities = cred.capabilities();
        let (uid, gid, capability_set) = if effective {
            (credentials.fsuid, credentials.fsgid, capabilities.effective)
        } else {
            let capability_set = if capabilities.securebits & SECBIT_NO_SETUID_FIXUP != 0 {
                capabilities.effective
            } else if credentials.ruid == 0 {
                capabilities.permitted
            } else {
                [0; CAPABILITY_WORDS]
            };
            (credentials.ruid, credentials.rgid, capability_set)
        };
        DacCredentialView::new(uid, gid, cred.groups().clone(), capability_set)
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
        if !update.old().has_effective_capability(CAP_SETPCAP) {
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
        if !update.old().has_effective_capability(CAP_SETPCAP) {
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
        if old.euid == 0 && new.euid != 0 {
            caps.effective = [0; CAPABILITY_WORDS];
            if old.ruid == 0
                && old.suid == 0
                && new.ruid != 0
                && new.euid != 0
                && new.suid != 0
                && caps.securebits & SECBIT_KEEP_CAPS == 0
            {
                caps.permitted = [0; CAPABILITY_WORDS];
                caps.clear_ambient();
            }
        } else if old.euid != 0 && new.euid == 0 {
            caps.effective = caps.permitted;
        }
    }

    fn fixup_capabilities_for_fsuid_change(
        old_fsuid: u32,
        new_fsuid: u32,
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
            if old_fsuid == 0 && new_fsuid != 0 {
                caps.effective[word] &= !mask;
            } else if old_fsuid != 0 && new_fsuid == 0 && caps.permitted[word] & mask != 0 {
                caps.effective[word] |= mask;
            }
        }
    }

    pub fn setuid(&self, uid: u32) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let can_setuid = update.old().has_effective_capability(CAP_SETUID);
        let old = update.builder.ids;
        if can_setuid {
            update.builder.ids.ruid = uid;
            update.builder.ids.euid = uid;
            update.builder.ids.suid = uid;
            update.builder.ids.fsuid = uid;
            Self::fixup_capabilities_for_uid_change(
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
                old,
                update.builder.ids,
                &mut update.builder.caps,
            );
            update.finish()?.commit();
            return Ok(());
        }
        Err(AxError::OperationNotPermitted)
    }

    pub fn setgid(&self, gid: u32) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let can_setgid = update.old().has_effective_capability(CAP_SETGID);
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

    pub fn setreuid(&self, ruid: Option<u32>, euid: Option<u32>) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let can_setuid = update.old().has_effective_capability(CAP_SETUID);
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
        Self::fixup_capabilities_for_uid_change(old, update.builder.ids, &mut update.builder.caps);
        update.finish()?.commit();
        Ok(())
    }

    pub fn setregid(&self, rgid: Option<u32>, egid: Option<u32>) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let can_setgid = update.old().has_effective_capability(CAP_SETGID);
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

    pub fn setresuid(
        &self,
        ruid: Option<u32>,
        euid: Option<u32>,
        suid: Option<u32>,
    ) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let can_setuid = update.old().has_effective_capability(CAP_SETUID);
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
        Self::fixup_capabilities_for_uid_change(old, update.builder.ids, &mut update.builder.caps);
        update.finish()?.commit();
        Ok(())
    }

    pub fn setresgid(
        &self,
        rgid: Option<u32>,
        egid: Option<u32>,
        sgid: Option<u32>,
    ) -> AxResult<()> {
        let mut update = self.credential.prepare();
        let can_setgid = update.old().has_effective_capability(CAP_SETGID);
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

    pub fn setfsuid(&self, fsuid: u32) -> AxResult<u32> {
        let mut update = self.credential.prepare();
        let can_setuid = update.old().has_effective_capability(CAP_SETUID);
        let old_fsuid = update.builder.ids.fsuid;
        if fsuid == u32::MAX {
            return Ok(old_fsuid);
        }
        if can_setuid
            || fsuid == update.builder.ids.ruid
            || fsuid == update.builder.ids.euid
            || fsuid == update.builder.ids.suid
            || fsuid == update.builder.ids.fsuid
        {
            update.builder.ids.fsuid = fsuid;
        }
        if update.builder.ids.fsuid == old_fsuid {
            return Ok(old_fsuid);
        }
        Self::fixup_capabilities_for_fsuid_change(
            old_fsuid,
            update.builder.ids.fsuid,
            &mut update.builder.caps,
        );
        update.finish()?.commit();
        Ok(old_fsuid)
    }

    pub fn setfsgid(&self, fsgid: u32) -> AxResult<u32> {
        let mut update = self.credential.prepare();
        let can_setgid = update.old().has_effective_capability(CAP_SETGID);
        let old_fsgid = update.builder.ids.fsgid;
        if fsgid == u32::MAX {
            return Ok(old_fsgid);
        }
        if can_setgid
            || fsgid == update.builder.ids.rgid
            || fsgid == update.builder.ids.egid
            || fsgid == update.builder.ids.sgid
            || fsgid == update.builder.ids.fsgid
        {
            update.builder.ids.fsgid = fsgid;
        }
        if update.builder.ids.fsgid == old_fsgid {
            return Ok(old_fsgid);
        }
        update.finish()?.commit();
        Ok(old_fsgid)
    }
}
