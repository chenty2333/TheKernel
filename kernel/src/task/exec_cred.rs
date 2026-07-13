//! Transactional Linux exec credential derivation.
//!
//! This module contains the Linux-visible credential algebra and a prepared
//! publication token. Executable loading and process-image replacement stay
//! outside it, so parsing, derivation, allocation, and security authorization
//! can all finish before the exec commit point.

use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use linux_raw_sys::general::CAP_SETUID;

use super::{
    Dumpability, FileCapabilities, Kgid, Kuid, Thread, UserNamespace, cred_error,
    creds::{
        CAPABILITY_WORDS, CapabilityState, Cred, CredentialUpdate, Credentials, PreparedCred,
        SECBIT_KEEP_CAPS, SECBIT_NOROOT,
    },
};

/// Maps the policy-neutral parser error at the kernel adapter boundary.
pub(crate) fn parse_file_capabilities(value: &[u8]) -> AxResult<FileCapabilities> {
    thekernel_linux_cred::parse_file_capabilities(value).map_err(cred_error)
}

/// Facts frozen from the final executable and the pre-exec tracing state.
///
/// The loader must derive `set_gid` only when both the set-GID and group
/// execute mode bits are present, as Linux's `bprm_fill_uid()` does.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExecCredentialRequest {
    pub(crate) file_uid: Option<Kuid>,
    pub(crate) file_gid: Option<Kgid>,
    pub(crate) set_uid: bool,
    pub(crate) set_gid: bool,
    pub(crate) nosuid: bool,
    pub(crate) ptrace_suppresses_privilege: bool,
    pub(crate) executable_unreadable: bool,
    pub(crate) file_capabilities: Option<FileCapabilities>,
}

/// Identity values installed in the new ELF auxiliary vector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecAuxIdentity {
    pub(crate) uid: usize,
    pub(crate) euid: usize,
    pub(crate) gid: usize,
    pub(crate) egid: usize,
    pub(crate) secure: bool,
}

impl ExecAuxIdentity {
    pub(crate) const fn trusted_boot() -> Self {
        Self {
            uid: 0,
            euid: 0,
            gid: 0,
            egid: 0,
            secure: false,
        }
    }

