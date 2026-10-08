//! RX metadata helpers translated from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::DeviceFamily;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxMetadataError {
    DescriptorTooShort,
}

/// Extract the better (less negative) antenna energy from an RX MPDU descriptor.
// upstream: if_iwx.c iwx_rxmq_get_signal_strength()
pub fn signal_strength_dbm(
    family: DeviceFamily,
    descriptor: &[u8],
) -> Result<i16, RxMetadataError> {
    let energy_offset = if family >= DeviceFamily::Ax210 {
        20 // absolute offset from iwx_rx_mpdu_desc start (v3)
    } else {
        12 // RX_MPDU_RES_START_API_S_VER_1 descriptor v1
    };
    let energies = descriptor
        .get(energy_offset..energy_offset + 2)
        .ok_or(RxMetadataError::DescriptorTooShort)?;
    let energy_a = if energies[0] != 0 {
        -(energies[0] as i16)
    } else {
        -256
    };
    let energy_b = if energies[1] != 0 {
        -(energies[1] as i16)
    } else {
        -256
    };
    Ok(energy_a.max(energy_b))
}

/// Average nonzero beacon-silence RSSI values and convert to dBm.
// upstream: if_iwx.c iwx_get_noise()
pub fn noise_dbm(beacon_silence_rssi: [u32; 3]) -> i16 {
    let mut total = 0u32;
    let mut count = 0u32;
    for value in beacon_silence_rssi {
        let noise = u32::from_le(value) & 0xff;
        if noise != 0 {
            total += noise;
            count += 1;
        }
    }
    if count == 0 {
        -127
    } else {
        (total / count) as i16 - 107
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_energy_offsets_follow_descriptor_generation() {
        let mut desc = [0; 24];
        desc[12] = 51;
        desc[13] = 42;
        desc[20] = 30;
        desc[21] = 40;
        assert_eq!(
            signal_strength_dbm(DeviceFamily::Family22000, &desc),
            Ok(-42)
        );
        assert_eq!(signal_strength_dbm(DeviceFamily::Ax210, &desc), Ok(-30));
        assert_eq!(
            signal_strength_dbm(DeviceFamily::Ax210, &desc[..21]),
            Err(RxMetadataError::DescriptorTooShort)
        );
    }

    #[test]
    fn noise_ignores_empty_antennas_and_uses_minus_127_fallback() {
        assert_eq!(noise_dbm([0, 0, 0]), -127);
        assert_eq!(noise_dbm([120, 0, u32::from_le(130)]), 18);
    }
}
