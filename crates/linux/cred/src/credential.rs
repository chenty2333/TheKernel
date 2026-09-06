//! Immutable Linux credential values and namespace-relative capability rules.
//!
//! This module deliberately owns no task, process, hook registry, or
//! publication lock. An embedding kernel serializes writers, prepares an
//! exact-old-bound [`PreparedCredential`], attaches its own unpublished state,
//! and atomically publishes the fully validated replacement.

use alloc::{sync::Arc, vec::Vec};

use linux_raw_sys::general::{CAP_LAST_CAP, NGROUPS_MAX};

use crate::{CredError, Kgid, Kuid};

/// Number of 32-bit words needed for every Linux capability through
/// `CAP_LAST_CAP`.
pub const CAPABILITY_WORDS: usize = 2;

const fn capability_valid_mask_word(word: usize) -> u32 {
    let first_capability = word as u32 * u32::BITS;
    if CAP_LAST_CAP < first_capability {
        return 0;
    }

    let last_bit = CAP_LAST_CAP - first_capability;
    if last_bit >= u32::BITS - 1 {
        u32::MAX
    } else {
        (1_u32 << (last_bit + 1)) - 1
    }
}

/// Valid capability bits in each capability word.
pub const CAPABILITY_VALID_MASK: [u32; CAPABILITY_WORDS] =
    [capability_valid_mask_word(0), capability_valid_mask_word(1)];

/// Disable legacy UID-0 capability fixups.
pub const SECBIT_NOROOT: u32 = 1 << 0;
/// Lock [`SECBIT_NOROOT`].
pub const SECBIT_NOROOT_LOCKED: u32 = 1 << 1;
/// Disable automatic capability changes across set-ID transitions.
pub const SECBIT_NO_SETUID_FIXUP: u32 = 1 << 2;
/// Lock [`SECBIT_NO_SETUID_FIXUP`].
pub const SECBIT_NO_SETUID_FIXUP_LOCKED: u32 = 1 << 3;
/// Retain permitted capabilities after leaving the namespace root identity.
pub const SECBIT_KEEP_CAPS: u32 = 1 << 4;
/// Lock [`SECBIT_KEEP_CAPS`].
pub const SECBIT_KEEP_CAPS_LOCKED: u32 = 1 << 5;
/// Disallow raising ambient capabilities.
pub const SECBIT_NO_CAP_AMBIENT_RAISE: u32 = 1 << 6;
/// Lock [`SECBIT_NO_CAP_AMBIENT_RAISE`].
pub const SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED: u32 = 1 << 7;
/// Ask userspace interpreters to require a successful kernel execution check
/// before interpreting a file.
pub const SECBIT_EXEC_RESTRICT_FILE: u32 = 1 << 8;
/// Lock [`SECBIT_EXEC_RESTRICT_FILE`].
pub const SECBIT_EXEC_RESTRICT_FILE_LOCKED: u32 = 1 << 9;
/// Ask userspace interpreters to reject unchecked interactive commands.
pub const SECBIT_EXEC_DENY_INTERACTIVE: u32 = 1 << 10;
/// Lock [`SECBIT_EXEC_DENY_INTERACTIVE`].
pub const SECBIT_EXEC_DENY_INTERACTIVE_LOCKED: u32 = 1 << 11;
/// Every supported securebit value bit, excluding its adjacent lock bit.
pub const SECURE_ALL_BITS: u32 = SECBIT_NOROOT
    | SECBIT_NO_SETUID_FIXUP
    | SECBIT_KEEP_CAPS
    | SECBIT_NO_CAP_AMBIENT_RAISE
    | SECBIT_EXEC_RESTRICT_FILE
    | SECBIT_EXEC_DENY_INTERACTIVE;
/// Every supported securebit lock bit.
pub const SECURE_ALL_LOCKS: u32 = SECURE_ALL_BITS << 1;
/// Securebit values which Linux v6.18 allows an unprivileged actor to change.
///
/// Capability admission remains consumer-owned; this mask lets a consumer
/// reproduce Linux's `PR_SET_SECUREBITS` admission without importing raw UAPI
/// constants or duplicating the supported-bit set.
pub const SECURE_ALL_UNPRIVILEGED: u32 = SECBIT_EXEC_RESTRICT_FILE | SECBIT_EXEC_DENY_INTERACTIVE;

/// Complete kernel-global real, effective, saved, and filesystem IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialIds {
    /// Real user ID.
    pub ruid: Kuid,
    /// Effective user ID.
    pub euid: Kuid,
    /// Saved set-user-ID.
    pub suid: Kuid,
    /// Filesystem user ID.
    pub fsuid: Kuid,
    /// Real group ID.
    pub rgid: Kgid,
    /// Effective group ID.
    pub egid: Kgid,
    /// Saved set-group-ID.
    pub sgid: Kgid,
    /// Filesystem group ID.
    pub fsgid: Kgid,
}

impl CredentialIds {
    /// Initial-user-namespace root identity in every ID role.
    pub const fn initial_root() -> Self {
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

    /// Compatibility spelling for [`Self::initial_root`].
    pub const fn root() -> Self {
        Self::initial_root()
    }
}

/// Immutable, sorted supplementary groups.
#[derive(Debug)]
pub struct GroupInfo {
    groups: Vec<Kgid>,
}

impl GroupInfo {
    /// Validates and shares one supplementary-group list.
    pub fn try_new(mut groups: Vec<Kgid>) -> Result<Arc<Self>, CredError> {
        if groups.len() > NGROUPS_MAX as usize {
            return Err(CredError::InvalidInput);
        }
        groups.sort_unstable();
        Arc::try_new(Self { groups }).map_err(|_| CredError::NoMemory)
    }

    /// Returns the sorted group IDs.
    pub fn as_slice(&self) -> &[Kgid] {
        &self.groups
    }

