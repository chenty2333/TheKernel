//! Pure Linux credential-mutation planners.
//!
//! The planners in this module consume one explicit immutable old credential,
//! normalized kernel-global IDs, and a typed result of consumer-owned
//! capability admission. They return field-private next-state values without
//! owning a task, credential slot, security-hook registry, usercopy path,
//! publication lock, or errno mapping.

use alloc::sync::Arc;

use crate::{
    CAPABILITY_VALID_MASK, CAPABILITY_WORDS, CapabilitySets, CredError, Credential, CredentialIds,
    Kgid, Kuid, PreparedCredential, SECBIT_KEEP_CAPS, SECBIT_NO_SETUID_FIXUP, UserNamespaceView,
};
use linux_raw_sys::general::{
    CAP_CHOWN, CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FOWNER, CAP_FSETID, CAP_LINUX_IMMUTABLE,
    CAP_MAC_OVERRIDE, CAP_MKNOD,
};

/// The consumer's `CAP_SETUID` decision for one exact old credential.
///
/// Select [`Self::CAP_SETUID`] only after the embedding kernel has completed
/// its typed set-ID capability hook against the same credential passed to the
/// planner. Keeping this result explicit prevents the policy leaf from doing
/// a hidden current-task lookup or bypassing stacked security modules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserIdAuthority {
    privileged: bool,
}

impl UserIdAuthority {
    /// No successful `CAP_SETUID` decision; only unprivileged transitions are
    /// available.
    pub const UNPRIVILEGED: Self = Self { privileged: false };

    /// The exact old credential passed its consumer-owned `CAP_SETUID` hook.
    pub const CAP_SETUID: Self = Self { privileged: true };

    const fn is_privileged(self) -> bool {
        self.privileged
    }
}

/// The consumer's `CAP_SETGID` decision for one exact old credential.
///
/// Select [`Self::CAP_SETGID`] only after the embedding kernel has completed
/// its typed set-ID capability hook against the same credential passed to the
/// planner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupIdAuthority {
    privileged: bool,
}

impl GroupIdAuthority {
    /// No successful `CAP_SETGID` decision; only unprivileged transitions are
    /// available.
    pub const UNPRIVILEGED: Self = Self { privileged: false };

    /// The exact old credential passed its consumer-owned `CAP_SETGID` hook.
    pub const CAP_SETGID: Self = Self { privileged: true };

    const fn is_privileged(self) -> bool {
        self.privileged
    }
}

/// The commoncap `CAP_SETPCAP` result used for one `capset` transition.
///
/// This token controls only whether new inheritable authority is additionally
/// limited by the old permitted set. The old bounding-set restriction always
/// applies, including when `CAP_SETPCAP` was granted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapsetAuthority {
    setpcap: bool,
}

impl CapsetAuthority {
    /// The actor lacks `CAP_SETPCAP`; raised inheritable bits must be admitted
    /// by both the old permitted and bounding constraints.
    pub const RESTRICTED: Self = Self { setpcap: false };

    /// The actor has `CAP_SETPCAP`; the old permitted-set constraint is
    /// relaxed while the old bounding-set constraint remains mandatory.
    pub const CAP_SETPCAP: Self = Self { setpcap: true };

    const fn has_setpcap(self) -> bool {
        self.setpcap
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UserIdTransitionKind {
    Uid(Kuid),
    ReUid {
        ruid: Option<Kuid>,
        euid: Option<Kuid>,
    },
    ResUid {
        ruid: Option<Kuid>,
        euid: Option<Kuid>,
        suid: Option<Kuid>,
    },
    FsUid(Kuid),
}

/// Normalized kernel-global input to one Linux user-ID transition.
///
/// Syscall sentinels and namespace-visible IDs must be decoded before this
/// value is constructed. Private representation prevents callers from
/// inventing an unclassified set-ID operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserIdTransitionInput {
    kind: UserIdTransitionKind,
}

impl UserIdTransitionInput {
    /// Constructs `setuid` input.
    pub const fn setuid(uid: Kuid) -> Self {
        Self {
            kind: UserIdTransitionKind::Uid(uid),
        }
    }

    /// Constructs `setreuid` input. `None` preserves the corresponding ID.
    pub const fn setreuid(ruid: Option<Kuid>, euid: Option<Kuid>) -> Self {
        Self {
            kind: UserIdTransitionKind::ReUid { ruid, euid },
        }
    }

    /// Constructs `setresuid` input. `None` preserves the corresponding ID.
    pub const fn setresuid(ruid: Option<Kuid>, euid: Option<Kuid>, suid: Option<Kuid>) -> Self {
        Self {
            kind: UserIdTransitionKind::ResUid { ruid, euid, suid },
        }
    }

    /// Constructs `setfsuid` input.
    pub const fn setfsuid(fsuid: Kuid) -> Self {
        Self {
            kind: UserIdTransitionKind::FsUid(fsuid),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupIdTransitionKind {
    Gid(Kgid),
    ReGid {
        rgid: Option<Kgid>,
        egid: Option<Kgid>,
    },
    ResGid {
        rgid: Option<Kgid>,
        egid: Option<Kgid>,
        sgid: Option<Kgid>,
    },
    FsGid(Kgid),
}

/// Normalized kernel-global input to one Linux group-ID transition.
///
/// Syscall sentinels and namespace-visible IDs must be decoded before this
/// value is constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupIdTransitionInput {
    kind: GroupIdTransitionKind,
}

impl GroupIdTransitionInput {
    /// Constructs `setgid` input.
    pub const fn setgid(gid: Kgid) -> Self {
        Self {
            kind: GroupIdTransitionKind::Gid(gid),
        }
    }

    /// Constructs `setregid` input. `None` preserves the corresponding ID.
    pub const fn setregid(rgid: Option<Kgid>, egid: Option<Kgid>) -> Self {
        Self {
            kind: GroupIdTransitionKind::ReGid { rgid, egid },
        }
    }

    /// Constructs `setresgid` input. `None` preserves the corresponding ID.
    pub const fn setresgid(rgid: Option<Kgid>, egid: Option<Kgid>, sgid: Option<Kgid>) -> Self {
        Self {
            kind: GroupIdTransitionKind::ResGid { rgid, egid, sgid },
        }
    }

    /// Constructs `setfsgid` input.
    pub const fn setfsgid(fsgid: Kgid) -> Self {
        Self {
            kind: GroupIdTransitionKind::FsGid(fsgid),
        }
    }
}

/// Normalized effective, permitted, and inheritable `capset` request.
///
/// Linux ABI importers may mask unsupported words for legacy compatibility,
/// but every word passed here must already be within
/// [`CAPABILITY_VALID_MASK`]. Admission against the old credential is deferred
/// to [`plan_capset`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapsetRequest {
    effective: [u32; CAPABILITY_WORDS],
    permitted: [u32; CAPABILITY_WORDS],
    inheritable: [u32; CAPABILITY_WORDS],
}

impl CapsetRequest {
    /// Constructs a mask-valid request without yet applying transition policy.
    pub fn try_new(
        effective: [u32; CAPABILITY_WORDS],
        permitted: [u32; CAPABILITY_WORDS],
        inheritable: [u32; CAPABILITY_WORDS],
    ) -> Result<Self, CredError> {
        for word in 0..CAPABILITY_WORDS {
            let combined = effective[word] | permitted[word] | inheritable[word];
            if combined & !CAPABILITY_VALID_MASK[word] != 0 {
                return Err(CredError::InvalidInput);
            }
        }
        Ok(Self {
            effective,
            permitted,
            inheritable,
        })
    }

