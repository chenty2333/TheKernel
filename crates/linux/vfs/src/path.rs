/// Linux `openat2()` scoped-resolution flags after strict validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolveFlags(u64);

impl ResolveFlags {
    /// No scoped-resolution restrictions.
    pub const EMPTY: Self = Self(0);
    /// Reject crossing mount points, including bind-style mount aliases.
    pub const NO_XDEV: Self = Self(0x01);
    /// Reject magic-link traversal.
    pub const NO_MAGIC_LINKS: Self = Self(0x02);
    /// Reject every symbolic link, including magic links.
    pub const NO_SYMLINKS: Self = Self(0x04);
    /// Keep lookup beneath its starting directory and reject absolute restarts.
    pub const BENEATH: Self = Self(0x08);
    /// Treat the starting directory as a temporary operation root.
    pub const IN_ROOT: Self = Self(0x10);
    /// Permit only cache-resident, nonblocking resolution.
    pub const CACHED: Self = Self(0x20);

    const ALL_BITS: u64 = Self::NO_XDEV.0
        | Self::NO_MAGIC_LINKS.0
        | Self::NO_SYMLINKS.0
        | Self::BENEATH.0
        | Self::IN_ROOT.0
        | Self::CACHED.0;

    /// Strictly decodes Linux bits, rejecting unknown or incompatible flags.
    pub const fn from_bits(bits: u64) -> Result<Self, ResolveFlagsError> {
        if bits & !Self::ALL_BITS != 0 {
            return Err(ResolveFlagsError::UnknownBits(bits & !Self::ALL_BITS));
        }
        if bits & Self::BENEATH.0 != 0 && bits & Self::IN_ROOT.0 != 0 {
            return Err(ResolveFlagsError::IncompatibleScope);
        }
        Ok(Self(bits))
    }

    /// Returns the validated Linux bit representation.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Returns whether all bits in `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Invalid Linux scoped-resolution flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveFlagsError {
    /// Unknown bits must not be silently accepted.
    UnknownBits(u64),
    /// `RESOLVE_BENEATH` and `RESOLVE_IN_ROOT` cannot be combined.
    IncompatibleScope,
}

/// A topology-sensitive event reported by a generic VFS walker.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum TopologyEvent<'a, L> {
    /// A normal symbolic link is about to be followed.
    FollowSymlink {
        /// Stable link handle.
        link: &'a L,
        /// Whether the link is the final pathname component.
        final_component: bool,
    },
    /// A filesystem-provided magic link is about to be followed.
    FollowMagicLink {
        /// Stable link handle.
        link: &'a L,
        /// Whether the link is the final pathname component.
        final_component: bool,
        /// Whether the walker proved that the target remains on this mount.
        target_stays_on_mount: bool,
    },
    /// Lookup is about to cross a mount topology edge.
    CrossMount {
        /// Source location.
        from: &'a L,
        /// Destination location.
        to: &'a L,
    },
    /// An absolute path or symlink target requests a root restart.
    AbsoluteRestart {
        /// Location from which the restart was requested.
        from: &'a L,
        /// Operation root handle.
        root: &'a L,
    },
    /// A `..` component attempts to move above the operation root.
    EscapeRoot {
        /// Operation root handle.
        root: &'a L,
    },
    /// Cached-only lookup encountered an entry requiring blocking revalidation.
    CacheMiss {
        /// Location whose result requires retry or I/O.
        at: &'a L,
    },
}

/// Instruction returned to the generic walker after a topology event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TraversalAction {
    /// Continue the walk normally.
    Continue,
    /// Restart at the operation root rather than the system root.
    RestartAtOperationRoot,
    /// Clamp `..` at the operation root.
    ClampAtOperationRoot,
}

/// Linux-visible scoped-walk failure before errno mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WalkError {
    /// A scoped lookup would escape or cross a forbidden topology edge.
    CrossDevice,
    /// Symlink or magic-link traversal is forbidden.
    SymbolicLinkLoop,
    /// Cached-only lookup must be retried without `RESOLVE_CACHED`.
    RetryWithoutCached,
    /// A bounded walk resource was exhausted.
    Limit(PathLimitError),
}

/// Validated Linux scoped-resolution policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Openat2Policy {
    flags: ResolveFlags,
}

impl Openat2Policy {
    /// Creates a policy from already validated flags.
    pub const fn new(flags: ResolveFlags) -> Self {
        Self { flags }
    }

    /// Returns the validated flags.
    pub const fn flags(self) -> ResolveFlags {
        self.flags
    }

