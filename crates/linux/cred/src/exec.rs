//! Linux exec credential derivation over immutable credential values.
//!
//! This module accepts only already-frozen executable facts. It owns no VFS
//! lookup, process state, ptrace relationship, hook registry, publication
//! lock, address space, or parent-death signal. An embedding kernel samples
//! those facts, derives an opaque proposal here, authorizes it, and publishes
//! it through its own transaction boundary.

use alloc::sync::Arc;

use linux_raw_sys::general::CAP_SETUID;

use crate::{
    CAPABILITY_WORDS, CapabilitySets, CredError, Credential, CredentialIds,
    CredentialTransitionMode, FileCapabilities, IdMap, Kgid, Kuid, SECBIT_KEEP_CAPS, SECBIT_NOROOT,
    USER_NAMESPACE_OVERFLOW_ID, UserGid, UserNamespaceView, UserUid,
};

const MODE_SET_UID: u16 = 0o4000;
const MODE_SET_GID: u16 = 0o2000;
const MODE_GROUP_EXECUTE: u16 = 0o0010;

/// Namespace mapping surface required by exec credential derivation.
///
/// Implementations must clone the UID and GID maps at one coherent
/// linearization point. The derivation uses this single pair both to decide
/// whether the executable owner is representable and to build the auxiliary
/// vector identity, preventing mixed UID/GID map observations.
pub trait ExecUserNamespaceView: UserNamespaceView {
    /// Returns one coherent immutable UID/GID map snapshot.
    fn exec_id_map_snapshot(&self) -> (Arc<IdMap>, Arc<IdMap>);
}

/// Kernel-global UID/GID pair sampled from one executable inode.
///
/// The fields are private so an input cannot independently replace one half
/// after construction. Invalid all-ones inode IDs are represented by omitting
/// the owner from [`ExecCredentialInput`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecFileOwner {
    uid: Kuid,
    gid: Kgid,
}

impl ExecFileOwner {
    /// Constructs one already-sampled executable owner pair.
    pub const fn new(uid: Kuid, gid: Kgid) -> Self {
        Self { uid, gid }
    }

    /// Returns the kernel-global executable owner UID.
    pub const fn uid(self) -> Kuid {
        self.uid
    }

    /// Returns the kernel-global executable owner GID.
    pub const fn gid(self) -> Kgid {
        self.gid
    }
}

/// Mount policy governing executable privilege metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecMountPrivilege {
    /// Honor set-ID mode bits and a valid file-capability record.
    Honor,
    /// Ignore both set-ID mode bits and file capabilities, as on `MS_NOSUID`.
    NoSuid,
}

impl ExecMountPrivilege {
    /// Reports whether executable privilege metadata may be honored.
    pub const fn honors_file_privilege(self) -> bool {
        matches!(self, Self::Honor)
    }
}

/// Frozen tracing state relevant to unsafe exec privilege transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecTraceState {
    /// No tracing relationship currently suppresses executable privilege.
    NotSuppressingPrivilege,
    /// The frozen tracing relationship suppresses executable privilege.
    SuppressingPrivilege,
}

impl ExecTraceState {
    /// Reports whether this state suppresses an unsafe privilege transition.
    pub const fn suppresses_privilege(self) -> bool {
        matches!(self, Self::SuppressingPrivilege)
    }
}

/// Readability of the complete executable/interpreter image chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecImageReadability {
    /// Every executable image in the resolved chain was readable.
    Readable,
    /// At least one executable image in the resolved chain was unreadable.
    Unreadable,
}

impl ExecImageReadability {
    /// Reports whether the new image must be treated as unreadable.
    pub const fn is_unreadable(self) -> bool {
        matches!(self, Self::Unreadable)
    }
}

/// Immutable facts used to derive one exec credential proposal.
///
/// Set-ID intent is deliberately absent from this API. The crate derives it
/// from `mode`, the paired owner, the coherent namespace-map snapshot,
/// `mount_privilege`, and the old credential's `no_new_privs` state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecCredentialInput {
    mode: u16,
    owner: Option<ExecFileOwner>,
    mount_privilege: ExecMountPrivilege,
    trace_state: ExecTraceState,
    image_readability: ExecImageReadability,
    file_capabilities: Option<FileCapabilities>,
}

impl ExecCredentialInput {
    /// Constructs one frozen set of final-executable facts.
    pub const fn new(
        mode: u16,
        owner: Option<ExecFileOwner>,
        mount_privilege: ExecMountPrivilege,
        trace_state: ExecTraceState,
        image_readability: ExecImageReadability,
        file_capabilities: Option<FileCapabilities>,
    ) -> Self {
        Self {
            mode,
            owner,
            mount_privilege,
            trace_state,
            image_readability,
            file_capabilities,
        }
    }

    /// Returns the executable mode word.
    pub const fn mode(self) -> u16 {
        self.mode
    }

    /// Returns the paired executable owner, if both raw inode IDs were valid.
    pub const fn owner(self) -> Option<ExecFileOwner> {
        self.owner
    }

    /// Returns the frozen mount privilege policy.
    pub const fn mount_privilege(self) -> ExecMountPrivilege {
        self.mount_privilege
    }

    /// Returns the tracing state sampled during preparation.
    pub const fn trace_state(self) -> ExecTraceState {
        self.trace_state
    }

    /// Returns the complete executable-chain readability fact.
    pub const fn image_readability(self) -> ExecImageReadability {
        self.image_readability
    }

    /// Returns the strictly parsed file-capability record, if one was present.
    pub const fn file_capabilities(self) -> Option<FileCapabilities> {
        self.file_capabilities
    }
}

/// Dumpability selected for a newly executed process image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExecDumpability {
    /// The new image must not be user-dumpable.
    NotDumpable,
    /// The new image may be dumped by its user under the kernel's normal rules.
    UserDumpable,
}

/// Namespace-visible identity installed in the new ELF auxiliary vector.
///
/// The fields are private so only credential derivation and the explicit
/// trusted-boot constructor can produce this value. Secure execution has one
/// source here; [`ExecCredentialEffects`] does not duplicate the flag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecAuxIdentity {
    uid: UserUid,
    euid: UserUid,
    gid: UserGid,
    egid: UserGid,
    secure: bool,
}

impl ExecAuxIdentity {
    /// Projects an already-authorized credential snapshot into an ELF auxv
    /// identity without performing an exec privilege transition. Kernel
    /// usermode helpers use this after capturing their caller credentials;
    /// unlike `trusted_boot`, it preserves user-namespace mappings.
    pub fn from_captured_ids(ids: CredentialIds, uid_map: &IdMap, gid_map: &IdMap) -> Self {
        Self::from_ids(ids, uid_map, gid_map, false)
    }

    /// Identity used only by the kernel's trusted pre-userspace boot path.
    pub const fn trusted_boot() -> Self {
        Self {
            uid: UserUid::ROOT,
            euid: UserUid::ROOT,
            gid: UserGid::ROOT,
            egid: UserGid::ROOT,
            secure: false,
        }
    }

    fn from_ids(ids: CredentialIds, uid_map: &IdMap, gid_map: &IdMap, secure: bool) -> Self {
        let uid = uid_map
            .kernel_uid_to_user(ids.ruid)
            .unwrap_or_else(overflow_uid);
        let euid = uid_map
            .kernel_uid_to_user(ids.euid)
            .unwrap_or_else(overflow_uid);
        let gid = gid_map
            .kernel_gid_to_user(ids.rgid)
            .unwrap_or_else(overflow_gid);
        let egid = gid_map
            .kernel_gid_to_user(ids.egid)
            .unwrap_or_else(overflow_gid);
        Self {
            uid,
            euid,
            gid,
            egid,
            secure,
        }
    }