    /// Returns the requested effective set.
    pub const fn effective(self) -> [u32; CAPABILITY_WORDS] {
        self.effective
    }

    /// Returns the requested permitted set.
    pub const fn permitted(self) -> [u32; CAPABILITY_WORDS] {
        self.permitted
    }

    /// Returns the requested inheritable set.
    pub const fn inheritable(self) -> [u32; CAPABILITY_WORDS] {
        self.inheritable
    }
}

/// Planned user-ID transition bound to one borrowed old credential.
#[must_use = "a user-ID plan has no effect until the consumer prepares and publishes it"]
pub struct UserIdTransitionPlan<'a, N: UserNamespaceView> {
    old: &'a Credential<N>,
    input: UserIdTransitionInput,
    authority: UserIdAuthority,
    ids: CredentialIds,
    capabilities: CapabilitySets,
}

impl<'a, N: UserNamespaceView> UserIdTransitionPlan<'a, N> {
    /// Borrows the exact old credential used by policy.
    pub const fn old(&self) -> &'a Credential<N> {
        self.old
    }

    /// Returns the normalized input retained by this plan.
    pub const fn input(&self) -> UserIdTransitionInput {
        self.input
    }

    /// Returns the explicit authority retained by this plan.
    pub const fn authority(&self) -> UserIdAuthority {
        self.authority
    }

    /// Returns the complete planned ID state.
    pub const fn ids(&self) -> CredentialIds {
        self.ids
    }

    /// Returns the complete planned capability and securebits state.
    pub const fn capabilities(&self) -> CapabilitySets {
        self.capabilities
    }

    /// Returns the filesystem user ID visible before this operation.
    pub const fn previous_fsuid(&self) -> Kuid {
        self.old.ids().fsuid
    }

    /// Reports whether publication would change IDs or capabilities.
    pub fn changes_credential(&self) -> bool {
        self.ids != self.old.ids() || self.capabilities != self.old.capabilities()
    }

    /// Allocates the existing exact-old-bound ordinary credential proposal.
    ///
    /// `expected_old` must be the [`Arc`] whose borrowed value was passed to
    /// [`plan_user_id_transition`]. Groups, `no_new_privs`, and the user
    /// namespace are preserved from that exact old credential.
    pub fn try_prepare_credential(
        self,
        expected_old: &Arc<Credential<N>>,
    ) -> Result<PreparedCredential<N>, CredError> {
        try_prepare_planned_credential(self.old, expected_old, self.ids, self.capabilities)
    }
}

/// Planned group-ID transition bound to one borrowed old credential.
#[must_use = "a group-ID plan has no effect until the consumer prepares and publishes it"]
pub struct GroupIdTransitionPlan<'a, N: UserNamespaceView> {
    old: &'a Credential<N>,
    input: GroupIdTransitionInput,
    authority: GroupIdAuthority,
    ids: CredentialIds,
}

impl<'a, N: UserNamespaceView> GroupIdTransitionPlan<'a, N> {
    /// Borrows the exact old credential used by policy.
    pub const fn old(&self) -> &'a Credential<N> {
        self.old
    }

    /// Returns the normalized input retained by this plan.
    pub const fn input(&self) -> GroupIdTransitionInput {
        self.input
    }

    /// Returns the explicit authority retained by this plan.
    pub const fn authority(&self) -> GroupIdAuthority {
        self.authority
    }

    /// Returns the complete planned ID state.
    pub const fn ids(&self) -> CredentialIds {
        self.ids
    }

    /// Returns the unchanged complete capability and securebits state.
    pub const fn capabilities(&self) -> CapabilitySets {
        self.old.capabilities()
    }

    /// Returns the filesystem group ID visible before this operation.
    pub const fn previous_fsgid(&self) -> Kgid {
        self.old.ids().fsgid
    }

    /// Reports whether publication would change any ID.
    pub fn changes_credential(&self) -> bool {
        self.ids != self.old.ids()
    }

    /// Allocates the existing exact-old-bound ordinary credential proposal.
    ///
    /// `expected_old` must be the [`Arc`] whose borrowed value was passed to
    /// [`plan_group_id_transition`]. Capabilities, groups, `no_new_privs`, and
    /// the user namespace are preserved from that exact old credential.
    pub fn try_prepare_credential(
        self,
        expected_old: &Arc<Credential<N>>,
    ) -> Result<PreparedCredential<N>, CredError> {
        try_prepare_planned_credential(
            self.old,
            expected_old,
            self.ids,
            expected_old.capabilities(),
        )
    }
}

/// Planned `capset` transition bound to one borrowed old credential.
#[must_use = "a capset plan has no effect until the consumer prepares and publishes it"]
pub struct CapsetPlan<'a, N: UserNamespaceView> {
    old: &'a Credential<N>,
    request: CapsetRequest,
    authority: CapsetAuthority,
    capabilities: CapabilitySets,
}

impl<'a, N: UserNamespaceView> CapsetPlan<'a, N> {
    /// Borrows the exact old credential used by policy.
    pub const fn old(&self) -> &'a Credential<N> {
        self.old
    }

    /// Returns the normalized request retained by this plan.
    pub const fn request(&self) -> CapsetRequest {
        self.request
    }

    /// Returns the explicit commoncap authority retained by this plan.
    pub const fn authority(&self) -> CapsetAuthority {
        self.authority
    }

    /// Returns the unchanged complete ID state.
    pub const fn ids(&self) -> CredentialIds {
        self.old.ids()
    }

    /// Returns the complete planned capability and securebits state.
    pub const fn capabilities(&self) -> CapabilitySets {
        self.capabilities
    }

    /// Reports whether publication would change capability state.
    pub fn changes_credential(&self) -> bool {
        self.capabilities != self.old.capabilities()
    }

    /// Allocates the existing exact-old-bound ordinary credential proposal.
    ///
    /// `expected_old` must be the [`Arc`] whose borrowed value was passed to
    /// [`plan_capset`]. IDs, groups, `no_new_privs`, and the user namespace are
    /// preserved from that exact old credential.
    pub fn try_prepare_credential(
        self,
        expected_old: &Arc<Credential<N>>,
    ) -> Result<PreparedCredential<N>, CredError> {
        try_prepare_planned_credential(
            self.old,
            expected_old,
            expected_old.ids(),
            self.capabilities,
        )
    }
}

