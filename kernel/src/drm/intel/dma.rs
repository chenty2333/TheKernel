// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Intel GPU DMA identity-lease admission.
//!
//! The i915 port keeps physical GGTT addresses, so it relies on the shared boot
//! identity aperture; this lease does not switch the GPU context or provide
//! requester isolation. The firmware console pages the GGTT reservation maps
//! are the ones admitted here ([`super::initial_plane`]).

use super::{Error, pci::Bdf, traced_refusal};

/// The only requester the lease is taken for: the N305 integrated GPU at
/// 00:02.0. Any other function is refused.
fn gpu_requester(bdf: Bdf) -> Result<tk_vtd::PciRequester, Error> {
    if (bdf.bus, bdf.device, bdf.function) != (0, 2, 0) {
        return Err(traced_refusal());
    }
    Ok(tk_vtd::PciRequester {
        segment: 0,
        bus: bdf.bus,
        device: bdf.device,
        function: bdf.function,
    })
}

#[cfg(target_os = "none")]
pub(super) fn requester(bdf: Bdf) -> Result<tk_vtd::PciRequester, Error> {
    gpu_requester(bdf)
}

/// Take the identity DMA lease for the GPU, covering the firmware console's
/// stolen pages.
#[cfg(target_os = "none")]
pub(super) fn acquire_identity_lease(
    bdf: Bdf,
    initial_pages: &[u64],
) -> Result<tk_vtd::IdentityDmaLease, Error> {
    let requester = requester(bdf)?;
    tk_vtd::platform_acquire_identity_dma(requester, initial_pages).map_err(|_| traced_refusal())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_measured_n305_integrated_gpu_is_a_dma_lease_requester() {
        let gpu = Bdf::new(0, 2, 0);
        assert_eq!(
            gpu_requester(gpu),
            Ok(tk_vtd::PciRequester {
                segment: 0,
                bus: 0,
                device: 2,
                function: 0,
            })
        );
        for other in [Bdf::new(0, 0, 0), Bdf::new(0, 2, 1), Bdf::new(1, 2, 0)] {
            assert_eq!(gpu_requester(other), Err(Error::Refused));
        }
    }
}
