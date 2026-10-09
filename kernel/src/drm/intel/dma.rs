// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Intel GPU DMA identity-lease admission and checked firmware reads.
//!
//! N305's native display and GT paths keep physical GGTT/PPGTT addresses, so
//! they may run under VT-d only after an exact, requester-scoped identity map
//! has been installed and invalidated. Firmware scanout pages are admitted by
//! `fastboot::ownership` before this module asks the VT-d owner to switch the
//! GPU requester context. Kernel-owned pages are added through the returned
//! lease only while pinned and are unmapped after hardware retirement.
#[cfg(target_os = "none")]
use alloc::vec::Vec;

use intel_display::Error;

fn gpu_requester(bdf: super::pci::Bdf) -> Result<tk_vtd::PciRequester, Error> {
    if (bdf.bus, bdf.device, bdf.function) != (0, 2, 0) {
        return Err(Error::Refused);
    }
    Ok(tk_vtd::PciRequester {
        segment: 0,
        bus: bdf.bus,
        device: bdf.device,
        function: bdf.function,
    })
}

#[cfg(target_os = "none")]
pub(super) fn requester(bdf: super::pci::Bdf) -> Result<tk_vtd::PciRequester, Error> {
    gpu_requester(bdf)
}

#[cfg(target_os = "none")]
pub(super) fn acquire_identity_lease(
    bdf: super::pci::Bdf,
    initial_pages: &[u64],
) -> Result<tk_vtd::IdentityDmaLease, Error> {
    let requester = requester(bdf)?;
    tk_vtd::platform_acquire_identity_dma(requester, initial_pages).map_err(|_| Error::Refused)
}

#[cfg(target_os = "none")]
pub(super) fn lease_matches(lease: &tk_vtd::IdentityDmaLease, bdf: super::pci::Bdf) -> bool {
    requester(bdf).is_ok_and(|requester| lease.requester() == requester)
}

#[cfg(target_os = "none")]
pub(super) fn firmware_bytes(physical: u64, len: usize) -> Result<Vec<u8>, Error> {
    // Observed firmware pointers only, bounded before dereference, no alias of
    // usable RAM (including kernel/modules). Boot API doesn't retain all NVS.
    use axhal::mem::{PhysAddr, phys_ram_ranges};
    let end = physical.checked_add(len as u64).ok_or(Error::Refused)?;
    if physical < 0x1000
        || end > 1 << 52
        || len > 1 << 20
        || phys_ram_ranges().iter().any(|&(base, size)| {
            physical < (base as u64).saturating_add(size as u64) && end > base as u64
        })
    {
        return Err(Error::Refused);
    }
    let mut data = Vec::new();
    data.try_reserve_exact(len).map_err(|_| Error::Refused)?;
    data.resize(len, 0);
    let physical = usize::try_from(physical).map_err(|_| Error::Refused)?;
    let mapping = axmm::iomap(PhysAddr::from_usize(physical), len).map_err(|_| Error::Refused)?;
    for (i, b) in data.iter_mut().enumerate() {
        // SAFETY: checked extent at observed firmware pointer; live UC mapping.
        *b = unsafe { mapping.as_ptr().add(i).read_volatile() };
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_measured_n305_integrated_gpu_is_a_dma_lease_requester() {
        let gpu = super::super::pci::Bdf::new(0, 2, 0);
        assert_eq!(
            gpu_requester(gpu),
            Ok(tk_vtd::PciRequester {
                segment: 0,
                bus: 0,
                device: 2,
                function: 0,
            })
        );
        for other in [
            super::super::pci::Bdf::new(0, 0, 0),
            super::super::pci::Bdf::new(0, 2, 1),
            super::super::pci::Bdf::new(1, 2, 0),
        ] {
            assert_eq!(gpu_requester(other), Err(Error::Refused));
        }
    }
}
