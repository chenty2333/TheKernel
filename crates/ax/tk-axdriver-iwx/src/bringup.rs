//! Firmware load and MVM init ordering from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

use crate::{DeviceFamily, DmaAllocator, DmaRegion, FirmwareDmaImages};

pub const FIRMWARE_ALIVE_TIMEOUT_NS: u64 = 1_000_000_000;

/// Error path for loading firmware sections and waiting for ALIVE.
#[derive(Debug, PartialEq, Eq)]
pub enum FirmwareLoadError<E> {
    Context(E),
    Wait(E),
    FirmwareNotAlive,
}

/// Set uCode state, publish context-info and retire staging memory after ALIVE.
// upstream: if_iwx.c iwx_load_firmware()
pub fn load_firmware<A, E>(
    family: DeviceFamily,
    firmware: &mut FirmwareDmaImages<A::Region>,
    iml_dma: &mut Option<A::Region>,
    mut initialize_context: impl FnMut(DeviceFamily, &mut FirmwareDmaImages<A::Region>) -> Result<(), E>,
    mut wait_for_alive: impl FnMut(u64) -> Result<bool, E>,
) -> Result<(), FirmwareLoadError<E>>
where
    A: DmaAllocator,
    A::Region: DmaRegion,
{
    initialize_context(family, firmware).map_err(FirmwareLoadError::Context)?;
    let alive = wait_for_alive(FIRMWARE_ALIVE_TIMEOUT_NS);
    if !matches!(alive, Ok(true)) {
        firmware.free_paging();
    }
    iml_dma.take();
    firmware.free_firmware_sections();
    match alive {
        Err(error) => Err(FirmwareLoadError::Wait(error)),
        Ok(false) => Err(FirmwareLoadError::FirmwareNotAlive),
        Ok(true) => Ok(()),
    }
}

/// Error origin in the read/start/PNVM/ALIVE/post-alive sequence.
#[derive(Debug, PartialEq, Eq)]
pub enum UcodeStartError<E> {
    ReadFirmware(E),
    StartFirmware(E),
    LoadPnvm(E),
}

/// Read uCode, start it, load Gen3 PNVM, and run post-ALIVE setup in order.
// upstream: if_iwx.c iwx_load_ucode_wait_alive()
pub fn load_ucode_wait_alive<E>(
    family: DeviceFamily,
    mut read_firmware: impl FnMut() -> Result<(), E>,
    mut start_firmware: impl FnMut() -> Result<(), E>,
    mut load_pnvm: impl FnMut() -> Result<(), E>,
    mut post_alive: impl FnMut(),
) -> Result<(), UcodeStartError<E>> {
    read_firmware().map_err(UcodeStartError::ReadFirmware)?;
    start_firmware().map_err(UcodeStartError::StartFirmware)?;
    if family >= DeviceFamily::Ax210 {
        load_pnvm().map_err(UcodeStartError::LoadPnvm)?;
    }
    post_alive();
    Ok(())
}

/// State passed from runtime-init firmware NVM processing to the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitFirmwareState {
    pub use_mld_api: bool,
    pub init_complete: u32,
}