    /// Tests supplementary membership with a binary search.
    pub fn contains(&self, gid: Kgid) -> bool {
        self.groups.binary_search(&gid).is_ok()
    }
}

/// Linux effective, permitted, inheritable, bounding, and ambient capability
/// sets plus securebits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilitySets {
    effective: [u32; CAPABILITY_WORDS],
    permitted: [u32; CAPABILITY_WORDS],
    inheritable: [u32; CAPABILITY_WORDS],
    bounding: [u32; CAPABILITY_WORDS],
    ambient: [u32; CAPABILITY_WORDS],
    securebits: u32,
}

impl CapabilitySets {
    /// Constructs and validates complete capability state.
    pub fn try_new(
        effective: [u32; CAPABILITY_WORDS],
        permitted: [u32; CAPABILITY_WORDS],
        inheritable: [u32; CAPABILITY_WORDS],
        bounding: [u32; CAPABILITY_WORDS],
        ambient: [u32; CAPABILITY_WORDS],
        securebits: u32,
    ) -> Result<Self, CredError> {
        let sets = Self {
            effective,
            permitted,
            inheritable,
            bounding,
            ambient,
            securebits,
        };
        sets.validate()?;
        Ok(sets)
    }

    /// Full namespace-relative capability authority with default securebits.
    pub const fn full() -> Self {
        Self {
            effective: CAPABILITY_VALID_MASK,
            permitted: CAPABILITY_VALID_MASK,
            inheritable: [0; CAPABILITY_WORDS],
            bounding: CAPABILITY_VALID_MASK,
            ambient: [0; CAPABILITY_WORDS],
            securebits: 0,
        }
    }

    /// Empty capability authority with default securebits.
    pub const fn empty() -> Self {
        Self {
            effective: [0; CAPABILITY_WORDS],
            permitted: [0; CAPABILITY_WORDS],
            inheritable: [0; CAPABILITY_WORDS],
            bounding: [0; CAPABILITY_WORDS],
            ambient: [0; CAPABILITY_WORDS],
            securebits: 0,
        }
    }

    /// Returns the effective set.
    pub const fn effective(self) -> [u32; CAPABILITY_WORDS] {
        self.effective
    }

    /// Returns the permitted set.
    pub const fn permitted(self) -> [u32; CAPABILITY_WORDS] {
        self.permitted
    }

    /// Returns the inheritable set.
    pub const fn inheritable(self) -> [u32; CAPABILITY_WORDS] {
        self.inheritable
    }

    /// Returns the bounding set.
    pub const fn bounding(self) -> [u32; CAPABILITY_WORDS] {
        self.bounding
    }

    /// Returns the ambient set.
    pub const fn ambient(self) -> [u32; CAPABILITY_WORDS] {
        self.ambient
    }

    /// Returns the securebits word.
    pub const fn securebits(self) -> u32 {
        self.securebits
    }

    /// Resolves one Linux capability number into a word and bit mask.
    pub const fn cap_mask(capability: u32) -> Option<(usize, u32)> {
        if capability > CAP_LAST_CAP {
            return None;
        }
        let word = capability as usize / u32::BITS as usize;
        if word >= CAPABILITY_WORDS {
            None
        } else {
            Some((word, 1_u32 << (capability % u32::BITS)))
        }
    }

    /// Tests the effective set.
    pub fn has_effective(self, capability: u32) -> bool {
        Self::cap_mask(capability).is_some_and(|(word, mask)| self.effective[word] & mask != 0)
    }

    /// Tests the bounding set.
    pub fn bounding_contains(self, capability: u32) -> bool {
        Self::cap_mask(capability).is_some_and(|(word, mask)| self.bounding[word] & mask != 0)
    }

    /// Tests the ambient set.
    pub fn ambient_contains(self, capability: u32) -> bool {
        Self::cap_mask(capability).is_some_and(|(word, mask)| self.ambient[word] & mask != 0)
    }

    /// Raises one ambient capability when permitted and inheritable both
    /// contain it and securebits allow the transition.
    pub fn try_raise_ambient(&mut self, capability: u32) -> Result<(), CredError> {
        let Some((word, mask)) = Self::cap_mask(capability) else {
            return Err(CredError::InvalidInput);
        };
        if self.securebits & SECBIT_NO_CAP_AMBIENT_RAISE != 0
            || self.permitted[word] & mask == 0
            || self.inheritable[word] & mask == 0
        {
            return Err(CredError::NotPermitted);
        }
        self.ambient[word] |= mask;
        Ok(())
    }

    /// Lowers one ambient capability.
    pub fn try_lower_ambient(&mut self, capability: u32) -> Result<(), CredError> {
        let Some((word, mask)) = Self::cap_mask(capability) else {
            return Err(CredError::InvalidInput);
        };
        self.ambient[word] &= !mask;
        Ok(())
    }

    /// Clears every ambient capability.
    pub fn clear_ambient(&mut self) {
        self.ambient = [0; CAPABILITY_WORDS];
    }

    /// Intersects ambient authority with permitted and inheritable authority.
    pub fn reconcile_ambient(&mut self) {
        for word in 0..CAPABILITY_WORDS {
            self.ambient[word] &= self.permitted[word] & self.inheritable[word];
        }
    }

    /// Drops one bounding capability.
    pub fn try_drop_bounding(&mut self, capability: u32) -> Result<(), CredError> {
        let Some((word, mask)) = Self::cap_mask(capability) else {
            return Err(CredError::InvalidInput);
        };
        self.bounding[word] &= !mask;
        Ok(())
    }

    /// Replaces the securebits word after enforcing Linux's ordinary
    /// lock/value transition rules.
    ///
    /// Capability admission remains a consumer responsibility. This method
    /// only validates the requested state against this exact old value; it
    /// does not consult a current task or dispatch a security hook.
    pub fn try_set_securebits(&mut self, requested: u32) -> Result<(), CredError> {
        if requested & !(SECURE_ALL_BITS | SECURE_ALL_LOCKS) != 0 {
            return Err(CredError::NotPermitted);
        }

        let locked_values = (self.securebits & SECURE_ALL_LOCKS) >> 1;
        let changed_locked_values = locked_values & (self.securebits ^ requested) & SECURE_ALL_BITS;
        let cleared_locks = self.securebits & SECURE_ALL_LOCKS & !requested;
        if changed_locked_values != 0 || cleared_locks != 0 {
            return Err(CredError::NotPermitted);
        }

        self.securebits = requested;
        Ok(())
    }

