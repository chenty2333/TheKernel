//! ALIVE firmware notification decoding from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

pub const ALIVE_STATUS_OK: u16 = 0xcafe;
pub const ALIVE_V4_BYTES: usize = 116;
pub const ALIVE_V5_BYTES: usize = 128;
pub const ALIVE_V6_BYTES: usize = 152;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AliveInfo {
    pub notification_version: u8,
    pub status: u16,
    pub alive_ok: bool,
    pub lmac_error_event_table: [u32; 2],
    pub lmac_log_event_table: u32,
    pub umac_error_info_address: u32,
    pub sku_id: Option<[u32; 3]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliveError {
    InvalidPayloadLength,
}

/// Decode ALIVE v4/v5/v6/v7 payloads, preserving version-specific size checks.
// upstream: if_iwx.c iwx_rx_pkt() IWX_ALIVE case
pub fn parse_alive(notification_version: u8, payload: &[u8]) -> Result<AliveInfo, AliveError> {
    let (required, has_sku) = match notification_version {
        6 | 7 => (ALIVE_V6_BYTES, true),
        5 => (ALIVE_V5_BYTES, true),
        _ => (ALIVE_V4_BYTES, false),
    };
    if payload.len() != required {
        return Err(AliveError::InvalidPayloadLength);
    }
    let status = read_u16(payload, 0)?;
    let lmac0 = read_u32(payload, 20)?;
    let lmac0_log = read_u32(payload, 24)?;
    let lmac1 = read_u32(payload, 68)?;
    let umac_error = read_u32(payload, 108)?;
    let sku_id = if has_sku {
        Some([
            read_u32(payload, 116)?,
            read_u32(payload, 120)?,
            read_u32(payload, 124)?,
        ])
    } else {
        None
    };
    Ok(AliveInfo {
        notification_version,
        status,
        alive_ok: status == ALIVE_STATUS_OK,
        lmac_error_event_table: [lmac0, lmac1],
        lmac_log_event_table: lmac0_log,
        umac_error_info_address: umac_error,
        sku_id,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, AliveError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or(AliveError::InvalidPayloadLength)?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, AliveError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(AliveError::InvalidPayloadLength)?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn alive_v4_v5_v6_payloads_use_versioned_size_and_sku_offsets() {
        let mut v4 = vec![0; ALIVE_V4_BYTES];
        v4[..2].copy_from_slice(&ALIVE_STATUS_OK.to_le_bytes());
        v4[20..24].copy_from_slice(&0x1111u32.to_le_bytes());
        v4[24..28].copy_from_slice(&0x2222u32.to_le_bytes());
        v4[68..72].copy_from_slice(&0x3333u32.to_le_bytes());
        v4[108..112].copy_from_slice(&0x4444u32.to_le_bytes());
        let info = parse_alive(4, &v4).unwrap();
        assert!(info.alive_ok);
        assert_eq!(info.lmac_error_event_table, [0x1111, 0x3333]);
        assert_eq!(info.lmac_log_event_table, 0x2222);
        assert_eq!(info.umac_error_info_address, 0x4444);
        assert_eq!(info.sku_id, None);
        assert_eq!(
            parse_alive(4, &v4[..v4.len() - 1]),
            Err(AliveError::InvalidPayloadLength)
        );

        let mut v5 = vec![0; ALIVE_V5_BYTES];
        v5[..2].copy_from_slice(&0xdeadu16.to_le_bytes());
        v5[116..128].copy_from_slice(&[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
        let info = parse_alive(5, &v5).unwrap();
        assert!(!info.alive_ok);
        assert_eq!(info.sku_id, Some([1, 2, 3]));

        assert_eq!(
            parse_alive(6, &vec![0; ALIVE_V5_BYTES]),
            Err(AliveError::InvalidPayloadLength)
        );
    }
}