    /// Returns the namespace-visible real UID.
    pub const fn uid(self) -> UserUid {
        self.uid
    }

    /// Returns the namespace-visible effective UID.
    pub const fn euid(self) -> UserUid {
        self.euid
    }

    /// Returns the namespace-visible real GID.
    pub const fn gid(self) -> UserGid {
        self.gid
    }

    /// Returns the namespace-visible effective GID.
    pub const fn egid(self) -> UserGid {
        self.egid
    }

    /// Reports whether this identity requires secure-exec treatment.
    pub const fn is_secure(self) -> bool {
        self.secure
    }
}

fn overflow_uid() -> UserUid {
    match UserUid::from_raw(USER_NAMESPACE_OVERFLOW_ID) {
        Some(uid) => uid,
        None => UserUid::ROOT,
    }
}

fn overflow_gid() -> UserGid {
    match UserGid::from_raw(USER_NAMESPACE_OVERFLOW_ID) {
        Some(gid) => gid,
        None => UserGid::ROOT,
    }
}

/// Process-image effects derived independently from publication mechanics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecCredentialEffects {
    aux_identity: ExecAuxIdentity,
    dumpability: ExecDumpability,
    clear_pdeath_signal: bool,
    clear_personality_on_setid: bool,
}

impl ExecCredentialEffects {
    /// Returns the exact namespace-visible auxiliary identity.
    pub const fn aux_identity(self) -> ExecAuxIdentity {
        self.aux_identity
    }

    /// Returns the selected dumpability for the new image.
    pub const fn dumpability(self) -> ExecDumpability {
        self.dumpability
    }

    /// Reports whether the exact executing task's parent-death signal must be
    /// cleared at commit.
    pub const fn clear_pdeath_signal(self) -> bool {
        self.clear_pdeath_signal
    }

    /// Reports Linux `bprm->per_clear`: only an honored set-ID transition or
    /// an actual permitted-capability gain clears these personality flags.
    pub const fn clear_personality_on_setid(self) -> bool {
        self.clear_personality_on_setid
    }

    /// Reports whether secure-exec treatment is required.
    ///
    /// This is derived from the typed auxiliary identity and is therefore not
    /// stored as a second potentially inconsistent boolean.
    pub const fn secure_exec(self) -> bool {
        self.aux_identity.is_secure()
    }
}

/// Ptrace fact that must be revalidated at the irreversible exec boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecPtraceRevalidation {
    privilege_sensitive: bool,
    prepared_trace_state: ExecTraceState,
}

impl ExecPtraceRevalidation {
    /// Reports whether suppression-free derivation observed a pre-downgrade
    /// effective-ID/group-membership change or a permitted-capability gain.
    pub const fn privilege_sensitive(self) -> bool {
        self.privilege_sensitive
    }

    /// Returns the tracing state frozen during derivation.
    pub const fn prepared_trace_state(self) -> ExecTraceState {
        self.prepared_trace_state
    }

    /// Detects a newly suppressing trace relationship which invalidates a
    /// privilege-sensitive proposal.
    pub const fn is_stale(self, current_trace_state: ExecTraceState) -> bool {
        self.privilege_sensitive
            && !self.prepared_trace_state.suppresses_privilege()
            && current_trace_state.suppresses_privilege()
    }
}

/// Fully derived but unpublished exec credential.
///
/// Construction is private to [`derive_exec_credential`]. The proposal owns a
/// clone of the exact old `Arc`, preventing a value derived from one writer
/// snapshot from being consumed by another transaction with equal-looking
/// credentials.
pub struct ExecCredentialProposal<N: ExecUserNamespaceView> {
    old: Arc<Credential<N>>,
    proposed: Arc<Credential<N>>,
    input: ExecCredentialInput,
    effects: ExecCredentialEffects,
    revalidation: ExecPtraceRevalidation,
}

impl<N: ExecUserNamespaceView> ExecCredentialProposal<N> {
    /// Borrows the exact old credential owner used for derivation.
    pub fn old(&self) -> &Credential<N> {
        self.old.as_ref()
    }

    /// Borrows the complete immutable proposed credential.
    pub fn proposed(&self) -> &Credential<N> {
        self.proposed.as_ref()
    }

    /// Returns the frozen derivation input.
    pub const fn input(&self) -> ExecCredentialInput {
        self.input
    }

    /// Returns the independently applicable process-image effects.
    pub const fn effects(&self) -> ExecCredentialEffects {
        self.effects
    }

    /// Returns the ptrace relationship fact that must be revalidated at commit.
    pub const fn revalidation(&self) -> ExecPtraceRevalidation {
        self.revalidation
    }

    /// Consumes the proposal only for the exact writer snapshot from which it
    /// was derived.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::NotPermitted`] when `expected_old` is a distinct
    /// `Arc`, even if every credential field compares equal by observation.
    pub fn try_into_proposed(
        self,
        expected_old: &Arc<Credential<N>>,
    ) -> Result<Arc<Credential<N>>, CredError> {
        if !Arc::ptr_eq(&self.old, expected_old) {
            return Err(CredError::NotPermitted);
        }
        Ok(self.proposed)
    }
}

fn file_root_owns_current_namespace<N: UserNamespaceView>(
    user_ns: &Arc<N>,
    current_root_kuid: Option<Kuid>,
    rootid: Kuid,
) -> bool {
    if current_root_kuid == Some(rootid) {
        return true;
    }
    let mut namespace = user_ns.clone();
    loop {
        let current_level = namespace.level();
        let Some(parent) = namespace.parent() else {
            return false;
        };
        if parent.level() >= current_level {
            return false;
        }
        namespace = parent;
        if namespace.root_kuid() == Some(rootid) {
            return true;
        }
    }
}

fn any_bits_outside(left: [u32; CAPABILITY_WORDS], right: [u32; CAPABILITY_WORDS]) -> bool {
    (0..CAPABILITY_WORDS).any(|word| left[word] & !right[word] != 0)
}

fn owner_is_mapped(owner: ExecFileOwner, uid_map: &IdMap, gid_map: &IdMap) -> bool {
    uid_map.kernel_uid_to_user(owner.uid).is_some()
        && gid_map.kernel_gid_to_user(owner.gid).is_some()
}

