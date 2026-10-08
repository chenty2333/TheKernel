//! Firmware regulatory-domain change hints from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MccUpdate {
    pub country_code: [u8; 2],
    pub source_id: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MccUpdateError {
    Truncated,
}

/// Decode the little-endian two-character country hint from CHUB.
// upstream: if_iwx.c iwx_mcc_update()
pub fn decode_mcc_update(payload: &[u8]) -> Result<MccUpdate, MccUpdateError> {
    if payload.len() < 4 {
        return Err(MccUpdateError::Truncated);
    }
    Ok(MccUpdate {
        country_code: [payload[1], payload[0]],
        source_id: payload[2],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcc_notification_decodes_country_order_and_source() {
        assert_eq!(
            decode_mcc_update(&[b'P', b'J', 2, 0]),
            Ok(MccUpdate {
                country_code: *b"JP",
                source_id: 2,
            })
        );
    }
}
