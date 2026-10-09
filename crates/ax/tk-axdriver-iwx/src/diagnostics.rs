//! Firmware error-log decoding from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

pub const UMAC_ERROR_WORDS: usize = 15;
pub const LMAC_ERROR_WORDS: usize = 38;
pub const ERROR_START_OFFSET_BYTES: usize = 4;
pub const ERROR_ELEMENT_SIZE_BYTES: usize = 28;
pub const FW_SYSASSERT_CPU_MASK: u32 = 0xf000_0000;
pub const LEGACY_ERROR_TABLE_MIN: u32 = 0x0040_0000;
pub const BZ_ERROR_TABLE_MIN: u32 = 0x000d_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorLogError {
    InvalidPointer(u32),
    Truncated,
}

/// Select the source address floor for LMAC/UMAC error-table reads.
// upstream: if_iwx.c iwx_nic_error()
pub fn validate_error_table_address(base: u32, bz_or_newer: bool) -> Result<u32, ErrorLogError> {
    let minimum = if bz_or_newer {
        BZ_ERROR_TABLE_MIN
    } else {
        LEGACY_ERROR_TABLE_MIN
    };
    if base < minimum {
        Err(ErrorLogError::InvalidPointer(base))
    } else {
        Ok(base)
    }
}

/// Decode the u32-accessed UMAC error record after device-memory endian conversion.
// upstream: if_iwx.c iwx_nic_umac_error()
pub fn parse_umac_error_table(bytes: &[u8]) -> Result<[u32; UMAC_ERROR_WORDS], ErrorLogError> {
    parse_words::<UMAC_ERROR_WORDS>(bytes)
}

/// Decode the complete v3 LMAC error record, retaining fields in source order.
// upstream: if_iwx.c iwx_nic_error()
pub fn parse_lmac_error_table(bytes: &[u8]) -> Result<[u32; LMAC_ERROR_WORDS], ErrorLogError> {
    parse_words::<LMAC_ERROR_WORDS>(bytes)
}

fn parse_words<const N: usize>(bytes: &[u8]) -> Result<[u32; N], ErrorLogError> {
    if bytes.len() < N * 4 {
        return Err(ErrorLogError::Truncated);
    }
    let mut words = [0u32; N];
    for (i, word) in words.iter_mut().enumerate() {
        let at = i * 4;
        *word = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    }
    Ok(words)
}

/// Resolve firmware SYSASSERT IDs after dropping the CPU-number high nibble.
// upstream: if_iwx.c iwx_desc_lookup()
pub fn error_description(number: u32) -> &'static str {
    match number & !FW_SYSASSERT_CPU_MASK {
        0x34 => "NMI_INTERRUPT_WDG",
        0x35 => "SYSASSERT",
        0x37 => "UCODE_VERSION_MISMATCH",
        0x38 | 0x39 => "BAD_COMMAND",
        0x3c => "NMI_INTERRUPT_DATA_ACTION_PT",
        0x3d => "FATAL_ERROR",
        0x46 => "NMI_TRM_HW_ERR",
        0x4c => "NMI_INTERRUPT_TRM",
        0x54 => "NMI_INTERRUPT_BREAK_POINT",
        0x5c => "NMI_INTERRUPT_WDG_RXF_FULL",
        0x64 => "NMI_INTERRUPT_WDG_NO_RBD_RXF_FULL",
        0x66 => "NMI_INTERRUPT_HOST",
        0x70 => "NMI_INTERRUPT_LMAC_FATAL",
        0x71 => "NMI_INTERRUPT_UMAC_FATAL",
        0x73 => "NMI_INTERRUPT_OTHER_LMAC_FATAL",
        0x7c => "NMI_INTERRUPT_ACTION_PT",
        0x84 => "NMI_INTERRUPT_UNKNOWN",
        0x86 => "NMI_INTERRUPT_INST_ACTION_PT",
        _ => "ADVANCED_SYSASSERT",
    }
}

/// Indicate whether the LMAC error record is valid and whether the source dump starts.
// upstream: if_iwx.c iwx_nic_error()
pub fn error_log_validity(valid: u32) -> (bool, bool) {
    (
        valid != 0,
        ERROR_START_OFFSET_BYTES <= (valid as usize).saturating_mul(ERROR_ELEMENT_SIZE_BYTES),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxRingDebug {
    pub queue_id: u16,
    pub current: u16,
    pub current_hardware: u32,
    pub queued: u16,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverDebugStatus<'a> {
    pub tx_rings: &'a [TxRingDebug],
    pub rx_cursor: u16,
    pub ieee80211_state: u8,
}

/// Produce the stable driver-status fields logged by iwx_dump_driver_status().
// upstream: if_iwx.c iwx_dump_driver_status()
pub fn driver_status(
    tx_rings: &[TxRingDebug],
    rx_cursor: u16,
    ieee80211_state: u8,
) -> DriverDebugStatus<'_> {
    DriverDebugStatus {
        tx_rings,
        rx_cursor,
        ieee80211_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_specific_pointer_floor_and_error_table_words_match_source() {
        assert_eq!(
            validate_error_table_address(0x3f_ffff, false),
            Err(ErrorLogError::InvalidPointer(0x3f_ffff))
        );
        assert_eq!(validate_error_table_address(0xd0000, true), Ok(0xd0000));
        let bytes: Vec<u8> = (0..LMAC_ERROR_WORDS as u32)
            .flat_map(u32::to_le_bytes)
            .collect();
        let lmac = parse_lmac_error_table(&bytes).unwrap();
        assert_eq!(lmac.len(), 38);
        assert_eq!(lmac[0], 0);
        assert_eq!(lmac[37], 37);
        assert_eq!(
            parse_umac_error_table(&bytes).unwrap().len(),
            UMAC_ERROR_WORDS
        );
        assert_eq!(
            parse_umac_error_table(&bytes[..UMAC_ERROR_WORDS * 4 - 1]),
            Err(ErrorLogError::Truncated)
        );
        assert_eq!(error_log_validity(0), (false, false));
        assert_eq!(error_log_validity(1), (true, true));
    }

    #[test]
    fn sysassert_cpu_mask_and_unknown_fallback_follow_description_table() {
        assert_eq!(error_description(0x3000_0035), "SYSASSERT");
        assert_eq!(error_description(0x38), "BAD_COMMAND");
        assert_eq!(error_description(0xdead_beef), "ADVANCED_SYSASSERT");
    }
}
