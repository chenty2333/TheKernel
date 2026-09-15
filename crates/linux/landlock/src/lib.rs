//! A bounded, immutable access-policy planner.
//!
//! This crate deliberately models only stable object identities.  It accepts
//! neither paths nor descriptor, task, or location handles: resolving those
//! mutable kernel concepts must happen before producing an [`IdentitySnapshot`].

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate std;

use core::fmt;

/// Linux Landlock's currently supported filesystem access-mask bits.
///
/// This is deliberately a raw UAPI mask: policy frontends keep the exact
/// userspace value and map their resolver-owned objects separately.  It is
/// `LANDLOCK_MASK_ACCESS_FS` for ABI 10, i.e. bits 0 through 16 including
/// `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`.
pub const FS_ACCESS_MASK: u64 = 0x1_ffff;
/// Linux ABI 10 network access mask (`LANDLOCK_MASK_ACCESS_NET`): TCP
/// bind/connect plus the two UDP rights added by ABI 10.
pub const NET_ACCESS_MASK: u64 = 0xf;
/// Linux ABI 6+ scope mask (`LANDLOCK_MASK_SCOPE`).
pub const SCOPE_MASK: u64 = 0x3;
/// The only flag `landlock_add_rule(2)` accepts besides zero.
pub const ADD_RULE_QUIET: u32 = 1 << 0;
/// `landlock_restrict_self(2)` flag applying the new configuration to every
/// thread of the calling process.
pub const RESTRICT_SELF_TSYNC: u32 = 1 << 3;
/// `LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF`: silences denials recorded by
/// nested layers of the new domain.
pub const LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF: u32 = 1 << 2;
/// Filesystem rights Linux checks when the object is used rather than opened
/// (`_LANDLOCK_ACCESS_FS_OPTIONAL`).  Only these participate in per-object
/// quiet logging, because the quiet decision is cached on the opened file.
pub const OPTIONAL_FS_ACCESS_MASK: u64 = (1 << 14) | (1 << 15);
/// Filesystem rights which may be attached to a non-directory rule target.
pub const NON_DIRECTORY_FS_ACCESS_MASK: u64 =
    (1 << 0) | (1 << 1) | (1 << 2) | (1 << 14) | (1 << 15) | (1 << 16);

/// Typed result of path-rule admission validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathRuleReject {
    /// A rule may not have an empty access mask.
    EmptyAccess,
    /// The rule requests rights not handled by its ruleset.
    UnhandledAccess,
    /// A non-directory target received a directory-only right.
    NonDirectoryAccess,
    /// `LANDLOCK_ADD_RULE_QUIET` was used on a ruleset without quiet access
    /// bits for this object type.
    QuietWithoutQuietMask,
}

/// Typed result of network-rule admission validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetRuleReject {
    /// A rule may not have an empty access mask.
    EmptyAccess,
    /// The rule requests rights not handled by its ruleset.
    UnhandledAccess,
    /// `LANDLOCK_ADD_RULE_QUIET` was used on a ruleset without quiet network
    /// access bits.
    QuietWithoutQuietMask,
    /// The port does not fit Linux's `u16` port field.
    PortOutOfRange,
}

/// Typed result of `struct landlock_ruleset_attr` validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RulesetAttrReject {
    /// `handled_access_fs` contains a bit this ABI does not define.
    UnknownFsAccess,
    /// `handled_access_net` contains a bit this ABI does not define.
    UnknownNetAccess,
    /// `scoped` contains a bit this ABI does not define.
    UnknownScope,
    /// `quiet_access_fs` is not a subset of `handled_access_fs`.
    QuietFsWithoutHandled,
    /// `quiet_access_net` is not a subset of `handled_access_net`.
    QuietNetWithoutHandled,
    /// `quiet_scoped` is not a subset of `scoped`.
    QuietScopeWithoutHandled,
    /// No handled access right and no scope was requested (`-ENOMSG`).
    Empty,
}

