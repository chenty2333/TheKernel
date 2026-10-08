//! SD Host Controller Interface register definitions and quirk bits.
//!
//! Translated from FreeBSD `sys/dev/sdhci/sdhci.h` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2008 Alexander Motin <mav@FreeBSD.org>.
//! SPDX-License-Identifier: BSD-2-Clause

/// Size of an SDMA bounce buffer for the specified controller boundary.
// upstream: sdhci.h SDHCI_SDMA_BNDRY_TO_BBUFSZ()
pub const fn sdma_bounce_buffer_size(boundary: u32) -> usize {
    4096usize << boundary
}

/// Encode the SDMA boundary and block length fields.
// upstream: sdhci.h SDHCI_MAKE_BLKSZ()
pub const fn make_block_size(boundary: u32, block_size: u32) -> u32 {
    ((boundary & 0x7) << 12) | (block_size & 0xfff)
}

pub const SDHCI_QUIRK_CLOCK_BEFORE_RESET: u32 = 1 << 0;
pub const SDHCI_QUIRK_FORCE_DMA: u32 = 1 << 1;
pub const SDHCI_QUIRK_BROKEN_DMA: u32 = 1 << 2;
pub const SDHCI_QUIRK_NO_CARD_NO_RESET: u32 = 1 << 3;
pub const SDHCI_QUIRK_RESET_ON_IOS: u32 = 1 << 4;
pub const SDHCI_QUIRK_32BIT_DMA_SIZE: u32 = 1 << 5;
pub const SDHCI_QUIRK_RESET_AFTER_REQUEST: u32 = 1 << 6;
pub const SDHCI_QUIRK_INCR_TIMEOUT_CONTROL: u32 = 1 << 7;
pub const SDHCI_QUIRK_BROKEN_TIMINGS: u32 = 1 << 8;
pub const SDHCI_QUIRK_LOWER_FREQUENCY: u32 = 1 << 9;
pub const SDHCI_QUIRK_DATA_TIMEOUT_USES_SDCLK: u32 = 1 << 10;
pub const SDHCI_QUIRK_BROKEN_TIMEOUT_VAL: u32 = 1 << 11;
pub const SDHCI_QUIRK_MISSING_CAPS: u32 = 1 << 12;
pub const SDHCI_QUIRK_DONT_SHIFT_RESPONSE: u32 = 1 << 13;
pub const SDHCI_QUIRK_WAITFOR_RESET_ASSERTED: u32 = 1 << 14;
pub const SDHCI_QUIRK_DONT_SET_HISPD_BIT: u32 = 1 << 15;
pub const SDHCI_QUIRK_BCM577XX_400KHZ_CLKSRC: u32 = 1 << 16;
pub const SDHCI_QUIRK_POLL_CARD_PRESENT: u32 = 1 << 17;
pub const SDHCI_QUIRK_ALL_SLOTS_NON_REMOVABLE: u32 = 1 << 18;
pub const SDHCI_QUIRK_INTEL_POWER_UP_RESET: u32 = 1 << 19;
pub const SDHCI_QUIRK_DATA_TIMEOUT_1MHZ: u32 = 1 << 20;
pub const SDHCI_QUIRK_BOOT_NOACC: u32 = 1 << 21;
pub const SDHCI_QUIRK_WAIT_WHILE_BUSY: u32 = 1 << 22;
pub const SDHCI_QUIRK_MMC_DDR52: u32 = 1 << 23;
pub const SDHCI_QUIRK_BROKEN_UHS_DDR50: u32 = 1 << 24;
pub const SDHCI_QUIRK_BROKEN_MMC_HS200: u32 = 1 << 25;
pub const SDHCI_QUIRK_CAPS_BIT63_FOR_MMC_HS400: u32 = 1 << 26;
pub const SDHCI_QUIRK_PRESET_VALUE_BROKEN: u32 = 1 << 27;
pub const SDHCI_QUIRK_BROKEN_AUTO_STOP: u32 = 1 << 28;
pub const SDHCI_QUIRK_MMC_HS400_IF_CAN_SDR104: u32 = 1 << 29;
pub const SDHCI_QUIRK_BROKEN_SDMA_BOUNDARY: u32 = 1 << 30;
pub const SDHCI_QUIRK_SLOTTYPE_BROKEN: u32 = 1 << 31;
pub const SDHCI_DMA_ADDRESS: u32 = 0x00;
pub const SDHCI_BLOCK_SIZE: u32 = 0x04;
pub const SDHCI_BLKSZ_SDMA_BNDRY_4K: u32 = 0x00;
pub const SDHCI_BLKSZ_SDMA_BNDRY_8K: u32 = 0x01;
pub const SDHCI_BLKSZ_SDMA_BNDRY_16K: u32 = 0x02;
pub const SDHCI_BLKSZ_SDMA_BNDRY_32K: u32 = 0x03;
pub const SDHCI_BLKSZ_SDMA_BNDRY_64K: u32 = 0x04;
pub const SDHCI_BLKSZ_SDMA_BNDRY_128K: u32 = 0x05;
pub const SDHCI_BLKSZ_SDMA_BNDRY_256K: u32 = 0x06;
pub const SDHCI_BLKSZ_SDMA_BNDRY_512K: u32 = 0x07;
pub const SDHCI_BLOCK_COUNT: u32 = 0x06;
pub const SDHCI_ARGUMENT: u32 = 0x08;
pub const SDHCI_TRANSFER_MODE: u32 = 0x0C;
pub const SDHCI_TRNS_DMA: u32 = 0x01;
pub const SDHCI_TRNS_BLK_CNT_EN: u32 = 0x02;
pub const SDHCI_TRNS_ACMD12: u32 = 0x04;
pub const SDHCI_TRNS_READ: u32 = 0x10;
pub const SDHCI_TRNS_MULTI: u32 = 0x20;
pub const SDHCI_COMMAND_FLAGS: u32 = 0x0E;
pub const SDHCI_CMD_RESP_NONE: u32 = 0x00;
pub const SDHCI_CMD_RESP_LONG: u32 = 0x01;
pub const SDHCI_CMD_RESP_SHORT: u32 = 0x02;
pub const SDHCI_CMD_RESP_SHORT_BUSY: u32 = 0x03;
pub const SDHCI_CMD_RESP_MASK: u32 = 0x03;
pub const SDHCI_CMD_CRC: u32 = 0x08;
pub const SDHCI_CMD_INDEX: u32 = 0x10;
pub const SDHCI_CMD_DATA: u32 = 0x20;
pub const SDHCI_CMD_TYPE_NORMAL: u32 = 0x00;
pub const SDHCI_CMD_TYPE_SUSPEND: u32 = 0x40;
pub const SDHCI_CMD_TYPE_RESUME: u32 = 0x80;
pub const SDHCI_CMD_TYPE_ABORT: u32 = 0xc0;
pub const SDHCI_CMD_TYPE_MASK: u32 = 0xc0;
pub const SDHCI_COMMAND: u32 = 0x0F;
pub const SDHCI_RESPONSE: u32 = 0x10;
pub const SDHCI_BUFFER: u32 = 0x20;
pub const SDHCI_PRESENT_STATE: u32 = 0x24;
pub const SDHCI_CMD_INHIBIT: u32 = 0x00000001;
pub const SDHCI_DAT_INHIBIT: u32 = 0x00000002;
pub const SDHCI_DAT_ACTIVE: u32 = 0x00000004;
pub const SDHCI_RETUNE_REQUEST: u32 = 0x00000008;
pub const SDHCI_DOING_WRITE: u32 = 0x00000100;
pub const SDHCI_DOING_READ: u32 = 0x00000200;
pub const SDHCI_SPACE_AVAILABLE: u32 = 0x00000400;
pub const SDHCI_DATA_AVAILABLE: u32 = 0x00000800;
pub const SDHCI_CARD_PRESENT: u32 = 0x00010000;
pub const SDHCI_CARD_STABLE: u32 = 0x00020000;
pub const SDHCI_CARD_PIN: u32 = 0x00040000;
pub const SDHCI_WRITE_PROTECT: u32 = 0x00080000;
pub const SDHCI_STATE_DAT_MASK: u32 = 0x00f00000;
pub const SDHCI_STATE_CMD: u32 = 0x01000000;
pub const SDHCI_HOST_CONTROL: u32 = 0x28;
pub const SDHCI_CTRL_LED: u32 = 0x01;
pub const SDHCI_CTRL_4BITBUS: u32 = 0x02;
pub const SDHCI_CTRL_HISPD: u32 = 0x04;
pub const SDHCI_CTRL_SDMA: u32 = 0x08;
pub const SDHCI_CTRL_ADMA2: u32 = 0x10;
pub const SDHCI_CTRL_ADMA264: u32 = 0x18;
pub const SDHCI_CTRL_DMA_MASK: u32 = 0x18;
pub const SDHCI_CTRL_8BITBUS: u32 = 0x20;
pub const SDHCI_CTRL_CARD_DET: u32 = 0x40;
pub const SDHCI_CTRL_FORCE_CARD: u32 = 0x80;
pub const SDHCI_POWER_CONTROL: u32 = 0x29;
pub const SDHCI_POWER_ON: u32 = 0x01;
pub const SDHCI_POWER_180: u32 = 0x0A;
pub const SDHCI_POWER_300: u32 = 0x0C;
pub const SDHCI_POWER_330: u32 = 0x0E;
pub const SDHCI_BLOCK_GAP_CONTROL: u32 = 0x2A;
pub const SDHCI_WAKE_UP_CONTROL: u32 = 0x2B;
pub const SDHCI_CLOCK_CONTROL: u32 = 0x2C;
pub const SDHCI_DIVIDER_MASK: u32 = 0xff;
pub const SDHCI_DIVIDER_MASK_LEN: u32 = 8;
pub const SDHCI_DIVIDER_SHIFT: u32 = 8;
pub const SDHCI_DIVIDER_HI_MASK: u32 = 3;
pub const SDHCI_DIVIDER_HI_SHIFT: u32 = 6;
pub const SDHCI_CLOCK_CARD_EN: u32 = 0x0004;
pub const SDHCI_CLOCK_INT_STABLE: u32 = 0x0002;
pub const SDHCI_CLOCK_INT_EN: u32 = 0x0001;
pub const SDHCI_TIMEOUT_CONTROL: u32 = 0x2E;
pub const SDHCI_SOFTWARE_RESET: u32 = 0x2F;
pub const SDHCI_RESET_ALL: u32 = 0x01;
pub const SDHCI_RESET_CMD: u32 = 0x02;
pub const SDHCI_RESET_DATA: u32 = 0x04;
pub const SDHCI_INT_STATUS: u32 = 0x30;
pub const SDHCI_INT_ENABLE: u32 = 0x34;
pub const SDHCI_SIGNAL_ENABLE: u32 = 0x38;
pub const SDHCI_INT_RESPONSE: u32 = 0x00000001;
pub const SDHCI_INT_DATA_END: u32 = 0x00000002;
pub const SDHCI_INT_BLOCK_GAP: u32 = 0x00000004;
pub const SDHCI_INT_DMA_END: u32 = 0x00000008;
pub const SDHCI_INT_SPACE_AVAIL: u32 = 0x00000010;
pub const SDHCI_INT_DATA_AVAIL: u32 = 0x00000020;
pub const SDHCI_INT_CARD_INSERT: u32 = 0x00000040;
pub const SDHCI_INT_CARD_REMOVE: u32 = 0x00000080;
pub const SDHCI_INT_CARD_INT: u32 = 0x00000100;
pub const SDHCI_INT_INT_A: u32 = 0x00000200;
pub const SDHCI_INT_INT_B: u32 = 0x00000400;
pub const SDHCI_INT_INT_C: u32 = 0x00000800;
pub const SDHCI_INT_RETUNE: u32 = 0x00001000;
pub const SDHCI_INT_ERROR: u32 = 0x00008000;
pub const SDHCI_INT_TIMEOUT: u32 = 0x00010000;
pub const SDHCI_INT_CRC: u32 = 0x00020000;
pub const SDHCI_INT_END_BIT: u32 = 0x00040000;
pub const SDHCI_INT_INDEX: u32 = 0x00080000;
pub const SDHCI_INT_DATA_TIMEOUT: u32 = 0x00100000;
pub const SDHCI_INT_DATA_CRC: u32 = 0x00200000;
pub const SDHCI_INT_DATA_END_BIT: u32 = 0x00400000;
pub const SDHCI_INT_BUS_POWER: u32 = 0x00800000;
pub const SDHCI_INT_ACMD12ERR: u32 = 0x01000000;
pub const SDHCI_INT_ADMAERR: u32 = 0x02000000;
pub const SDHCI_INT_TUNEERR: u32 = 0x04000000;
pub const SDHCI_INT_NORMAL_MASK: u32 = 0x00007FFF;
pub const SDHCI_INT_ERROR_MASK: u32 = 0xFFFF8000;
pub const SDHCI_INT_CMD_ERROR_MASK: u32 = SDHCI_INT_TIMEOUT | SDHCI_INT_CRC | SDHCI_INT_END_BIT | SDHCI_INT_INDEX;
pub const SDHCI_INT_CMD_MASK: u32 = SDHCI_INT_RESPONSE | SDHCI_INT_CMD_ERROR_MASK;
pub const SDHCI_INT_DATA_MASK: u32 = SDHCI_INT_DATA_END | SDHCI_INT_DMA_END | SDHCI_INT_DATA_AVAIL | SDHCI_INT_SPACE_AVAIL | SDHCI_INT_DATA_TIMEOUT | SDHCI_INT_DATA_CRC | SDHCI_INT_DATA_END_BIT;
pub const SDHCI_DIVIDERS_MASK: u32 = (SDHCI_DIVIDER_MASK << SDHCI_DIVIDER_SHIFT) | (SDHCI_DIVIDER_HI_MASK << SDHCI_DIVIDER_HI_SHIFT);
pub const SDHCI_ACMD12_ERR: u32 = 0x3C;
pub const SDHCI_HOST_CONTROL2: u32 = 0x3E;
pub const SDHCI_CTRL2_PRESET_VALUE: u32 = 0x8000;
pub const SDHCI_CTRL2_ASYNC_INTR: u32 = 0x4000;
pub const SDHCI_CTRL2_64BIT_ENABLE: u32 = 0x2000;
pub const SDHCI_CTRL2_HOST_V4_ENABLE: u32 = 0x1000;
pub const SDHCI_CTRL2_CMD23_ENABLE: u32 = 0x0800;
pub const SDHCI_CTRL2_ADMA2_LENGTH_MODE: u32 = 0x0400;
pub const SDHCI_CTRL2_UHS2_IFACE_ENABLE: u32 = 0x0100;
pub const SDHCI_CTRL2_SAMPLING_CLOCK: u32 = 0x0080;
pub const SDHCI_CTRL2_EXEC_TUNING: u32 = 0x0040;
pub const SDHCI_CTRL2_DRIVER_TYPE_MASK: u32 = 0x0030;
pub const SDHCI_CTRL2_DRIVER_TYPE_B: u32 = 0x0000;
pub const SDHCI_CTRL2_DRIVER_TYPE_A: u32 = 0x0010;
pub const SDHCI_CTRL2_DRIVER_TYPE_C: u32 = 0x0020;
pub const SDHCI_CTRL2_DRIVER_TYPE_D: u32 = 0x0030;
pub const SDHCI_CTRL2_S18_ENABLE: u32 = 0x0008;
pub const SDHCI_CTRL2_UHS_MASK: u32 = 0x0007;
pub const SDHCI_CTRL2_UHS_SDR12: u32 = 0x0000;
pub const SDHCI_CTRL2_UHS_SDR25: u32 = 0x0001;
pub const SDHCI_CTRL2_UHS_SDR50: u32 = 0x0002;
pub const SDHCI_CTRL2_UHS_SDR104: u32 = 0x0003;
pub const SDHCI_CTRL2_UHS_DDR50: u32 = 0x0004;
pub const SDHCI_CTRL2_MMC_HS400: u32 = 0x0005;
pub const SDHCI_CAPABILITIES: u32 = 0x40;
pub const SDHCI_TIMEOUT_CLK_MASK: u32 = 0x0000003F;
pub const SDHCI_TIMEOUT_CLK_SHIFT: u32 = 0;
pub const SDHCI_TIMEOUT_CLK_UNIT: u32 = 0x00000080;
pub const SDHCI_CLOCK_BASE_MASK: u32 = 0x00003F00;
pub const SDHCI_CLOCK_V3_BASE_MASK: u32 = 0x0000FF00;
pub const SDHCI_CLOCK_BASE_SHIFT: u32 = 8;
pub const SDHCI_MAX_BLOCK_MASK: u32 = 0x00030000;
pub const SDHCI_MAX_BLOCK_SHIFT: u32 = 16;
pub const SDHCI_CAN_DO_8BITBUS: u32 = 0x00040000;
pub const SDHCI_CAN_DO_ADMA2: u32 = 0x00080000;
pub const SDHCI_CAN_DO_HISPD: u32 = 0x00200000;
pub const SDHCI_CAN_DO_DMA: u32 = 0x00400000;
pub const SDHCI_CAN_DO_SUSPEND: u32 = 0x00800000;
pub const SDHCI_CAN_VDD_330: u32 = 0x01000000;
pub const SDHCI_CAN_VDD_300: u32 = 0x02000000;
pub const SDHCI_CAN_VDD_180: u32 = 0x04000000;
pub const SDHCI_CAN_DO_64BIT: u32 = 0x10000000;
pub const SDHCI_CAN_ASYNC_INTR: u32 = 0x20000000;
pub const SDHCI_SLOTTYPE_MASK: u32 = 0xC0000000;
pub const SDHCI_SLOTTYPE_REMOVABLE: u32 = 0x00000000;
pub const SDHCI_SLOTTYPE_EMBEDDED: u32 = 0x40000000;
pub const SDHCI_SLOTTYPE_SHARED: u32 = 0x80000000;
pub const SDHCI_CAPABILITIES2: u32 = 0x44;
pub const SDHCI_CAN_SDR50: u32 = 0x00000001;
pub const SDHCI_CAN_SDR104: u32 = 0x00000002;
pub const SDHCI_CAN_DDR50: u32 = 0x00000004;
pub const SDHCI_CAN_DRIVE_TYPE_A: u32 = 0x00000010;
pub const SDHCI_CAN_DRIVE_TYPE_C: u32 = 0x00000020;
pub const SDHCI_CAN_DRIVE_TYPE_D: u32 = 0x00000040;
pub const SDHCI_RETUNE_CNT_MASK: u32 = 0x00000F00;
pub const SDHCI_RETUNE_CNT_SHIFT: u32 = 8;
pub const SDHCI_TUNE_SDR50: u32 = 0x00002000;
pub const SDHCI_RETUNE_MODES_MASK: u32 = 0x0000C000;
pub const SDHCI_RETUNE_MODES_SHIFT: u32 = 14;
pub const SDHCI_CLOCK_MULT_MASK: u32 = 0x00FF0000;
pub const SDHCI_CLOCK_MULT_SHIFT: u32 = 16;
pub const SDHCI_CAN_MMC_HS400: u32 = 0x80000000;
pub const SDHCI_MAX_CURRENT: u32 = 0x48;
pub const SDHCI_FORCE_AUTO_EVENT: u32 = 0x50;
pub const SDHCI_FORCE_INTR_EVENT: u32 = 0x52;
pub const SDHCI_ADMA_ERR: u32 = 0x54;
pub const SDHCI_ADMA_ERR_LENGTH: u32 = 0x04;
pub const SDHCI_ADMA_ERR_STATE_MASK: u32 = 0x03;
pub const SDHCI_ADMA_ERR_STATE_STOP: u32 = 0x00;
pub const SDHCI_ADMA_ERR_STATE_FDS: u32 = 0x01;
pub const SDHCI_ADMA_ERR_STATE_TFR: u32 = 0x03;
pub const SDHCI_ADMA_ADDRESS_LO: u32 = 0x58;
pub const SDHCI_ADMA_ADDRESS_HI: u32 = 0x5C;
pub const SDHCI_PRESET_VALUE: u32 = 0x60;
pub const SDHCI_SHARED_BUS_CTRL: u32 = 0xE0;
pub const SDHCI_SLOT_INT_STATUS: u32 = 0xFC;
pub const SDHCI_HOST_VERSION: u32 = 0xFE;
pub const SDHCI_VENDOR_VER_MASK: u32 = 0xFF00;
pub const SDHCI_VENDOR_VER_SHIFT: u32 = 8;
pub const SDHCI_SPEC_VER_MASK: u32 = 0x00FF;
pub const SDHCI_SPEC_VER_SHIFT: u32 = 0;
pub const SDHCI_SPEC_100: u32 = 0;
pub const SDHCI_SPEC_200: u32 = 1;
pub const SDHCI_SPEC_300: u32 = 2;
pub const SDHCI_SPEC_400: u32 = 3;
pub const SDHCI_SPEC_410: u32 = 4;
pub const SDHCI_SPEC_420: u32 = 5;
pub const SDHCI_HAVE_DMA: u32 = 0x01;
pub const SDHCI_PLATFORM_TRANSFER: u32 = 0x02;
pub const SDHCI_NON_REMOVABLE: u32 = 0x04;
pub const SDHCI_TUNING_SUPPORTED: u32 = 0x08;
pub const SDHCI_TUNING_ENABLED: u32 = 0x10;
pub const SDHCI_SDR50_NEEDS_TUNING: u32 = 0x20;
pub const SDHCI_SLOT_EMBEDDED: u32 = 0x40;
pub const SDHCI_RETUNE_MODE_1: u32 = 0x00;
pub const SDHCI_RETUNE_MODE_2: u32 = 0x01;
pub const SDHCI_RETUNE_MODE_3: u32 = 0x02;
pub const SDHCI_RETUNE_REQ_NEEDED: u32 = 0x01;
pub const SDHCI_RETUNE_REQ_RESET: u32 = 0x02;
pub const SDHCI_USE_DMA: u32 = 4;
pub const SDHCI_VERSION: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_size_and_sdma_boundary_match_upstream_encoding() {
        assert_eq!(make_block_size(5, 512), 0x5000 | 512);
        assert_eq!(sdma_bounce_buffer_size(0), 4096);
        assert_eq!(sdma_bounce_buffer_size(5), 131_072);
    }

    #[test]
    fn host_register_offsets_and_masks_are_stable() {
        assert_eq!(SDHCI_SOFTWARE_RESET, 0x2f);
        assert_eq!(SDHCI_INT_STATUS, 0x30);
        assert_eq!(SDHCI_CAPABILITIES2, 0x44);
        assert_eq!(SDHCI_INT_CMD_MASK, 0x000f_0001);
        assert_eq!(SDHCI_SLOTTYPE_MASK, 0xc000_0000);
    }
}