    /// Enables or disables `KEEP_CAPS` unless its adjacent lock is set.
    ///
    /// This is the ordinary `prctl`-style transition. Exec's dedicated
    /// `KEEP_CAPS` clearing relaxation remains private to the exec planner.
    pub fn try_set_keep_caps(&mut self, enabled: bool) -> Result<(), CredError> {
        if self.securebits & SECBIT_KEEP_CAPS_LOCKED != 0 {
            return Err(CredError::NotPermitted);
        }
        if enabled {
            self.securebits |= SECBIT_KEEP_CAPS;
        } else {
            self.securebits &= !SECBIT_KEEP_CAPS;
        }
        Ok(())
    }

    fn validate(self) -> Result<(), CredError> {
        if self.securebits & !(SECURE_ALL_BITS | SECURE_ALL_LOCKS) != 0 {
            return Err(CredError::InvalidInput);
        }
        for (word, valid_mask) in CAPABILITY_VALID_MASK.iter().copied().enumerate() {
            let all = self.effective[word]
                | self.permitted[word]
                | self.inheritable[word]
                | self.bounding[word]
                | self.ambient[word];
            if all & !valid_mask != 0 {
                return Err(CredError::InvalidInput);
            }
            if self.effective[word] & !self.permitted[word] != 0
                || self.ambient[word] & !(self.permitted[word] & self.inheritable[word]) != 0
            {
                return Err(CredError::NotPermitted);
            }
        }
        Ok(())
    }
}

/// Immutable filesystem identity and effective capability snapshot.
///
/// VFS adapters may borrow this value and implement their own permission
/// traits without creating a dependency from credentials to a VFS package.
#[derive(Clone, Debug)]
pub struct FsCredentialSnapshot {
    uid: Kuid,
    gid: Kgid,
    groups: Arc<GroupInfo>,
    effective: [u32; CAPABILITY_WORDS],
    capabilities_apply_to_initial_user_ns: bool,
}

impl FsCredentialSnapshot {
    /// Constructs one already-frozen filesystem credential view.
    pub fn new(
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

    /// Returns the filesystem user ID.
    pub const fn uid(&self) -> Kuid {
        self.uid
    }

    /// Returns the filesystem group ID.
    pub const fn gid(&self) -> Kgid {
        self.gid
    }

    /// Returns sorted supplementary groups.
    pub fn supplementary_groups(&self) -> &[Kgid] {
        self.groups.as_slice()
    }

    /// Tests effective capability authority over an initial-namespace object.
    pub fn has_capability(&self, capability: u32) -> bool {
        if !self.capabilities_apply_to_initial_user_ns {
            return false;
        }
        Self::selected_capability(self, capability)
    }

    /// Tests the selected effective set without applying an object-namespace
    /// gate. Namespace-aware callers must perform their own `ns_capable()`
    /// selection before using this method.
    pub fn selected_capability(&self, capability: u32) -> bool {
        CapabilitySets::cap_mask(capability)
            .is_some_and(|(word, mask)| self.effective[word] & mask != 0)
    }
}

/// Read-only namespace topology required by credential policy.
///
/// Mapping storage, task lookup, procfs identity, and resource-accounting
/// extensions remain choices of the embedding kernel.
pub trait UserNamespaceView: Send + Sync + 'static {
    /// Returns the parent namespace, or `None` for the initial namespace.
    fn parent(self: &Arc<Self>) -> Option<Arc<Self>>;

    /// Returns the root-at-zero nesting level.
    fn level(&self) -> u32;

    /// Returns the kernel-global user ID which owns this namespace in its
    /// parent.
    fn owner_kuid(&self) -> Kuid;

    /// Returns the kernel-global user ID mapped from namespace-local root.
    fn root_kuid(&self) -> Option<Kuid>;

    /// Reports whether this is the initial namespace.
    fn is_initial(&self) -> bool;
}

/// One immutable Linux credential snapshot.
pub struct Credential<N: UserNamespaceView> {
    ids: CredentialIds,
    groups: Arc<GroupInfo>,
    caps: CapabilitySets,
    no_new_privs: bool,
    user_ns: Arc<N>,
}

/// Process-level effects derived from one ordinary credential transition.
///
/// The crate reports the Linux decision but deliberately does not own process
/// dumpability, parent-death signals, locking, or publication. A consumer must
/// apply these effects together with the prepared credential in its own
/// atomic transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialTransitionEffects {
    reset_process_security: bool,
}

impl CredentialTransitionEffects {
    fn between<N: UserNamespaceView>(old: &Credential<N>, proposed: &Credential<N>) -> Self {
        let old_ids = old.ids;
        let proposed_ids = proposed.ids;
        Self {
            reset_process_security: old_ids.euid != proposed_ids.euid
                || old_ids.egid != proposed_ids.egid
                || old_ids.fsuid != proposed_ids.fsuid
                || old_ids.fsgid != proposed_ids.fsgid
                || !credential_cap_is_subset(old, proposed),
        }
    }

    /// Reports whether the consumer must lower process dumpability before
    /// publishing the proposed credential.
    pub const fn requires_dumpability_drop(self) -> bool {
        self.reset_process_security
    }

    /// Reports whether the consumer must clear the parent-death signal before
    /// publishing the proposed credential.
    pub const fn clear_pdeath_signal(self) -> bool {
        self.reset_process_security
    }
}

