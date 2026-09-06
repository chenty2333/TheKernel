use crate::{LimitKind, Openat2Policy, PathLimits, ResolveFlags, ResolveFlagsError};

/// Invalid input rejected before a pathname operation begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathContextError {
    /// Linux scoped-resolution flags are unknown or incompatible.
    Resolve(ResolveFlagsError),
    /// A resource class was configured as effectively unbounded.
    Limits(LimitKind),
}

/// Immutable inputs retained for one complete pathname operation.
///
/// `C` is normally an immutable filesystem-credential snapshot, `N` a mount
/// namespace handle, `L` a strong location handle, and `H` a typed security
/// hook context. None of them is resampled while the operation is in flight.
#[derive(Debug)]
pub struct PathContext<C, N, L, H> {
    credentials: C,
    mount_namespace: N,
    root: L,
    cwd: L,
    resolve: Openat2Policy,
    security_hooks: H,
    limits: PathLimits,
}

impl<C, N, L, H> PathContext<C, N, L, H> {
    /// Creates an operation context after validating Linux `openat2` flags.
    pub fn new(
        credentials: C,
        mount_namespace: N,
        root: L,
        cwd: L,
        resolve_bits: u64,
        security_hooks: H,
        limits: PathLimits,
    ) -> Result<Self, PathContextError> {
        let resolve = ResolveFlags::from_bits(resolve_bits).map_err(PathContextError::Resolve)?;
        let limits = limits.validate().map_err(PathContextError::Limits)?;
        Ok(Self {
            credentials,
            mount_namespace,
            root,
            cwd,
            resolve: Openat2Policy::new(resolve),
            security_hooks,
            limits,
        })
    }

    /// Returns the credential snapshot for this operation.
    pub const fn credentials(&self) -> &C {
        &self.credentials
    }

    /// Returns the mount namespace snapshot for this operation.
    pub const fn mount_namespace(&self) -> &N {
        &self.mount_namespace
    }

    /// Returns the operation root handle.
    pub const fn root(&self) -> &L {
        &self.root
    }

    /// Returns the current-directory or resolved-dirfd start handle.
    pub const fn cwd(&self) -> &L {
        &self.cwd
    }

    /// Returns the validated scoped-lookup policy.
    pub const fn resolve_policy(&self) -> &Openat2Policy {
        &self.resolve
    }

    /// Returns the explicitly supplied typed security-hook context.
    pub const fn security_hooks(&self) -> &H {
        &self.security_hooks
    }

    /// Returns the resource limits for this walk.
    pub const fn limits(&self) -> PathLimits {
        self.limits
    }

    /// Decomposes the context without dropping any retained snapshot.
    pub fn into_parts(self) -> (C, N, L, L, Openat2Policy, H, PathLimits) {
        (
            self.credentials,
            self.mount_namespace,
            self.root,
            self.cwd,
            self.resolve,
            self.security_hooks,
            self.limits,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_retains_every_explicit_snapshot() {
        let context = PathContext::new(
            11u32,
            22u32,
            33u32,
            44u32,
            ResolveFlags::IN_ROOT.bits(),
            55u32,
            PathLimits::LINUX_DEFAULT,
        )
        .unwrap();
        assert_eq!(*context.credentials(), 11);
        assert_eq!(*context.mount_namespace(), 22);
        assert_eq!(*context.root(), 33);
        assert_eq!(*context.cwd(), 44);
        assert_eq!(*context.security_hooks(), 55);
        assert!(
            context
                .resolve_policy()
                .flags()
                .contains(ResolveFlags::IN_ROOT)
        );
    }

    #[test]
    fn context_rejects_invalid_flags_and_unbounded_limits() {
        assert!(matches!(
            PathContext::new((), (), (), (), 0x40, (), PathLimits::LINUX_DEFAULT),
            Err(PathContextError::Resolve(ResolveFlagsError::UnknownBits(
                0x40
            )))
        ));

        let mut limits = PathLimits::LINUX_DEFAULT;
        limits.max_mount_crossings = usize::MAX;
        assert!(matches!(
            PathContext::new((), (), (), (), 0, (), limits),
            Err(PathContextError::Limits(LimitKind::MountCrossings))
        ));
    }
}
