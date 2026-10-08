//! PCI match and early attach boundary for Intel iwx.
//!
//! The target hardware path is continued in `tk-axdriver-iwx`; this module
//! wires the upstream PCI match decision into TheKernel's PCI driver walk.

use axdriver_iwx::{
    AX211_DEVICE_ID, INTEL_VENDOR_ID, RuntimeConfig, lookup_config, matches_pci_device,
};
use axdriver_pci::{BarInfo, DeviceFunction, DeviceFunctionInfo, PciRoot};
use axhal::mem::phys_to_virt;

use crate::drivers::BusProbeResult;

const CSR_HW_REV: usize = 0x028;
const CSR_HW_RF_ID: usize = 0x09c;
const CSR_HW_RFID_TYPE_MASK: u32 = 0x0fff_000;
const CSR_HW_RFID_TYPE_SHIFT: u32 = 12;
const RF_TYPE_GF: u16 = 0x10d;
const SPECIAL_BZ_DEVICE: u16 = 0x7740;

fn pci_match_decision(vendor_id: u16, device_id: u16, rf_id: Option<u32>) -> bool {
    if !matches_pci_device(vendor_id, device_id) {
        return false;
    }
    if device_id != SPECIAL_BZ_DEVICE {
        return true;
    }
    rf_id.is_some_and(|raw| {
        ((raw & CSR_HW_RFID_TYPE_MASK) >> CSR_HW_RFID_TYPE_SHIFT) as u16 == RF_TYPE_GF
    })
}

/// Apply `iwx_match()` to one PCI function, including the BZ/GF runtime RF check.
pub(crate) fn probe(
    root: &mut PciRoot,
    bdf: DeviceFunction,
    info: &DeviceFunctionInfo,
) -> BusProbeResult {
    if !matches_pci_device(info.vendor_id, info.device_id) {
        return BusProbeResult::NotMatched;
    }

    let bar = match root.bar_info(bdf, 0) {
        Ok(BarInfo::Memory { address, size, .. })
            if usize::try_from(size).is_ok_and(|bytes| bytes >= CSR_HW_RF_ID + 4) =>
        {
            (address, size)
        }
        Ok(BarInfo::IO { .. }) | Err(_) => {
            if info.device_id == SPECIAL_BZ_DEVICE {
                return BusProbeResult::NotMatched;
            }
            warn!("iwx: {bdf}: matched PCI ID but BAR0 cannot expose the CSR aperture");
            return BusProbeResult::Claimed;
        }
        Ok(BarInfo::Memory { .. }) => {
            warn!("iwx: {bdf}: matched PCI ID but BAR0 is too short for RF identification");
            return BusProbeResult::Claimed;
        }
    };

    let address = match usize::try_from(bar.0) {
        Ok(address) if address != 0 => address,
        _ => {
            warn!(
                "iwx: {bdf}: BAR0 address {:#x} is not CPU-addressable",
                bar.0
            );
            return BusProbeResult::Claimed;
        }
    };
    // SAFETY: the PCI walk mapped assigned memory BARs into the platform
    // direct map before calling DriverProbe; range is bounded by `bar.1`.
    let base = phys_to_virt(address.into()).as_usize();
    let read_register = |offset: usize| -> u32 {
        if offset & 3 != 0 || offset.checked_add(4).is_none_or(|end| end > bar.1 as usize) {
            u32::MAX
        } else {
            // SAFETY: validated dword inside the mapped device BAR.
            unsafe { ((base + offset) as *const u32).read_volatile() }
        }
    };
    if info.device_id == SPECIAL_BZ_DEVICE {
        let rf_id = read_register(CSR_HW_RF_ID);
        if !pci_match_decision(info.vendor_id, info.device_id, Some(rf_id)) {
            return BusProbeResult::NotMatched;
        }
    }

    let subsystem = root.endpoint_subsystem_ids(bdf).1;
    let hardware_revision = read_register(CSR_HW_REV);
    let rf_id = read_register(CSR_HW_RF_ID);
    if hardware_revision == u32::MAX || rf_id == u32::MAX {
        warn!("iwx: {bdf}: PCI ID matched but hardware identity CSRs are unavailable");
        return BusProbeResult::Claimed;
    }
    let config = RuntimeConfig::from_hardware(
        info.device_id,
        subsystem,
        ((hardware_revision & 0x0000_fff0) >> 4) as u16,
        (hardware_revision & 0x3) as u8,
        ((rf_id & CSR_HW_RFID_TYPE_MASK) >> CSR_HW_RFID_TYPE_SHIFT) as u16,
        ((rf_id >> 28) & 1) as u8,
        ((rf_id >> 29) & 1) as u8,
    );
    match lookup_config(config) {
        Some(selected) => info!(
            "iwx: {bdf}: PCI {:#06x}:{:#06x} matched {selected:?} (subsystem {:#06x}:{:#06x})",
            info.vendor_id,
            info.device_id,
            root.endpoint_subsystem_ids(bdf).0,
            subsystem,
        ),
        None => warn!("iwx: {bdf}: matched PCI function has no runtime iwx config"),
    }
    debug_assert_eq!(info.vendor_id, INTEL_VENDOR_ID);
    if info.device_id == AX211_DEVICE_ID {
        info!("iwx: {bdf}: AX211 PCI discovery complete; firmware runtime attach follows");
    }
    // The upstream match has claimed this PCI function. Full interface
    // publication is performed by the iwx network/net80211 adapter.
    BusProbeResult::Claimed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openbsd_pci_match_table_and_bz_gf_exception_are_preserved() {
        assert!(pci_match_decision(INTEL_VENDOR_ID, AX211_DEVICE_ID, None));
        assert!(!pci_match_decision(0xffff, AX211_DEVICE_ID, None));
        assert!(!pci_match_decision(INTEL_VENDOR_ID, 0xffff, None));
        assert!(!pci_match_decision(
            INTEL_VENDOR_ID,
            SPECIAL_BZ_DEVICE,
            None
        ));
        assert!(!pci_match_decision(
            INTEL_VENDOR_ID,
            SPECIAL_BZ_DEVICE,
            Some(0)
        ));
        assert!(pci_match_decision(
            INTEL_VENDOR_ID,
            SPECIAL_BZ_DEVICE,
            Some(u32::from(RF_TYPE_GF) << 12)
        ));
    }
}