/// Validates one decoded `struct landlock_ruleset_attr`.
///
/// The check order is Linux `SYSCALL_DEFINE3(landlock_create_ruleset, ...)`
/// (`security/landlock/syscalls.c`) followed by its `landlock_create_ruleset()`
/// call (`security/landlock/ruleset.c`), because the resulting `-EINVAL`
/// versus `-ENOMSG` distinction is observable.
pub const fn admit_ruleset_attr(
    handled_fs: u64,
    handled_net: u64,
    scoped: u64,
    quiet_fs: u64,
    quiet_net: u64,
    quiet_scoped: u64,
) -> Result<(), RulesetAttrReject> {
    if handled_fs & !FS_ACCESS_MASK != 0 {
        return Err(RulesetAttrReject::UnknownFsAccess);
    }
    if handled_net & !NET_ACCESS_MASK != 0 {
        return Err(RulesetAttrReject::UnknownNetAccess);
    }
    if scoped & !SCOPE_MASK != 0 {
        return Err(RulesetAttrReject::UnknownScope);
    }
    if quiet_fs & !handled_fs != 0 {
        return Err(RulesetAttrReject::QuietFsWithoutHandled);
    }
    if quiet_net & !handled_net != 0 {
        return Err(RulesetAttrReject::QuietNetWithoutHandled);
    }
    if quiet_scoped & !scoped != 0 {
        return Err(RulesetAttrReject::QuietScopeWithoutHandled);
    }
    if handled_fs == 0 && handled_net == 0 && scoped == 0 {
        return Err(RulesetAttrReject::Empty);
    }
    Ok(())
}

/// Validates the `landlock_add_rule(2)` flag word: zero or
/// [`ADD_RULE_QUIET`], nothing else.
#[must_use]
pub const fn admit_add_rule_flags(flags: u32) -> bool {
    flags == 0 || flags == ADD_RULE_QUIET
}

/// Whether `landlock_restrict_self(2)` may omit its ruleset descriptor.
///
/// Linux `SYSCALL_DEFINE2(landlock_restrict_self, ...)`
/// (`security/landlock/syscalls.c`) only skips `get_ruleset_from_fd()` for
/// `ruleset_fd == -1` with exactly `LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF`,
/// optionally combined with [`RESTRICT_SELF_TSYNC`].  Every other flag
/// combination still resolves the descriptor and therefore reports `-EBADF`,
/// so the flag word is checked before the descriptor is known to be absent.
#[must_use]
pub const fn restrict_self_without_ruleset(ruleset_fd: i32, flags: u32) -> bool {
    ruleset_fd == -1 && (flags & !RESTRICT_SELF_TSYNC) == LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF
}

/// Validates a raw Linux path-beneath rule without resolving its target.
///
/// Descriptor lookup and the opaque target mapping deliberately remain with
/// the caller, preserving its usercopy and VFS validation order.
pub const fn admit_path_rule(
    ruleset_handled: u64,
    allowed: u64,
    quiet: bool,
    ruleset_quiet_fs: u64,
    target_is_directory: bool,
) -> Result<(), PathRuleReject> {
    match admit_path_rule_access(ruleset_handled, allowed, quiet) {
        Ok(()) => {}
        Err(error) => return Err(error),
    }
    if quiet && ruleset_quiet_fs == 0 {
        return Err(PathRuleReject::QuietWithoutQuietMask);
    }
    if !target_is_directory && allowed & !NON_DIRECTORY_FS_ACCESS_MASK != 0 {
        return Err(PathRuleReject::NonDirectoryAccess);
    }
    Ok(())
}

/// Validates the mask-only portion of path-rule admission.  Callers use this
/// before descriptor resolution when matching Linux's validation order.
pub const fn admit_path_rule_access(
    ruleset_handled: u64,
    allowed: u64,
    quiet: bool,
) -> Result<(), PathRuleReject> {
    // "Informs about useless rule: empty allowed_access (i.e. deny rules) are
    // ignored in path walks.  However, the rule is not useless if it is there
    // to hold a quiet flag."
    if allowed == 0 && !quiet {
        return Err(PathRuleReject::EmptyAccess);
    }
    if allowed & !ruleset_handled != 0 {
        return Err(PathRuleReject::UnhandledAccess);
    }
    Ok(())
}