/// Fully validated but unpublished ordinary credential transition.
///
/// The token owns a clone of the exact old [`Arc`] used for preparation. Its
/// proposed owner can only be released by consuming the token with that same
/// old pointer, preventing an equal-looking credential from authorizing a
/// different Linux-core writer snapshot. Accessors intentionally expose
/// borrowed credential values rather than cloneable owners. A kernel that
/// wraps this value with module state must additionally bind its own exact
/// outer credential or publication slot; this leaf cannot identify those
/// consumer-owned objects.
pub struct PreparedCredential<N: UserNamespaceView> {
    old: Arc<Credential<N>>,
    proposed: Arc<Credential<N>>,
    effects: CredentialTransitionEffects,
}

impl<N: UserNamespaceView> PreparedCredential<N> {
    /// Borrows the exact old credential used to prepare this transition.
    pub fn old(&self) -> &Credential<N> {
        self.old.as_ref()
    }

    /// Borrows the complete immutable proposed credential.
    pub fn proposed(&self) -> &Credential<N> {
        self.proposed.as_ref()
    }

    /// Returns the process-level effects that must accompany publication.
    pub const fn effects(&self) -> CredentialTransitionEffects {
        self.effects
    }

    /// Releases the proposed owner only for the exact writer snapshot from
    /// which this token was prepared.
    ///
    /// # Errors
    ///
    /// Returns [`CredError::NotPermitted`] when `expected_old` is a distinct
    /// [`Arc`], even if every observable credential field is equal.
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

/// Special invariant relaxation allowed while finalizing a credential inside
/// this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialTransitionMode {
    /// Ordinary credential transition; every locked securebit is immutable.
    Normal,
    /// Exec may clear `KEEP_CAPS` even when its lock bit remains set.
    ExecClearsKeepCaps,
}

impl<N: UserNamespaceView> Credential<N> {
    /// Constructs the explicit initial root credential.
    pub fn try_root(user_ns: Arc<N>) -> Result<Arc<Self>, CredError> {
        if !user_ns.is_initial() {
            return Err(CredError::InvalidInput);
        }
        let mut root_group = Vec::new();
        root_group
            .try_reserve_exact(1)
            .map_err(|_| CredError::NoMemory)?;
        root_group.push(Kgid::INITIAL_ROOT);
        let groups = GroupInfo::try_new(root_group)?;
        Arc::try_new(Self {
            ids: CredentialIds::initial_root(),
            groups,
            caps: CapabilitySets::full(),
            no_new_privs: false,
            user_ns,
        })
        .map_err(|_| CredError::NoMemory)
    }

    /// Constructs the credential installed on entry to another user
    /// namespace. Kernel-global IDs and groups remain unchanged while
    /// namespace-relative capability state resets to full authority.
    pub fn try_with_user_namespace(
        current: &Self,
        user_ns: Arc<N>,
    ) -> Result<Arc<Self>, CredError> {
        let expected_level = current
            .user_ns
            .level()
            .checked_add(1)
            .ok_or(CredError::NotPermitted)?;
        let parent = user_ns.parent().ok_or(CredError::NotPermitted)?;
        if user_ns.is_initial()
            || user_ns.level() != expected_level
            || !Arc::ptr_eq(&parent, &current.user_ns)
            || user_ns.owner_kuid() != current.ids.euid
        {
            return Err(CredError::NotPermitted);
        }

        // Securebits and their locks are namespace-relative. Entering a new
        // user namespace resets them together with the capability sets, so
        // this dedicated constructor must not apply an ordinary transition's
        // locked-securebit comparison against the parent namespace.
        Arc::try_new(Self {
            ids: current.ids,
            groups: current.groups.clone(),
            caps: CapabilitySets::full(),
            no_new_privs: current.no_new_privs,
            user_ns,
        })
        .map_err(|_| CredError::NoMemory)
    }

    /// Validates and prepares one ordinary immutable replacement bound to the
    /// exact old credential owner.
    ///
    /// The user namespace is inherited from `old`, `no_new_privs` remains
    /// monotonic, and ordinary transitions cannot use exec-only securebit
    /// relaxations. Failure or dropping the returned token leaves `old`
    /// untouched and publishes nothing.
    pub fn try_prepare_transition(
        old: &Arc<Self>,
        ids: CredentialIds,
        groups: Arc<GroupInfo>,
        caps: CapabilitySets,
        no_new_privs: bool,
    ) -> Result<PreparedCredential<N>, CredError> {
        let proposed = Self::try_from_transition(
            old.as_ref(),
            ids,
            groups,
            caps,
            no_new_privs,
            old.user_ns.clone(),
            CredentialTransitionMode::Normal,
        )?;
        let effects = CredentialTransitionEffects::between(old.as_ref(), proposed.as_ref());
        Ok(PreparedCredential {
            old: old.clone(),
            proposed,
            effects,
        })
    }