/// Derives and allocates one complete immutable exec credential proposal.
///
/// The caller must keep its own transaction guard alive while authorizing and
/// consuming the result. This function clones one coherent UID/GID map pair,
/// derives set-ID and file-capability semantics, validates the final capability
/// sets, and constructs the proposed credential with exec's sole allowed
/// `KEEP_CAPS` transition.
///
/// # Errors
///
/// Returns [`CredError::NotPermitted`] when a forced effective file capability
/// cannot be supplied or a credential transition invariant fails,
/// [`CredError::InvalidInput`] for invalid final capability state, or
/// [`CredError::NoMemory`] if the proposed immutable credential cannot be
/// allocated.
pub fn derive_exec_credential<N: ExecUserNamespaceView>(
    old: &Arc<Credential<N>>,
    input: ExecCredentialInput,
) -> Result<ExecCredentialProposal<N>, CredError> {
    let (uid_map, gid_map) = old.user_ns().exec_id_map_snapshot();
    let mapped_owner = input
        .owner
        .filter(|owner| owner_is_mapped(*owner, &uid_map, &gid_map));
    let old_ids = old.ids();
    let old_caps = old.capabilities();
    let root_kuid = uid_map.user_uid_to_kernel(UserUid::ROOT);

    // Linux ignores both set-ID bits if either inode owner ID is invalid or
    // unmapped. S_ISGID additionally requires group execute permission.
    let set_uid = mapped_owner.is_some() && input.mode & MODE_SET_UID != 0;
    let set_gid = mapped_owner.is_some()
        && input.mode & (MODE_SET_GID | MODE_GROUP_EXECUTE) == (MODE_SET_GID | MODE_GROUP_EXECUTE);

    // bprm_fill_uid() applies set-ID before commoncap's unsafe-exec downgrade.
    // Both nosuid and no_new_privs prevent the inode bits from taking effect.
    let setid_admitted = input.mount_privilege.honors_file_privilege() && !old.no_new_privs();
    let mut ids = old_ids;
    if setid_admitted && let Some(owner) = mapped_owner {
        if set_uid {
            ids.euid = owner.uid;
        }
        if set_gid {
            ids.egid = owner.gid;
        }
    }

    // Freeze Linux commoncap's pre-downgrade identity predicates separately.
    // `id_changed` controls unsafe-exec downgrades and ambient clearing: a new
    // effective UID differs from the old effective UID, or the proposed
    // effective GID is neither the old filesystem GID nor supplementary.
    // Secure-exec additionally compares the resulting IDs with the old real
    // IDs, so the two predicates are intentionally not interchangeable.
    let id_changed =
        ids.euid != old_ids.euid || (ids.egid != old_ids.fsgid && !old.groups().contains(ids.egid));

    // nosuid removes the file-capability record. no_new_privs instead allows
    // derivation and then intersects any gain with the old permitted set.
    let file_capabilities = input
        .mount_privilege
        .honors_file_privilege()
        .then_some(input.file_capabilities)
        .flatten()
        .filter(|caps| file_root_owns_current_namespace(old.user_ns(), root_kuid, caps.rootid()));
    let has_fcap = file_capabilities.is_some();

    let mut file_permitted = [0; CAPABILITY_WORDS];
    let mut file_inheritable = [0; CAPABILITY_WORDS];
    let mut file_effective = false;
    if let Some(file) = file_capabilities {
        file_permitted = file.permitted();
        file_inheritable = file.inheritable();
        file_effective = file.effective();
    }

    let old_permitted = old_caps.permitted();
    let old_inheritable = old_caps.inheritable();
    let old_bounding = old_caps.bounding();
    let old_ambient = old_caps.ambient();
    let mut permitted_without_ambient = [0; CAPABILITY_WORDS];
    for word in 0..CAPABILITY_WORDS {
        permitted_without_ambient[word] = (old_bounding[word] & file_permitted[word])
            | (old_inheritable[word] & file_inheritable[word]);
    }
    if file_effective && any_bits_outside(file_permitted, permitted_without_ambient) {
        return Err(CredError::NotPermitted);
    }

    // Legacy root compatibility is disabled by SECBIT_NOROOT. A setuid-root
    // executable with a valid file-capability record receives only its explicit
    // file capability sets.
    let proposed_is_setuid_root = root_kuid != Some(ids.ruid) && root_kuid == Some(ids.euid);
    let root_compat = old_caps.securebits() & SECBIT_NOROOT == 0
        && (root_kuid == Some(ids.ruid) || root_kuid == Some(ids.euid))
        && !(has_fcap && proposed_is_setuid_root);
    if root_compat {
        for word in 0..CAPABILITY_WORDS {
            permitted_without_ambient[word] = old_bounding[word] | old_inheritable[word];
        }
        if root_kuid == Some(ids.euid) {
            file_effective = true;
        }
    }

    let permitted_gained_before_unsafe = any_bits_outside(permitted_without_ambient, old_permitted);
    let privilege_sensitive = id_changed || permitted_gained_before_unsafe;
    let setid_downgraded = privilege_sensitive
        && (old.no_new_privs() || input.trace_state.suppresses_privilege())
        && (old.no_new_privs() || !old.has_effective_capability_in_own_user_ns(CAP_SETUID));
    if privilege_sensitive && (old.no_new_privs() || input.trace_state.suppresses_privilege()) {
        if setid_downgraded {
            ids.euid = ids.ruid;
            ids.egid = ids.rgid;
        }
        for word in 0..CAPABILITY_WORDS {
            permitted_without_ambient[word] &= old_permitted[word];
        }
    }

    ids.suid = ids.euid;
    ids.fsuid = ids.euid;
    ids.sgid = ids.egid;
    ids.fsgid = ids.egid;

    // Linux performs this secure-exec comparison after unsafe downgrade has
    // selected the final effective IDs. Keep it separate from the frozen
    // pre-downgrade `id_changed` predicate above.
    let differs_from_real_ids = ids.euid != old_ids.ruid || ids.egid != old_ids.rgid;

    let mut ambient = if has_fcap || id_changed {
        [0; CAPABILITY_WORDS]
    } else {
        old_ambient
    };
    let mut permitted = permitted_without_ambient;
    for word in 0..CAPABILITY_WORDS {
        permitted[word] |= ambient[word];
    }
    let effective = if file_effective { permitted } else { ambient };
    for word in 0..CAPABILITY_WORDS {
        ambient[word] &= permitted[word] & old_inheritable[word];
    }
    let capabilities = CapabilitySets::try_new(
        effective,
        permitted,
        old_inheritable,
        old_bounding,
        ambient,
        old_caps.securebits() & !SECBIT_KEEP_CAPS,
    )?;

    let changed_effective_ids = old_ids.euid != ids.euid || old_ids.egid != ids.egid;
    let changed_effective_or_fs_ids =
        changed_effective_ids || old_ids.fsuid != ids.fsuid || old_ids.fsgid != ids.fsgid;
    let permitted_gained = any_bits_outside(capabilities.permitted(), old_permitted);
    // bprm_fill_uid()/cap_bprm_creds_from_file decide per_clear before the
    // later unsafe-exec downgrade.  Do not retract it when ptrace subsequently
    // restores IDs or intersects the final permitted set.
    let honored_setuid = setid_admitted && set_uid;
    let honored_setgid = setid_admitted && set_gid;
    let capabilities_beyond_ambient =
        any_bits_outside(capabilities.permitted(), capabilities.ambient());
    let non_root_file_privilege =
        root_kuid != Some(ids.ruid) && (file_effective || capabilities_beyond_ambient);
    let secure_exec = id_changed || differs_from_real_ids || non_root_file_privilege;

    let pre_exec_ids_mismatched = old_ids.euid != old_ids.ruid || old_ids.egid != old_ids.rgid;
    let dumpability = if input.image_readability.is_unreadable()
        || pre_exec_ids_mismatched
        || changed_effective_or_fs_ids
        || permitted_gained
    {
        ExecDumpability::NotDumpable
    } else {
        ExecDumpability::UserDumpable
    };
    let effects = ExecCredentialEffects {
        aux_identity: ExecAuxIdentity::from_ids(ids, &uid_map, &gid_map, secure_exec),
        dumpability,
        clear_pdeath_signal: secure_exec || changed_effective_or_fs_ids || permitted_gained,
        clear_personality_on_setid: honored_setuid
            || honored_setgid
            || permitted_gained_before_unsafe,
    };
    let revalidation = ExecPtraceRevalidation {
        privilege_sensitive,
        prepared_trace_state: input.trace_state,
    };

    let proposed = Credential::try_from_transition(
        old,
        ids,
        old.groups().clone(),
        capabilities,
        old.no_new_privs(),
        old.user_ns().clone(),
        CredentialTransitionMode::ExecClearsKeepCaps,
    )?;
    Ok(ExecCredentialProposal {
        old: old.clone(),
        proposed,
        input,
        effects,
        revalidation,
    })
}