/// Validates one network-port rule against its ruleset.
///
/// The order matches Linux `add_rule_net_port()`: empty-access `-ENOMSG`,
/// unhandled-access `-EINVAL`, useless quiet flag `-EINVAL`, then the `u16`
/// port check.
pub const fn admit_net_rule(
    ruleset_handled_net: u64,
    allowed: u64,
    quiet: bool,
    ruleset_quiet_net: u64,
    port: u64,
) -> Result<(), NetRuleReject> {
    if allowed == 0 && !quiet {
        return Err(NetRuleReject::EmptyAccess);
    }
    if allowed & !ruleset_handled_net != 0 {
        return Err(NetRuleReject::UnhandledAccess);
    }
    if quiet && ruleset_quiet_net == 0 {
        return Err(NetRuleReject::QuietWithoutQuietMask);
    }
    if port > u16::MAX as u64 {
        return Err(NetRuleReject::PortOutOfRange);
    }
    Ok(())
}

/// Linux `landlock_log_denial()` decision for a filesystem or network denial.
///
/// A record is suppressed only when the youngest denying layer marked this
/// object with `LANDLOCK_ADD_RULE_QUIET` *and* every denied access bit is part
/// of that layer's corresponding quiet mask.  Callers pass the accesses that
/// layer actually denied; passing a superset only ever keeps a record Linux
/// would have suppressed, never the reverse.
#[must_use]
pub const fn quiet_object_denial(object_marked_quiet: bool, quiet_mask: u64, denied: u64) -> bool {
    object_marked_quiet && (quiet_mask & denied) == denied
}

/// Linux `landlock_log_denial()` decision for a scoped denial.
///
/// Scope denials are never tied to a per-object quiet flag: a layer created
/// with the matching `quiet_scoped` bit suppresses them unconditionally.
#[must_use]
pub const fn quiet_scope_denial(quiet_scoped: u64, scope: u64) -> bool {
    scope != 0 && (quiet_scoped & scope) == scope
}

/// Linux `hook_unix_find()` decision for one layer of the connecting domain.
///
/// `security/landlock/fs.c:hook_unix_find()` resolves
/// `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` through the ordinary path rules and then
/// calls `unmask_scoped_access()`: a layer that would deny the lookup stops
/// denying when the connecting and the creating domain are the same hierarchy
/// node at that layer depth.  A layer that does not handle the right never
/// denies it.
#[must_use]
pub const fn unix_resolution_layer_allows(
    handles_resolve_unix: bool,
    grants_path: bool,
    same_domain: bool,
) -> bool {
    !handles_resolve_unix || grants_path || same_domain
}

/// Decides one raw filesystem access request from resolver-selected ancestor
/// rules.  The iterator must contain exactly the rules whose opaque targets
/// are ancestors of the requested opaque target.
#[must_use]
pub fn allows_path_access(
    ruleset_handled: u64,
    requested: u64,
    ancestor_rule_accesses: impl Iterator<Item = u64>,
) -> bool {
    let requested = ruleset_handled & requested;
    if requested == 0 {
        return true;
    }
    let mut allowed = 0;
    for access in ancestor_rule_accesses {
        allowed |= access;
    }
    allowed & requested == requested
}

/// Checks Landlock's cross-directory no-less-restrictive destination rule.
#[must_use]
pub const fn destination_is_no_less_restrictive(
    ruleset_handled: u64,
    compared_access: u64,
    source_allowed: u64,
    destination_allowed: u64,
) -> bool {
    destination_allowed & ruleset_handled & compared_access & !(source_allowed & ruleset_handled)
        == 0
}

/// A stable, resolver-supplied filesystem object identity.
///
/// This value is deliberately not a location.  A resolver chooses an identity
/// only after its own race-free lookup and mount policy have completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ObjectIdentity(u128);

impl ObjectIdentity {
    /// Creates an identity from a resolver-owned stable value.
    #[must_use]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }
}

/// An opaque target usable in a policy rule or access request.
///
/// It intentionally exposes no pathname, file descriptor, task, or mutable
/// filesystem object reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct RuleTarget(ObjectIdentity);

impl RuleTarget {
    /// Wraps a stable identity as a policy target.
    #[must_use]
    pub const fn from_identity(identity: ObjectIdentity) -> Self {
        Self(identity)
    }
}

/// An immutable principal identity supplied by the credential subsystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Principal(u128);

impl Principal {
    /// Creates a principal from a credential subsystem stable identity.
    #[must_use]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }
}