    /// Authorizes one event from the actual generic VFS walk.
    pub fn authorize<L>(&self, event: TopologyEvent<'_, L>) -> Result<TraversalAction, WalkError> {
        match event {
            TopologyEvent::FollowSymlink { .. } => {
                if self.flags.contains(ResolveFlags::NO_SYMLINKS) {
                    Err(WalkError::SymbolicLinkLoop)
                } else {
                    Ok(TraversalAction::Continue)
                }
            }
            TopologyEvent::FollowMagicLink {
                target_stays_on_mount,
                ..
            } => {
                if self.flags.contains(ResolveFlags::NO_SYMLINKS)
                    || self.flags.contains(ResolveFlags::NO_MAGIC_LINKS)
                {
                    return Err(WalkError::SymbolicLinkLoop);
                }
                if self.flags.contains(ResolveFlags::BENEATH)
                    || self.flags.contains(ResolveFlags::IN_ROOT)
                    || (self.flags.contains(ResolveFlags::NO_XDEV) && !target_stays_on_mount)
                {
                    return Err(WalkError::CrossDevice);
                }
                Ok(TraversalAction::Continue)
            }
            TopologyEvent::CrossMount { .. } => {
                if self.flags.contains(ResolveFlags::NO_XDEV) {
                    Err(WalkError::CrossDevice)
                } else {
                    Ok(TraversalAction::Continue)
                }
            }
            TopologyEvent::AbsoluteRestart { .. } => {
                if self.flags.contains(ResolveFlags::BENEATH) {
                    Err(WalkError::CrossDevice)
                } else if self.flags.contains(ResolveFlags::IN_ROOT) {
                    Ok(TraversalAction::RestartAtOperationRoot)
                } else {
                    Ok(TraversalAction::Continue)
                }
            }
            TopologyEvent::EscapeRoot { .. } => {
                if self.flags.contains(ResolveFlags::BENEATH) {
                    Err(WalkError::CrossDevice)
                } else if self.flags.contains(ResolveFlags::IN_ROOT) {
                    Ok(TraversalAction::ClampAtOperationRoot)
                } else {
                    Ok(TraversalAction::Continue)
                }
            }
            TopologyEvent::CacheMiss { .. } => {
                if self.flags.contains(ResolveFlags::CACHED) {
                    Err(WalkError::RetryWithoutCached)
                } else {
                    Ok(TraversalAction::Continue)
                }
            }
        }
    }
}

/// Resource limits retained by a single pathname walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathLimits {
    /// Maximum total pathname bytes consumed across restarts.
    pub max_path_bytes: usize,
    /// Maximum bytes in one component.
    pub max_component_bytes: usize,
    /// Maximum components consumed across restarts.
    pub max_components: usize,
    /// Maximum symbolic links followed.
    pub max_symlinks: usize,
    /// Maximum absolute/symlink restart operations.
    pub max_restarts: usize,
    /// Maximum mount edges crossed.
    pub max_mount_crossings: usize,
    /// Maximum cache invalidation/revalidation retries.
    pub max_retries: usize,
}

impl PathLimits {
    /// Linux-oriented conservative defaults with an explicit finite budget.
    pub const LINUX_DEFAULT: Self = Self {
        max_path_bytes: 4096,
        max_component_bytes: 255,
        max_components: 4096,
        max_symlinks: 40,
        max_restarts: 40,
        max_mount_crossings: 256,
        max_retries: 64,
    };

    /// Rejects `usize::MAX`, which would make a user-triggered class
    /// effectively unbounded. Zero is valid and explicitly forbids that class.
    pub const fn validate(self) -> Result<Self, LimitKind> {
        if self.max_path_bytes == usize::MAX {
            Err(LimitKind::PathBytes)
        } else if self.max_component_bytes == usize::MAX {
            Err(LimitKind::ComponentBytes)
        } else if self.max_components == usize::MAX {
            Err(LimitKind::Components)
        } else if self.max_symlinks == usize::MAX {
            Err(LimitKind::Symlinks)
        } else if self.max_restarts == usize::MAX {
            Err(LimitKind::Restarts)
        } else if self.max_mount_crossings == usize::MAX {
            Err(LimitKind::MountCrossings)
        } else if self.max_retries == usize::MAX {
            Err(LimitKind::Retries)
        } else {
            Ok(self)
        }
    }
}

impl Default for PathLimits {
    fn default() -> Self {
        Self::LINUX_DEFAULT
    }
}