fn try_prepare_planned_credential<N: UserNamespaceView>(
    planned_old: &Credential<N>,
    expected_old: &Arc<Credential<N>>,
    ids: CredentialIds,
    capabilities: CapabilitySets,
) -> Result<PreparedCredential<N>, CredError> {
    if !core::ptr::eq(planned_old, expected_old.as_ref()) {
        return Err(CredError::NotPermitted);
    }
    Credential::try_prepare_transition(
        expected_old,
        ids,
        expected_old.groups().clone(),
        capabilities,
        expected_old.no_new_privs(),
    )
}

/// Plans one Linux `setuid`, `setreuid`, `setresuid`, or `setfsuid`
/// transition.
///
/// Ordinary unauthorized operations return [`CredError::NotPermitted`]. An
/// unauthorized or unchanged `setfsuid` request instead returns an unchanged
/// plan, preserving Linux's old-FSUID return convention. UID and FSUID
/// capability fixups use the mapped root of the old credential's user
/// namespace and honor `NO_SETUID_FIXUP` and `KEEP_CAPS`.
pub fn plan_user_id_transition<'a, N: UserNamespaceView>(
    old: &'a Credential<N>,
    input: UserIdTransitionInput,
    authority: UserIdAuthority,
) -> Result<UserIdTransitionPlan<'a, N>, CredError> {
    let old_ids = old.ids();
    let mut ids = old_ids;
    let mut capabilities = old.capabilities();

    match input.kind {
        UserIdTransitionKind::Uid(uid) => {
            if authority.is_privileged() {
                ids.ruid = uid;
                ids.euid = uid;
                ids.suid = uid;
            } else if uid != old_ids.ruid && uid != old_ids.suid {
                return Err(CredError::NotPermitted);
            } else {
                ids.euid = uid;
            }
            ids.fsuid = uid;
            capabilities =
                fixup_uid_capabilities(old.user_ns().root_kuid(), old_ids, ids, capabilities)?;
        }
        UserIdTransitionKind::ReUid { ruid, euid } => {
            if !authority.is_privileged()
                && (ruid.is_some_and(|id| id != old_ids.ruid && id != old_ids.euid)
                    || euid.is_some_and(|id| {
                        id != old_ids.ruid && id != old_ids.euid && id != old_ids.suid
                    }))
            {
                return Err(CredError::NotPermitted);
            }

            ids.ruid = ruid.unwrap_or(old_ids.ruid);
            ids.euid = euid.unwrap_or(old_ids.euid);
            ids.fsuid = ids.euid;
            if ruid.is_some() || euid.is_some_and(|id| id != old_ids.ruid) {
                ids.suid = ids.euid;
            }
            capabilities =
                fixup_uid_capabilities(old.user_ns().root_kuid(), old_ids, ids, capabilities)?;
        }
        UserIdTransitionKind::ResUid { ruid, euid, suid } => {
            if setresuid_is_noop(old_ids, ruid, euid, suid) {
                return Ok(UserIdTransitionPlan {
                    old,
                    input,
                    authority,
                    ids,
                    capabilities,
                });
            }
            if !authority.is_privileged()
                && [ruid, euid, suid]
                    .into_iter()
                    .flatten()
                    .any(|id| id != old_ids.ruid && id != old_ids.euid && id != old_ids.suid)
            {
                return Err(CredError::NotPermitted);
            }

            ids.ruid = ruid.unwrap_or(old_ids.ruid);
            ids.euid = euid.unwrap_or(old_ids.euid);
            ids.suid = suid.unwrap_or(old_ids.suid);
            ids.fsuid = ids.euid;
            capabilities =
                fixup_uid_capabilities(old.user_ns().root_kuid(), old_ids, ids, capabilities)?;
        }
        UserIdTransitionKind::FsUid(fsuid) => {
            let admitted = authority.is_privileged()
                || fsuid == old_ids.ruid
                || fsuid == old_ids.euid
                || fsuid == old_ids.suid
                || fsuid == old_ids.fsuid;
            if admitted {
                ids.fsuid = fsuid;
                capabilities = fixup_fsuid_capabilities(
                    old.user_ns().root_kuid(),
                    old_ids.fsuid,
                    ids.fsuid,
                    capabilities,
                )?;
            }
        }
    }

    Ok(UserIdTransitionPlan {
        old,
        input,
        authority,
        ids,
        capabilities,
    })
}

/// Plans one Linux `setgid`, `setregid`, `setresgid`, or `setfsgid`
/// transition.
///
/// Ordinary unauthorized operations return [`CredError::NotPermitted`]. An
/// unauthorized or unchanged `setfsgid` request instead returns an unchanged
/// plan, preserving Linux's old-FSGID return convention. Group transitions do
/// not alter capability sets.
pub fn plan_group_id_transition<'a, N: UserNamespaceView>(
    old: &'a Credential<N>,
    input: GroupIdTransitionInput,
    authority: GroupIdAuthority,
) -> Result<GroupIdTransitionPlan<'a, N>, CredError> {
    let old_ids = old.ids();
    let mut ids = old_ids;

    match input.kind {
        GroupIdTransitionKind::Gid(gid) => {
            if authority.is_privileged() {
                ids.rgid = gid;
                ids.egid = gid;
                ids.sgid = gid;
            } else if gid != old_ids.rgid && gid != old_ids.sgid {
                return Err(CredError::NotPermitted);
            } else {
                ids.egid = gid;
            }
            ids.fsgid = gid;
        }
        GroupIdTransitionKind::ReGid { rgid, egid } => {
            if !authority.is_privileged()
                && (rgid.is_some_and(|id| id != old_ids.rgid && id != old_ids.egid)
                    || egid.is_some_and(|id| {
                        id != old_ids.rgid && id != old_ids.egid && id != old_ids.sgid
                    }))
            {
                return Err(CredError::NotPermitted);
            }

            ids.rgid = rgid.unwrap_or(old_ids.rgid);
            ids.egid = egid.unwrap_or(old_ids.egid);
            ids.fsgid = ids.egid;
            if rgid.is_some() || egid.is_some_and(|id| id != old_ids.rgid) {
                ids.sgid = ids.egid;
            }
        }
        GroupIdTransitionKind::ResGid { rgid, egid, sgid } => {
            if setresgid_is_noop(old_ids, rgid, egid, sgid) {
                return Ok(GroupIdTransitionPlan {
                    old,
                    input,
                    authority,
                    ids,
                });
            }
            if !authority.is_privileged()
                && [rgid, egid, sgid]
                    .into_iter()
                    .flatten()
                    .any(|id| id != old_ids.rgid && id != old_ids.egid && id != old_ids.sgid)
            {
                return Err(CredError::NotPermitted);
            }

            ids.rgid = rgid.unwrap_or(old_ids.rgid);
            ids.egid = egid.unwrap_or(old_ids.egid);
            ids.sgid = sgid.unwrap_or(old_ids.sgid);
            ids.fsgid = ids.egid;
        }
        GroupIdTransitionKind::FsGid(fsgid) => {
            let admitted = authority.is_privileged()
                || fsgid == old_ids.rgid
                || fsgid == old_ids.egid
                || fsgid == old_ids.sgid
                || fsgid == old_ids.fsgid;
            if admitted {
                ids.fsgid = fsgid;
            }
        }
    }

    Ok(GroupIdTransitionPlan {
        old,
        input,
        authority,
        ids,
    })
}