/// Filesystem operations understood by this policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum AccessRight {
    /// Read object contents or enumerate a directory.
    Read = 0,
    /// Write object contents.
    Write = 1,
    /// Execute a regular file.
    Execute = 2,
    /// Create a child beneath a directory target.
    Create = 3,
    /// Remove a child beneath a directory target.
    Remove = 4,
    /// Change object metadata.
    Refer = 5,
}

/// A non-empty set of [`AccessRight`] values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Access(u8);

impl Access {
    const KNOWN: u8 = (1 << 6) - 1;

    /// The empty access set.
    pub const NONE: Self = Self(0);

    /// Creates a singleton access set.
    #[must_use]
    pub const fn one(right: AccessRight) -> Self {
        Self(1 << right as u8)
    }

    /// Combines two access sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns whether every right in `other` is contained in this set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns whether this access set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    const fn is_valid(self) -> bool {
        !self.is_empty() && self.0 & !Self::KNOWN == 0
    }
}

/// A stable input snapshot produced after identity resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct IdentitySnapshot {
    principal: Principal,
    target: RuleTarget,
    /// Resolver generation, carried into plans to reject stale commits.
    generation: u64,
}

impl IdentitySnapshot {
    /// Captures the identities and generation used for a single policy check.
    #[must_use]
    pub const fn new(principal: Principal, target: RuleTarget, generation: u64) -> Self {
        Self {
            principal,
            target,
            generation,
        }
    }

    /// Returns the snapshot principal.
    #[must_use]
    pub const fn principal(self) -> Principal {
        self.principal
    }

    /// Returns the opaque snapshot target.
    #[must_use]
    pub const fn target(self) -> RuleTarget {
        self.target
    }

    /// Returns the resolver generation recorded with this snapshot.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// An operation presented to the policy engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AccessRequest {
    target: RuleTarget,
    access: Access,
}

impl AccessRequest {
    /// Creates an access request.
    #[must_use]
    pub const fn new(target: RuleTarget, access: Access) -> Self {
        Self { target, access }
    }

    /// Returns the opaque target requested.
    #[must_use]
    pub const fn target(self) -> RuleTarget {
        self.target
    }

    /// Returns the requested access set.
    #[must_use]
    pub const fn access(self) -> Access {
        self.access
    }
}

/// One immutable policy rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Rule {
    principal: Principal,
    target: RuleTarget,
    allowed: Access,
}

impl Rule {
    /// Creates a rule, rejecting empty or unknown access rights.
    pub const fn try_new(
        principal: Principal,
        target: RuleTarget,
        allowed: Access,
    ) -> Result<Self, RuleError> {
        if !allowed.is_valid() {
            return Err(RuleError::InvalidAccess);
        }
        Ok(Self {
            principal,
            target,
            allowed,
        })
    }
}

/// Rule construction and bounded ruleset admission failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuleError {
    /// The access set was empty or contained an unknown operation bit.
    InvalidAccess,
    /// A ruleset's fixed capacity was exhausted.
    Capacity {
        /// Maximum number of rules.
        maximum: usize,
    },
}

impl fmt::Display for RuleError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAccess => output.write_str("invalid access set"),
            Self::Capacity { .. } => output.write_str("ruleset capacity exhausted"),
        }
    }
}

/// A fixed-capacity immutable sequence of policy rules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ruleset<const MAX_RULES: usize> {
    rules: [Option<Rule>; MAX_RULES],
    len: usize,
}

impl<const MAX_RULES: usize> Ruleset<MAX_RULES> {
    /// Creates an empty immutable ruleset.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            rules: [None; MAX_RULES],
            len: 0,
        }
    }

    /// Returns the number of admitted rules.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether this ruleset has no rules.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns a new ruleset with `rule` appended, or a typed capacity reject.
    pub fn try_with_rule(mut self, rule: Rule) -> Result<Self, RuleError> {
        if self.len == MAX_RULES {
            return Err(RuleError::Capacity { maximum: MAX_RULES });
        }
        self.rules[self.len] = Some(rule);
        self.len += 1;
        Ok(self)
    }

    fn allowed_for(&self, principal: Principal, target: RuleTarget) -> Access {
        let mut allowed = Access::NONE;
        let mut index = 0;
        while index < self.len {
            let rule = self.rules[index].expect("rules before length are initialized");
            if rule.principal == principal && rule.target == target {
                allowed = allowed.union(rule.allowed);
            }
            index += 1;
        }
        allowed
    }
}

