//! AX211 configuration selection translated from OpenBSD iwx's device table.
//!
//! Upstream: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230 (2026-09-19),
//! `iwx_find_device_cfg()` and the So-F/So GF table rows; `if_iwxvar.h`,
//! `iwx_2ax_cfg_so_gf_a0` and firmware name constants. ISC.

/// Firmware and PNVM names selected from an iwx device configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwareConfig {
    pub firmware: &'static str,
    pub pnvm: Option<&'static str>,
    pub uhb_supported: bool,
    pub xtal_latency: u32,
    pub low_latency_xtal: bool,
}

/// Supported subset of the runtime iwx config table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceConfig {
    SoGfAx211,
    SoGf4Ax411,
}

impl DeviceConfig {
    pub const fn firmware(self) -> FirmwareConfig {
        match self {
            Self::SoGfAx211 => FirmwareConfig {
                firmware: "/lib/firmware/iwlwifi-so-a0-gf-a0-89.ucode",
                pnvm: Some("/lib/firmware/iwlwifi-so-a0-gf-a0.pnvm"),
                uhb_supported: true,
                xtal_latency: 0,
                low_latency_xtal: false,
            },
            Self::SoGf4Ax411 => FirmwareConfig {
                firmware: "/lib/firmware/iwlwifi-so-a0-gf4-a0-89.ucode",
                pnvm: Some("/lib/firmware/iwlwifi-so-a0-gf4-a0.pnvm"),
                uhb_supported: true,
                xtal_latency: 12_000,
                low_latency_xtal: true,
            },
        }
    }
}

/// Hardware facts consumed by the matching subset of OpenBSD's iwx table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// `IWX_CSR_HW_REV_TYPE` value.
    pub mac_type: u16,
    /// `IWX_CSR_HW_RFID_TYPE` value.
    pub rf_type: u16,
    /// Whether the device has the no-160-MHz subsystem flag.
    pub no_160: bool,
    /// Whether the device is a dual-comb (CDB) configuration.
    pub cdb: bool,
}

const MAC_TYPE_SO: u16 = 0x42;
const MAC_TYPE_SOF: u16 = 0x43;
const RF_TYPE_GF: u16 = 0x10d;

/// Match the So/So-F GF entries used by the AX211/AX411 portion of iwx.
///
/// OpenBSD walks its full table in reverse and checks additional fields in
/// other rows. This focused table preserves the same runtime predicates for
/// the two matching rows; unrelated device families are deliberately absent.
// upstream: if_iwx.c iwx_find_device_cfg()
pub const fn lookup_config(runtime: RuntimeConfig) -> Option<DeviceConfig> {
    if (runtime.mac_type == MAC_TYPE_SOF || runtime.mac_type == MAC_TYPE_SO)
        && runtime.rf_type == RF_TYPE_GF
        && !runtime.no_160
    {
        if runtime.cdb {
            Some(DeviceConfig::SoGf4Ax411)
        } else {
            Some(DeviceConfig::SoGfAx211)
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AX211: RuntimeConfig = RuntimeConfig {
        mac_type: MAC_TYPE_SOF,
        rf_type: RF_TYPE_GF,
        no_160: false,
        cdb: false,
    };

    #[test]
    fn ax211_uses_so_gf_firmware_and_pnvm() {
        assert_eq!(lookup_config(AX211), Some(DeviceConfig::SoGfAx211));
        assert_eq!(
            DeviceConfig::SoGfAx211.firmware(),
            FirmwareConfig {
                firmware: "/lib/firmware/iwlwifi-so-a0-gf-a0-89.ucode",
                pnvm: Some("/lib/firmware/iwlwifi-so-a0-gf-a0.pnvm"),
                uhb_supported: true,
                xtal_latency: 0,
                low_latency_xtal: false,
            }
        );
    }

    #[test]
    fn cdb_uses_gf4_configuration() {
        assert_eq!(
            lookup_config(RuntimeConfig { cdb: true, ..AX211 }),
            Some(DeviceConfig::SoGf4Ax411)
        );
    }

    #[test]
    fn no_160_and_unrelated_rf_or_mac_do_not_match() {
        assert_eq!(
            lookup_config(RuntimeConfig {
                no_160: true,
                ..AX211
            }),
            None
        );
        assert_eq!(
            lookup_config(RuntimeConfig {
                rf_type: 0,
                ..AX211
            }),
            None
        );
        assert_eq!(
            lookup_config(RuntimeConfig {
                mac_type: 0,
                ..AX211
            }),
            None
        );
    }
}