fn permitted_is_subset<N: UserNamespaceView>(
    proposed: &Credential<N>,
    old: &Credential<N>,
) -> bool {
    proposed
        .capabilities()
        .permitted()
        .iter()
        .zip(old.capabilities().permitted().iter())
        .all(|(proposed, old)| proposed & !old == 0)
}

/// Applies commoncap's post-derivation exec authorization invariants.
///
/// This pure check is suitable for a kernel's typed exec hook adapter. It does
/// not dispatch hooks or map errors into an embedding errno type.
///
/// # Errors
///
/// Returns [`CredError::NotPermitted`] if the proposal changes namespace or
/// `no_new_privs`, or if a no-new-privileges/traced transition expands the old
/// permitted capability set.
pub fn commoncap_exec_transition<N: ExecUserNamespaceView>(
    proposal: &ExecCredentialProposal<N>,
) -> Result<(), CredError> {
    let old = proposal.old.as_ref();
    let proposed = proposal.proposed.as_ref();
    if !Arc::ptr_eq(old.user_ns(), proposed.user_ns())
        || old.no_new_privs() != proposed.no_new_privs()
    {
        return Err(CredError::NotPermitted);
    }
    if (old.no_new_privs() || proposal.input.trace_state.suppresses_privilege())
        && !permitted_is_subset(proposed, old)
    {
        return Err(CredError::NotPermitted);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{sync::Arc, vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use linux_raw_sys::general::{CAP_CHOWN, CAP_DAC_OVERRIDE};

    use super::*;
    use crate::{
        CAPABILITY_VALID_MASK, GroupInfo, IdMapInputExtent, SECBIT_EXEC_DENY_INTERACTIVE,
        SECBIT_EXEC_DENY_INTERACTIVE_LOCKED, SECBIT_EXEC_RESTRICT_FILE,
        SECBIT_EXEC_RESTRICT_FILE_LOCKED, SECBIT_KEEP_CAPS_LOCKED,
    };

    struct MockNamespace {
        level: u32,
        parent: Option<Arc<Self>>,
        owner: Kuid,
        uid_map: Arc<IdMap>,
        gid_map: Arc<IdMap>,
        reported_root: Option<Kuid>,
        snapshot_calls: AtomicUsize,
    }

    impl MockNamespace {
        fn root() -> Arc<Self> {
            let identity = IdMap::try_identity().unwrap();
            Self::root_with_maps(identity.clone(), identity, Some(Kuid::INITIAL_ROOT))
        }

        fn root_with_maps(
            uid_map: Arc<IdMap>,
            gid_map: Arc<IdMap>,
            reported_root: Option<Kuid>,
        ) -> Arc<Self> {
            Arc::new(Self {
                level: 0,
                parent: None,
                owner: Kuid::INITIAL_ROOT,
                uid_map,
                gid_map,
                reported_root,
                snapshot_calls: AtomicUsize::new(0),
            })
        }

        fn child(
            parent: &Arc<Self>,
            owner: Kuid,
            uid_map: Arc<IdMap>,
            gid_map: Arc<IdMap>,
        ) -> Arc<Self> {
            Arc::new(Self {
                level: parent.level + 1,
                parent: Some(parent.clone()),
                owner,
                reported_root: uid_map.user_uid_to_kernel(UserUid::ROOT),
                uid_map,
                gid_map,
                snapshot_calls: AtomicUsize::new(0),
            })
        }
    }

    impl UserNamespaceView for MockNamespace {
        fn parent(self: &Arc<Self>) -> Option<Arc<Self>> {
            self.parent.clone()
        }

        fn level(&self) -> u32 {
            self.level
        }

        fn owner_kuid(&self) -> Kuid {
            self.owner
        }

        fn root_kuid(&self) -> Option<Kuid> {
            self.reported_root
        }

        fn is_initial(&self) -> bool {
            self.parent.is_none()
        }
    }

    impl ExecUserNamespaceView for MockNamespace {
        fn exec_id_map_snapshot(&self) -> (Arc<IdMap>, Arc<IdMap>) {
            self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
            (self.uid_map.clone(), self.gid_map.clone())
        }
    }

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    fn bit(capability: u32) -> [u32; CAPABILITY_WORDS] {
        let mut bits = [0; CAPABILITY_WORDS];
        let (word, mask) = CapabilitySets::cap_mask(capability).unwrap();
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

    fn capabilities(
        effective: [u32; CAPABILITY_WORDS],
        permitted: [u32; CAPABILITY_WORDS],
        inheritable: [u32; CAPABILITY_WORDS],
        bounding: [u32; CAPABILITY_WORDS],
        ambient: [u32; CAPABILITY_WORDS],
        securebits: u32,
    ) -> CapabilitySets {
        CapabilitySets::try_new(
            effective,
            permitted,
            inheritable,
            bounding,
            ambient,
            securebits,
        )
        .unwrap()
    }

    fn transition(
        old: &Arc<Credential<MockNamespace>>,
        ids: CredentialIds,
        groups: Arc<GroupInfo>,
        caps: CapabilitySets,
        no_new_privs: bool,
    ) -> Arc<Credential<MockNamespace>> {
        Credential::try_from_transition(
            old,
            ids,
            groups,
            caps,
            no_new_privs,
            old.user_ns().clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap()
    }

    fn with_state(
        old: &Arc<Credential<MockNamespace>>,
        ids: CredentialIds,
        caps: CapabilitySets,
        no_new_privs: bool,
    ) -> Arc<Credential<MockNamespace>> {
        transition(old, ids, old.groups().clone(), caps, no_new_privs)
    }

    fn root_credential() -> Arc<Credential<MockNamespace>> {
        Credential::try_root(MockNamespace::root()).unwrap()
    }

    fn unprivileged_from_root(
        root: &Arc<Credential<MockNamespace>>,
    ) -> Arc<Credential<MockNamespace>> {
        let uid = kuid(1000);
        let gid = kgid(1000);
        transition(
            root,
            CredentialIds {
                ruid: uid,
                euid: uid,
                suid: uid,
                fsuid: uid,
                rgid: gid,
                egid: gid,
                sgid: gid,
                fsgid: gid,
            },
            GroupInfo::try_new(vec![gid]).unwrap(),
            capabilities(
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                0,
            ),
            false,
        )
    }

    fn unprivileged_credential() -> Arc<Credential<MockNamespace>> {
        unprivileged_from_root(&root_credential())
    }

    fn file_caps(
        permitted: [u32; CAPABILITY_WORDS],
        inheritable: [u32; CAPABILITY_WORDS],
        effective: bool,
        rootid: Kuid,
    ) -> FileCapabilities {
        FileCapabilities::try_new(permitted, inheritable, effective, rootid).unwrap()
    }

    fn input(
        mode: u16,
        owner: Option<ExecFileOwner>,
        mount_privilege: ExecMountPrivilege,
        trace_state: ExecTraceState,
        image_readability: ExecImageReadability,
        file_capabilities: Option<FileCapabilities>,
    ) -> ExecCredentialInput {
        ExecCredentialInput::new(
            mode,
            owner,
            mount_privilege,
            trace_state,
            image_readability,
            file_capabilities,
        )
    }

    fn ordinary_input() -> ExecCredentialInput {
        input(
            0o755,
            Some(ExecFileOwner::new(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT)),
            ExecMountPrivilege::Honor,
            ExecTraceState::NotSuppressingPrivilege,
            ExecImageReadability::Readable,
            None,
        )
    }

    #[test]
    fn setid_exec_updates_saved_and_filesystem_ids_and_is_secure() {
        let old = unprivileged_credential();
        let proposal = derive_exec_credential(
            &old,
            input(
                0o6755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();
        let ids = proposal.proposed().ids();
        assert_eq!(ids.euid, Kuid::INITIAL_ROOT);
        assert_eq!(ids.suid, Kuid::INITIAL_ROOT);
        assert_eq!(ids.fsuid, Kuid::INITIAL_ROOT);
        assert_eq!(ids.egid, Kgid::INITIAL_ROOT);
        assert_eq!(ids.sgid, Kgid::INITIAL_ROOT);
        assert_eq!(ids.fsgid, Kgid::INITIAL_ROOT);
        assert!(proposal.effects().secure_exec());
        assert!(proposal.effects().clear_pdeath_signal());
        assert_eq!(
            proposal.effects().dumpability(),
            ExecDumpability::NotDumpable
        );
        assert_eq!(proposal.effects().aux_identity().euid().into_raw(), 0);
        assert_eq!(proposal.effects().aux_identity().egid().into_raw(), 0);
        assert!(proposal.revalidation().privilege_sensitive());
    }

    #[test]
    fn ordinary_exec_resets_fsids_and_lowers_dumpability_if_that_changes_identity() {
        let old = unprivileged_credential();
        let mut ids = old.ids();
        ids.fsuid = kuid(2000);
        ids.fsgid = kgid(2000);
        let old = with_state(&old, ids, old.capabilities(), false);
        let proposal = derive_exec_credential(&old, ordinary_input()).unwrap();
        let ids = proposal.proposed().ids();
        assert_eq!(ids.fsuid, ids.euid);
        assert_eq!(ids.fsgid, ids.egid);
        assert!(proposal.effects().clear_pdeath_signal());
        assert!(!proposal.effects().secure_exec());
        assert!(!proposal.revalidation().privilege_sensitive());
        assert_eq!(
            proposal.effects().dumpability(),
            ExecDumpability::NotDumpable
        );
    }

    #[test]
    fn unreadable_ordinary_exec_is_nondumpable_without_becoming_secure() {
        let old = unprivileged_credential();
        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Unreadable,
                None,
            ),
        )
        .unwrap();
        assert_eq!(
            proposal.effects().dumpability(),
            ExecDumpability::NotDumpable
        );
        assert!(!proposal.effects().secure_exec());
        assert!(!proposal.effects().clear_pdeath_signal());
    }

    #[test]
    fn nosuid_no_new_privs_and_ptrace_suppress_gain_with_linux_secureexec_rules() {
        for suppressor in 0..3 {
            let old = unprivileged_credential();
            let old = if suppressor == 1 {
                with_state(&old, old.ids(), old.capabilities(), true)
            } else {
                old
            };
            let proposal = derive_exec_credential(
                &old,
                input(
                    0o4755,
                    ordinary_input().owner(),
                    if suppressor == 0 {
                        ExecMountPrivilege::NoSuid
                    } else {
                        ExecMountPrivilege::Honor
                    },
                    if suppressor == 2 {
                        ExecTraceState::SuppressingPrivilege
                    } else {
                        ExecTraceState::NotSuppressingPrivilege
                    },
                    ExecImageReadability::Readable,
                    Some(file_caps(
                        bit(CAP_CHOWN),
                        [0; CAPABILITY_WORDS],
                        true,
                        Kuid::INITIAL_ROOT,
                    )),
                ),
            )
            .unwrap();
            assert_eq!(proposal.proposed().ids().euid, kuid(1000));
            assert_eq!(
                proposal.proposed().capabilities().permitted(),
                [0; CAPABILITY_WORDS]
            );
            assert_eq!(
                proposal.proposed().capabilities().effective(),
                [0; CAPABILITY_WORDS]
            );
            assert_eq!(proposal.effects().secure_exec(), suppressor != 0);
            assert_eq!(proposal.effects().clear_pdeath_signal(), suppressor != 0);
            assert_eq!(
                proposal.revalidation().prepared_trace_state(),
                if suppressor == 2 {
                    ExecTraceState::SuppressingPrivilege
                } else {
                    ExecTraceState::NotSuppressingPrivilege
                }
            );
        }
    }

    #[test]
    fn ptrace_downgrades_preexisting_setuid_identity_against_real_uid() {
        let old = unprivileged_credential();
        let retained = bit(CAP_CHOWN);
        let mut ids = old.ids();
        ids.euid = Kuid::INITIAL_ROOT;
        ids.suid = Kuid::INITIAL_ROOT;
        ids.fsuid = Kuid::INITIAL_ROOT;
        let old = with_state(
            &old,
            ids,
            capabilities(
                retained,
                retained,
                [0; CAPABILITY_WORDS],
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                0,
            ),
            false,
        );
        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::SuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().ids().euid, kuid(1000));
        assert_eq!(proposal.proposed().ids().suid, kuid(1000));
        assert_eq!(proposal.proposed().capabilities().permitted(), retained);
        assert_eq!(proposal.proposed().capabilities().effective(), retained);
        assert_eq!(
            proposal.proposed().capabilities().ambient(),
            [0; CAPABILITY_WORDS]
        );
        assert!(proposal.effects().secure_exec());
    }

    #[test]
    fn suppressed_non_effective_fcap_downgrade_to_real_ids_is_not_secure() {
        for no_new_privs in [false, true] {
            let old = unprivileged_credential();
            let inherited = bit(CAP_CHOWN);
            let mut ids = old.ids();
            ids.euid = kuid(2000);
            ids.suid = kuid(2000);
            ids.fsuid = kuid(2000);
            let old = with_state(
                &old,
                ids,
                capabilities(
                    [0; CAPABILITY_WORDS],
                    [0; CAPABILITY_WORDS],
                    inherited,
                    CAPABILITY_VALID_MASK,
                    [0; CAPABILITY_WORDS],
                    0,
                ),
                no_new_privs,
            );
            let trace_state = if no_new_privs {
                ExecTraceState::NotSuppressingPrivilege
            } else {
                ExecTraceState::SuppressingPrivilege
            };

            let proposal = derive_exec_credential(
                &old,
                input(
                    0o755,
                    ordinary_input().owner(),
                    ExecMountPrivilege::Honor,
                    trace_state,
                    ExecImageReadability::Readable,
                    Some(file_caps(
                        [0; CAPABILITY_WORDS],
                        inherited,
                        false,
                        Kuid::INITIAL_ROOT,
                    )),
                ),
            )
            .unwrap();

            let proposed = proposal.proposed();
            assert_eq!(proposed.ids().euid, old.ids().ruid);
            assert_eq!(proposed.ids().suid, old.ids().ruid);
            assert_eq!(proposed.ids().fsuid, old.ids().ruid);
            assert_eq!(proposed.capabilities().permitted(), [0; CAPABILITY_WORDS]);
            assert_eq!(proposed.capabilities().effective(), [0; CAPABILITY_WORDS]);
            assert_eq!(proposed.capabilities().ambient(), [0; CAPABILITY_WORDS]);
            assert!(proposal.revalidation().privilege_sensitive());
            assert_eq!(proposal.revalidation().prepared_trace_state(), trace_state);
            assert!(!proposal.effects().secure_exec());
            assert!(!proposal.effects().aux_identity().is_secure());
            assert_eq!(
                proposal.effects().dumpability(),
                ExecDumpability::NotDumpable
            );
            assert!(proposal.effects().clear_pdeath_signal());
        }
    }

    #[test]
    fn ptrace_does_not_downgrade_an_unchanged_nonroot_effective_uid() {
        let old = unprivileged_credential();
        let mut ids = old.ids();
        ids.euid = kuid(2000);
        ids.suid = kuid(2000);
        ids.fsuid = kuid(2000);
        let old = with_state(&old, ids, old.capabilities(), false);

        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::SuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();

        assert_eq!(proposal.proposed().ids().euid, kuid(2000));
        assert_eq!(proposal.proposed().ids().suid, kuid(2000));
        assert_eq!(proposal.proposed().ids().fsuid, kuid(2000));
        assert!(!proposal.revalidation().privilege_sensitive());
        assert!(proposal.effects().secure_exec());
    }

    #[test]
    fn already_effective_root_with_file_caps_does_not_regain_full_bounding_set() {
        let old = unprivileged_credential();
        let retained = bit(CAP_CHOWN);
        let mut ids = old.ids();
        ids.euid = Kuid::INITIAL_ROOT;
        ids.suid = Kuid::INITIAL_ROOT;
        ids.fsuid = Kuid::INITIAL_ROOT;
        let old = with_state(
            &old,
            ids,
            capabilities(
                retained,
                retained,
                [0; CAPABILITY_WORDS],
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                0,
            ),
            false,
        );
        let explicit = bit(CAP_DAC_OVERRIDE);
        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    explicit,
                    [0; CAPABILITY_WORDS],
                    true,
                    Kuid::INITIAL_ROOT,
                )),
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().ids().euid, Kuid::INITIAL_ROOT);
        assert_eq!(proposal.proposed().capabilities().permitted(), explicit);
        assert_eq!(proposal.proposed().capabilities().effective(), explicit);
    }

    #[test]
    fn no_new_privs_downgrades_a_restricted_effective_root_transition() {
        let old = unprivileged_credential();
        let retained = bit(CAP_CHOWN);
        let mut ids = old.ids();
        ids.euid = Kuid::INITIAL_ROOT;
        ids.suid = Kuid::INITIAL_ROOT;
        ids.fsuid = Kuid::INITIAL_ROOT;
        let old = with_state(
            &old,
            ids,
            capabilities(
                retained,
                retained,
                [0; CAPABILITY_WORDS],
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                0,
            ),
            true,
        );
        let proposal = derive_exec_credential(
            &old,
            input(
                0o4755,
                Some(ExecFileOwner::new(kuid(2000), Kgid::INITIAL_ROOT)),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().ids().euid, kuid(1000));
        assert_eq!(proposal.proposed().ids().suid, kuid(1000));
        assert_eq!(proposal.proposed().capabilities().permitted(), retained);
        assert_eq!(proposal.proposed().capabilities().effective(), retained);
        assert!(proposal.effects().secure_exec());
        assert!(proposal.effects().clear_pdeath_signal());
    }

    #[test]
    fn empty_valid_file_capability_record_still_clears_ambient() {
        let old = unprivileged_credential();
        let ambient = bit(CAP_CHOWN);
        let old = with_state(
            &old,
            old.ids(),
            capabilities(ambient, ambient, ambient, CAPABILITY_VALID_MASK, ambient, 0),
            false,
        );
        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    [0; CAPABILITY_WORDS],
                    [0; CAPABILITY_WORDS],
                    false,
                    Kuid::INITIAL_ROOT,
                )),
            ),
        )
        .unwrap();
        assert_eq!(
            proposal.proposed().capabilities().ambient(),
            [0; CAPABILITY_WORDS]
        );
        assert_eq!(
            proposal.proposed().capabilities().permitted(),
            [0; CAPABILITY_WORDS]
        );
        assert_eq!(
            proposal.proposed().capabilities().effective(),
            [0; CAPABILITY_WORDS]
        );
    }

    #[test]
    fn forced_file_permitted_can_arrive_through_the_inheritable_path() {
        let old = unprivileged_credential();
        let inherited = bit(CAP_CHOWN);
        let old = with_state(
            &old,
            old.ids(),
            capabilities(
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                inherited,
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                0,
            ),
            false,
        );
        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(inherited, inherited, true, Kuid::INITIAL_ROOT)),
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().capabilities().permitted(), inherited);
        assert_eq!(proposal.proposed().capabilities().effective(), inherited);
    }

    #[test]
    fn activating_already_permitted_file_cap_is_secure_but_remains_dumpable() {
        let old = unprivileged_credential();
        let existing = bit(CAP_CHOWN);
        let old = with_state(
            &old,
            old.ids(),
            capabilities(
                [0; CAPABILITY_WORDS],
                existing,
                existing,
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                0,
            ),
            false,
        );
        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    existing,
                    [0; CAPABILITY_WORDS],
                    true,
                    Kuid::INITIAL_ROOT,
                )),
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().capabilities().permitted(), existing);
        assert_eq!(proposal.proposed().capabilities().effective(), existing);
        assert!(proposal.effects().secure_exec());
        assert!(proposal.effects().clear_pdeath_signal());
        assert_eq!(
            proposal.effects().dumpability(),
            ExecDumpability::UserDumpable
        );
    }

    #[test]
    fn setgid_to_supplementary_group_is_secure_without_clearing_ambient() {
        let old = unprivileged_credential();
        let ambient = bit(CAP_CHOWN);
        let supplemental = kgid(2000);
        let old = transition(
            &old,
            old.ids(),
            GroupInfo::try_new(vec![supplemental]).unwrap(),
            capabilities(ambient, ambient, ambient, CAPABILITY_VALID_MASK, ambient, 0),
            false,
        );
        let proposal = derive_exec_credential(
            &old,
            input(
                0o2755,
                Some(ExecFileOwner::new(Kuid::INITIAL_ROOT, supplemental)),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().ids().egid, supplemental);
        assert_eq!(proposal.proposed().capabilities().ambient(), ambient);
        assert_eq!(proposal.proposed().capabilities().permitted(), ambient);
        assert_eq!(proposal.proposed().capabilities().effective(), ambient);
        assert!(!proposal.revalidation().privilege_sensitive());
        assert!(proposal.effects().secure_exec());
    }

    #[test]
    fn setgid_to_filesystem_group_is_secure_without_clearing_ambient() {
        let old = unprivileged_credential();
        let ambient = bit(CAP_CHOWN);
        let filesystem_group = kgid(2000);
        let mut ids = old.ids();
        ids.fsgid = filesystem_group;
        let old = with_state(
            &old,
            ids,
            capabilities(ambient, ambient, ambient, CAPABILITY_VALID_MASK, ambient, 0),
            false,
        );
        assert_eq!(old.ids().fsgid, filesystem_group);
        assert!(!old.groups().contains(filesystem_group));

        let proposal = derive_exec_credential(
            &old,
            input(
                0o2755,
                Some(ExecFileOwner::new(Kuid::INITIAL_ROOT, filesystem_group)),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();

        assert_eq!(proposal.proposed().ids().egid, filesystem_group);
        assert_eq!(proposal.proposed().capabilities().ambient(), ambient);
        assert_eq!(proposal.proposed().capabilities().permitted(), ambient);
        assert_eq!(proposal.proposed().capabilities().effective(), ambient);
        assert!(!proposal.revalidation().privilege_sensitive());
        assert!(proposal.effects().secure_exec());
    }

    #[test]
    fn setuid_to_real_uid_clears_ambient_and_remains_privilege_sensitive() {
        let old = unprivileged_credential();
        let ambient = bit(CAP_CHOWN);
        let mut ids = old.ids();
        ids.euid = kuid(2000);
        ids.suid = kuid(2000);
        ids.fsuid = kuid(2000);
        let old = with_state(
            &old,
            ids,
            capabilities(ambient, ambient, ambient, CAPABILITY_VALID_MASK, ambient, 0),
            false,
        );

        let proposal = derive_exec_credential(
            &old,
            input(
                0o4755,
                Some(ExecFileOwner::new(old.ids().ruid, old.ids().rgid)),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();

        assert_eq!(proposal.proposed().ids().euid, old.ids().ruid);
        assert_eq!(
            proposal.proposed().capabilities().ambient(),
            [0; CAPABILITY_WORDS]
        );
        assert_eq!(
            proposal.proposed().capabilities().permitted(),
            [0; CAPABILITY_WORDS]
        );
        assert!(proposal.revalidation().privilege_sensitive());
        assert!(proposal.effects().secure_exec());
        assert!(proposal.effects().aux_identity().is_secure());
    }

    #[test]
    fn ambient_survives_ordinary_exec_and_clears_for_privileged_file() {
        let old = unprivileged_credential();
        let ambient = bit(CAP_CHOWN);
        let old = with_state(
            &old,
            old.ids(),
            capabilities(ambient, ambient, ambient, CAPABILITY_VALID_MASK, ambient, 0),
            false,
        );
        let ordinary = derive_exec_credential(&old, ordinary_input()).unwrap();
        assert_eq!(ordinary.proposed().capabilities().ambient(), ambient);
        assert_eq!(ordinary.proposed().capabilities().permitted(), ambient);
        assert_eq!(ordinary.proposed().capabilities().effective(), ambient);

        let privileged = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    bit(CAP_DAC_OVERRIDE),
                    [0; CAPABILITY_WORDS],
                    true,
                    Kuid::INITIAL_ROOT,
                )),
            ),
        )
        .unwrap();
        assert_eq!(
            privileged.proposed().capabilities().ambient(),
            [0; CAPABILITY_WORDS]
        );
        assert_eq!(
            privileged.proposed().capabilities().effective(),
            bit(CAP_DAC_OVERRIDE)
        );
        assert!(privileged.effects().secure_exec());
    }

    #[test]
    fn effective_file_caps_reject_bounding_and_inheritable_truncation() {
        let old = unprivileged_credential();
        let old = with_state(
            &old,
            old.ids(),
            capabilities(
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                0,
            ),
            false,
        );
        let result = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    bit(CAP_CHOWN),
                    [0; CAPABILITY_WORDS],
                    true,
                    Kuid::INITIAL_ROOT,
                )),
            ),
        );
        assert!(matches!(result, Err(CredError::NotPermitted)));
    }

    #[test]
    fn non_effective_file_caps_are_safely_truncated_by_bounding_set() {
        let old = unprivileged_credential();
        let old = with_state(
            &old,
            old.ids(),
            capabilities(
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                0,
            ),
            false,
        );
        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    bit(CAP_CHOWN),
                    [0; CAPABILITY_WORDS],
                    false,
                    Kuid::INITIAL_ROOT,
                )),
            ),
        )
        .unwrap();
        assert_eq!(
            proposal.proposed().capabilities().permitted(),
            [0; CAPABILITY_WORDS]
        );
        assert_eq!(
            proposal.proposed().capabilities().effective(),
            [0; CAPABILITY_WORDS]
        );
    }

    #[test]
    fn nonroot_setuid_root_with_file_caps_does_not_gain_full_root_set() {
        let old = unprivileged_credential();
        let explicit = bit(CAP_CHOWN);
        let proposal = derive_exec_credential(
            &old,
            input(
                0o4755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    explicit,
                    [0; CAPABILITY_WORDS],
                    true,
                    Kuid::INITIAL_ROOT,
                )),
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().ids().euid, Kuid::INITIAL_ROOT);
        assert_eq!(proposal.proposed().capabilities().permitted(), explicit);
        assert_eq!(proposal.proposed().capabilities().effective(), explicit);
    }

    #[test]
    fn file_inheritable_intersects_process_inheritable() {
        let old = unprivileged_credential();
        let inherited = bit(CAP_CHOWN);
        let ignored = bit(CAP_DAC_OVERRIDE);
        let old = with_state(
            &old,
            old.ids(),
            capabilities(
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                inherited,
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                0,
            ),
            false,
        );
        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    [0; CAPABILITY_WORDS],
                    union(inherited, ignored),
                    true,
                    Kuid::INITIAL_ROOT,
                )),
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().capabilities().permitted(), inherited);
        assert_eq!(proposal.proposed().capabilities().effective(), inherited);
    }

    #[test]
    fn noroot_disables_legacy_root_capability_compatibility() {
        let root = root_credential();
        let proposal = derive_exec_credential(&root, ordinary_input()).unwrap();
        assert_eq!(
            proposal.proposed().capabilities().permitted(),
            CAPABILITY_VALID_MASK
        );
        assert_eq!(
            proposal.proposed().capabilities().effective(),
            CAPABILITY_VALID_MASK
        );

        let old_caps = root.capabilities();
        let noroot = with_state(
            &root,
            root.ids(),
            capabilities(
                old_caps.effective(),
                old_caps.permitted(),
                old_caps.inheritable(),
                old_caps.bounding(),
                old_caps.ambient(),
                old_caps.securebits() | SECBIT_NOROOT,
            ),
            false,
        );
        let proposal = derive_exec_credential(&noroot, ordinary_input()).unwrap();
        assert_eq!(
            proposal.proposed().capabilities().permitted(),
            [0; CAPABILITY_WORDS]
        );
        assert_eq!(
            proposal.proposed().capabilities().effective(),
            [0; CAPABILITY_WORDS]
        );
    }

    #[test]
    fn exec_clears_keep_caps_but_preserves_its_lock_and_other_securebits() {
        let root = root_credential();
        let old_caps = root.capabilities();
        let securebits = old_caps.securebits()
            | SECBIT_KEEP_CAPS
            | SECBIT_KEEP_CAPS_LOCKED
            | SECBIT_NOROOT
            | SECBIT_EXEC_RESTRICT_FILE
            | SECBIT_EXEC_RESTRICT_FILE_LOCKED
            | SECBIT_EXEC_DENY_INTERACTIVE
            | SECBIT_EXEC_DENY_INTERACTIVE_LOCKED;
        let root = with_state(
            &root,
            root.ids(),
            capabilities(
                old_caps.effective(),
                old_caps.permitted(),
                old_caps.inheritable(),
                old_caps.bounding(),
                old_caps.ambient(),
                securebits,
            ),
            false,
        );
        let proposal = derive_exec_credential(&root, ordinary_input()).unwrap();
        let proposed = proposal.proposed().capabilities().securebits();
        assert_eq!(proposed & SECBIT_KEEP_CAPS, 0);
        assert_ne!(proposed & SECBIT_KEEP_CAPS_LOCKED, 0);
        assert_ne!(proposed & SECBIT_NOROOT, 0);
        assert_ne!(proposed & SECBIT_EXEC_RESTRICT_FILE, 0);
        assert_ne!(proposed & SECBIT_EXEC_RESTRICT_FILE_LOCKED, 0);
        assert_ne!(proposed & SECBIT_EXEC_DENY_INTERACTIVE, 0);
        assert_ne!(proposed & SECBIT_EXEC_DENY_INTERACTIVE_LOCKED, 0);
    }

    #[test]
    fn namespaced_v3_rootid_must_name_current_or_ancestor_root() {
        let root_ns = MockNamespace::root();
        let root_cred = Credential::try_root(root_ns.clone()).unwrap();
        let uid_map =
            IdMap::try_from_parent(vec![IdMapInputExtent::new(0, 1000, 1)], &root_ns.uid_map)
                .unwrap();
        let gid_map =
            IdMap::try_from_parent(vec![IdMapInputExtent::new(0, 1000, 1)], &root_ns.gid_map)
                .unwrap();
        let child_ns = MockNamespace::child(&root_ns, Kuid::INITIAL_ROOT, uid_map, gid_map);
        let child_cred = Credential::try_with_user_namespace(&root_cred, child_ns).unwrap();

        let accepted = derive_exec_credential(
            &child_cred,
            input(
                0o755,
                None,
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    bit(CAP_CHOWN),
                    [0; CAPABILITY_WORDS],
                    true,
                    kuid(1000),
                )),
            ),
        )
        .unwrap();
        assert_eq!(
            accepted.proposed().capabilities().effective(),
            bit(CAP_CHOWN)
        );

        let rejected = derive_exec_credential(
            &child_cred,
            input(
                0o755,
                None,
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    bit(CAP_CHOWN),
                    [0; CAPABILITY_WORDS],
                    true,
                    kuid(2000),
                )),
            ),
        )
        .unwrap();
        assert_eq!(
            rejected.proposed().capabilities().effective(),
            [0; CAPABILITY_WORDS]
        );
    }

    #[test]
    fn mode_and_owner_mapping_are_derived_from_one_snapshot() {
        let uid_map = IdMap::try_identity().unwrap();
        let gid_map = IdMap::try_empty().unwrap();
        let namespace = MockNamespace::root_with_maps(uid_map, gid_map, Some(Kuid::INITIAL_ROOT));
        let root = Credential::try_root(namespace.clone()).unwrap();
        let uid = kuid(1000);
        let gid = kgid(1000);
        let old = transition(
            &root,
            CredentialIds {
                ruid: uid,
                euid: uid,
                suid: uid,
                fsuid: uid,
                rgid: gid,
                egid: gid,
                sgid: gid,
                fsgid: gid,
            },
            GroupInfo::try_new(vec![gid]).unwrap(),
            capabilities(
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                0,
            ),
            false,
        );
        let proposal = derive_exec_credential(
            &old,
            input(
                0o6755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().ids().euid, uid);
        assert_eq!(proposal.proposed().ids().egid, gid);
        assert_eq!(namespace.snapshot_calls.load(Ordering::SeqCst), 1);

        let old = unprivileged_credential();
        let proposal = derive_exec_credential(
            &old,
            input(
                0o6745,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().ids().euid, Kuid::INITIAL_ROOT);
        assert_eq!(proposal.proposed().ids().egid, kgid(1000));
    }

    #[test]
    fn current_root_compatibility_reuses_the_frozen_uid_map() {
        let parent = IdMap::try_identity().unwrap();
        let uid_map =
            IdMap::try_from_parent(vec![IdMapInputExtent::new(0, 1000, 1)], &parent).unwrap();
        let namespace = MockNamespace::root_with_maps(uid_map, parent, Some(kuid(2000)));
        let old = Credential::try_root(namespace.clone()).unwrap();
        let proposal = derive_exec_credential(
            &old,
            input(
                0o755,
                None,
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    bit(CAP_CHOWN),
                    [0; CAPABILITY_WORDS],
                    true,
                    kuid(1000),
                )),
            ),
        )
        .unwrap();
        assert_eq!(
            proposal.proposed().capabilities().effective(),
            bit(CAP_CHOWN)
        );
        assert_eq!(namespace.snapshot_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nosuid_suppresses_mode_and_file_caps_before_privilege_derivation() {
        let old = unprivileged_credential();
        let proposal = derive_exec_credential(
            &old,
            input(
                0o6755,
                ordinary_input().owner(),
                ExecMountPrivilege::NoSuid,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                Some(file_caps(
                    bit(CAP_CHOWN),
                    [0; CAPABILITY_WORDS],
                    true,
                    Kuid::INITIAL_ROOT,
                )),
            ),
        )
        .unwrap();
        assert_eq!(proposal.proposed().ids(), old.ids());
        assert_eq!(
            proposal.proposed().capabilities().permitted(),
            [0; CAPABILITY_WORDS]
        );
        assert!(!proposal.effects().secure_exec());
        assert!(!proposal.revalidation().privilege_sensitive());
    }

    #[test]
    fn proposal_release_requires_the_exact_old_arc() {
        let old = unprivileged_credential();
        let equal_but_distinct = with_state(&old, old.ids(), old.capabilities(), false);
        let proposal = derive_exec_credential(&old, ordinary_input()).unwrap();
        assert!(matches!(
            proposal.try_into_proposed(&equal_but_distinct),
            Err(CredError::NotPermitted)
        ));

        let proposal = derive_exec_credential(&old, ordinary_input()).unwrap();
        let proposed = proposal.try_into_proposed(&old).unwrap();
        assert_eq!(proposed.ids(), old.ids());
        assert!(!Arc::ptr_eq(&proposed, &old));
    }

    #[test]
    fn ptrace_revalidation_only_rejects_a_new_suppressor_for_sensitive_exec() {
        let old = unprivileged_credential();
        let privileged = derive_exec_credential(
            &old,
            input(
                0o4755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::NotSuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();
        assert!(
            privileged
                .revalidation()
                .is_stale(ExecTraceState::SuppressingPrivilege)
        );
        assert!(
            !privileged
                .revalidation()
                .is_stale(ExecTraceState::NotSuppressingPrivilege)
        );

        let ordinary = derive_exec_credential(&old, ordinary_input()).unwrap();
        assert!(
            !ordinary
                .revalidation()
                .is_stale(ExecTraceState::SuppressingPrivilege)
        );
    }

    fn forged_proposal(
        old: Arc<Credential<MockNamespace>>,
        proposed: Arc<Credential<MockNamespace>>,
        input: ExecCredentialInput,
    ) -> ExecCredentialProposal<MockNamespace> {
        ExecCredentialProposal {
            old,
            proposed,
            input,
            effects: ExecCredentialEffects {
                aux_identity: ExecAuxIdentity::trusted_boot(),
                dumpability: ExecDumpability::UserDumpable,
                clear_pdeath_signal: false,
                clear_personality_on_setid: false,
            },
            revalidation: ExecPtraceRevalidation {
                privilege_sensitive: false,
                prepared_trace_state: input.trace_state(),
            },
        }
    }

    #[test]
    fn commoncap_rejects_namespace_nnp_and_suppressed_capability_forgery() {
        let valid_old = unprivileged_credential();
        let valid = derive_exec_credential(
            &valid_old,
            input(
                0o4755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::SuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        )
        .unwrap();
        commoncap_exec_transition(&valid).unwrap();

        let root = root_credential();
        let unprivileged = unprivileged_from_root(&root);
        let suppressed = forged_proposal(
            unprivileged,
            root,
            input(
                0o755,
                ordinary_input().owner(),
                ExecMountPrivilege::Honor,
                ExecTraceState::SuppressingPrivilege,
                ExecImageReadability::Readable,
                None,
            ),
        );
        assert_eq!(
            commoncap_exec_transition(&suppressed),
            Err(CredError::NotPermitted)
        );

        let old = root_credential();
        let nnp = with_state(&old, old.ids(), old.capabilities(), true);
        let changed_nnp = forged_proposal(old, nnp, ordinary_input());
        assert_eq!(
            commoncap_exec_transition(&changed_nnp),
            Err(CredError::NotPermitted)
        );

        let old = root_credential();
        let other_namespace = root_credential();
        let changed_namespace = forged_proposal(old, other_namespace, ordinary_input());
        assert_eq!(
            commoncap_exec_transition(&changed_namespace),
            Err(CredError::NotPermitted)
        );
    }
}
