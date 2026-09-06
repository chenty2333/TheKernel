//! Immutable identity of the boot resource domain.
//!
//! Identity travels with the existing process and I/O owners. It does not
//! select providers or create a second resource-lifetime registry.

use spin::Lazy;
use thekernel_linux_profile::{BoundProfile, Capability, LinuxProfile, Port, ResourceLimits};

/// The kernel owns the immutable composition; Linux namespaces and existing
/// per-process accounts continue to own their own registries and budgets.
pub(crate) struct WorldContext {
    identity: WorldId,
    profile: BoundProfile,
}

static BOOT_WORLD: Lazy<WorldContext> = Lazy::new(|| {
    let profile = LinuxProfile {
        capabilities: &[
            Capability::Core,
            Capability::AsyncFileIo,
            Capability::Network,
            Capability::Graphics,
            Capability::Seccomp,
        ],
        limits: ResourceLimits {
            io_uring_entries: thekernel_linux_io_uring::IORING_MAX_ENTRIES,
            registered_buffers: thekernel_linux_io_uring::IORING_MAX_REGISTERED_BUFFERS,
        },
    }
    .bind(&[
        Port::UserMemory,
        Port::ProcessDomain,
        Port::Clock,
        Port::VirtualMemory,
        Port::VfsMutation,
        Port::Readiness,
        Port::AsyncRetirement,
        Port::Network,
        Port::Display,
        Port::SeccompExecutor,
    ])
    .expect("boot Linux profile requires all statically linked adapter ports");
    WorldContext { identity: WorldId::BOOT, profile }
});

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorldId(u64);

impl WorldId {
    /// The boot domain has a stable identity across fork and exec.
    pub(crate) const BOOT: Self = Self(1);

    #[cfg(test)]
    pub(crate) const FOREIGN_TEST: Self = Self(2);

    pub(crate) fn admits(self, owner: Self) -> bool {
        self == owner
    }

    /// Only the boot World can resolve its static composition. Identity is
    /// never interpreted as an authority to access another process's objects.
    pub(crate) fn profile(self) -> &'static BoundProfile {
        assert_eq!(self, BOOT_WORLD.identity, "unknown static World binding");
        &BOOT_WORLD.profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_world_does_not_admit_boot_resources() {
        assert!(WorldId::BOOT.admits(WorldId::BOOT));
        assert!(!WorldId(2).admits(WorldId::BOOT));
        assert!(!WorldId::BOOT.admits(WorldId(2)));
    }

    #[test]
    fn boot_composition_retains_linux_async_limits() {
        let profile = WorldId::BOOT.profile();
        assert!(profile.enables(Capability::AsyncFileIo));
        assert_eq!(profile.limits().io_uring_entries,
            thekernel_linux_io_uring::IORING_MAX_ENTRIES);
    }
}
