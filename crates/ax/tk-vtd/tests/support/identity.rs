// Standalone host-test services. Kernel tests must supply their real implementation.
use tk_vtd::{Error, PciRequester, PlatformDma, PlatformIdentityDma, PlatformInterruptRemap};

struct HostedIdentity;

#[crate_interface::impl_interface]
impl PlatformDma for HostedIdentity {
    fn pci_dma_allowed() -> bool {
        true
    }
    fn map(physical: u64, _length: usize) -> Result<u64, Error> {
        Ok(physical)
    }
    fn unmap(_device_address: u64, _length: usize) -> Result<(), Error> {
        Ok(())
    }
    fn map_for(_requester: PciRequester, physical: u64, _length: usize) -> Result<u64, Error> {
        Ok(physical)
    }
    fn unmap_for(
        _requester: PciRequester,
        _device_address: u64,
        _length: usize,
    ) -> Result<(), Error> {
        Ok(())
    }
}

#[crate_interface::impl_interface]
impl PlatformInterruptRemap for HostedIdentity {
    fn map_msi(
        _requester: PciRequester,
        _vector: u8,
        _destination: u32,
    ) -> Result<Option<(u64, u32)>, Error> {
        Ok(None)
    }
    fn unmap_msi(_vector: u8) -> Result<(), Error> {
        Ok(())
    }
    fn msi_vector(
        _requester: PciRequester,
        _message_address: u64,
        _message_data: u32,
    ) -> Result<Option<u8>, Error> {
        Ok(None)
    }
}

#[crate_interface::impl_interface]
impl PlatformIdentityDma for HostedIdentity {
    fn acquire_identity_dma(
        _requester: PciRequester,
        _initial_pages: &[u64],
    ) -> Result<u64, Error> {
        Err(Error::Unsupported)
    }
    fn map_identity_pages(
        _requester: PciRequester,
        _lease_id: u64,
        _pages: &[u64],
    ) -> Result<u64, Error> {
        Err(Error::Unsupported)
    }
    fn unmap_identity_pages(
        _requester: PciRequester,
        _lease_id: u64,
        _mapping_id: u64,
    ) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    fn release_identity_dma(_requester: PciRequester, _lease_id: u64) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
}
