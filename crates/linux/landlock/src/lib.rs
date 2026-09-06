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
/// userspace value and map their resolver-owned objects separately.
pub const FS_ACCESS_MASK: u64 = 0xffff;
/// Filesystem rights which may be attached to a non-directory rule target.
pub const NON_DIRECTORY_FS_ACCESS_MASK: u64 =
    (1 << 0) | (1 << 1) | (1 << 2) | (1 << 14) | (1 << 15);

/// Typed result of path-rule admission validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathRuleReject {
    /// A rule may not have an empty access mask.
    EmptyAccess,
    /// The rule requests rights not handled by its ruleset.
    UnhandledAccess,
    /// A non-directory target received a directory-only right.
    NonDirectoryAccess,
}

/// Validates a raw Linux path-beneath rule without resolving its target.
///
/// Descriptor lookup and the opaque target mapping deliberately remain with
/// the caller, preserving its usercopy and VFS validation order.
pub const fn admit_path_rule(
    ruleset_handled: u64,
    allowed: u64,
    target_is_directory: bool,
) -> Result<(), PathRuleReject> {
    match admit_path_rule_access(ruleset_handled, allowed) {
        Ok(()) => {}
        Err(error) => return Err(error),
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
) -> Result<(), PathRuleReject> {
    if allowed == 0 {
        return Err(PathRuleReject::EmptyAccess);
    }
    if allowed & !ruleset_handled != 0 {
        return Err(PathRuleReject::UnhandledAccess);
    }
    Ok(())
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
            admit_path_rule_access(0b11, 0b100),
            Err(PathRuleReject::UnhandledAccess)
        );
        assert_eq!(
            admit_path_rule(0xffff, 1 << 7, false),
            Err(PathRuleReject::NonDirectoryAccess)
        );
        assert!(allows_path_access(0b11, 0b11, [0b01, 0b10].into_iter()));
        assert!(!allows_path_access(0b11, 0b11, [0b01].into_iter()));
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