fn setresuid_is_noop(
    old: CredentialIds,
    ruid: Option<Kuid>,
    euid: Option<Kuid>,
    suid: Option<Kuid>,
) -> bool {
    ruid.is_none_or(|id| id == old.ruid)
        && euid.is_none_or(|id| id == old.euid && id == old.fsuid)
        && suid.is_none_or(|id| id == old.suid)
}

fn setresgid_is_noop(
    old: CredentialIds,
    rgid: Option<Kgid>,
    egid: Option<Kgid>,
    sgid: Option<Kgid>,
) -> bool {
    rgid.is_none_or(|id| id == old.rgid)
        && egid.is_none_or(|id| id == old.egid && id == old.fsgid)
        && sgid.is_none_or(|id| id == old.sgid)
}

/// Validates and plans one Linux commoncap `capset` transition.
///
/// Effective authority must remain within the requested permitted set, and
/// permitted authority cannot grow beyond the old permitted set. New
/// inheritable bits are always bounded by `old inheritable | old bounding`;
/// without `CAP_SETPCAP` they are additionally bounded by `old inheritable |
/// old permitted`. Bounding and securebits are preserved, while ambient bits
/// are reconciled with the new permitted and inheritable sets.
pub fn plan_capset<'a, N: UserNamespaceView>(
    old: &'a Credential<N>,
    request: CapsetRequest,
    authority: CapsetAuthority,
) -> Result<CapsetPlan<'a, N>, CredError> {
    let old_capabilities = old.capabilities();
    if !capability_subset(request.effective, request.permitted)
        || !capability_subset(request.permitted, old_capabilities.permitted())
    {
        return Err(CredError::NotPermitted);
    }

    let inheritable_with_bounding =
        capability_union(old_capabilities.inheritable(), old_capabilities.bounding());
    if !capability_subset(request.inheritable, inheritable_with_bounding) {
        return Err(CredError::NotPermitted);
    }

    if !authority.has_setpcap() {
        let inheritable_without_setpcap =
            capability_union(old_capabilities.inheritable(), old_capabilities.permitted());
        if !capability_subset(request.inheritable, inheritable_without_setpcap) {
            return Err(CredError::NotPermitted);
        }
    }

    let mut ambient = old_capabilities.ambient();
    for ((ambient, permitted), inheritable) in ambient
        .iter_mut()
        .zip(request.permitted)
        .zip(request.inheritable)
    {
        *ambient &= permitted & inheritable;
    }
    let capabilities = CapabilitySets::try_new(
        request.effective,
        request.permitted,
        request.inheritable,
        old_capabilities.bounding(),
        ambient,
        old_capabilities.securebits(),
    )?;

    Ok(CapsetPlan {
        old,
        request,
        authority,
        capabilities,
    })
}

fn capability_subset(subset: [u32; CAPABILITY_WORDS], set: [u32; CAPABILITY_WORDS]) -> bool {
    subset
        .iter()
        .zip(set.iter())
        .all(|(subset, set)| subset & !set == 0)
}

fn capability_union(
    lhs: [u32; CAPABILITY_WORDS],
    rhs: [u32; CAPABILITY_WORDS],
) -> [u32; CAPABILITY_WORDS] {
    let mut union = [0; CAPABILITY_WORDS];
    for word in 0..CAPABILITY_WORDS {
        union[word] = lhs[word] | rhs[word];
    }
    union
}

fn fixup_uid_capabilities(
    root_kuid: Option<Kuid>,
    old_ids: CredentialIds,
    new_ids: CredentialIds,
    capabilities: CapabilitySets,
) -> Result<CapabilitySets, CredError> {
    if (old_ids.ruid == new_ids.ruid
        && old_ids.euid == new_ids.euid
        && old_ids.suid == new_ids.suid)
        || capabilities.securebits() & SECBIT_NO_SETUID_FIXUP != 0
    {
        return Ok(capabilities);
    }

    let mut effective = capabilities.effective();
    let mut permitted = capabilities.permitted();
    let mut ambient = capabilities.ambient();
    let old_had_root = [old_ids.ruid, old_ids.euid, old_ids.suid]
        .into_iter()
        .any(|id| root_kuid == Some(id));
    let new_has_root = [new_ids.ruid, new_ids.euid, new_ids.suid]
        .into_iter()
        .any(|id| root_kuid == Some(id));

    if old_had_root && !new_has_root {
        if capabilities.securebits() & SECBIT_KEEP_CAPS == 0 {
            permitted = [0; CAPABILITY_WORDS];
            effective = [0; CAPABILITY_WORDS];
        }
        ambient = [0; CAPABILITY_WORDS];
    }
    if root_kuid == Some(old_ids.euid) && root_kuid != Some(new_ids.euid) {
        effective = [0; CAPABILITY_WORDS];
    }
    if root_kuid != Some(old_ids.euid) && root_kuid == Some(new_ids.euid) {
        effective = permitted;
    }

    CapabilitySets::try_new(
        effective,
        permitted,
        capabilities.inheritable(),
        capabilities.bounding(),
        ambient,
        capabilities.securebits(),
    )
}

