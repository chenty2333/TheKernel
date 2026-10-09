//! Post-NVM interface capability setup from OpenBSD iwx preinit.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{
    ChannelInfo, HtRateCapabilities, MacAddress, NvmInfo, VhtRateCapabilities, init_channel_map,
    mimo_enabled, setup_ht_rate_capabilities, setup_vht_rate_capabilities,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreinitPlan {
    /// An already-attached interface may have had its link address changed.
    RefreshAddress(MacAddress),
    Initialize {
        hardware_address: MacAddress,
        channels: Vec<ChannelInfo>,
        ht_rates: Option<HtRateCapabilities>,
        vht_rates: Option<VhtRateCapabilities>,
        disable_5ghz_rates: bool,
    },
}

/// Build the source HT/VHT, channel, address and band policy after Init NVM.
// upstream: if_iwx.c iwx_preinit()
pub fn preinit_plan(
    attached: bool,
    current_interface_address: MacAddress,
    nvm: &NvmInfo,
    uhb_supported: bool,
    user_no_mimo: bool,
) -> PreinitPlan {
    if attached {
        return PreinitPlan::RefreshAddress(current_interface_address);
    }
    let mimo = mimo_enabled(nvm, user_no_mimo);
    PreinitPlan::Initialize {
        hardware_address: nvm.hardware_address,
        channels: init_channel_map(nvm, uhb_supported),
        ht_rates: nvm.supports_11n.then(|| {
            setup_ht_rate_capabilities(nvm.valid_tx_antennas, nvm.valid_rx_antennas, mimo)
        }),
        vht_rates: nvm.supports_11ac.then(|| {
            setup_vht_rate_capabilities(nvm.valid_tx_antennas, nvm.valid_rx_antennas, mimo)
        }),
        disable_5ghz_rates: !nvm.band_52ghz,
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn nvm() -> NvmInfo {
        NvmInfo {
            hardware_address: [0, 1, 2, 3, 4, 5],
            nvm_version: 1,
            board_type: 0,
            hardware_address_count: 1,
            empty_otp: false,
            band_24ghz: true,
            band_52ghz: false,
            supports_11n: true,
            supports_11ac: true,
            supports_11ax: false,
            mimo_disabled: false,
            valid_tx_antennas: 3,
            valid_rx_antennas: 3,
            lar_enabled: false,
            channel_profiles: vec![crate::NVM_CHANNEL_VALID | crate::NVM_CHANNEL_ACTIVE],
        }
    }

    #[test]
    fn first_preinit_configures_supported_rates_channels_and_address() {
        let PreinitPlan::Initialize {
            hardware_address,
            channels,
            ht_rates,
            vht_rates,
            disable_5ghz_rates,
        } = preinit_plan(false, [9; 6], &nvm(), true, false)
        else {
            panic!("first preinit must initialize the interface profile")
        };
        assert_eq!(hardware_address, [0, 1, 2, 3, 4, 5]);
        assert_eq!(channels.len(), 1);
        assert!(ht_rates.unwrap().supported_mcs[1] == 0xff);
        assert_eq!((vht_rates.unwrap().rx_mcs_map >> 2) & 3, 2);
        assert!(disable_5ghz_rates);
    }

    #[test]
    fn attached_preinit_only_refreshes_interface_address() {
        assert_eq!(
            preinit_plan(true, [9; 6], &nvm(), false, true),
            PreinitPlan::RefreshAddress([9; 6])
        );
    }
}