/// Capability that permits a principal to create an immutable policy domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DomainAuthority {
    principal: Principal,
}

impl DomainAuthority {
    /// Constructs a domain authority from an explicitly designated principal.
    ///
    /// Obtaining this capability is outside this crate's policy boundary.
    #[must_use]
    pub const fn new(principal: Principal) -> Self {
        Self { principal }
    }
}

/// A fully immutable policy domain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Domain<const MAX_RULES: usize> {
    owner: Principal,
    ruleset: Ruleset<MAX_RULES>,
}

impl<const MAX_RULES: usize> Domain<MAX_RULES> {
    /// Creates a domain owned by `authority`.
    #[must_use]
    pub const fn new(authority: DomainAuthority, ruleset: Ruleset<MAX_RULES>) -> Self {
        Self {
            owner: authority.principal,
            ruleset,
        }
    }

    /// Returns the immutable domain owner's principal identity.
    #[must_use]
    pub const fn owner(&self) -> Principal {
        self.owner
    }

    /// Validates an input snapshot and request, producing a fallible plan.
    pub fn plan(
        &self,
        snapshot: IdentitySnapshot,
        request: AccessRequest,
    ) -> Result<AccessPlan, Reject> {
        if snapshot.target != request.target {
            return Err(Reject::SnapshotTargetMismatch);
        }
        if !request.access.is_valid() {
            return Err(Reject::InvalidAccess);
        }
        let allowed = self.ruleset.allowed_for(snapshot.principal, request.target);
        if !allowed.contains(request.access) {
            return Err(Reject::PermissionDenied {
                requested: request.access,
                allowed,
            });
        }
        Ok(AccessPlan { snapshot, request })
    }
}

/// A typed denial during policy planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Reject {
    /// The request was not for the target represented by the identity snapshot.
    SnapshotTargetMismatch,
    /// The request included no right or an unknown right.
    InvalidAccess,
    /// Matching rules did not grant all requested rights.
    PermissionDenied {
        /// Requested right set.
        requested: Access,
        /// Union of rights granted by matching rules.
        allowed: Access,
    },
    /// The resolver reported a generation other than the plan's snapshot.
    StaleSnapshot {
        /// Generation used while planning.
        planned: u64,
        /// Current generation.
        current: u64,
    },
}

/// An immutable successful policy decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessPlan {
    snapshot: IdentitySnapshot,
    request: AccessRequest,
}

impl AccessPlan {
    /// Returns the exact snapshot evaluated by the policy.
    #[must_use]
    pub const fn snapshot(self) -> IdentitySnapshot {
        self.snapshot
    }

    /// Returns the exact request approved by the policy.
    #[must_use]
    pub const fn request(self) -> AccessRequest {
        self.request
    }

    /// Starts a single-use admission transaction for this immutable decision.
    pub const fn prepare(self) -> PreparedAccess {
        PreparedAccess { plan: Some(self) }
    }
}

/// A single-use, rollback-safe policy admission.
///
/// Dropping this value or calling [`rollback`](Self::rollback) grants nothing.
/// [`commit`](Self::commit) checks the resolver generation before returning the
/// immutable grant capability.
#[must_use = "a prepared admission must be committed or is rolled back on drop"]
pub struct PreparedAccess {
    plan: Option<AccessPlan>,
}

impl PreparedAccess {
    /// Commits this plan if the resolver still reports its snapshot generation.
    pub fn commit(mut self, current_generation: u64) -> Result<GrantedAccess, Reject> {
        let plan = self
            .plan
            .take()
            .expect("prepared access is consumed exactly once");
        if plan.snapshot.generation != current_generation {
            return Err(Reject::StaleSnapshot {
                planned: plan.snapshot.generation,
                current: current_generation,
            });
        }
        Ok(GrantedAccess { plan })
    }

    /// Explicitly abandons this admission without granting access.
    pub fn rollback(mut self) {
        let _ = self.plan.take();
    }
}

/// A committed, immutable authorization for exactly one checked request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GrantedAccess {
    plan: AccessPlan,
}