/// One exhausted walk resource class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitKind {
    /// Total pathname byte budget.
    PathBytes,
    /// Per-component byte budget.
    ComponentBytes,
    /// Component count.
    Components,
    /// Symlink-follow count.
    Symlinks,
    /// Absolute/symlink restart count.
    Restarts,
    /// Mount-crossing count.
    MountCrossings,
    /// Revalidation retry count.
    Retries,
}

/// Details for a rejected bounded-walk operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathLimitError {
    /// Exhausted resource class.
    pub kind: LimitKind,
    /// Attempted cumulative value.
    pub attempted: usize,
    /// Configured upper bound.
    pub limit: usize,
}

/// Mutable accounting retained by exactly one pathname walk.
#[derive(Debug, Clone, Copy)]
pub struct WalkBudget {
    limits: PathLimits,
    path_bytes: usize,
    components: usize,
    symlinks: usize,
    restarts: usize,
    mount_crossings: usize,
    retries: usize,
}

impl WalkBudget {
    /// Creates zeroed accounting after validating its finite limits.
    pub const fn new(limits: PathLimits) -> Result<Self, LimitKind> {
        match limits.validate() {
            Ok(limits) => Ok(Self {
                limits,
                path_bytes: 0,
                components: 0,
                symlinks: 0,
                restarts: 0,
                mount_crossings: 0,
                retries: 0,
            }),
            Err(error) => Err(error),
        }
    }

    fn add(
        current: &mut usize,
        amount: usize,
        limit: usize,
        kind: LimitKind,
    ) -> Result<(), WalkError> {
        let attempted = current
            .checked_add(amount)
            .ok_or(WalkError::Limit(PathLimitError {
                kind,
                attempted: usize::MAX,
                limit,
            }))?;
        if attempted > limit {
            return Err(WalkError::Limit(PathLimitError {
                kind,
                attempted,
                limit,
            }));
        }
        *current = attempted;
        Ok(())
    }

    /// Accounts one real component before lookup.
    pub fn component(&mut self, bytes: usize) -> Result<(), WalkError> {
        if bytes > self.limits.max_component_bytes {
            return Err(WalkError::Limit(PathLimitError {
                kind: LimitKind::ComponentBytes,
                attempted: bytes,
                limit: self.limits.max_component_bytes,
            }));
        }
        let next_path_bytes =
            self.path_bytes
                .checked_add(bytes)
                .ok_or(WalkError::Limit(PathLimitError {
                    kind: LimitKind::PathBytes,
                    attempted: usize::MAX,
                    limit: self.limits.max_path_bytes,
                }))?;
        if next_path_bytes > self.limits.max_path_bytes {
            return Err(WalkError::Limit(PathLimitError {
                kind: LimitKind::PathBytes,
                attempted: next_path_bytes,
                limit: self.limits.max_path_bytes,
            }));
        }
        let next_components =
            self.components
                .checked_add(1)
                .ok_or(WalkError::Limit(PathLimitError {
                    kind: LimitKind::Components,
                    attempted: usize::MAX,
                    limit: self.limits.max_components,
                }))?;
        if next_components > self.limits.max_components {
            return Err(WalkError::Limit(PathLimitError {
                kind: LimitKind::Components,
                attempted: next_components,
                limit: self.limits.max_components,
            }));
        }
        self.path_bytes = next_path_bytes;
        self.components = next_components;
        Ok(())
    }

    /// Accounts one followed symlink.
    pub fn symlink(&mut self) -> Result<(), WalkError> {
        Self::add(
            &mut self.symlinks,
            1,
            self.limits.max_symlinks,
            LimitKind::Symlinks,
        )
    }

    /// Accounts one absolute or symlink-target restart.
    pub fn restart(&mut self) -> Result<(), WalkError> {
        Self::add(
            &mut self.restarts,
            1,
            self.limits.max_restarts,
            LimitKind::Restarts,
        )
    }

    /// Accounts one mount topology edge.
    pub fn mount_crossing(&mut self) -> Result<(), WalkError> {
        Self::add(
            &mut self.mount_crossings,
            1,
            self.limits.max_mount_crossings,
            LimitKind::MountCrossings,
        )
    }

