//! Final RX metadata and upper-layer handoff from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::ProcessedRxMpdu;

pub const IWX_MIN_DBM: i16 = -100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxFrameInfo {
    pub channel_index: u8,
    /// OpenBSD net80211's RSSI value normalized from the dBm energy.
    pub rssi: u8,
    pub noise_dbm: i16,
    pub timestamp: u32,
    pub rate_n_flags: u32,
    pub short_preamble: bool,
    pub hardware_decrypted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxFrameMetadataConfig {
    pub channel_count: usize,
    pub fallback_channel_index: u8,
    pub max_rssi: u8,
    pub noise_dbm: i16,
}

/// Normalize channel/RSSI and deliver a processed MPDU to the 802.11 input API.
// upstream: if_iwx.c iwx_rx_frame()
pub fn deliver_rx_frame<E>(
    mpdu: ProcessedRxMpdu,
    config: RxFrameMetadataConfig,
    mut input: impl FnMut(&[u8], RxFrameInfo) -> Result<(), E>,
) -> Result<(), E> {
    let channel_index = if usize::from(mpdu.metadata.channel_index) < config.channel_count {
        mpdu.metadata.channel_index
    } else {
        config.fallback_channel_index
    };
    let energy_a = if mpdu.metadata.energy_a != 0 {
        -i16::from(mpdu.metadata.energy_a)
    } else {
        -256
    };
    let energy_b = if mpdu.metadata.energy_b != 0 {
        -i16::from(mpdu.metadata.energy_b)
    } else {
        -256
    };
    let rssi = (IWX_MIN_DBM.saturating_neg() + energy_a.max(energy_b))
        .clamp(0, i16::from(config.max_rssi)) as u8;
    input(
        &mpdu.frame,
        RxFrameInfo {
            channel_index,
            rssi,
            noise_dbm: config.noise_dbm,
            timestamp: mpdu.metadata.device_timestamp,
            rate_n_flags: mpdu.metadata.rate_n_flags,
            short_preamble: mpdu.metadata.short_preamble(),
            hardware_decrypted: mpdu.hardware_decrypted,
        },
    )
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::RxMpduMetadata;

    #[test]
    fn rx_frame_normalizes_energy_and_falls_back_to_current_channel() {
        let mpdu = ProcessedRxMpdu {
            metadata: RxMpduMetadata {
                descriptor_bytes: 68,
                frame_bytes: 24,
                status: 0,
                reorder_data: 0x7f00_0000,
                phy_info: 1 << 7,
                mac_flags2: 0,
                amsdu_info: 0,
                rate_n_flags: 0x1234,
                channel_index: 9,
                energy_a: 48,
                energy_b: 42,
                device_timestamp: 0x1122_3344,
            },
            frame: vec![1, 2, 3],
            hardware_decrypted: true,
            same_sequence: false,
            tid_index: 0,
            reorder_tid: 0,
        };
        let mut frame_seen = false;
        deliver_rx_frame(
            mpdu,
            RxFrameMetadataConfig {
                channel_count: 3,
                fallback_channel_index: 1,
                max_rssi: 67,
                noise_dbm: -96,
            },
            |frame, info| {
                frame_seen = true;
                assert_eq!(frame, [1, 2, 3]);
                assert_eq!(info.channel_index, 1);
                assert_eq!(info.rssi, 58);
                assert_eq!(info.noise_dbm, -96);
                assert_eq!(info.timestamp, 0x1122_3344);
                assert_eq!(info.rate_n_flags, 0x1234);
                assert!(info.short_preamble && info.hardware_decrypted);
                Ok::<_, ()>(())
            },
        )
        .unwrap();
        assert!(frame_seen);
    }
}