impl GrantedAccess {
    /// Returns the approved request.
    #[must_use]
    pub const fn request(self) -> AccessRequest {
        self.plan.request
    }

    /// Returns the identity snapshot that authorized the request.
    #[must_use]
    pub const fn snapshot(self) -> IdentitySnapshot {
        self.plan.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: Principal = Principal::new(7);

    #[test]
    fn raw_path_admission_and_ancestor_decision_preserve_linux_masks() {
        assert_eq!(
            admit_path_rule_access(0b11, 0b100, false),
            Err(PathRuleReject::UnhandledAccess)
        );
        assert_eq!(
            admit_path_rule_access(0b11, 0, false),
            Err(PathRuleReject::EmptyAccess)
        );
        assert_eq!(
            admit_path_rule(FS_ACCESS_MASK, 1 << 7, false, 0, false),
            Err(PathRuleReject::NonDirectoryAccess)
        );
        assert!(allows_path_access(0b11, 0b11, [0b01, 0b10].into_iter()));
        assert!(!allows_path_access(0b11, 0b11, [0b01].into_iter()));
    }

    #[test]
    fn abi10_masks_match_linux_723_limits() {
        // security/landlock/limits.h: LAST_ACCESS_FS = RESOLVE_UNIX (bit 16),
        // LAST_ACCESS_NET = CONNECT_SEND_UDP (bit 3), LAST_SCOPE = SIGNAL.
        assert_eq!(FS_ACCESS_MASK, (1 << 17) - 1);
        assert_eq!(NET_ACCESS_MASK, (1 << 4) - 1);
        assert_eq!(SCOPE_MASK, (1 << 2) - 1);
        // Linux ACCESS_FILE includes RESOLVE_UNIX.
        assert_ne!(NON_DIRECTORY_FS_ACCESS_MASK & (1 << 16), 0);
        assert_eq!(OPTIONAL_FS_ACCESS_MASK, (1 << 14) | (1 << 15));
    }

    #[test]
    fn ruleset_attr_validation_order_matches_landlock_create_ruleset() {
        let ok = admit_ruleset_attr(1, 0, 0, 0, 0, 0);
        assert_eq!(ok, Ok(()));
        assert_eq!(
            admit_ruleset_attr(1 << 17, 0, 0, 0, 0, 0),
            Err(RulesetAttrReject::UnknownFsAccess)
        );
        assert_eq!(
            admit_ruleset_attr(1, 1 << 4, 0, 0, 0, 0),
            Err(RulesetAttrReject::UnknownNetAccess)
        );
        assert_eq!(
            admit_ruleset_attr(1, 0, 1 << 2, 0, 0, 0),
            Err(RulesetAttrReject::UnknownScope)
        );
        assert_eq!(
            admit_ruleset_attr(1, 0, 0, 2, 0, 0),
            Err(RulesetAttrReject::QuietFsWithoutHandled)
        );
        assert_eq!(
            admit_ruleset_attr(0, 2, 0, 0, 4, 0),
            Err(RulesetAttrReject::QuietNetWithoutHandled)
        );
        assert_eq!(
            admit_ruleset_attr(0, 0, 1, 0, 0, 2),
            Err(RulesetAttrReject::QuietScopeWithoutHandled)
        );
        // Linux reaches landlock_create_ruleset()'s -ENOMSG only after the
        // three mask checks and the three quiet-subset checks.
        assert_eq!(
            admit_ruleset_attr(0, 0, 0, 0, 0, 0),
            Err(RulesetAttrReject::Empty)
        );
        // A quiet-only ruleset is still empty and therefore -ENOMSG.
        assert_eq!(
            admit_ruleset_attr(0, 0, 0, 0, 0, 0),
            Err(RulesetAttrReject::Empty)
        );
        assert_eq!(admit_ruleset_attr(1 << 16, 1 << 2, 1, 1 << 16, 4, 1), Ok(()));
    }

    #[test]
    fn quiet_rules_need_a_quiet_access_bit() {
        assert!(admit_add_rule_flags(0));
        assert!(admit_add_rule_flags(ADD_RULE_QUIET));
        assert!(!admit_add_rule_flags(2));
        // A quiet rule may carry an empty allowed mask...
        assert_eq!(
            admit_path_rule_access(FS_ACCESS_MASK, 0, true),
            Ok(())
        );
        assert_eq!(
            admit_net_rule(NET_ACCESS_MASK, 0, true, NET_ACCESS_MASK, 80),
            Ok(())
        );
        // ...but only when the ruleset actually has quiet bits to spend.
        assert_eq!(
            admit_path_rule(FS_ACCESS_MASK, 0, true, 0, true),
            Err(PathRuleReject::QuietWithoutQuietMask)
        );
        assert_eq!(
            admit_net_rule(NET_ACCESS_MASK, 0, true, 0, 80),
            Err(NetRuleReject::QuietWithoutQuietMask)
        );
        // Unhandled access still wins over the quiet check.
        assert_eq!(
            admit_path_rule(1, 2, true, 0, true),
            Err(PathRuleReject::UnhandledAccess)
        );
    }

    #[test]
    fn net_rule_validation_matches_add_rule_net_port() {
        assert_eq!(
            admit_net_rule(NET_ACCESS_MASK, 0, false, 0, 80),
            Err(NetRuleReject::EmptyAccess)
        );
        assert_eq!(
            admit_net_rule(1, 2, false, 0, 80),
            Err(NetRuleReject::UnhandledAccess)
        );
        assert_eq!(
            admit_net_rule(NET_ACCESS_MASK, 4, false, 0, 65536),
            Err(NetRuleReject::PortOutOfRange)
        );
        assert_eq!(
            admit_net_rule(NET_ACCESS_MASK, 4, false, 0, 65535),
            Ok(())
        );
        // Port 0 is an ordinary rule key: it is what grants `bind(2)` on an
        // ephemeral port and the implicit autobind of `connect(2)`
        // (tools/testing/selftests/landlock/net_test.c: `bind_ephemeral`).
        assert_eq!(admit_net_rule(NET_ACCESS_MASK, 4, false, 0, 0), Ok(()));
    }

    #[test]
    fn ruleset_free_restrict_self_matches_landlock_restrict_self() {
        const SUBDOMAINS_OFF: u32 = LANDLOCK_RESTRICT_SELF_LOG_SUBDOMAINS_OFF;
        const SAME_EXEC_OFF: u32 = 1 << 0;
        const NEW_EXEC_ON: u32 = 1 << 1;

        // Exactly the subdomain-logging flag, optionally with TSYNC.
        assert!(restrict_self_without_ruleset(-1, SUBDOMAINS_OFF));
        assert!(restrict_self_without_ruleset(
            -1,
            SUBDOMAINS_OFF | RESTRICT_SELF_TSYNC
        ));
        // Any other flag word still resolves the descriptor, so `-1` is EBADF.
        assert!(!restrict_self_without_ruleset(-1, 0));
        assert!(!restrict_self_without_ruleset(-1, SAME_EXEC_OFF));
        assert!(!restrict_self_without_ruleset(-1, NEW_EXEC_ON));
        assert!(!restrict_self_without_ruleset(
            -1,
            SUBDOMAINS_OFF | NEW_EXEC_ON
        ));
        assert!(!restrict_self_without_ruleset(
            -1,
            SUBDOMAINS_OFF | RESTRICT_SELF_TSYNC | SAME_EXEC_OFF
        ));
        // A real descriptor never takes this path.
        assert!(!restrict_self_without_ruleset(3, SUBDOMAINS_OFF));
    }

    #[test]
    fn quiet_denial_decision_matches_landlock_log_denial() {
        // Object not marked quiet: nothing is suppressed for path/net.
        assert!(!quiet_object_denial(false, u64::MAX, 1));
        // Marked quiet, but the denied right is not in the quiet mask.
        assert!(!quiet_object_denial(true, 1 << 14, 1 << 15));
        assert!(quiet_object_denial(true, 1 << 14, 1 << 14));
        assert!(quiet_object_denial(
            true,
            (1 << 14) | (1 << 15),
            (1 << 14) | (1 << 15)
        ));
        // Scope quietness never depends on a per-object flag.
        assert!(quiet_scope_denial(SCOPE_MASK, 1 << 1));
        assert!(!quiet_scope_denial(1, 1 << 1));
        assert!(!quiet_scope_denial(SCOPE_MASK, 0));
    }

    #[test]
    fn unix_resolution_follows_hook_unix_find() {
        // A layer that does not handle RESOLVE_UNIX never denies a lookup.
        assert!(unix_resolution_layer_allows(false, false, false));
        // Handled and granted: allowed even without a shared domain.
        assert!(unix_resolution_layer_allows(true, true, false));
        // Handled, denied, but the peer was created in the same domain.
        assert!(unix_resolution_layer_allows(true, false, true));
        // Handled, denied, other domain: this is the EACCES case.
        assert!(!unix_resolution_layer_allows(true, false, false));
    }
    const BOB: Principal = Principal::new(8);
    const TARGET: RuleTarget = RuleTarget::from_identity(ObjectIdentity::new(42));
    const OTHER: RuleTarget = RuleTarget::from_identity(ObjectIdentity::new(43));

    fn domain() -> Domain<2> {
        let read = Rule::try_new(ALICE, TARGET, Access::one(AccessRight::Read)).unwrap();
        let write = Rule::try_new(ALICE, TARGET, Access::one(AccessRight::Write)).unwrap();
        let rules = Ruleset::empty()
            .try_with_rule(read)
            .unwrap()
            .try_with_rule(write)
            .unwrap();
        Domain::new(DomainAuthority::new(ALICE), rules)
    }

    #[test]
    fn policy_is_monotone_when_rules_are_added() {
        let read = Rule::try_new(ALICE, TARGET, Access::one(AccessRight::Read)).unwrap();
        let write = Rule::try_new(ALICE, TARGET, Access::one(AccessRight::Write)).unwrap();
        let first: Ruleset<2> = Ruleset::empty().try_with_rule(read).unwrap();
        let extended = first.clone().try_with_rule(write).unwrap();
        let snapshot = IdentitySnapshot::new(ALICE, TARGET, 1);
        let request = AccessRequest::new(TARGET, Access::one(AccessRight::Read));
        assert!(
            Domain::new(DomainAuthority::new(ALICE), first)
                .plan(snapshot, request)
                .is_ok()
        );
        assert!(
            Domain::new(DomainAuthority::new(ALICE), extended)
                .plan(snapshot, request)
                .is_ok()
        );
    }

    #[test]
    fn permissions_require_every_requested_right_and_matching_principal() {
        let snapshot = IdentitySnapshot::new(ALICE, TARGET, 2);
        let both = Access::one(AccessRight::Read).union(Access::one(AccessRight::Write));
        assert!(
            domain()
                .plan(snapshot, AccessRequest::new(TARGET, both))
                .is_ok()
        );
        assert!(matches!(
            domain().plan(IdentitySnapshot::new(BOB, TARGET, 2), AccessRequest::new(TARGET, Access::one(AccessRight::Read))),
            Err(Reject::PermissionDenied { allowed, .. }) if allowed == Access::NONE
        ));
    }

    #[test]
    fn snapshot_and_ruleset_capacity_fail_without_partial_admission() {
        let read = Rule::try_new(ALICE, TARGET, Access::one(AccessRight::Read)).unwrap();
        let full: Ruleset<1> = Ruleset::empty().try_with_rule(read).unwrap();
        assert_eq!(
            full.clone().try_with_rule(read),
            Err(RuleError::Capacity { maximum: 1 })
        );
        assert_eq!(full.len(), 1);
        assert_eq!(
            domain().plan(
                IdentitySnapshot::new(ALICE, TARGET, 1),
                AccessRequest::new(OTHER, Access::one(AccessRight::Read))
            ),
            Err(Reject::SnapshotTargetMismatch)
        );
    }

    #[test]
    fn stale_commit_and_rollback_do_not_grant_access() {
        let snapshot = IdentitySnapshot::new(ALICE, TARGET, 10);
        let plan = domain()
            .plan(
                snapshot,
                AccessRequest::new(TARGET, Access::one(AccessRight::Read)),
            )
            .unwrap();
        assert!(matches!(
            plan.prepare().commit(11),
            Err(Reject::StaleSnapshot {
                planned: 10,
                current: 11
            })
        ));
        plan.prepare().rollback();
        let grant = plan.prepare().commit(10).unwrap();
        assert_eq!(
            grant.request(),
            AccessRequest::new(TARGET, Access::one(AccessRight::Read))
        );
    }
}