    /// Accounts one revalidation retry.
    pub fn retry(&mut self) -> Result<(), WalkError> {
        Self::add(
            &mut self.retries,
            1,
            self.limits.max_retries,
            LimitKind::Retries,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(bits: u64) -> Openat2Policy {
        Openat2Policy::new(ResolveFlags::from_bits(bits).unwrap())
    }

    #[test]
    fn flags_reject_unknown_and_incompatible_scope() {
        assert_eq!(
            ResolveFlags::from_bits(0x40),
            Err(ResolveFlagsError::UnknownBits(0x40))
        );
        assert_eq!(
            ResolveFlags::from_bits(ResolveFlags::BENEATH.bits() | ResolveFlags::IN_ROOT.bits()),
            Err(ResolveFlagsError::IncompatibleScope)
        );
    }

    #[test]
    fn scope_policy_returns_walker_actions_without_escaping() {
        let loc = 1;
        assert_eq!(
            policy(ResolveFlags::BENEATH.bits()).authorize(TopologyEvent::AbsoluteRestart {
                from: &loc,
                root: &loc,
            }),
            Err(WalkError::CrossDevice)
        );
        assert_eq!(
            policy(ResolveFlags::IN_ROOT.bits()).authorize(TopologyEvent::AbsoluteRestart {
                from: &loc,
                root: &loc,
            }),
            Ok(TraversalAction::RestartAtOperationRoot)
        );
        assert_eq!(
            policy(ResolveFlags::IN_ROOT.bits())
                .authorize(TopologyEvent::EscapeRoot { root: &loc }),
            Ok(TraversalAction::ClampAtOperationRoot)
        );
    }

    #[test]
    fn no_xdev_and_cached_fail_honestly() {
        let loc = 1;
        assert_eq!(
            policy(ResolveFlags::NO_XDEV.bits()).authorize(TopologyEvent::CrossMount {
                from: &loc,
                to: &loc,
            }),
            Err(WalkError::CrossDevice)
        );
        assert_eq!(
            policy(ResolveFlags::CACHED.bits()).authorize(TopologyEvent::CacheMiss { at: &loc }),
            Err(WalkError::RetryWithoutCached)
        );
    }

    #[test]
    fn magic_link_restrictions_are_distinct() {
        let loc = 1;
        assert_eq!(
            policy(ResolveFlags::NO_MAGIC_LINKS.bits()).authorize(TopologyEvent::FollowMagicLink {
                link: &loc,
                final_component: true,
                target_stays_on_mount: true,
            },),
            Err(WalkError::SymbolicLinkLoop)
        );
        assert_eq!(
            policy(ResolveFlags::NO_MAGIC_LINKS.bits()).authorize(TopologyEvent::FollowSymlink {
                link: &loc,
                final_component: true,
            }),
            Ok(TraversalAction::Continue)
        );
    }

    #[test]
    fn budget_is_bounded_and_does_not_wrap() {
        let limits = PathLimits {
            max_path_bytes: 4,
            max_component_bytes: 3,
            max_components: 2,
            max_symlinks: 1,
            max_restarts: 1,
            max_mount_crossings: 1,
            max_retries: 1,
        };
        let mut budget = WalkBudget::new(limits).unwrap();
        budget.component(2).unwrap();
        budget.component(2).unwrap();
        assert_eq!(
            budget.component(1),
            Err(WalkError::Limit(PathLimitError {
                kind: LimitKind::PathBytes,
                attempted: 5,
                limit: 4,
            }))
        );
        budget.symlink().unwrap();
        assert!(matches!(budget.symlink(), Err(WalkError::Limit(_))));
    }

    #[test]
    fn failed_component_admission_does_not_consume_another_budget() {
        let limits = PathLimits {
            max_path_bytes: 8,
            max_component_bytes: 8,
            max_components: 1,
            max_symlinks: 0,
            max_restarts: 0,
            max_mount_crossings: 0,
            max_retries: 0,
        };
        let mut budget = WalkBudget::new(limits).unwrap();
        budget.component(2).unwrap();
        assert!(matches!(
            budget.component(3),
            Err(WalkError::Limit(PathLimitError {
                kind: LimitKind::Components,
                ..
            }))
        ));
        assert_eq!(budget.path_bytes, 2);
    }

    #[test]
    fn zero_is_explicit_denial_but_max_is_rejected_as_unbounded() {
        let zero = PathLimits {
            max_path_bytes: 0,
            max_component_bytes: 0,
            max_components: 0,
            max_symlinks: 0,
            max_restarts: 0,
            max_mount_crossings: 0,
            max_retries: 0,
        };
        assert!(WalkBudget::new(zero).is_ok());
        let mut unbounded = zero;
        unbounded.max_retries = usize::MAX;
        assert!(matches!(
            WalkBudget::new(unbounded),
            Err(LimitKind::Retries)
        ));
    }
}