    fn from_ids(ids: Credentials, user_ns: &UserNamespace, secure: bool) -> Self {
        Self {
            uid: user_ns.from_kuid_munged(ids.ruid) as usize,
            euid: user_ns.from_kuid_munged(ids.euid) as usize,
            gid: user_ns.from_kgid_munged(ids.rgid) as usize,
            egid: user_ns.from_kgid_munged(ids.egid) as usize,
            secure,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecCredentialEffects {
    pub(crate) aux_identity: ExecAuxIdentity,
    pub(crate) dumpability: Dumpability,
    /// Applies only to the exact thread executing the transition.
    pub(crate) clear_pdeath_signal: bool,
    pub(crate) secure_exec: bool,
    pub(crate) ptrace_suppressed: bool,
    pub(crate) gains_file_privilege: bool,
}

struct DerivedExecCredential {
    ids: Credentials,
    caps: CapabilityState,
    effects: ExecCredentialEffects,
}

fn file_root_owns_current_namespace(user_ns: &Arc<UserNamespace>, rootid: Kuid) -> bool {
    let mut namespace = Some(user_ns.clone());
    while let Some(current) = namespace {
        if current.root_kuid() == Some(rootid) {
            return true;
        }
        namespace = current.parent();
    }
    false
}

fn any_bits_outside(left: [u32; CAPABILITY_WORDS], right: [u32; CAPABILITY_WORDS]) -> bool {
    (0..CAPABILITY_WORDS).any(|word| left[word] & !right[word] != 0)
}

fn derive_exec_credential(
    old: &Cred,
    request: ExecCredentialRequest,
) -> AxResult<DerivedExecCredential> {
    let old_ids = old.ids();
    let old_caps = CapabilityState::from_committed(old.capabilities());
    let root_kuid = old.user_ns().root_kuid();

    // Linux applies set-ID before commoncap's unsafe-exec downgrade. nosuid
    // and no_new_privs prevent bprm_fill_uid() from applying inode bits.
    let mut ids = old_ids;
    if !request.nosuid && !old.no_new_privs() {
        if request.set_uid
            && let Some(file_uid) = request.file_uid
            && old.user_ns().kernel_uid_to_user(file_uid).is_some()
        {
            ids.euid = file_uid;
        }
        if request.set_gid
            && let Some(file_gid) = request.file_gid
            && old.user_ns().kernel_gid_to_user(file_gid).is_some()
        {
            ids.egid = file_gid;
        }
    }

    // Linux's __is_setuid/__is_setgid compare the proposed effective IDs to
    // the old real IDs. Keep this fact across a later unsafe downgrade: it
    // controls ambient clearing and AT_SECURE even if IDs are restored.
    let is_setid = ids.euid != old_ids.ruid || ids.egid != old_ids.rgid;

    // An xattr suppressed by nosuid disappears. no_new_privs instead lets
    // commoncap derive it and then intersects a gaining transition with pP.
    let file_capabilities = (!request.nosuid)
        .then_some(request.file_capabilities)
        .flatten()
        .filter(|caps| file_root_owns_current_namespace(old.user_ns(), caps.rootid()));
    let has_fcap = file_capabilities.is_some();

    let mut file_permitted = [0; CAPABILITY_WORDS];
    let mut file_inheritable = [0; CAPABILITY_WORDS];
    let mut file_effective = false;
    if let Some(file) = file_capabilities {
        file_permitted = file.permitted();
        file_inheritable = file.inheritable();
        file_effective = file.effective();
    }

    // pP' before ambient is (pB & fP) | (pI & fI).
    let mut permitted_without_ambient = [0; CAPABILITY_WORDS];
    for word in 0..CAPABILITY_WORDS {
        permitted_without_ambient[word] = (old_caps.bounding[word] & file_permitted[word])
            | (old_caps.inheritable[word] & file_inheritable[word]);
    }
    if file_effective && any_bits_outside(file_permitted, permitted_without_ambient) {
        // Forced fP may arrive through either the bounding or inheritable
        // path. An effective record which cannot supply all fP must fail.
        return Err(AxError::OperationNotPermitted);
    }

    // Legacy root compatibility is disabled by SECBIT_NOROOT. A setuid-root
    // executable carrying a valid file-capability record receives only its
    // explicit file capability sets, even if euid was already root.
    let proposed_is_setuid_root = root_kuid != Some(ids.ruid) && root_kuid == Some(ids.euid);
    let root_compat = old_caps.securebits & SECBIT_NOROOT == 0
        && (root_kuid == Some(ids.ruid) || root_kuid == Some(ids.euid))
        && !(has_fcap && proposed_is_setuid_root);
    if root_compat {
        for word in 0..CAPABILITY_WORDS {
            permitted_without_ambient[word] = old_caps.bounding[word] | old_caps.inheritable[word];
        }
        if root_kuid == Some(ids.euid) {
            file_effective = true;
        }
    }

    let permitted_gained_before_unsafe =
        any_bits_outside(permitted_without_ambient, old_caps.permitted);
    let privilege_transition = is_setid || permitted_gained_before_unsafe;
    if privilege_transition && (old.no_new_privs() || request.ptrace_suppresses_privilege) {
        if old.no_new_privs() || !old.has_effective_capability_in_own_user_ns(CAP_SETUID) {
            ids.euid = ids.ruid;
            ids.egid = ids.rgid;
        }
        for word in 0..CAPABILITY_WORDS {
            permitted_without_ambient[word] &= old_caps.permitted[word];
        }
    }

    ids.suid = ids.euid;
    ids.fsuid = ids.euid;
    ids.sgid = ids.egid;
    ids.fsgid = ids.egid;

    // File capabilities or a set-ID image cancel ambient capabilities, even
    // if unsafe-exec handling subsequently restored the effective IDs.
    let ambient = if has_fcap || is_setid {
        [0; CAPABILITY_WORDS]
    } else {
        old_caps.ambient
    };
    let mut permitted = permitted_without_ambient;
    for word in 0..CAPABILITY_WORDS {
        permitted[word] |= ambient[word];
    }
    let effective = if file_effective { permitted } else { ambient };
    let mut caps = CapabilityState {
        effective,
        permitted,
        inheritable: old_caps.inheritable,
        bounding: old_caps.bounding,
        ambient,
        securebits: old_caps.securebits & !SECBIT_KEEP_CAPS,
    };
    caps.reconcile_ambient();

    let changed_effective_ids = old_ids.euid != ids.euid || old_ids.egid != ids.egid;
    let changed_effective_or_fs_ids =
        changed_effective_ids || old_ids.fsuid != ids.fsuid || old_ids.fsgid != ids.fsgid;
    let permitted_gained = any_bits_outside(caps.permitted, old_caps.permitted);
    let capabilities_beyond_ambient = any_bits_outside(caps.permitted, caps.ambient);
    let non_root_file_privilege =
        root_kuid != Some(ids.ruid) && (file_effective || capabilities_beyond_ambient);
    let secure_exec = is_setid || non_root_file_privilege;

    // setup_new_exec() first derives dumpability from the old real/effective
    // ID relation and executable readability. commit_creds() subsequently
    // lowers it for effective/fs-ID changes or a permitted capability gain.
    let pre_exec_ids_mismatched = old_ids.euid != old_ids.ruid || old_ids.egid != old_ids.rgid;
    let dumpability = if request.executable_unreadable
        || pre_exec_ids_mismatched
        || changed_effective_or_fs_ids
        || permitted_gained
    {
        Dumpability::NotDumpable
    } else {
        Dumpability::UserDumpable
    };

    let effects = ExecCredentialEffects {
        aux_identity: ExecAuxIdentity::from_ids(ids, old.user_ns(), secure_exec),
        dumpability,
        clear_pdeath_signal: secure_exec || changed_effective_or_fs_ids || permitted_gained,
        secure_exec,
        ptrace_suppressed: request.ptrace_suppresses_privilege,
        gains_file_privilege: privilege_transition,
    };
    Ok(DerivedExecCredential { ids, caps, effects })
}

/// Typed authorization facts for a stackable exec-security hook.
///
/// The algebra requires an authorizer instead of providing an allow-by-default
/// entry point. The eventual security module can stack deny-only hooks over
/// this context without re-deriving or re-sampling any credential facts.
pub(crate) struct ExecCredentialSecurityContext<'a> {
    pub(in crate::task) old: &'a Cred,
    pub(in crate::task) proposed: &'a Cred,
    pub(in crate::task) request: &'a ExecCredentialRequest,
    pub(in crate::task) effects: ExecCredentialEffects,
}

/// Fully prepared exec credential whose drop path is a complete abort.
pub(crate) struct PreparedExecCredential<'a> {
    prepared: PreparedCred<'a>,
    effects: ExecCredentialEffects,
}

impl<'a> PreparedExecCredential<'a> {
    pub(crate) fn effects(&self) -> ExecCredentialEffects {
        self.effects
    }