    /// Validates a complete unpublished value and allocates the immutable
    /// replacement. Failure leaves `old` untouched and publishes nothing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_from_transition(
        old: &Self,
        ids: CredentialIds,
        groups: Arc<GroupInfo>,
        caps: CapabilitySets,
        no_new_privs: bool,
        user_ns: Arc<N>,
        mode: CredentialTransitionMode,
    ) -> Result<Arc<Self>, CredError> {
        caps.validate()?;
        if !Arc::ptr_eq(&old.user_ns, &user_ns) || (old.no_new_privs && !no_new_privs) {
            return Err(CredError::NotPermitted);
        }

        let old_securebits = old.caps.securebits;
        let new_securebits = caps.securebits;
        let locked_values = (old_securebits & SECURE_ALL_LOCKS) >> 1;
        let mut changed_locked_values =
            locked_values & (old_securebits ^ new_securebits) & SECURE_ALL_BITS;
        if mode == CredentialTransitionMode::ExecClearsKeepCaps
            && old_securebits & (SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED)
                == (SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED)
            && new_securebits & SECBIT_KEEP_CAPS == 0
        {
            changed_locked_values &= !SECBIT_KEEP_CAPS;
        }
        let cleared_locks = old_securebits & SECURE_ALL_LOCKS & !new_securebits;
        if changed_locked_values != 0 || cleared_locks != 0 {
            return Err(CredError::NotPermitted);
        }

        Arc::try_new(Self {
            ids,
            groups,
            caps,
            no_new_privs,
            user_ns,
        })
        .map_err(|_| CredError::NoMemory)
    }

    /// Returns all kernel-global identity fields.
    pub const fn ids(&self) -> CredentialIds {
        self.ids
    }

    /// Returns the shared supplementary-group owner.
    pub const fn groups(&self) -> &Arc<GroupInfo> {
        &self.groups
    }

    /// Returns complete capability and securebits state.
    pub const fn capabilities(&self) -> CapabilitySets {
        self.caps
    }

    /// Returns the irreversible no-new-privileges bit.
    pub const fn no_new_privs(&self) -> bool {
        self.no_new_privs
    }

    /// Returns the owning user namespace.
    pub const fn user_ns(&self) -> &Arc<N> {
        &self.user_ns
    }

    /// Freezes the filesystem IDs, groups, and effective capability set used
    /// by one DAC operation.
    pub fn fs_credential_snapshot(&self) -> FsCredentialSnapshot {
        FsCredentialSnapshot::new(
            self.ids.fsuid,
            self.ids.fsgid,
            self.groups.clone(),
            self.caps.effective,
            self.user_ns.is_initial(),
        )
    }

    /// Tests effective capability authority over legacy objects governed by
    /// the initial user namespace.
    pub fn has_effective_capability(&self, capability: u32) -> bool {
        self.user_ns.is_initial() && self.caps.has_effective(capability)
    }

    /// Tests an effective capability relative to this credential's own user
    /// namespace.
    pub fn has_effective_capability_in_own_user_ns(&self, capability: u32) -> bool {
        self.caps.has_effective(capability)
    }

    /// Reports initial-namespace effective UID 0 without granting authority by
    /// itself.
    pub fn is_initial_root_euid(&self) -> bool {
        self.user_ns.is_initial() && self.ids.euid == Kuid::INITIAL_ROOT
    }

    /// Reports initial-namespace real UID 0 without granting authority by
    /// itself.
    pub fn is_initial_root_ruid(&self) -> bool {
        self.user_ns.is_initial() && self.ids.ruid == Kuid::INITIAL_ROOT
    }
}

/// Linux `cred_cap_issubset()` over immutable credentials and namespace
/// ancestry.
pub fn credential_cap_is_subset<N: UserNamespaceView>(
    set: &Credential<N>,
    subset: &Credential<N>,
) -> bool {
    if Arc::ptr_eq(set.user_ns(), subset.user_ns()) {
        return subset
            .caps
            .permitted
            .iter()
            .zip(set.caps.permitted.iter())
            .all(|(subset, set)| subset & !set == 0);
    }

    let set_level = set.user_ns.level();
    let mut subset_ns = subset.user_ns.clone();
    while subset_ns.level() > set_level {
        let current_level = subset_ns.level();
        let Some(parent) = subset_ns.parent() else {
            return false;
        };
        if parent.level() >= current_level {
            return false;
        }
        if Arc::ptr_eq(&parent, set.user_ns()) && subset_ns.owner_kuid() == set.ids.euid {
            return true;
        }
        subset_ns = parent;
    }
    false
}

