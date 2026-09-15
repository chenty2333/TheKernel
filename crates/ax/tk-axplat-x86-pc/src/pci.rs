//! Runtime PCI platform facts.
//!
//! The kernel used to hardcode its memory-mapped configuration (ECAM) base in
//! the platform configuration file.  That value is a *machine* fact, so it now
//! comes from the firmware's ACPI MCFG table when one is present, and from
//! `[devices] pci-ecam-base` only as a documented fallback.
//!
//! Discovery happens once, in the primary CPU's early initialization, while
//! firmware memory is still mapped (see [`crate::acpi`]).  Everything here is
//! therefore a cheap read of an already-decided value: callers such as the PCI
//! bus driver must not repeat the ACPI walk, and cannot — the tables are gone
//! from the runtime direct map by the time drivers initialize.
//!
//! This module is the platform's *public* ECAM surface, so it also has to
//! build for hosted tests, where `axconfig` describes the host rather than a
//! PCI machine.  The host branch lives in `axhal` and reports the configured
//! fallback and nothing else; no host test may conclude that discovery
//! happened.

pub use crate::acpi::McfgStatus;

/// Physical base address of the memory-mapped PCI configuration window the
/// kernel uses.
///
/// Reports the configured `[devices] pci-ecam-base` until early initialization
/// has published a discovery.
pub fn ecam_base() -> usize {
    crate::acpi::pci_ecam_base()
}

/// Whether [`ecam_base`] came from the MCFG table rather than configuration.
///
/// Reported as a bool, not as the internal [`EcamSource`] enum, because the
/// `axhal` re-export of this module must also compile for hosted tests, where
/// the platform crate is not linked and its type cannot be named.
pub fn ecam_discovered() -> bool {
    crate::acpi::ecam_source().discovered()
}

/// PCI segment group of the window [`ecam_base`] describes.
pub fn ecam_segment() -> u16 {
    crate::acpi::pci_ecam_segment()
}

/// Inclusive bus range of the window [`ecam_base`] describes.
pub fn ecam_bus_range() -> (u8, u8) {
    crate::acpi::pci_ecam_bus_range()
}