    pub(crate) fn into_prepared(self) -> PreparedCred<'a> {
        self.prepared
    }

    pub(crate) fn proposed_user_ns(&self) -> &Arc<UserNamespace> {
        self.prepared.proposed().user_ns()
    }
}

pub(in crate::task) fn prepare_exec_update<'a>(
    mut update: CredentialUpdate<'a>,
    request: ExecCredentialRequest,
    authorize: impl FnOnce(ExecCredentialSecurityContext<'_>) -> AxResult<()>,
) -> AxResult<PreparedExecCredential<'a>> {
    let derived = derive_exec_credential(update.old(), request)?;
    update.builder.ids = derived.ids;
    update.builder.caps = derived.caps;
    let prepared = update.finish_exec_keep_caps_clear()?;
    authorize(ExecCredentialSecurityContext {
        old: prepared.old(),
        proposed: prepared.proposed(),
        request: &request,
        effects: derived.effects,
    })?;
    Ok(PreparedExecCredential {
        prepared,
        effects: derived.effects,
    })
}

impl Thread {
    pub(crate) fn prepare_exec_credential_with(
        &self,
        request: ExecCredentialRequest,
        authorize: impl FnOnce(ExecCredentialSecurityContext<'_>) -> AxResult<()>,
    ) -> AxResult<PreparedExecCredential<'_>> {
        prepare_exec_update(self.credential.prepare(), request, authorize)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{sync::Arc, vec};

    use linux_raw_sys::general::{CAP_CHOWN, CAP_DAC_OVERRIDE};

    use super::*;
    use crate::task::{
        CredentialSlot, IdMapInputExtent,
        creds::{CAPABILITY_VALID_MASK, GroupInfo},
    };

    fn bit(capability: u32) -> [u32; CAPABILITY_WORDS] {
        let mut bits = [0; CAPABILITY_WORDS];
        let (word, mask) = CapabilityState::cap_mask(capability).unwrap();
        bits[word] = mask;
        bits
    }

    fn union(
        left: [u32; CAPABILITY_WORDS],
        right: [u32; CAPABILITY_WORDS],
    ) -> [u32; CAPABILITY_WORDS] {
        let mut result = [0; CAPABILITY_WORDS];
        for word in 0..CAPABILITY_WORDS {
            result[word] = left[word] | right[word];
        }
        result
    }

    fn file_capabilities(
        permitted: [u32; CAPABILITY_WORDS],
        inheritable: [u32; CAPABILITY_WORDS],
        effective: bool,
        rootid: Kuid,
    ) -> FileCapabilities {
        FileCapabilities::try_new(permitted, inheritable, effective, rootid).unwrap()
    }

    fn root_slot() -> Arc<CredentialSlot> {
        let namespace = UserNamespace::try_new_root().unwrap();
        CredentialSlot::try_new(Cred::try_root(namespace).unwrap()).unwrap()
    }

    fn unprivileged_slot() -> Arc<CredentialSlot> {
        let slot = root_slot();
        let uid = Kuid::from_raw(1000).unwrap();
        let gid = Kgid::from_raw(1000).unwrap();
        let mut update = slot.prepare();
        update.builder.ids = Credentials {
            ruid: uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
        };
        update.builder.groups = GroupInfo::try_new(vec![gid]).unwrap();
        update.builder.caps = CapabilityState {
            effective: [0; CAPABILITY_WORDS],
            permitted: [0; CAPABILITY_WORDS],
            inheritable: [0; CAPABILITY_WORDS],
            bounding: CAPABILITY_VALID_MASK,
            ambient: [0; CAPABILITY_WORDS],
            securebits: 0,
        };
        update.finish().unwrap().commit();
        slot
    }

    fn ordinary_request() -> ExecCredentialRequest {
        ExecCredentialRequest {
            file_uid: Some(Kuid::INITIAL_ROOT),
            file_gid: Some(Kgid::INITIAL_ROOT),
            set_uid: false,
            set_gid: false,
            nosuid: false,
            ptrace_suppresses_privilege: false,
            executable_unreadable: false,
            file_capabilities: None,
        }
    }

    #[test]
    fn setid_exec_updates_saved_and_filesystem_ids_and_is_secure() {
        let slot = unprivileged_slot();
        let mut request = ordinary_request();
        request.set_uid = true;
        request.set_gid = true;

        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.ids.euid, Kuid::INITIAL_ROOT);
        assert_eq!(derived.ids.suid, Kuid::INITIAL_ROOT);
        assert_eq!(derived.ids.fsuid, Kuid::INITIAL_ROOT);
        assert_eq!(derived.ids.egid, Kgid::INITIAL_ROOT);
        assert_eq!(derived.ids.sgid, Kgid::INITIAL_ROOT);
        assert_eq!(derived.ids.fsgid, Kgid::INITIAL_ROOT);
        assert!(derived.effects.secure_exec);
        assert!(derived.effects.clear_pdeath_signal);
        assert!(derived.effects.gains_file_privilege);
        assert_eq!(derived.effects.dumpability, Dumpability::NotDumpable);
        assert_eq!(derived.effects.aux_identity.euid, 0);
        assert_eq!(derived.effects.aux_identity.egid, 0);
        assert!(derived.effects.aux_identity.secure);
    }