/// Linux `ns_capable()` direction over one immutable actor credential.
///
/// Effective capabilities apply in the actor's own namespace and descendants.
/// The kernel-global owner of an immediate child additionally owns every
/// capability in that child and its descendants.
///
/// This is the pure topology predicate. Security-hook adapters should use
/// [`crate::authorize_capability_core`] to bind a validated capability and
/// operation to the exact immutable inputs before stacked deny-first dispatch.
pub fn ns_capable<N: UserNamespaceView>(
    actor: &Credential<N>,
    target_user_ns: &Arc<N>,
    capability: u32,
) -> bool {
    if CapabilitySets::cap_mask(capability).is_none() {
        return false;
    }
    let actor_user_ns = actor.user_ns();
    let actor_euid = actor.ids.euid;
    let mut namespace = target_user_ns.clone();
    loop {
        if Arc::ptr_eq(&namespace, actor_user_ns) {
            return actor.has_effective_capability_in_own_user_ns(capability);
        }
        if namespace.level() <= actor_user_ns.level() {
            return false;
        }
        let current_level = namespace.level();
        let Some(parent) = namespace.parent() else {
            return false;
        };
        if parent.level() >= current_level {
            return false;
        }
        if Arc::ptr_eq(&parent, actor_user_ns) && namespace.owner_kuid() == actor_euid {
            return true;
        }
        namespace = parent;
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{sync::Arc, vec};

    use linux_raw_sys::general::{CAP_CHOWN, CAP_KILL, CAP_SYS_ADMIN};

    use super::*;

    struct MockNamespace {
        parent: Option<Arc<Self>>,
        level: u32,
        owner: Kuid,
        root: Option<Kuid>,
    }

    impl MockNamespace {
        fn root() -> Arc<Self> {
            Arc::new(Self {
                parent: None,
                level: 0,
                owner: Kuid::INITIAL_ROOT,
                root: Some(Kuid::INITIAL_ROOT),
            })
        }

        fn child(parent: &Arc<Self>, owner: Kuid) -> Arc<Self> {
            Arc::new(Self {
                parent: Some(parent.clone()),
                level: parent.level + 1,
                owner,
                root: Some(owner),
            })
        }

        fn child_with_level(parent: &Arc<Self>, level: u32, owner: Kuid) -> Arc<Self> {
            Arc::new(Self {
                parent: Some(parent.clone()),
                level,
                owner,
                root: Some(owner),
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
            self.root
        }

        fn is_initial(&self) -> bool {
            self.parent.is_none()
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

    fn ids(uid: u32, gid: u32) -> CredentialIds {
        let uid = kuid(uid);
        let gid = kgid(gid);
        CredentialIds {
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

    fn caps(
        permitted: [u32; CAPABILITY_WORDS],
        effective: [u32; CAPABILITY_WORDS],
        securebits: u32,
    ) -> CapabilitySets {
        CapabilitySets::try_new(
            effective,
            permitted,
            [0; CAPABILITY_WORDS],
            CAPABILITY_VALID_MASK,
            [0; CAPABILITY_WORDS],
            securebits,
        )
        .unwrap()
    }

    #[test]
    fn group_info_is_sorted_preserves_duplicates_and_is_bounded() {
        let groups = GroupInfo::try_new(vec![kgid(3), kgid(1), kgid(3), kgid(2)]).unwrap();
        assert_eq!(groups.as_slice(), &[kgid(1), kgid(2), kgid(3), kgid(3)]);
        assert!(groups.contains(kgid(2)));

        let too_many = vec![kgid(1); NGROUPS_MAX as usize + 1];
        assert!(matches!(
            GroupInfo::try_new(too_many),
            Err(CredError::InvalidInput)
        ));
    }

    #[test]
    fn capability_invariants_reject_mixed_or_invalid_authority() {
        assert!(matches!(
            CapabilitySets::try_new(
                bit(CAP_CHOWN),
                [0; CAPABILITY_WORDS],
                [0; CAPABILITY_WORDS],
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                0,
            ),
            Err(CredError::NotPermitted)
        ));
        assert!(matches!(
            CapabilitySets::try_new(
                [0; CAPABILITY_WORDS],
                bit(CAP_CHOWN),
                [0; CAPABILITY_WORDS],
                CAPABILITY_VALID_MASK,
                bit(CAP_CHOWN),
                0,
            ),
            Err(CredError::NotPermitted)
        ));

        let mut invalid = [0; CAPABILITY_WORDS];
        invalid[CAPABILITY_WORDS - 1] = !CAPABILITY_VALID_MASK[CAPABILITY_WORDS - 1];
        assert!(matches!(
            CapabilitySets::try_new(
                [0; CAPABILITY_WORDS],
                invalid,
                [0; CAPABILITY_WORDS],
                CAPABILITY_VALID_MASK,
                [0; CAPABILITY_WORDS],
                0,
            ),
            Err(CredError::InvalidInput)
        ));
    }

    #[test]
    fn no_new_privs_and_locked_securebits_are_monotonic() {
        let namespace = MockNamespace::root();
        let root = Credential::try_root(namespace.clone()).unwrap();
        let locked = caps(
            CAPABILITY_VALID_MASK,
            CAPABILITY_VALID_MASK,
            SECBIT_NOROOT | SECBIT_NOROOT_LOCKED,
        );
        let committed = Credential::try_from_transition(
            &root,
            root.ids(),
            root.groups().clone(),
            locked,
            true,
            namespace.clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();

        assert!(matches!(
            Credential::try_from_transition(
                &committed,
                committed.ids(),
                committed.groups().clone(),
                caps(
                    CAPABILITY_VALID_MASK,
                    CAPABILITY_VALID_MASK,
                    SECBIT_NOROOT_LOCKED,
                ),
                true,
                namespace.clone(),
                CredentialTransitionMode::Normal,
            ),
            Err(CredError::NotPermitted)
        ));
        assert!(matches!(
            Credential::try_from_transition(
                &committed,
                committed.ids(),
                committed.groups().clone(),
                committed.capabilities(),
                false,
                namespace,
                CredentialTransitionMode::Normal,
            ),
            Err(CredError::NotPermitted)
        ));
    }

    #[test]
    fn exec_can_only_clear_locked_keep_caps_value() {
        let namespace = MockNamespace::root();
        let root = Credential::try_root(namespace.clone()).unwrap();
        let old = Credential::try_from_transition(
            &root,
            root.ids(),
            root.groups().clone(),
            caps(
                CAPABILITY_VALID_MASK,
                CAPABILITY_VALID_MASK,
                SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED,
            ),
            false,
            namespace.clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        let exec_caps = caps(
            CAPABILITY_VALID_MASK,
            CAPABILITY_VALID_MASK,
            SECBIT_KEEP_CAPS_LOCKED,
        );

        assert!(matches!(
            Credential::try_from_transition(
                &old,
                old.ids(),
                old.groups().clone(),
                exec_caps,
                false,
                namespace.clone(),
                CredentialTransitionMode::Normal,
            ),
            Err(CredError::NotPermitted)
        ));
        assert!(
            Credential::try_from_transition(
                &old,
                old.ids(),
                old.groups().clone(),
                exec_caps,
                false,
                namespace,
                CredentialTransitionMode::ExecClearsKeepCaps,
            )
            .is_ok()
        );
    }

    #[test]
    fn ordinary_transition_rejects_user_namespace_replacement() {
        let namespace = MockNamespace::root();
        let root = Credential::try_root(namespace.clone()).unwrap();
        let replacement_root = MockNamespace::root();
        let child = MockNamespace::child(&namespace, Kuid::INITIAL_ROOT);

        for replacement in [replacement_root, child] {
            assert!(matches!(
                Credential::try_from_transition(
                    &root,
                    root.ids(),
                    root.groups().clone(),
                    root.capabilities(),
                    root.no_new_privs(),
                    replacement,
                    CredentialTransitionMode::Normal,
                ),
                Err(CredError::NotPermitted)
            ));
        }
    }

    #[test]
    fn entering_user_namespace_keeps_ids_and_resets_capabilities() {
        let root_ns = MockNamespace::root();
        let root = Credential::try_root(root_ns.clone()).unwrap();
        let restricted = Credential::try_from_transition(
            &root,
            ids(1000, 100),
            GroupInfo::try_new(vec![kgid(100), kgid(200)]).unwrap(),
            CapabilitySets::empty(),
            true,
            root_ns.clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        let child_ns = MockNamespace::child(&root_ns, kuid(1000));
        let child = Credential::try_with_user_namespace(&restricted, child_ns.clone()).unwrap();

        assert_eq!(child.ids(), restricted.ids());
        assert_eq!(child.groups().as_slice(), restricted.groups().as_slice());
        assert_eq!(child.capabilities(), CapabilitySets::full());
        assert!(child.no_new_privs());
        assert!(Arc::ptr_eq(child.user_ns(), &child_ns));
    }

    #[test]
    fn entering_user_namespace_resets_parent_securebit_locks() {
        let root_ns = MockNamespace::root();
        let root = Credential::try_root(root_ns.clone()).unwrap();
        let locked = Credential::try_from_transition(
            &root,
            root.ids(),
            root.groups().clone(),
            caps(
                CAPABILITY_VALID_MASK,
                CAPABILITY_VALID_MASK,
                SECBIT_NOROOT | SECBIT_NOROOT_LOCKED,
            ),
            false,
            root_ns.clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        let child_ns = MockNamespace::child(&root_ns, Kuid::INITIAL_ROOT);
        let child = Credential::try_with_user_namespace(&locked, child_ns).unwrap();
        assert_eq!(child.capabilities(), CapabilitySets::full());
    }

    #[test]
    fn entering_user_namespace_rejects_invalid_relationships() {
        let root_ns = MockNamespace::root();
        let root = Credential::try_root(root_ns.clone()).unwrap();
        let owner = Credential::try_from_transition(
            &root,
            ids(1000, 100),
            root.groups().clone(),
            CapabilitySets::empty(),
            false,
            root_ns.clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        let current_ns = MockNamespace::child(&root_ns, kuid(1000));
        let current = Credential::try_with_user_namespace(&owner, current_ns.clone()).unwrap();

        let sibling = MockNamespace::child(&root_ns, kuid(1000));
        let wrong_owner = MockNamespace::child(&current_ns, kuid(2000));
        let skipped_level =
            MockNamespace::child_with_level(&current_ns, current_ns.level() + 2, kuid(1000));
        let nonmonotonic_level =
            MockNamespace::child_with_level(&current_ns, current_ns.level(), kuid(1000));

        for rejected in [
            root_ns,
            sibling,
            wrong_owner,
            skipped_level,
            nonmonotonic_level,
        ] {
            assert!(matches!(
                Credential::try_with_user_namespace(&current, rejected),
                Err(CredError::NotPermitted)
            ));
        }
    }

    #[test]
    fn failed_or_dropped_transition_has_zero_effect_on_old_value() {
        let namespace = MockNamespace::root();
        let old = Credential::try_root(namespace.clone()).unwrap();
        let before = (old.ids(), old.capabilities(), old.no_new_privs());

        let proposed = Credential::try_from_transition(
            &old,
            ids(1000, 100),
            old.groups().clone(),
            CapabilitySets::empty(),
            false,
            namespace,
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        drop(proposed);
        assert_eq!((old.ids(), old.capabilities(), old.no_new_privs()), before);
    }

    #[test]
    fn prepared_transition_rejects_equal_looking_distinct_old_arc() {
        let namespace = MockNamespace::root();
        let old = Credential::try_root(namespace.clone()).unwrap();
        let equal_but_distinct = Credential::try_from_transition(
            old.as_ref(),
            old.ids(),
            old.groups().clone(),
            old.capabilities(),
            old.no_new_privs(),
            namespace,
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        assert!(!Arc::ptr_eq(&old, &equal_but_distinct));

        let old_refcount = Arc::strong_count(&old);
        let prepared = Credential::try_prepare_transition(
            &old,
            ids(1000, 100),
            old.groups().clone(),
            CapabilitySets::empty(),
            true,
        )
        .unwrap();
        assert_eq!(prepared.old().ids(), old.ids());
        assert_eq!(prepared.proposed().ids(), ids(1000, 100));
        assert_eq!(Arc::strong_count(&old), old_refcount + 1);
        assert!(matches!(
            prepared.try_into_proposed(&equal_but_distinct),
            Err(CredError::NotPermitted)
        ));
        assert_eq!(Arc::strong_count(&old), old_refcount);

        let prepared = Credential::try_prepare_transition(
            &old,
            ids(1000, 100),
            old.groups().clone(),
            CapabilitySets::empty(),
            true,
        )
        .unwrap();
        let proposed = prepared.try_into_proposed(&old).unwrap();
        assert_eq!(proposed.ids(), ids(1000, 100));
        assert!(proposed.no_new_privs());
    }

    #[test]
    fn ordinary_transition_effects_follow_linux_process_security_reset_rules() {
        let namespace = MockNamespace::root();
        let root = Credential::try_root(namespace.clone()).unwrap();

        let assert_reset = |ids: CredentialIds, caps: CapabilitySets, expected: bool| {
            let prepared = Credential::try_prepare_transition(
                &root,
                ids,
                root.groups().clone(),
                caps,
                root.no_new_privs(),
            )
            .unwrap();
            let effects = prepared.effects();
            assert_eq!(effects.requires_dumpability_drop(), expected);
            assert_eq!(effects.clear_pdeath_signal(), expected);
        };

        assert_reset(root.ids(), root.capabilities(), false);

        let mut changed = root.ids();
        changed.ruid = kuid(1000);
        changed.suid = kuid(1000);
        changed.rgid = kgid(100);
        changed.sgid = kgid(100);
        assert_reset(changed, root.capabilities(), false);

        for change in 0..4 {
            let mut changed = root.ids();
            match change {
                0 => changed.euid = kuid(1000),
                1 => changed.egid = kgid(100),
                2 => changed.fsuid = kuid(1000),
                3 => changed.fsgid = kgid(100),
                _ => unreachable!(),
            }
            assert_reset(changed, root.capabilities(), true);
        }

        assert_reset(root.ids(), CapabilitySets::empty(), false);

        let restricted = Credential::try_from_transition(
            root.as_ref(),
            root.ids(),
            root.groups().clone(),
            CapabilitySets::empty(),
            false,
            namespace,
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        let gained = Credential::try_prepare_transition(
            &restricted,
            restricted.ids(),
            restricted.groups().clone(),
            caps(bit(CAP_CHOWN), [0; CAPABILITY_WORDS], 0),
            restricted.no_new_privs(),
        )
        .unwrap();
        assert!(gained.effects().requires_dumpability_drop());
        assert!(gained.effects().clear_pdeath_signal());
    }

    #[test]
    fn filesystem_snapshot_keeps_groups_and_requires_initial_namespace_authority() {
        let root_ns = MockNamespace::root();
        let root = Credential::try_root(root_ns.clone()).unwrap();
        let snapshot = root.fs_credential_snapshot();
        assert_eq!(snapshot.uid(), Kuid::INITIAL_ROOT);
        assert_eq!(snapshot.gid(), Kgid::INITIAL_ROOT);
        assert!(snapshot.has_capability(CAP_CHOWN));

        let child_ns = MockNamespace::child(&root_ns, Kuid::INITIAL_ROOT);
        let child = Credential::try_with_user_namespace(&root, child_ns).unwrap();
        let snapshot = child.fs_credential_snapshot();
        assert!(snapshot.selected_capability(CAP_CHOWN));
        assert!(!snapshot.has_capability(CAP_CHOWN));
    }

    #[test]
    fn ns_capable_follows_ancestry_not_siblings_or_ancestors() {
        let root_ns = MockNamespace::root();
        let child_ns = MockNamespace::child(&root_ns, Kuid::INITIAL_ROOT);
        let sibling_ns = MockNamespace::child(&root_ns, Kuid::INITIAL_ROOT);
        let grandchild_ns = MockNamespace::child(&child_ns, kuid(1000));
        let root = Credential::try_root(root_ns.clone()).unwrap();
        let child = Credential::try_with_user_namespace(&root, child_ns.clone()).unwrap();
        let sibling = Credential::try_with_user_namespace(&root, sibling_ns.clone()).unwrap();

        assert!(ns_capable(&root, &root_ns, CAP_KILL));
        assert!(ns_capable(&root, &child_ns, CAP_KILL));
        assert!(ns_capable(&root, &grandchild_ns, CAP_KILL));
        assert!(ns_capable(&child, &child_ns, CAP_KILL));
        assert!(ns_capable(&child, &grandchild_ns, CAP_KILL));
        assert!(!ns_capable(&child, &root_ns, CAP_KILL));
        assert!(!ns_capable(&child, &sibling_ns, CAP_KILL));
        assert!(!ns_capable(&sibling, &child_ns, CAP_KILL));
    }

    #[test]
    fn direct_child_owner_authority_does_not_require_capability_bit() {
        let root_ns = MockNamespace::root();
        let root = Credential::try_root(root_ns.clone()).unwrap();
        let owner = Credential::try_from_transition(
            &root,
            ids(1000, 100),
            root.groups().clone(),
            CapabilitySets::empty(),
            false,
            root_ns.clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        let owned_child = MockNamespace::child(&root_ns, kuid(1000));
        let owned_grandchild = MockNamespace::child(&owned_child, kuid(3000));
        let sibling = MockNamespace::child(&root_ns, kuid(2000));

        assert!(!ns_capable(&owner, &root_ns, CAP_SYS_ADMIN));
        assert!(ns_capable(&owner, &owned_child, CAP_SYS_ADMIN));
        assert!(ns_capable(&owner, &owned_grandchild, CAP_SYS_ADMIN));
        assert!(!ns_capable(&owner, &sibling, CAP_SYS_ADMIN));
    }

    #[test]
    fn ns_capable_rejects_invalid_capability_before_owner_shortcuts() {
        let root_ns = MockNamespace::root();
        let root = Credential::try_root(root_ns.clone()).unwrap();
        let owner = Credential::try_from_transition(
            &root,
            ids(1000, 100),
            root.groups().clone(),
            CapabilitySets::empty(),
            false,
            root_ns.clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        let owned_child = MockNamespace::child(&root_ns, kuid(1000));
        let owned_grandchild = MockNamespace::child(&owned_child, kuid(3000));

        assert!(!ns_capable(&root, &root_ns, u32::MAX));
        assert!(!ns_capable(&owner, &owned_child, u32::MAX));
        assert!(!ns_capable(&owner, &owned_grandchild, u32::MAX));
        assert!(ns_capable(&owner, &owned_child, CAP_SYS_ADMIN));
    }

    #[test]
    fn uid_zero_without_effective_capability_is_not_privileged() {
        let namespace = MockNamespace::root();
        let root = Credential::try_root(namespace.clone()).unwrap();
        let dropped = Credential::try_from_transition(
            &root,
            root.ids(),
            root.groups().clone(),
            CapabilitySets::empty(),
            false,
            namespace.clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();

        assert!(dropped.is_initial_root_euid());
        assert!(!dropped.has_effective_capability(CAP_SYS_ADMIN));
        assert!(!ns_capable(&dropped, &namespace, CAP_SYS_ADMIN));
    }

    #[test]
    fn capability_subset_uses_namespace_owner_direction() {
        let root_ns = MockNamespace::root();
        let root = Credential::try_root(root_ns.clone()).unwrap();
        let owner = Credential::try_from_transition(
            &root,
            ids(1000, 100),
            root.groups().clone(),
            caps(bit(CAP_CHOWN), bit(CAP_CHOWN), 0),
            false,
            root_ns.clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        let child_ns = MockNamespace::child(&root_ns, kuid(1000));
        let child = Credential::try_with_user_namespace(&owner, child_ns).unwrap();
        assert!(credential_cap_is_subset(&owner, &child));
        assert!(!credential_cap_is_subset(&child, &owner));
    }
}
