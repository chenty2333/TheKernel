//! Static composition of Linux semantic components and explicit adapter ports.
//!
//! These declarations describe enabled components, not Linux compatibility
//! grades. A port declaration is an embedding kernel's obligation to provide
//! the corresponding component traits; it is not a runtime provider registry
//! or evidence that its implementation has passed semantic tests.
#![no_std]
#![warn(missing_docs)]

/// A semantic component selected independently of machine/build configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    /// Processes, credentials, signals and ordinary file/memory operations.
    Core,
    /// Asynchronous file operations with retained buffers and completion leases.
    AsyncFileIo,
    /// Socket state and packet delivery.
    Network,
    /// DRM objects and device submission.
    Graphics,
    /// Seccomp policy execution.
    Seccomp,
}

/// Explicit integration contracts required by semantic components.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    /// Address-space-specific user memory access.
    UserMemory,
    /// Process identities and lifecycle publication.
    ProcessDomain,
    /// Monotonic and realtime clocks.
    Clock,
    /// Mapping and retained memory ownership.
    VirtualMemory,
    /// File object mutation and publication.
    VfsMutation,
    /// Readiness registration and delivery.
    Readiness,
    /// Completion delivery and physical resource retirement.
    AsyncRetirement,
    /// Socket transport and packet execution.
    Network,
    /// Display submission and device reset.
    Display,
    /// Filter execution against explicit syscall context.
    SeccompExecutor,
}

impl Capability {
    /// Ports the embedding kernel must bind for this component.
    pub const fn required_ports(self) -> &'static [Port] {
        match self {
            Self::Core => &[
                Port::UserMemory,
                Port::ProcessDomain,
                Port::Clock,
                Port::VirtualMemory,
                Port::VfsMutation,
                Port::Readiness,
            ],
            Self::AsyncFileIo => &[
                Port::UserMemory,
                Port::VirtualMemory,
                Port::VfsMutation,
                Port::Readiness,
                Port::AsyncRetirement,
            ],
            Self::Network => &[Port::Clock, Port::Readiness, Port::Network],
            Self::Graphics => &[Port::VirtualMemory, Port::Readiness, Port::Display],
            Self::Seccomp => &[Port::ProcessDomain, Port::SeccompExecutor],
        }
    }
}

/// Composition limits; per-process Linux rlimits remain separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Maximum resolved submission queue entries in each io_uring.
    pub io_uring_entries: u32,
    /// Maximum registered buffers in each io_uring.
    pub registered_buffers: u32,
}

/// Immutable description supplied by the embedding OS at startup.
#[derive(Clone, Copy, Debug)]
pub struct LinuxProfile {
    /// Selected components; compatibility claims are recorded separately.
    pub capabilities: &'static [Capability],
    /// Resource ceilings used by the integration admission path.
    pub limits: ResourceLimits,
}

/// An invalid or incomplete static composition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindError {
    /// An enabled component lacks a required adapter contract.
    MissingPort(Port),
    /// Async I/O requires nonzero queue and buffer limits.
    InvalidAsyncLimits,
}

/// A validated immutable composition, retained by an embedding World.
#[derive(Clone, Copy, Debug)]
pub struct BoundProfile {
    profile: LinuxProfile,
}

impl LinuxProfile {
    /// Validate a static port declaration before publishing a World.
    pub fn bind(self, ports: &[Port]) -> Result<BoundProfile, BindError> {
        for capability in self.capabilities {
            for port in capability.required_ports() {
                if !ports.contains(port) {
                    return Err(BindError::MissingPort(*port));
                }
            }
        }
        if self.capabilities.contains(&Capability::AsyncFileIo)
            && (self.limits.io_uring_entries == 0 || self.limits.registered_buffers == 0)
        {
            return Err(BindError::InvalidAsyncLimits);
        }
        Ok(BoundProfile { profile: self })
    }
}

impl BoundProfile {
    /// Whether the component is enabled in this composition.
    pub fn enables(&self, capability: Capability) -> bool {
        self.profile.capabilities.contains(&capability)
    }

    /// Immutable composition limits used during resource admission.
    pub const fn limits(&self) -> ResourceLimits {
        self.profile.limits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASYNC: LinuxProfile = LinuxProfile {
        capabilities: &[Capability::AsyncFileIo],
        limits: ResourceLimits {
            io_uring_entries: 64,
            registered_buffers: 8,
        },
    };
    const PORTS: &[Port] = &[
        Port::UserMemory,
        Port::VirtualMemory,
        Port::VfsMutation,
        Port::Readiness,
        Port::AsyncRetirement,
    ];

    #[test]
    fn async_binding_requires_physical_retirement() {
        assert!(matches!(
            ASYNC.bind(&PORTS[..4]),
            Err(BindError::MissingPort(Port::AsyncRetirement))
        ));
    }

    #[test]
    fn independent_adapter_can_bind_without_kernel_runtime() {
        let bound = ASYNC.bind(PORTS).unwrap();
        assert!(bound.enables(Capability::AsyncFileIo));
        assert!(!bound.enables(Capability::Graphics));
        assert_eq!(bound.limits().registered_buffers, 8);
    }

    #[test]
    fn enabled_async_component_requires_resource_budget() {
        let mut profile = ASYNC;
        profile.limits.io_uring_entries = 0;
        assert!(matches!(
            profile.bind(PORTS),
            Err(BindError::InvalidAsyncLimits)
        ));
    }
}