    #[test]
    fn ordinary_exec_resets_fsids_and_lowers_dumpability_if_that_changes_identity() {
        let slot = unprivileged_slot();
        let mut update = slot.prepare();
        update.builder.ids.fsuid = Kuid::from_raw(2000).unwrap();
        update.builder.ids.fsgid = Kgid::from_raw(2000).unwrap();
        update.finish().unwrap().commit();

        let derived = derive_exec_credential(&slot.current(), ordinary_request()).unwrap();
        assert_eq!(derived.ids.fsuid, derived.ids.euid);
        assert_eq!(derived.ids.fsgid, derived.ids.egid);
        assert!(derived.effects.clear_pdeath_signal);
        assert!(!derived.effects.secure_exec);
        assert!(!derived.effects.gains_file_privilege);
        assert_eq!(derived.effects.dumpability, Dumpability::NotDumpable);
        assert!(!derived.effects.aux_identity.secure);
    }

    #[test]
    fn unreadable_ordinary_exec_is_nondumpable_without_becoming_secure() {
        let slot = unprivileged_slot();
        let mut request = ordinary_request();
        request.executable_unreadable = true;
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.effects.dumpability, Dumpability::NotDumpable);
        assert!(!derived.effects.secure_exec);
        assert!(!derived.effects.clear_pdeath_signal);
    }

    #[test]
    fn nosuid_no_new_privs_and_ptrace_suppress_gain_with_linux_secureexec_rules() {
        for suppressor in 0..3 {
            let slot = unprivileged_slot();
            if suppressor == 1 {
                let mut update = slot.prepare();
                update.builder.no_new_privs = true;
                update.finish().unwrap().commit();
            }
            let mut request = ordinary_request();
            request.set_uid = true;
            request.nosuid = suppressor == 0;
            request.ptrace_suppresses_privilege = suppressor == 2;
            request.file_capabilities = Some(file_capabilities(
                bit(CAP_CHOWN),
                [0; CAPABILITY_WORDS],
                true,
                Kuid::INITIAL_ROOT,
            ));

            let derived = derive_exec_credential(&slot.current(), request).unwrap();
            assert_eq!(derived.ids.euid, Kuid::from_raw(1000).unwrap());
            assert_eq!(derived.caps.permitted, [0; CAPABILITY_WORDS]);
            assert_eq!(derived.caps.effective, [0; CAPABILITY_WORDS]);
            assert_eq!(derived.effects.secure_exec, suppressor != 0);
            assert_eq!(derived.effects.clear_pdeath_signal, suppressor != 0);
            assert_eq!(derived.effects.ptrace_suppressed, suppressor == 2);
        }
    }

    #[test]
    fn ptrace_downgrades_preexisting_setuid_identity_against_real_uid() {
        let slot = unprivileged_slot();
        let retained = bit(CAP_CHOWN);
        let mut update = slot.prepare();
        update.builder.ids.euid = Kuid::INITIAL_ROOT;
        update.builder.ids.suid = Kuid::INITIAL_ROOT;
        update.builder.ids.fsuid = Kuid::INITIAL_ROOT;
        update.builder.caps.permitted = retained;
        update.builder.caps.effective = retained;
        update.finish().unwrap().commit();

        let mut request = ordinary_request();
        request.ptrace_suppresses_privilege = true;
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.ids.euid, Kuid::from_raw(1000).unwrap());
        assert_eq!(derived.ids.suid, Kuid::from_raw(1000).unwrap());
        assert_eq!(derived.caps.permitted, retained);
        assert_eq!(derived.caps.effective, retained);
        assert_eq!(derived.caps.ambient, [0; CAPABILITY_WORDS]);
        assert!(derived.effects.secure_exec);
        assert!(derived.effects.ptrace_suppressed);
    }

    #[test]
    fn already_effective_root_with_file_caps_does_not_regain_full_bounding_set() {
        let slot = unprivileged_slot();
        let retained = bit(CAP_CHOWN);
        let mut update = slot.prepare();
        update.builder.ids.euid = Kuid::INITIAL_ROOT;
        update.builder.ids.suid = Kuid::INITIAL_ROOT;
        update.builder.ids.fsuid = Kuid::INITIAL_ROOT;
        update.builder.caps.permitted = retained;
        update.builder.caps.effective = retained;
        update.finish().unwrap().commit();

        let explicit = bit(CAP_DAC_OVERRIDE);
        let mut request = ordinary_request();
        request.file_capabilities = Some(file_capabilities(
            explicit,
            [0; CAPABILITY_WORDS],
            true,
            Kuid::INITIAL_ROOT,
        ));
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.ids.ruid, Kuid::from_raw(1000).unwrap());
        assert_eq!(derived.ids.euid, Kuid::INITIAL_ROOT);
        assert_eq!(derived.caps.permitted, explicit);
        assert_eq!(derived.caps.effective, explicit);
    }

    #[test]
    fn no_new_privs_downgrades_a_restricted_effective_root_transition() {
        let slot = unprivileged_slot();
        let retained = bit(CAP_CHOWN);
        let mut update = slot.prepare();
        update.builder.ids.euid = Kuid::INITIAL_ROOT;
        update.builder.ids.suid = Kuid::INITIAL_ROOT;
        update.builder.ids.fsuid = Kuid::INITIAL_ROOT;
        update.builder.caps.permitted = retained;
        update.builder.caps.effective = retained;
        update.builder.no_new_privs = true;
        update.finish().unwrap().commit();

        let mut request = ordinary_request();
        request.file_uid = Some(Kuid::from_raw(2000).unwrap());
        request.set_uid = true;
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.ids.euid, Kuid::from_raw(1000).unwrap());
        assert_eq!(derived.ids.suid, Kuid::from_raw(1000).unwrap());
        assert_eq!(derived.caps.permitted, retained);
        assert_eq!(derived.caps.effective, retained);
        assert!(derived.effects.secure_exec);
        assert!(derived.effects.clear_pdeath_signal);
    }

    #[test]
    fn empty_valid_file_capability_record_still_clears_ambient() {
        let slot = unprivileged_slot();
        let ambient = bit(CAP_CHOWN);
        let mut update = slot.prepare();
        update.builder.caps.permitted = ambient;
        update.builder.caps.inheritable = ambient;
        update.builder.caps.ambient = ambient;
        update.finish().unwrap().commit();

        let mut request = ordinary_request();
        request.file_capabilities = Some(file_capabilities(
            [0; CAPABILITY_WORDS],
            [0; CAPABILITY_WORDS],
            false,
            Kuid::INITIAL_ROOT,
        ));
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.caps.ambient, [0; CAPABILITY_WORDS]);
        assert_eq!(derived.caps.permitted, [0; CAPABILITY_WORDS]);
        assert_eq!(derived.caps.effective, [0; CAPABILITY_WORDS]);
    }

    #[test]
    fn forced_file_permitted_can_arrive_through_the_inheritable_path() {
        let slot = unprivileged_slot();
        let inherited = bit(CAP_CHOWN);
        let mut update = slot.prepare();
        update.builder.caps.inheritable = inherited;
        update.builder.caps.bounding = [0; CAPABILITY_WORDS];
        update.finish().unwrap().commit();

        let mut request = ordinary_request();
        request.file_capabilities = Some(file_capabilities(
            inherited,
            inherited,
            true,
            Kuid::INITIAL_ROOT,
        ));
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.caps.permitted, inherited);
        assert_eq!(derived.caps.effective, inherited);
    }

    #[test]
    fn activating_already_permitted_file_cap_is_secure_but_remains_dumpable() {
        let slot = unprivileged_slot();
        let existing = bit(CAP_CHOWN);
        let mut update = slot.prepare();
        update.builder.caps.permitted = existing;
        update.builder.caps.inheritable = existing;
        update.finish().unwrap().commit();

        let mut request = ordinary_request();
        request.file_capabilities = Some(file_capabilities(
            existing,
            [0; CAPABILITY_WORDS],
            true,
            Kuid::INITIAL_ROOT,
        ));
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.caps.permitted, existing);
        assert_eq!(derived.caps.effective, existing);
        assert!(derived.effects.secure_exec);
        assert!(derived.effects.clear_pdeath_signal);
        assert_eq!(derived.effects.dumpability, Dumpability::UserDumpable);
    }

    #[test]
    fn setgid_to_supplementary_group_is_still_setid_and_clears_ambient() {
        let slot = unprivileged_slot();
        let ambient = bit(CAP_CHOWN);
        let supplemental = Kgid::from_raw(2000).unwrap();
        let mut update = slot.prepare();
        update.builder.groups = GroupInfo::try_new(vec![supplemental]).unwrap();
        update.builder.caps.permitted = ambient;
        update.builder.caps.inheritable = ambient;
        update.builder.caps.ambient = ambient;
        update.finish().unwrap().commit();

        let mut request = ordinary_request();
        request.file_gid = Some(supplemental);
        request.set_gid = true;
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.ids.egid, supplemental);
        assert_eq!(derived.caps.ambient, [0; CAPABILITY_WORDS]);
        assert_eq!(derived.caps.permitted, [0; CAPABILITY_WORDS]);
        assert_eq!(derived.caps.effective, [0; CAPABILITY_WORDS]);
        assert!(derived.effects.secure_exec);
    }

    #[test]
    fn ambient_survives_ordinary_exec_and_clears_for_privileged_file() {
        let slot = unprivileged_slot();
        let ambient = bit(CAP_CHOWN);
        let mut update = slot.prepare();
        update.builder.caps.permitted = ambient;
        update.builder.caps.inheritable = ambient;
        update.builder.caps.ambient = ambient;
        update.finish().unwrap().commit();

        let ordinary = derive_exec_credential(&slot.current(), ordinary_request()).unwrap();
        assert_eq!(ordinary.caps.ambient, ambient);
        assert_eq!(ordinary.caps.permitted, ambient);
        assert_eq!(ordinary.caps.effective, ambient);

        let mut privileged = ordinary_request();
        privileged.file_capabilities = Some(file_capabilities(
            bit(CAP_DAC_OVERRIDE),
            [0; CAPABILITY_WORDS],
            true,
            Kuid::INITIAL_ROOT,
        ));
        let privileged = derive_exec_credential(&slot.current(), privileged).unwrap();
        assert_eq!(privileged.caps.ambient, [0; CAPABILITY_WORDS]);
        assert_eq!(privileged.caps.effective, bit(CAP_DAC_OVERRIDE));
        assert!(privileged.effects.secure_exec);
    }

    #[test]
    fn effective_file_caps_reject_bounding_and_inheritable_truncation() {
        let slot = unprivileged_slot();
        let requested = bit(CAP_CHOWN);
        let mut request = ordinary_request();
        request.file_capabilities = Some(file_capabilities(
            requested,
            [0; CAPABILITY_WORDS],
            true,
            Kuid::INITIAL_ROOT,
        ));
        let mut update = slot.prepare();
        update.builder.caps.bounding = [0; CAPABILITY_WORDS];
        update.finish().unwrap().commit();
        assert_eq!(
            derive_exec_credential(&slot.current(), request).err(),
            Some(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn non_effective_file_caps_are_safely_truncated_by_bounding_set() {
        let slot = unprivileged_slot();
        let mut request = ordinary_request();
        request.file_capabilities = Some(file_capabilities(
            bit(CAP_CHOWN),
            [0; CAPABILITY_WORDS],
            false,
            Kuid::INITIAL_ROOT,
        ));
        let mut update = slot.prepare();
        update.builder.caps.bounding = [0; CAPABILITY_WORDS];
        update.finish().unwrap().commit();
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.caps.permitted, [0; CAPABILITY_WORDS]);
        assert_eq!(derived.caps.effective, [0; CAPABILITY_WORDS]);
    }

    #[test]
    fn nonroot_setuid_root_with_file_caps_does_not_gain_full_root_set() {
        let slot = unprivileged_slot();
        let explicit = bit(CAP_CHOWN);
        let mut request = ordinary_request();
        request.set_uid = true;
        request.file_capabilities = Some(file_capabilities(
            explicit,
            [0; CAPABILITY_WORDS],
            true,
            Kuid::INITIAL_ROOT,
        ));
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.ids.euid, Kuid::INITIAL_ROOT);
        assert_eq!(derived.caps.permitted, explicit);
        assert_eq!(derived.caps.effective, explicit);
    }

    #[test]
    fn file_inheritable_intersects_process_inheritable() {
        let slot = unprivileged_slot();
        let inherited = bit(CAP_CHOWN);
        let ignored = bit(CAP_DAC_OVERRIDE);
        let mut update = slot.prepare();
        update.builder.caps.inheritable = inherited;
        update.finish().unwrap().commit();

        let mut request = ordinary_request();
        request.file_capabilities = Some(file_capabilities(
            [0; CAPABILITY_WORDS],
            union(inherited, ignored),
            true,
            Kuid::INITIAL_ROOT,
        ));
        let derived = derive_exec_credential(&slot.current(), request).unwrap();
        assert_eq!(derived.caps.permitted, inherited);
        assert_eq!(derived.caps.effective, inherited);
    }

    #[test]
    fn noroot_disables_legacy_root_capability_compatibility() {
        let slot = root_slot();
        let root = derive_exec_credential(&slot.current(), ordinary_request()).unwrap();
        assert_eq!(root.caps.permitted, CAPABILITY_VALID_MASK);
        assert_eq!(root.caps.effective, CAPABILITY_VALID_MASK);

        let mut update = slot.prepare();
        update.builder.caps.securebits |= SECBIT_NOROOT;
        update.finish().unwrap().commit();
        let noroot = derive_exec_credential(&slot.current(), ordinary_request()).unwrap();
        assert_eq!(noroot.caps.permitted, [0; CAPABILITY_WORDS]);
        assert_eq!(noroot.caps.effective, [0; CAPABILITY_WORDS]);
    }

    #[test]
    fn exec_clears_keep_caps_but_preserves_its_lock_and_other_securebits() {
        let slot = root_slot();
        let mut update = slot.prepare();
        update.builder.caps.securebits |=
            SECBIT_KEEP_CAPS | super::super::creds::SECBIT_KEEP_CAPS_LOCKED | SECBIT_NOROOT;
        update.finish().unwrap().commit();

        let prepared = prepare_exec_update(slot.prepare(), ordinary_request(), |_| Ok(())).unwrap();
        let proposed = prepared.prepared.proposed().capabilities().securebits();
        assert_eq!(proposed & SECBIT_KEEP_CAPS, 0);
        assert_ne!(proposed & super::super::creds::SECBIT_KEEP_CAPS_LOCKED, 0);
        assert_ne!(proposed & SECBIT_NOROOT, 0);
    }

    #[test]
    fn namespaced_v3_rootid_must_name_current_or_ancestor_root() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        child
            .publish_uid_map(
                child
                    .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
                    .unwrap(),
            )
            .unwrap();
        child.update_setgroups_policy(false).unwrap();
        child
            .publish_gid_map(
                child
                    .try_build_gid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
                    .unwrap(),
                true,
            )
            .unwrap();
        let root_cred = Cred::try_root(root).unwrap();
        let child_cred = Cred::try_with_user_ns(&root_cred, child).unwrap();

        let mut request = ordinary_request();
        request.file_capabilities = Some(file_capabilities(
            bit(CAP_CHOWN),
            [0; CAPABILITY_WORDS],
            true,
            Kuid::from_raw(1000).unwrap(),
        ));
        assert_eq!(
            derive_exec_credential(&child_cred, request)
                .unwrap()
                .caps
                .effective,
            bit(CAP_CHOWN)
        );

        request.file_capabilities = Some(file_capabilities(
            bit(CAP_CHOWN),
            [0; CAPABILITY_WORDS],
            true,
            Kuid::from_raw(2000).unwrap(),
        ));
        assert_eq!(
            derive_exec_credential(&child_cred, request)
                .unwrap()
                .caps
                .effective,
            [0; CAPABILITY_WORDS]
        );
    }

    #[test]
    fn authorizer_observes_frozen_old_proposed_request_and_effects() {
        let slot = unprivileged_slot();
        let mut request = ordinary_request();
        request.set_uid = true;
        let prepared = prepare_exec_update(slot.prepare(), request, |context| {
            assert_eq!(context.old.ids().euid, Kuid::from_raw(1000).unwrap());
            assert_eq!(context.proposed.ids().euid, Kuid::INITIAL_ROOT);
            assert!(context.request.set_uid);
            assert!(context.effects.secure_exec);
            assert_eq!(context.effects.dumpability, Dumpability::NotDumpable);
            Ok(())
        })
        .unwrap();
        assert!(prepared.effects().clear_pdeath_signal);
    }

    #[test]
    fn authorizer_denial_and_dropped_preparation_are_zero_effect_rollbacks() {
        let slot = unprivileged_slot();
        let old = slot.current();
        let mut request = ordinary_request();
        request.set_uid = true;

        let denied = prepare_exec_update(slot.prepare(), request, |_| {
            Err(AxError::OperationNotPermitted)
        });
        assert_eq!(denied.err(), Some(AxError::OperationNotPermitted));
        assert!(Arc::ptr_eq(&old, &slot.current()));

        let prepared = prepare_exec_update(slot.prepare(), request, |_| Ok(())).unwrap();
        assert_eq!(prepared.effects().dumpability, Dumpability::NotDumpable);
        drop(prepared);
        assert!(Arc::ptr_eq(&old, &slot.current()));
    }
}