fn fixup_fsuid_capabilities(
    root_kuid: Option<Kuid>,
    old_fsuid: Kuid,
    new_fsuid: Kuid,
    capabilities: CapabilitySets,
) -> Result<CapabilitySets, CredError> {
    if old_fsuid == new_fsuid || capabilities.securebits() & SECBIT_NO_SETUID_FIXUP != 0 {
        return Ok(capabilities);
    }

    const FILESYSTEM_CAPABILITIES: [u32; 8] = [
        CAP_CHOWN,
        CAP_MKNOD,
        CAP_DAC_OVERRIDE,
        CAP_DAC_READ_SEARCH,
        CAP_FOWNER,
        CAP_FSETID,
        CAP_MAC_OVERRIDE,
        CAP_LINUX_IMMUTABLE,
    ];

    let mut effective = capabilities.effective();
    for capability in FILESYSTEM_CAPABILITIES {
        let Some((word, mask)) = CapabilitySets::cap_mask(capability) else {
            continue;
        };
        if root_kuid == Some(old_fsuid) && root_kuid != Some(new_fsuid) {
            effective[word] &= !mask;
        } else if root_kuid != Some(old_fsuid)
            && root_kuid == Some(new_fsuid)
            && capabilities.permitted()[word] & mask != 0
        {
            effective[word] |= mask;
        }
    }

    CapabilitySets::try_new(
        effective,
        capabilities.permitted(),
        capabilities.inheritable(),
        capabilities.bounding(),
        capabilities.ambient(),
        capabilities.securebits(),
    )
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec::Vec};

    use linux_raw_sys::general::{CAP_KILL, CAP_SETPCAP};

    use super::*;
    use crate::{
        GroupInfo, SECBIT_EXEC_DENY_INTERACTIVE, SECBIT_EXEC_DENY_INTERACTIVE_LOCKED,
        SECBIT_EXEC_RESTRICT_FILE, SECBIT_EXEC_RESTRICT_FILE_LOCKED, SECBIT_KEEP_CAPS_LOCKED,
        SECBIT_NOROOT, SECURE_ALL_UNPRIVILEGED,
    };

    struct TestNamespace {
        root: Option<Kuid>,
    }

    impl UserNamespaceView for TestNamespace {
        fn parent(self: &Arc<Self>) -> Option<Arc<Self>> {
            None
        }

        fn level(&self) -> u32 {
            0
        }

        fn owner_kuid(&self) -> Kuid {
            Kuid::INITIAL_ROOT
        }

        fn root_kuid(&self) -> Option<Kuid> {
            self.root
        }

        fn is_initial(&self) -> bool {
            true
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
        lhs: [u32; CAPABILITY_WORDS],
        rhs: [u32; CAPABILITY_WORDS],
    ) -> [u32; CAPABILITY_WORDS] {
        capability_union(lhs, rhs)
    }

    fn ids() -> CredentialIds {
        CredentialIds {
            ruid: kuid(10),
            euid: kuid(20),
            suid: kuid(30),
            fsuid: kuid(40),
            rgid: kgid(110),
            egid: kgid(120),
            sgid: kgid(130),
            fsgid: kgid(140),
        }
    }

    fn credential_with(
        ids: CredentialIds,
        capabilities: CapabilitySets,
        root: Option<Kuid>,
    ) -> Arc<Credential<TestNamespace>> {
        let namespace = Arc::new(TestNamespace { root });
        let root_credential = Credential::try_root(namespace).unwrap();
        let groups = GroupInfo::try_new(Vec::new()).unwrap();
        Credential::try_prepare_transition(&root_credential, ids, groups, capabilities, false)
            .unwrap()
            .try_into_proposed(&root_credential)
            .unwrap()
    }

    fn unprivileged_credential() -> Arc<Credential<TestNamespace>> {
        credential_with(ids(), CapabilitySets::empty(), Some(Kuid::INITIAL_ROOT))
    }

    #[test]
    fn setuid_unprivileged_and_capability_matrices_are_complete() {
        let old = unprivileged_credential();
        for requested in [kuid(10), kuid(20), kuid(30), kuid(50)] {
            let unprivileged = plan_user_id_transition(
                &old,
                UserIdTransitionInput::setuid(requested),
                UserIdAuthority::UNPRIVILEGED,
            );
            let allowed = requested == old.ids().ruid || requested == old.ids().suid;
            assert_eq!(unprivileged.is_ok(), allowed, "requested={requested:?}");
            if let Ok(plan) = unprivileged {
                assert_eq!(plan.ids().ruid, old.ids().ruid);
                assert_eq!(plan.ids().euid, requested);
                assert_eq!(plan.ids().suid, old.ids().suid);
                assert_eq!(plan.ids().fsuid, requested);
            }

            let privileged = plan_user_id_transition(
                &old,
                UserIdTransitionInput::setuid(requested),
                UserIdAuthority::CAP_SETUID,
            )
            .unwrap();
            assert_eq!(privileged.ids().ruid, requested);
            assert_eq!(privileged.ids().euid, requested);
            assert_eq!(privileged.ids().suid, requested);
            assert_eq!(privileged.ids().fsuid, requested);
        }
    }

    #[test]
    fn setreuid_unprivileged_matrix_and_saved_id_rule_are_complete() {
        let old = unprivileged_credential();
        let choices = [
            None,
            Some(kuid(10)),
            Some(kuid(20)),
            Some(kuid(30)),
            Some(kuid(50)),
        ];
        for ruid in choices {
            for euid in choices {
                let result = plan_user_id_transition(
                    &old,
                    UserIdTransitionInput::setreuid(ruid, euid),
                    UserIdAuthority::UNPRIVILEGED,
                );
                let real_allowed =
                    ruid.is_none_or(|id| id == old.ids().ruid || id == old.ids().euid);
                let effective_allowed = euid.is_none_or(|id| {
                    id == old.ids().ruid || id == old.ids().euid || id == old.ids().suid
                });
                assert_eq!(result.is_ok(), real_allowed && effective_allowed);
                if let Ok(plan) = result {
                    let expected_ruid = ruid.unwrap_or(old.ids().ruid);
                    let expected_euid = euid.unwrap_or(old.ids().euid);
                    let expected_suid =
                        if ruid.is_some() || euid.is_some_and(|id| id != old.ids().ruid) {
                            expected_euid
                        } else {
                            old.ids().suid
                        };
                    assert_eq!(plan.ids().ruid, expected_ruid);
                    assert_eq!(plan.ids().euid, expected_euid);
                    assert_eq!(plan.ids().suid, expected_suid);
                    assert_eq!(plan.ids().fsuid, expected_euid);
                }

                assert!(
                    plan_user_id_transition(
                        &old,
                        UserIdTransitionInput::setreuid(ruid, euid),
                        UserIdAuthority::CAP_SETUID,
                    )
                    .is_ok()
                );
            }
        }
    }

    #[test]
    fn setresuid_unprivileged_matrix_is_complete() {
        let old = unprivileged_credential();
        let choices = [
            None,
            Some(kuid(10)),
            Some(kuid(20)),
            Some(kuid(30)),
            Some(kuid(50)),
        ];
        for ruid in choices {
            for euid in choices {
                for suid in choices {
                    let result = plan_user_id_transition(
                        &old,
                        UserIdTransitionInput::setresuid(ruid, euid, suid),
                        UserIdAuthority::UNPRIVILEGED,
                    );
                    let allowed = [ruid, euid, suid].into_iter().flatten().all(|id| {
                        id == old.ids().ruid || id == old.ids().euid || id == old.ids().suid
                    });
                    assert_eq!(result.is_ok(), allowed);
                    if let Ok(plan) = result {
                        assert_eq!(plan.ids().ruid, ruid.unwrap_or(old.ids().ruid));
                        assert_eq!(plan.ids().euid, euid.unwrap_or(old.ids().euid));
                        assert_eq!(plan.ids().suid, suid.unwrap_or(old.ids().suid));
                        let expected_fsuid = if setresuid_is_noop(old.ids(), ruid, euid, suid) {
                            old.ids().fsuid
                        } else {
                            plan.ids().euid
                        };
                        assert_eq!(plan.ids().fsuid, expected_fsuid);
                    }
                    assert!(
                        plan_user_id_transition(
                            &old,
                            UserIdTransitionInput::setresuid(ruid, euid, suid),
                            UserIdAuthority::CAP_SETUID,
                        )
                        .is_ok()
                    );
                }
            }
        }
    }

    #[test]
    fn setresuid_noop_preserves_a_distinct_fsuid_until_effective_is_supplied() {
        let old = unprivileged_credential();

        for input in [
            UserIdTransitionInput::setresuid(None, None, None),
            UserIdTransitionInput::setresuid(Some(old.ids().ruid), None, None),
            UserIdTransitionInput::setresuid(None, None, Some(old.ids().suid)),
            UserIdTransitionInput::setresuid(Some(old.ids().ruid), None, Some(old.ids().suid)),
        ] {
            let plan = plan_user_id_transition(&old, input, UserIdAuthority::UNPRIVILEGED).unwrap();
            assert_eq!(plan.ids(), old.ids());
            assert!(!plan.changes_credential());
        }

        let explicit_effective = plan_user_id_transition(
            &old,
            UserIdTransitionInput::setresuid(None, Some(old.ids().euid), None),
            UserIdAuthority::UNPRIVILEGED,
        )
        .unwrap();
        assert_eq!(explicit_effective.ids().euid, old.ids().euid);
        assert_eq!(explicit_effective.ids().fsuid, old.ids().euid);
        assert!(explicit_effective.changes_credential());
    }

    #[test]
    fn setfsuid_denial_is_an_unchanged_success_with_old_result() {
        let old = unprivileged_credential();
        for requested in [kuid(10), kuid(20), kuid(30), kuid(40), kuid(50)] {
            let plan = plan_user_id_transition(
                &old,
                UserIdTransitionInput::setfsuid(requested),
                UserIdAuthority::UNPRIVILEGED,
            )
            .unwrap();
            let admitted = requested != kuid(50);
            assert_eq!(
                plan.ids().fsuid,
                if admitted { requested } else { kuid(40) }
            );
            assert_eq!(plan.previous_fsuid(), kuid(40));
            assert_eq!(plan.changes_credential(), admitted && requested != kuid(40));
        }
    }

    #[test]
    fn plan_preparation_rejects_an_equal_looking_distinct_old_arc() {
        let old = unprivileged_credential();
        let equal_looking = Credential::try_prepare_transition(
            &old,
            old.ids(),
            old.groups().clone(),
            old.capabilities(),
            old.no_new_privs(),
        )
        .unwrap()
        .try_into_proposed(&old)
        .unwrap();
        let rejected = plan_user_id_transition(
            &old,
            UserIdTransitionInput::setfsuid(old.ids().fsuid),
            UserIdAuthority::UNPRIVILEGED,
        )
        .unwrap();
        assert_eq!(
            rejected.try_prepare_credential(&equal_looking).err(),
            Some(CredError::NotPermitted)
        );

        let accepted = plan_user_id_transition(
            &old,
            UserIdTransitionInput::setfsuid(old.ids().fsuid),
            UserIdAuthority::UNPRIVILEGED,
        )
        .unwrap()
        .try_prepare_credential(&old)
        .unwrap();
        assert!(core::ptr::eq(accepted.old(), old.as_ref()));
    }

    #[test]
    fn group_transition_matrices_match_user_transition_admission_without_fixups() {
        let old = unprivileged_credential();
        let choices = [
            None,
            Some(kgid(110)),
            Some(kgid(120)),
            Some(kgid(130)),
            Some(kgid(150)),
        ];

        for requested in [kgid(110), kgid(120), kgid(130), kgid(150)] {
            let result = plan_group_id_transition(
                &old,
                GroupIdTransitionInput::setgid(requested),
                GroupIdAuthority::UNPRIVILEGED,
            );
            let allowed = requested == old.ids().rgid || requested == old.ids().sgid;
            assert_eq!(result.is_ok(), allowed);
            if let Ok(plan) = result {
                assert_eq!(plan.ids().rgid, old.ids().rgid);
                assert_eq!(plan.ids().egid, requested);
                assert_eq!(plan.ids().sgid, old.ids().sgid);
                assert_eq!(plan.ids().fsgid, requested);
            }

            let privileged = plan_group_id_transition(
                &old,
                GroupIdTransitionInput::setgid(requested),
                GroupIdAuthority::CAP_SETGID,
            )
            .unwrap();
            assert_eq!(privileged.ids().rgid, requested);
            assert_eq!(privileged.ids().egid, requested);
            assert_eq!(privileged.ids().sgid, requested);
            assert_eq!(privileged.ids().fsgid, requested);
        }

        for rgid in choices {
            for egid in choices {
                let result = plan_group_id_transition(
                    &old,
                    GroupIdTransitionInput::setregid(rgid, egid),
                    GroupIdAuthority::UNPRIVILEGED,
                );
                let real_allowed =
                    rgid.is_none_or(|id| id == old.ids().rgid || id == old.ids().egid);
                let effective_allowed = egid.is_none_or(|id| {
                    id == old.ids().rgid || id == old.ids().egid || id == old.ids().sgid
                });
                assert_eq!(result.is_ok(), real_allowed && effective_allowed);
                if let Ok(plan) = result {
                    let expected_rgid = rgid.unwrap_or(old.ids().rgid);
                    let expected_egid = egid.unwrap_or(old.ids().egid);
                    let expected_sgid =
                        if rgid.is_some() || egid.is_some_and(|id| id != old.ids().rgid) {
                            expected_egid
                        } else {
                            old.ids().sgid
                        };
                    assert_eq!(plan.ids().rgid, expected_rgid);
                    assert_eq!(plan.ids().egid, expected_egid);
                    assert_eq!(plan.ids().sgid, expected_sgid);
                    assert_eq!(plan.ids().fsgid, expected_egid);
                }
                assert!(
                    plan_group_id_transition(
                        &old,
                        GroupIdTransitionInput::setregid(rgid, egid),
                        GroupIdAuthority::CAP_SETGID,
                    )
                    .is_ok()
                );
            }
        }

        for rgid in choices {
            for egid in choices {
                for sgid in choices {
                    let result = plan_group_id_transition(
                        &old,
                        GroupIdTransitionInput::setresgid(rgid, egid, sgid),
                        GroupIdAuthority::UNPRIVILEGED,
                    );
                    let allowed = [rgid, egid, sgid].into_iter().flatten().all(|id| {
                        id == old.ids().rgid || id == old.ids().egid || id == old.ids().sgid
                    });
                    assert_eq!(result.is_ok(), allowed);
                    if let Ok(plan) = result {
                        assert_eq!(plan.ids().rgid, rgid.unwrap_or(old.ids().rgid));
                        assert_eq!(plan.ids().egid, egid.unwrap_or(old.ids().egid));
                        assert_eq!(plan.ids().sgid, sgid.unwrap_or(old.ids().sgid));
                        let expected_fsgid = if setresgid_is_noop(old.ids(), rgid, egid, sgid) {
                            old.ids().fsgid
                        } else {
                            plan.ids().egid
                        };
                        assert_eq!(plan.ids().fsgid, expected_fsgid);
                    }
                    assert!(
                        plan_group_id_transition(
                            &old,
                            GroupIdTransitionInput::setresgid(rgid, egid, sgid),
                            GroupIdAuthority::CAP_SETGID,
                        )
                        .is_ok()
                    );
                }
            }
        }
    }

    #[test]
    fn setresgid_noop_preserves_a_distinct_fsgid_until_effective_is_supplied() {
        let old = unprivileged_credential();

        for input in [
            GroupIdTransitionInput::setresgid(None, None, None),
            GroupIdTransitionInput::setresgid(Some(old.ids().rgid), None, None),
            GroupIdTransitionInput::setresgid(None, None, Some(old.ids().sgid)),
            GroupIdTransitionInput::setresgid(Some(old.ids().rgid), None, Some(old.ids().sgid)),
        ] {
            let plan =
                plan_group_id_transition(&old, input, GroupIdAuthority::UNPRIVILEGED).unwrap();
            assert_eq!(plan.ids(), old.ids());
            assert!(!plan.changes_credential());
        }

        let explicit_effective = plan_group_id_transition(
            &old,
            GroupIdTransitionInput::setresgid(None, Some(old.ids().egid), None),
            GroupIdAuthority::UNPRIVILEGED,
        )
        .unwrap();
        assert_eq!(explicit_effective.ids().egid, old.ids().egid);
        assert_eq!(explicit_effective.ids().fsgid, old.ids().egid);
        assert!(explicit_effective.changes_credential());
    }

    #[test]
    fn setfsgid_denial_is_unchanged_and_capabilities_are_not_replanned() {
        let old = unprivileged_credential();
        for requested in [kgid(110), kgid(120), kgid(130), kgid(140), kgid(150)] {
            let plan = plan_group_id_transition(
                &old,
                GroupIdTransitionInput::setfsgid(requested),
                GroupIdAuthority::UNPRIVILEGED,
            )
            .unwrap();
            let admitted = requested != kgid(150);
            assert_eq!(
                plan.ids().fsgid,
                if admitted { requested } else { kgid(140) }
            );
            assert_eq!(plan.previous_fsgid(), kgid(140));
            assert_eq!(
                plan.changes_credential(),
                admitted && requested != kgid(140)
            );
        }

        let admitted = plan_group_id_transition(
            &old,
            GroupIdTransitionInput::setfsgid(kgid(150)),
            GroupIdAuthority::CAP_SETGID,
        )
        .unwrap();
        assert_eq!(admitted.ids().fsgid, kgid(150));
    }

    #[test]
    fn uid_root_loss_gain_keep_caps_and_no_fixup_edges_are_preserved() {
        let root = kuid(1000);
        let user = kuid(1001);
        let mut old_ids = ids();
        old_ids.ruid = root;
        old_ids.euid = root;
        old_ids.suid = root;
        let mut new_ids = old_ids;
        new_ids.ruid = user;
        new_ids.euid = user;
        new_ids.suid = user;

        let full = CapabilitySets::full();
        let dropped = fixup_uid_capabilities(Some(root), old_ids, new_ids, full).unwrap();
        assert_eq!(dropped.permitted(), [0; CAPABILITY_WORDS]);
        assert_eq!(dropped.effective(), [0; CAPABILITY_WORDS]);

        let inheritable = bit(CAP_CHOWN);
        let ambient = bit(CAP_CHOWN);
        let keep = CapabilitySets::try_new(
            CAPABILITY_VALID_MASK,
            CAPABILITY_VALID_MASK,
            inheritable,
            CAPABILITY_VALID_MASK,
            ambient,
            SECBIT_KEEP_CAPS,
        )
        .unwrap();
        let retained = fixup_uid_capabilities(Some(root), old_ids, new_ids, keep).unwrap();
        assert_eq!(retained.permitted(), CAPABILITY_VALID_MASK);
        assert_eq!(retained.effective(), [0; CAPABILITY_WORDS]);
        assert_eq!(retained.ambient(), [0; CAPABILITY_WORDS]);

        let no_fixup = CapabilitySets::try_new(
            CAPABILITY_VALID_MASK,
            CAPABILITY_VALID_MASK,
            [0; CAPABILITY_WORDS],
            CAPABILITY_VALID_MASK,
            [0; CAPABILITY_WORDS],
            SECBIT_NO_SETUID_FIXUP,
        )
        .unwrap();
        assert_eq!(
            fixup_uid_capabilities(Some(root), old_ids, new_ids, no_fixup).unwrap(),
            no_fixup
        );

        let gained = fixup_uid_capabilities(Some(root), new_ids, old_ids, retained).unwrap();
        assert_eq!(gained.effective(), gained.permitted());
        assert_eq!(
            fixup_uid_capabilities(None, old_ids, new_ids, full).unwrap(),
            full
        );
    }

    #[test]
    fn fsuid_root_crossing_changes_only_linux_filesystem_capabilities() {
        let root = kuid(1000);
        let user = kuid(1001);
        let dropped =
            fixup_fsuid_capabilities(Some(root), root, user, CapabilitySets::full()).unwrap();
        assert!(!dropped.has_effective(CAP_CHOWN));
        assert!(dropped.has_effective(CAP_KILL));

        let raised = fixup_fsuid_capabilities(Some(root), user, root, dropped).unwrap();
        assert!(raised.has_effective(CAP_CHOWN));
        assert!(raised.has_effective(CAP_KILL));

        let no_root = fixup_fsuid_capabilities(None, root, user, CapabilitySets::full()).unwrap();
        assert_eq!(no_root, CapabilitySets::full());
    }

    #[test]
    fn setid_and_setfsuid_keep_linux_fixup_families_distinct() {
        let root = kuid(1000);
        let mut old_ids = ids();
        old_ids.ruid = kuid(1001);
        old_ids.euid = kuid(1002);
        old_ids.suid = kuid(1003);
        old_ids.fsuid = root;
        let old = credential_with(old_ids, CapabilitySets::full(), Some(root));

        // Linux's ID-family commoncap path applies cap_emulate_setxuid only;
        // it does not silently run the LSM_SETID_FS branch as a second hook.
        let id_plan = plan_user_id_transition(
            &old,
            UserIdTransitionInput::setresuid(None, None, Some(old_ids.euid)),
            UserIdAuthority::UNPRIVILEGED,
        )
        .unwrap();
        assert_eq!(id_plan.ids().fsuid, old_ids.euid);
        assert!(id_plan.capabilities().has_effective(CAP_CHOWN));
        assert!(id_plan.capabilities().has_effective(CAP_KILL));

        let fs_plan = plan_user_id_transition(
            &old,
            UserIdTransitionInput::setfsuid(old_ids.euid),
            UserIdAuthority::UNPRIVILEGED,
        )
        .unwrap();
        assert!(!fs_plan.capabilities().has_effective(CAP_CHOWN));
        assert!(fs_plan.capabilities().has_effective(CAP_KILL));
    }

    #[test]
    fn capset_enforces_effective_permitted_bounding_and_setpcap_constraints() {
        let chown = bit(CAP_CHOWN);
        let kill = bit(CAP_KILL);
        let setpcap = bit(CAP_SETPCAP);
        let old_capabilities = CapabilitySets::try_new(
            setpcap,
            union(chown, setpcap),
            [0; CAPABILITY_WORDS],
            union(kill, setpcap),
            [0; CAPABILITY_WORDS],
            0,
        )
        .unwrap();
        let old = credential_with(ids(), old_capabilities, Some(Kuid::INITIAL_ROOT));

        let effective_outside_permitted =
            CapsetRequest::try_new(chown, [0; CAPABILITY_WORDS], [0; CAPABILITY_WORDS]).unwrap();
        assert_eq!(
            plan_capset(
                &old,
                effective_outside_permitted,
                CapsetAuthority::CAP_SETPCAP
            )
            .err(),
            Some(CredError::NotPermitted)
        );

        let raised_permitted = CapsetRequest::try_new(
            [0; CAPABILITY_WORDS],
            union(chown, kill),
            [0; CAPABILITY_WORDS],
        )
        .unwrap();
        assert_eq!(
            plan_capset(&old, raised_permitted, CapsetAuthority::CAP_SETPCAP).err(),
            Some(CredError::NotPermitted)
        );

        let bounded_not_permitted =
            CapsetRequest::try_new([0; CAPABILITY_WORDS], chown, kill).unwrap();
        assert_eq!(
            plan_capset(&old, bounded_not_permitted, CapsetAuthority::RESTRICTED).err(),
            Some(CredError::NotPermitted)
        );
        assert!(plan_capset(&old, bounded_not_permitted, CapsetAuthority::CAP_SETPCAP).is_ok());

        // CAP_SETPCAP never overrides the independent bounding constraint.
        let permitted_not_bounded =
            CapsetRequest::try_new([0; CAPABILITY_WORDS], chown, chown).unwrap();
        assert_eq!(
            plan_capset(&old, permitted_not_bounded, CapsetAuthority::CAP_SETPCAP).err(),
            Some(CredError::NotPermitted)
        );
    }

    #[test]
    fn capset_preserves_bounding_securebits_and_reconciles_ambient() {
        let chown = bit(CAP_CHOWN);
        let old_capabilities = CapabilitySets::try_new(
            chown,
            chown,
            chown,
            CAPABILITY_VALID_MASK,
            chown,
            SECBIT_NOROOT,
        )
        .unwrap();
        let old = credential_with(ids(), old_capabilities, Some(Kuid::INITIAL_ROOT));
        let request =
            CapsetRequest::try_new([0; CAPABILITY_WORDS], chown, [0; CAPABILITY_WORDS]).unwrap();
        let plan = plan_capset(&old, request, CapsetAuthority::RESTRICTED).unwrap();
        assert_eq!(plan.capabilities().bounding(), CAPABILITY_VALID_MASK);
        assert_eq!(plan.capabilities().securebits(), SECBIT_NOROOT);
        assert_eq!(plan.capabilities().ambient(), [0; CAPABILITY_WORDS]);
    }

    #[test]
    fn capset_rejects_invalid_masks_before_policy() {
        let mut invalid = [0; CAPABILITY_WORDS];
        invalid[CAPABILITY_WORDS - 1] = !CAPABILITY_VALID_MASK[CAPABILITY_WORDS - 1];
        assert_eq!(
            CapsetRequest::try_new(invalid, [0; CAPABILITY_WORDS], [0; CAPABILITY_WORDS]),
            Err(CredError::InvalidInput)
        );
    }

    #[test]
    fn securebits_helpers_enforce_lock_and_keep_caps_rules() {
        let mut capabilities = CapabilitySets::empty();
        capabilities.try_set_keep_caps(true).unwrap();
        assert_eq!(capabilities.securebits(), SECBIT_KEEP_CAPS);
        capabilities.try_set_keep_caps(false).unwrap();
        assert_eq!(capabilities.securebits(), 0);
        capabilities.try_set_securebits(SECBIT_KEEP_CAPS).unwrap();
        capabilities
            .try_set_securebits(SECBIT_KEEP_CAPS | SECBIT_KEEP_CAPS_LOCKED)
            .unwrap();
        assert_eq!(
            capabilities.try_set_keep_caps(false),
            Err(CredError::NotPermitted)
        );
        assert_eq!(
            capabilities.try_set_securebits(SECBIT_KEEP_CAPS_LOCKED),
            Err(CredError::NotPermitted)
        );
        assert_eq!(
            capabilities.try_set_securebits(u32::MAX),
            Err(CredError::NotPermitted)
        );
    }

    #[test]
    fn advisory_exec_securebits_validate_lock_and_transition_invariants() {
        let advisory = SECBIT_EXEC_RESTRICT_FILE | SECBIT_EXEC_DENY_INTERACTIVE;
        let locks = SECBIT_EXEC_RESTRICT_FILE_LOCKED | SECBIT_EXEC_DENY_INTERACTIVE_LOCKED;
        assert_eq!(SECURE_ALL_UNPRIVILEGED, advisory);

        let mut capabilities = CapabilitySets::empty();
        capabilities.try_set_securebits(advisory).unwrap();
        capabilities.try_set_securebits(advisory | locks).unwrap();
        assert_eq!(capabilities.securebits(), advisory | locks);
        assert_eq!(
            capabilities.try_set_securebits(locks),
            Err(CredError::NotPermitted)
        );
        assert_eq!(
            capabilities.try_set_securebits(advisory),
            Err(CredError::NotPermitted)
        );

        let old = credential_with(ids(), capabilities, Some(Kuid::INITIAL_ROOT));
        let plan = plan_user_id_transition(
            &old,
            UserIdTransitionInput::setfsuid(old.ids().euid),
            UserIdAuthority::UNPRIVILEGED,
        )
        .unwrap();
        assert_eq!(plan.capabilities().securebits(), advisory | locks);
        let proposed = plan
            .try_prepare_credential(&old)
            .unwrap()
            .try_into_proposed(&old)
            .unwrap();
        assert_eq!(proposed.capabilities().securebits(), advisory | locks);
    }
}