/// Run Init MVM, request NVM access completion, and gate MLD API enablement.
// upstream: if_iwx.c iwx_run_init_mvm_ucode()
pub fn run_init_mvm<E>(
    family: DeviceFamily,
    mac_type: u16,
    rfkill: bool,
    read_nvm: bool,
    mld_capability: bool,
    mut load_init: impl FnMut() -> Result<(), E>,
    mut send_init_config: impl FnMut() -> Result<(), E>,
    mut send_nvm_complete: impl FnMut() -> Result<(), E>,
    mut wait_init_complete: impl FnMut(u64) -> Result<(), E>,
    mut read_nvm_data: impl FnMut() -> Result<(), E>,
) -> Result<InitFirmwareState, InitFirmwareError<E>> {
    if rfkill && !read_nvm {
        return Err(InitFirmwareError::RadioDisabled);
    }
    load_init().map_err(InitFirmwareError::Load)?;
    send_init_config().map_err(InitFirmwareError::InitConfig)?;
    send_nvm_complete().map_err(InitFirmwareError::NvmComplete)?;
    wait_init_complete(2_000_000_000).map_err(InitFirmwareError::Wait)?;
    if read_nvm {
        read_nvm_data().map_err(InitFirmwareError::NvmRead)?;
    }
    let use_mld_api = mld_capability && (mac_type == 0x44 || family == DeviceFamily::Bz);
    Ok(InitFirmwareState {
        use_mld_api,
        init_complete: 1,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitFirmwareError<E> {
    RadioDisabled,
    Load(E),
    InitConfig(E),
    NvmComplete(E),
    Wait(E),
    NvmRead(E),
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::cell::{Cell, RefCell};

    use super::*;
    use crate::{DmaError, DmaRegion};

    struct Region {
        address: u64,
        bytes: Vec<u8>,
    }
    impl DmaRegion for Region {
        fn device_address(&self) -> u64 {
            self.address
        }
        fn capacity(&self) -> usize {
            self.bytes.len()
        }
        fn write(&mut self, bytes: &[u8]) -> Result<(), DmaError> {
            self.bytes.copy_from_slice(bytes);
            Ok(())
        }
    }
    struct Allocator(Cell<u64>);
    impl DmaAllocator for Allocator {
        type Region = Region;
        fn allocate(&mut self, size: usize) -> Result<Region, DmaError> {
            let address = self.0.get();
            self.0.set(address + 0x1000);
            Ok(Region {
                address,
                bytes: alloc::vec![0; size],
            })
        }
    }

    fn test_firmware() -> FirmwareDmaImages<Region> {
        FirmwareDmaImages {
            lmac: vec![Region {
                address: 1,
                bytes: vec![1],
            }],
            umac: vec![Region {
                address: 2,
                bytes: vec![2],
            }],
            paging: vec![Region {
                address: 3,
                bytes: vec![3],
            }],
            lmac_addresses: vec![1],
            umac_addresses: vec![2],
            paging_addresses: vec![3],
        }
    }

    #[test]
    fn load_firmware_cleans_sections_after_alive_and_paging_on_failure() {
        let mut firmware = test_firmware();
        let mut allocator = Allocator(Cell::new(0x1000));
        let mut iml = Some(allocator.allocate(4).unwrap());
        let mut timeout = None;
        load_firmware::<Allocator, ()>(
            DeviceFamily::Ax210,
            &mut firmware,
            &mut iml,
            |family, images| {
                assert_eq!(family, DeviceFamily::Ax210);
                assert_eq!(images.lmac_addresses, [1]);
                Ok(())
            },
            |ns| {
                timeout = Some(ns);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(timeout, Some(FIRMWARE_ALIVE_TIMEOUT_NS));
        assert!(iml.is_none());
        assert!(firmware.lmac.is_empty());
        assert_eq!(firmware.paging.len(), 1);

        let mut firmware = test_firmware();
        let mut iml = None;
        assert_eq!(
            load_firmware::<Allocator, ()>(
                DeviceFamily::Ax210,
                &mut firmware,
                &mut iml,
                |_, _| Ok(()),
                |_| Ok(false),
            ),
            Err(FirmwareLoadError::FirmwareNotAlive)
        );
        assert!(firmware.paging.is_empty());
        assert!(firmware.lmac.is_empty());
    }

    #[test]
    fn ucode_start_orders_optional_pnvm_before_post_alive() {
        let events = RefCell::new(Vec::new());
        load_ucode_wait_alive(
            DeviceFamily::Ax210,
            || {
                events.borrow_mut().push(1);
                Ok::<(), ()>(())
            },
            || {
                events.borrow_mut().push(2);
                Ok(())
            },
            || {
                events.borrow_mut().push(3);
                Ok(())
            },
            || events.borrow_mut().push(4),
        )
        .unwrap();
        assert_eq!(*events.borrow(), [1, 2, 3, 4]);

        events.borrow_mut().clear();
        load_ucode_wait_alive(
            DeviceFamily::Family22000,
            || {
                events.borrow_mut().push(1);
                Ok::<(), ()>(())
            },
            || {
                events.borrow_mut().push(2);
                Ok(())
            },
            || {
                events.borrow_mut().push(3);
                Ok(())
            },
            || events.borrow_mut().push(4),
        )
        .unwrap();
        assert_eq!(*events.borrow(), [1, 2, 4]);
    }

    #[test]
    fn init_ucode_keeps_rfkill_readnvm_exception_and_mld_gate() {
        let mut reads = 0;
        let state = run_init_mvm::<()>(
            DeviceFamily::Bz,
            0x37,
            true,
            true,
            true,
            || Ok(()),
            || Ok(()),
            || Ok(()),
            |timeout| {
                assert_eq!(timeout, 2_000_000_000);
                Ok(())
            },
            || {
                reads += 1;
                Ok(())
            },
        )
        .unwrap();
        assert!(state.use_mld_api);
        assert_eq!(reads, 1);

        let error = run_init_mvm::<()>(
            DeviceFamily::Ax210,
            0x37,
            true,
            false,
            true,
            || Ok(()),
            || Ok(()),
            || Ok(()),
            |_| Ok(()),
            || Ok(()),
        );
        assert_eq!(error, Err(InitFirmwareError::RadioDisabled));
    }
}
