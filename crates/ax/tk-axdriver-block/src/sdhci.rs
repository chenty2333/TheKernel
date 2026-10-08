//! SD Host Controller Interface register definitions and quirk bits.
//!
//! Translated from FreeBSD `sys/dev/sdhci/sdhci.h` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-2-Clause).
//! Copyright (c) 2008 Alexander Motin <mav@FreeBSD.org>.
//! SPDX-License-Identifier: BSD-2-Clause

use alloc::{format, string::String, sync::Arc};
use core::{
    ptr::NonNull,
    sync::atomic::{Ordering, fence},
};

use spin::Mutex;

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
const MMC_OCR_LOW_VOLTAGE: u32 = 1 << 7;
const MMC_OCR_290_300: u32 = 1 << 17;
const MMC_OCR_300_310: u32 = 1 << 18;
const MMC_OCR_320_330: u32 = 1 << 20;
const MMC_OCR_330_340: u32 = 1 << 21;
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
pub const SDHCI_INT_CMD_ERROR_MASK: u32 =
    SDHCI_INT_TIMEOUT | SDHCI_INT_CRC | SDHCI_INT_END_BIT | SDHCI_INT_INDEX;
pub const SDHCI_INT_CMD_MASK: u32 = SDHCI_INT_RESPONSE | SDHCI_INT_CMD_ERROR_MASK;
pub const SDHCI_INT_DATA_MASK: u32 = SDHCI_INT_DATA_END
    | SDHCI_INT_DMA_END
    | SDHCI_INT_DATA_AVAIL
    | SDHCI_INT_SPACE_AVAIL
    | SDHCI_INT_DATA_TIMEOUT
    | SDHCI_INT_DATA_CRC
    | SDHCI_INT_DATA_END_BIT;
pub const SDHCI_DIVIDERS_MASK: u32 =
    (SDHCI_DIVIDER_MASK << SDHCI_DIVIDER_SHIFT) | (SDHCI_DIVIDER_HI_MASK << SDHCI_DIVIDER_HI_SHIFT);
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
pub const SDHCI_200_MAX_DIVIDER: u32 = 256;
pub const SDHCI_300_MAX_DIVIDER: u32 = 2046;
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

/// Minimal MMIO boundary for one SDHCI slot.
pub trait SdhciIo: Send + Sync {
    fn read8(&mut self, offset: usize) -> u8;
    fn read16(&mut self, offset: usize) -> u16;
    fn read32(&mut self, offset: usize) -> u32;
    fn write8(&mut self, offset: usize, value: u8);
    fn write16(&mut self, offset: usize, value: u16);
    fn write32(&mut self, offset: usize, value: u32);
    fn delay_us(&mut self, micros: u32);
}

/// Error from a bounded SDHCI command or data transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SdhciError {
    Timeout,
    Controller(u32),
    NoCard,
    InvalidTransfer,
    UnsupportedClock,
}

/// The four response registers returned by the controller.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SdhciResponse(pub [u32; 4]);

/// Platform-owned, physically contiguous memory for SDMA bounce transfers.
pub struct SdhciDmaRegion {
    cpu: NonNull<u8>,
    bus: u64,
    len: usize,
    pages: usize,
    release: Option<unsafe fn(NonNull<u8>, usize)>,
}

// SAFETY: the allocation is pinned; DMA is serialized by the owning host.
unsafe impl Send for SdhciDmaRegion {}
unsafe impl Sync for SdhciDmaRegion {}

impl SdhciDmaRegion {
    /// # Safety
    /// `cpu..cpu+len` must be a pinned coherent DMA allocation mapping `bus`;
    /// `release`, when supplied, releases exactly `pages` after DMA stops.
    pub unsafe fn from_raw_parts(
        cpu: NonNull<u8>,
        bus: u64,
        len: usize,
        pages: usize,
        release: Option<unsafe fn(NonNull<u8>, usize)>,
    ) -> Self {
        Self {
            cpu,
            bus,
            len,
            pages,
            release,
        }
    }
}

impl Drop for SdhciDmaRegion {
    fn drop(&mut self) {
        if let Some(release) = self.release {
            // SAFETY: host destruction follows DMA quiescence.
            unsafe { release(self.cpu, self.pages) };
        }
    }
}

/// Capacity and partition metadata decoded from the 512-byte eMMC EXT_CSD.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MmcExtCsd {
    pub sectors: u32,
    pub card_type: u8,
    pub partition_config: u8,
    pub partition_support: u8,
    pub boot_sectors: u32,
    pub rpmb_sectors: u32,
    pub erase_group_sectors: u32,
}

/// Stable identity fields decoded from SD or MMC CID.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MmcCid {
    pub manufacturer_id: u8,
    pub oem_id: u16,
    pub product_name: [u8; 6],
    pub product_revision: u8,
    pub serial_number: u32,
    pub manufacturing_year: u16,
    pub manufacturing_month: u8,
}

/// Fields decoded from an SD or MMC CSD register.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MmcCsd {
    pub structure: u8,
    pub spec_version: u8,
    pub transfer_rate_hz: u32,
    pub command_classes: u16,
    pub read_block_len: u32,
    pub write_block_len: u32,
    pub capacity_bytes: u64,
    pub erase_block_sectors: u32,
    pub read_current_min_ma: u32,
    pub read_current_max_ma: u32,
    pub write_current_min_ma: u32,
    pub write_current_max_ma: u32,
    pub read_partial: bool,
    pub write_partial: bool,
    pub read_misaligned: bool,
    pub write_misaligned: bool,
    pub write_protect_group_size: u32,
    pub write_protect_group_enabled: bool,
    pub write_to_read_factor: u32,
}

const MMC_EXP: [u32; 8] = [1, 10, 100, 1_000, 10_000, 100_000, 1_000_000, 10_000_000];
const MMC_MANT: [u32; 16] = [
    0, 10, 12, 13, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 70, 80,
];
const MMC_CUR_MIN: [u32; 8] = [500, 1_000, 5_000, 10_000, 25_000, 35_000, 60_000, 100_000];
const MMC_CUR_MAX: [u32; 8] = [
    1_000, 5_000, 10_000, 25_000, 35_000, 45_000, 800_000, 200_000,
];

// upstream: mmc.c mmc_decode_csd_sd() and mmc_decode_csd_mmc()
pub fn decode_csd(response: SdhciResponse, mmc: bool) -> Option<MmcCsd> {
    let structure = response_bits(response, 126, 2) as u8;
    if !mmc && structure > 1 {
        return None;
    }
    let mmc_structure = response_bits(response, 126, 2) as u8;
    let read_block_len = 1u32.checked_shl(response_bits(response, 80, 4))?;
    let write_block_len = 1u32.checked_shl(response_bits(response, 22, 4))?;
    let (capacity_bytes, erase_block_sectors) = if !mmc && structure == 1 {
        (
            (u64::from(response_bits(response, 48, 22)) + 1) * 512 * 1024,
            response_bits(response, 46, 1) * (response_bits(response, 39, 7) + 1)
                + (1 - response_bits(response, 46, 1)),
        )
    } else if !mmc {
        let size = u64::from(response_bits(response, 62, 12) + 1);
        let multiplier = response_bits(response, 47, 3) + 2;
        (
            size.checked_shl(multiplier + read_block_len.trailing_zeros())?,
            response_bits(response, 46, 1) * (response_bits(response, 39, 7) + 1)
                + (1 - response_bits(response, 46, 1)),
        )
    } else {
        let size = u64::from(response_bits(response, 62, 12) + 1);
        let multiplier = response_bits(response, 47, 3) + 2;
        (
            size.checked_shl(multiplier + read_block_len.trailing_zeros())?,
            (response_bits(response, 42, 5) + 1) * (response_bits(response, 37, 5) + 1),
        )
    };
    let transfer_rate = MMC_EXP[response_bits(response, 96, 3) as usize]
        .saturating_mul(10_000)
        .saturating_mul(MMC_MANT[response_bits(response, 99, 4) as usize]);
    Some(MmcCsd {
        structure: if mmc { mmc_structure } else { structure },
        spec_version: if mmc {
            response_bits(response, 122, 4) as u8
        } else {
            0
        },
        transfer_rate_hz: transfer_rate,
        command_classes: response_bits(response, 84, 12) as u16,
        read_block_len,
        write_block_len,
        capacity_bytes,
        erase_block_sectors,
        read_current_min_ma: if mmc || structure == 0 {
            MMC_CUR_MIN[response_bits(response, 59, 3) as usize]
        } else {
            0
        },
        read_current_max_ma: if mmc || structure == 0 {
            MMC_CUR_MAX[response_bits(response, 56, 3) as usize]
        } else {
            0
        },
        write_current_min_ma: if mmc || structure == 0 {
            MMC_CUR_MIN[response_bits(response, 53, 3) as usize]
        } else {
            0
        },
        write_current_max_ma: if mmc || structure == 0 {
            MMC_CUR_MAX[response_bits(response, 50, 3) as usize]
        } else {
            0
        },
        read_partial: response_bits(response, 79, 1) != 0,
        write_partial: response_bits(response, 21, 1) != 0,
        read_misaligned: response_bits(response, 77, 1) != 0,
        write_misaligned: response_bits(response, 78, 1) != 0,
        write_protect_group_size: response_bits(response, 32, if mmc { 5 } else { 7 }),
        write_protect_group_enabled: response_bits(response, 31, 1) != 0,
        write_to_read_factor: 1u32.checked_shl(response_bits(response, 26, 3))?,
    })
}

/// Format the upstream MMC card ID and serial strings used by mmcsd.
// upstream: mmc.c mmc_format_card_id_string()
pub fn format_card_id(cid: MmcCid, mmc: bool, high_capacity: bool) -> (String, String) {
    let oid = if cid.oem_id <= u16::from(u8::MAX) {
        format!("0x{:04x}", cid.oem_id)
    } else {
        let high = (cid.oem_id >> 8) as u8;
        let low = cid.oem_id as u8;
        if (0x20..0x7f).contains(&high) && (0x20..0x7f).contains(&low) {
            format!("{}{}", high as char, low as char)
        } else {
            format!("0x{:04x}", cid.oem_id)
        }
    };
    let product_end = if mmc { 6 } else { 5 };
    let product: String = cid.product_name[..product_end]
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() {
                *byte as char
            } else {
                '?'
            }
        })
        .collect();
    let card_type = if mmc { "MMC" } else { "SD" };
    let capacity = if high_capacity { "HC" } else { "" };
    let id = format!(
        "{card_type}{capacity} {product} {}.{} SN {:08X} MFG {:02}/{:04} by {} {oid}",
        cid.product_revision >> 4,
        cid.product_revision & 0x0f,
        cid.serial_number,
        cid.manufacturing_month,
        cid.manufacturing_year,
        cid.manufacturer_id,
    );
    (id, format!("{:08X}", cid.serial_number))
}

// upstream: mmc.c mmc_read_ext_csd() decoding
pub fn parse_ext_csd(bytes: &[u8; 512]) -> MmcExtCsd {
    MmcExtCsd {
        sectors: u32::from_le_bytes([bytes[212], bytes[213], bytes[214], bytes[215]]),
        card_type: bytes[196],
        partition_config: bytes[179],
        partition_support: bytes[160],
        boot_sectors: u32::from(bytes[226]) * 256,
        rpmb_sectors: u32::from(bytes[168]) * 256,
        erase_group_sectors: u32::from(bytes[224]) * 1024,
    }
}

/// One bounded, polling SDHCI host slot. The platform supplies MMIO access;
/// command and PIO sequencing follows the generic FreeBSD SDHCI engine.
pub struct SdhciHost<I: SdhciIo> {
    io: I,
    capabilities: u32,
    capabilities2: u32,
    version: u8,
    base_clock_hz: u32,
    clock_hz: u32,
    power: u32,
    quirks: u32,
    dma: Option<SdhciDmaRegion>,
    dma_inflight: bool,
    single_block_only: bool,
    timeout_polls: usize,
}

impl<I: SdhciIo> SdhciHost<I> {
    pub const fn new(io: I, capabilities: u32, capabilities2: u32, version: u8) -> Self {
        Self::new_with_quirks(io, capabilities, capabilities2, version, 0)
    }

    pub const fn new_with_quirks(
        io: I,
        capabilities: u32,
        capabilities2: u32,
        version: u8,
        quirks: u32,
    ) -> Self {
        let base_mhz = (capabilities & SDHCI_CLOCK_V3_BASE_MASK) >> SDHCI_CLOCK_BASE_SHIFT;
        Self {
            io,
            capabilities,
            capabilities2,
            version,
            base_clock_hz: base_mhz * 1_000_000,
            clock_hz: 0,
            power: 0,
            quirks,
            dma: None,
            dma_inflight: false,
            single_block_only: false,
            timeout_polls: 100_000,
        }
    }

    pub fn io_mut(&mut self) -> &mut I {
        &mut self.io
    }

    pub fn with_dma_region(mut self, region: SdhciDmaRegion) -> Self {
        self.dma = Some(region);
        self
    }

    pub fn with_single_block_only(mut self) -> Self {
        self.single_block_only = true;
        self
    }

    pub const fn capabilities(&self) -> u32 {
        self.capabilities
    }

    pub const fn capabilities2(&self) -> u32 {
        self.capabilities2
    }

    pub const fn allows_multi_block(&self) -> bool {
        !self.single_block_only
    }

    pub const fn clock_hz(&self) -> u32 {
        self.clock_hz
    }

    // upstream: sdhci.c sdhci_generic_get_ro()
    pub fn card_write_protected(&mut self) -> bool {
        self.io.read32(SDHCI_PRESENT_STATE as usize) & SDHCI_WRITE_PROTECT == 0
    }

    // upstream: sdhci.c sdhci_generic_switch_vccq()
    pub fn switch_signal_voltage_18v(&mut self) -> Result<(), SdhciError> {
        if self.version < SDHCI_SPEC_300 as u8 || self.capabilities & SDHCI_CAN_VDD_180 == 0 {
            return Err(SdhciError::UnsupportedClock);
        }
        let clock = self.io.read16(SDHCI_CLOCK_CONTROL as usize);
        self.io.write16(
            SDHCI_CLOCK_CONTROL as usize,
            clock & !(SDHCI_CLOCK_CARD_EN as u16),
        );
        for _ in 0..self.timeout_polls {
            if self.io.read32(SDHCI_PRESENT_STATE as usize) & SDHCI_STATE_DAT_MASK == 0 {
                break;
            }
            self.io.delay_us(10);
        }
        if self.io.read32(SDHCI_PRESENT_STATE as usize) & SDHCI_STATE_DAT_MASK != 0 {
            self.io.write16(
                SDHCI_CLOCK_CONTROL as usize,
                clock | SDHCI_CLOCK_CARD_EN as u16,
            );
            return Err(SdhciError::Timeout);
        }
        let control2 = self.io.read16(SDHCI_HOST_CONTROL2 as usize);
        self.io.write16(
            SDHCI_HOST_CONTROL2 as usize,
            control2 | SDHCI_CTRL2_S18_ENABLE as u16,
        );
        self.io.delay_us(5_000);
        if self.io.read32(SDHCI_PRESENT_STATE as usize) & SDHCI_STATE_DAT_MASK
            != SDHCI_STATE_DAT_MASK
        {
            self.io.write16(
                SDHCI_HOST_CONTROL2 as usize,
                control2 & !(SDHCI_CTRL2_S18_ENABLE as u16),
            );
            self.io.write16(
                SDHCI_CLOCK_CONTROL as usize,
                clock | SDHCI_CLOCK_CARD_EN as u16,
            );
            return Err(SdhciError::Timeout);
        }
        self.io.write16(
            SDHCI_CLOCK_CONTROL as usize,
            clock | SDHCI_CLOCK_CARD_EN as u16,
        );
        Ok(())
    }

    // upstream: sdhci.c sdhci_generic_tune()/sdhci_exec_tuning()
    pub fn execute_tuning(&mut self, opcode: u8, bus_width: u8) -> Result<(), SdhciError> {
        if self.version < SDHCI_SPEC_300 as u8
            || !matches!(opcode, 19 | 21)
            || !matches!(bus_width, 1 | 4 | 8)
            || (opcode == 19 && self.capabilities2 & (SDHCI_CAN_SDR50 | SDHCI_CAN_SDR104) == 0)
            || (opcode == 21
                && self.capabilities2 & SDHCI_CAN_MMC_HS400 == 0
                && self.quirks & SDHCI_QUIRK_CAPS_BIT63_FOR_MMC_HS400 == 0)
        {
            return Err(SdhciError::UnsupportedClock);
        }
        let length = if opcode == 21 && bus_width == 8 {
            128
        } else {
            64
        };
        let mut control =
            self.io.read16(SDHCI_HOST_CONTROL2 as usize) | SDHCI_CTRL2_EXEC_TUNING as u16;
        self.io.write16(SDHCI_HOST_CONTROL2 as usize, control);
        for _ in 0..40 {
            let mut tuning_block = [0u8; 128];
            self.command(
                opcode,
                0,
                SD_R1 | SD_DATA,
                Some(&mut tuning_block[..length]),
                length,
            )?;
            control = self.io.read16(SDHCI_HOST_CONTROL2 as usize);
            if control & SDHCI_CTRL2_EXEC_TUNING as u16 == 0 {
                if control & SDHCI_CTRL2_SAMPLING_CLOCK as u16 != 0 {
                    return Ok(());
                }
                return Err(SdhciError::Controller(u32::from(control)));
            }
        }
        Err(SdhciError::Timeout)
    }

    // upstream: sdhci.c sdhci_set_bus_width()
    fn set_bus_width(&mut self, width: u8) {
        let mut control = self.io.read8(SDHCI_HOST_CONTROL as usize);
        control &= !(SDHCI_CTRL_4BITBUS | SDHCI_CTRL_8BITBUS) as u8;
        if width == 4 {
            control |= SDHCI_CTRL_4BITBUS as u8;
        } else if width == 8 {
            control |= SDHCI_CTRL_8BITBUS as u8;
        }
        self.io.write8(SDHCI_HOST_CONTROL as usize, control);
        if self.quirks & SDHCI_QUIRK_RESET_ON_IOS != 0 {
            let _ = self.reset(SDHCI_RESET_CMD as u8 | SDHCI_RESET_DATA as u8);
        }
    }

    // upstream: sdhci.c sdhci_set_uhs_timing() (legacy high-speed subset)
    fn set_high_speed(&mut self, clock_hz: u32) -> Result<(), SdhciError> {
        if self.quirks & SDHCI_QUIRK_BROKEN_TIMINGS != 0 {
            return Err(SdhciError::UnsupportedClock);
        }
        let mut control = self.io.read8(SDHCI_HOST_CONTROL as usize);
        control |= SDHCI_CTRL_HISPD as u8;
        self.io.write8(SDHCI_HOST_CONTROL as usize, control);
        let clock_hz = if self.quirks & SDHCI_QUIRK_LOWER_FREQUENCY != 0 {
            clock_hz.min(self.base_clock_hz / 2)
        } else {
            clock_hz
        };
        self.set_clock(clock_hz)
    }

    /// Reset command/data engines and establish the initial 400 kHz clock.
    // upstream: sdhci.c sdhci_generic_reset()
    pub fn initialize(&mut self) -> Result<(), SdhciError> {
        if self.quirks & SDHCI_QUIRK_CLOCK_BEFORE_RESET != 0 {
            self.set_clock(400_000).inspect_err(|error| {
                log::warn!("sdhci: initial clock-before-reset failed: {error:?}");
            })?;
        }
        if self.quirks & SDHCI_QUIRK_NO_CARD_NO_RESET == 0
            || self.io.read32(SDHCI_PRESENT_STATE as usize) & SDHCI_CARD_PRESENT != 0
        {
            self.reset(SDHCI_RESET_ALL as u8).inspect_err(|error| {
                log::warn!("sdhci: host reset failed: {error:?}");
            })?;
        }
        self.set_power(MMC_OCR_330_340)?;
        self.set_clock(400_000).inspect_err(|error| {
            log::warn!("sdhci: initial 400kHz clock failed: {error:?}");
        })?;
        // Use the largest host timeout exponent unless a future platform
        // integration provides the per-card timeout derived from CSD/EXT_CSD.
        let timeout = if self.quirks
            & (SDHCI_QUIRK_INCR_TIMEOUT_CONTROL | SDHCI_QUIRK_BROKEN_TIMEOUT_VAL)
            != 0
        {
            0x0f
        } else {
            0x0e
        };
        self.io
            .write8(SDHCI_TIMEOUT_CONTROL as usize, timeout as u8);
        self.io.write32(SDHCI_INT_STATUS as usize, u32::MAX);
        self.io.write32(
            SDHCI_INT_ENABLE as usize,
            SDHCI_INT_RESPONSE
                | SDHCI_INT_DATA_END
                | SDHCI_INT_SPACE_AVAIL
                | SDHCI_INT_DATA_AVAIL
                | SDHCI_INT_DMA_END
                | SDHCI_INT_ERROR
                | SDHCI_INT_CMD_ERROR_MASK
                | SDHCI_INT_DATA_TIMEOUT
                | SDHCI_INT_DATA_CRC
                | SDHCI_INT_DATA_END_BIT,
        );
        self.io.write32(SDHCI_SIGNAL_ENABLE as usize, 0);
        Ok(())
    }

    fn reset(&mut self, mask: u8) -> Result<(), SdhciError> {
        self.io.write8(SDHCI_SOFTWARE_RESET as usize, mask);
        if self.quirks & SDHCI_QUIRK_WAITFOR_RESET_ASSERTED != 0 {
            let mut asserted = false;
            for _ in 0..self.timeout_polls {
                if self.io.read8(SDHCI_SOFTWARE_RESET as usize) & mask != 0 {
                    asserted = true;
                    break;
                }
                self.io.delay_us(10);
            }
            if !asserted {
                return Err(SdhciError::Timeout);
            }
        }
        for _ in 0..self.timeout_polls {
            if self.io.read8(SDHCI_SOFTWARE_RESET as usize) & mask == 0 {
                return Ok(());
            }
            self.io.delay_us(10);
        }
        Err(SdhciError::Timeout)
    }

    fn set_clock(&mut self, target_hz: u32) -> Result<(), SdhciError> {
        if self.base_clock_hz == 0 || target_hz == 0 {
            return Err(SdhciError::UnsupportedClock);
        }
        self.io.write16(SDHCI_CLOCK_CONTROL as usize, 0);
        let (divisor, encoded_divisor) =
            calculate_clock_divider(self.base_clock_hz, target_hz, self.version);
        let encoded = (((encoded_divisor & SDHCI_DIVIDER_MASK) << SDHCI_DIVIDER_SHIFT)
            | (((encoded_divisor >> SDHCI_DIVIDER_MASK_LEN) & SDHCI_DIVIDER_HI_MASK)
                << SDHCI_DIVIDER_HI_SHIFT)) as u16;
        self.io.write16(
            SDHCI_CLOCK_CONTROL as usize,
            encoded | SDHCI_CLOCK_INT_EN as u16,
        );
        for _ in 0..self.timeout_polls {
            if self.io.read16(SDHCI_CLOCK_CONTROL as usize) & SDHCI_CLOCK_INT_STABLE as u16 != 0 {
                self.io.write16(
                    SDHCI_CLOCK_CONTROL as usize,
                    encoded | SDHCI_CLOCK_INT_EN as u16 | SDHCI_CLOCK_CARD_EN as u16,
                );
                self.clock_hz = self.base_clock_hz / divisor;
                return Ok(());
            }
            self.io.delay_us(10);
        }
        Err(SdhciError::Timeout)
    }

    // upstream: sdhci.c sdhci_set_power()
    fn set_power(&mut self, power: u32) -> Result<(), SdhciError> {
        if self.power == power {
            return Ok(());
        }
        self.io.write8(SDHCI_POWER_CONTROL as usize, 0);
        if power == 0 {
            self.power = 0;
            return Ok(());
        }
        let voltage = match power {
            MMC_OCR_LOW_VOLTAGE => SDHCI_POWER_180,
            MMC_OCR_290_300 | MMC_OCR_300_310 => SDHCI_POWER_300,
            MMC_OCR_320_330 | MMC_OCR_330_340 => SDHCI_POWER_330,
            _ => return Err(SdhciError::UnsupportedClock),
        } as u8;
        self.power = power;
        self.io.write8(SDHCI_POWER_CONTROL as usize, voltage);
        let power_control = voltage | SDHCI_POWER_ON as u8;
        let mut enabled = false;
        for _ in 0..20 {
            self.io.write8(SDHCI_POWER_CONTROL as usize, power_control);
            if self.io.read8(SDHCI_POWER_CONTROL as usize) & SDHCI_POWER_ON as u8 != 0 {
                enabled = true;
                break;
            }
            self.io.delay_us(100);
        }
        if !enabled {
            log::warn!("sdhci: bus power failed to enable");
        }
        if self.quirks & SDHCI_QUIRK_INTEL_POWER_UP_RESET != 0 {
            self.io
                .write8(SDHCI_POWER_CONTROL as usize, power_control | 0x10);
            self.io.delay_us(10);
            self.io.write8(SDHCI_POWER_CONTROL as usize, power_control);
            self.io.delay_us(300);
        }
        Ok(())
    }

    fn wait_status(&mut self, mask: u32) -> Result<u32, SdhciError> {
        for _ in 0..self.timeout_polls {
            let status = self.io.read32(SDHCI_INT_STATUS as usize);
            let errors = status & SDHCI_INT_ERROR_MASK;
            if errors != 0 {
                self.io.write32(SDHCI_INT_STATUS as usize, status);
                return Err(SdhciError::Controller(errors));
            }
            if status & mask != 0 {
                self.io.write32(SDHCI_INT_STATUS as usize, status & mask);
                return Ok(status);
            }
            self.io.delay_us(10);
        }
        Err(SdhciError::Timeout)
    }

    // upstream: sdhci.c sdhci_wait_for_busy()
    fn wait_busy(&mut self) -> Result<(), SdhciError> {
        for _ in 0..self.timeout_polls {
            if self.io.read32(SDHCI_PRESENT_STATE as usize) & (SDHCI_DAT_INHIBIT | SDHCI_DAT_ACTIVE)
                == 0
            {
                return Ok(());
            }
            self.io.delay_us(10);
        }
        log::warn!(
            "sdhci: busy wait timed out, PRESENT_STATE={:#010x}",
            self.io.read32(SDHCI_PRESENT_STATE as usize)
        );
        Err(SdhciError::Timeout)
    }

    // upstream: sdhci.c sdhci_card_present()
    fn card_present(&mut self) -> bool {
        if self.quirks & SDHCI_QUIRK_ALL_SLOTS_NON_REMOVABLE != 0
            || self.capabilities & SDHCI_SLOTTYPE_MASK == SDHCI_SLOTTYPE_EMBEDDED
        {
            return true;
        }
        if self.quirks & SDHCI_QUIRK_POLL_CARD_PRESENT != 0 {
            for _ in 0..self.timeout_polls {
                if self.io.read32(SDHCI_PRESENT_STATE as usize) & SDHCI_CARD_PRESENT != 0 {
                    return true;
                }
                self.io.delay_us(10);
            }
            return false;
        }
        self.io.read32(SDHCI_PRESENT_STATE as usize) & SDHCI_CARD_PRESENT != 0
    }

    // upstream: mmc.c mmc_wait_for_app_cmd()
    fn application_command(
        &mut self,
        rca: u16,
        index: u8,
        argument: u32,
        flags: u16,
    ) -> Result<SdhciResponse, SdhciError> {
        let prefix = self.command(SD_CMD_APP, u32::from(rca) << 16, SD_R1, None, 0)?;
        if prefix.0[0] & (1 << 5) == 0 {
            return Err(SdhciError::Controller(prefix.0[0]));
        }
        self.command(index, argument, flags, None, 0)
    }

    // upstream: mmc.c mmc_switch()
    fn mmc_switch(&mut self, index: u8, value: u8) -> Result<(), SdhciError> {
        let argument = (3 << 24) | (u32::from(index) << 16) | (u32::from(value) << 8);
        self.command(MMC_CMD_SWITCH, argument, SD_R1B, None, 0)?;
        self.wait_busy()
    }

    // upstream: mmc.c mmc_sd_switch()
    pub fn switch_sd_function(
        &mut self,
        mode: u8,
        group: u8,
        value: u8,
        status: &mut [u8; 64],
    ) -> Result<(), SdhciError> {
        if mode > 1 || group > 5 || value > 0xf {
            return Err(SdhciError::InvalidTransfer);
        }
        status.fill(0);
        let shift = u32::from(group) * 4;
        let argument =
            (u32::from(mode) << 31) | (0x00ff_ffff & !(0xf << shift)) | (u32::from(value) << shift);
        self.command(
            SD_CMD_SWITCH_FUNC,
            argument,
            SD_R1 | SD_DATA,
            Some(status),
            64,
        )?;
        Ok(())
    }

    /// Issue one command and optional PIO data transfer. Multi-block requests
    /// are bounded by the 16-bit SDHCI block-count register.
    // upstream: sdhci.c sdhci_generic_request()
    pub fn command(
        &mut self,
        index: u8,
        argument: u32,
        command_flags: u16,
        data: Option<&mut [u8]>,
        block_size: usize,
    ) -> Result<SdhciResponse, SdhciError> {
        let read = matches!(index, 8 | SD_CMD_READ_SINGLE | SD_CMD_READ_MULTIPLE)
            || (matches!(index, SD_CMD_SWITCH_FUNC | 19 | 21) && data.is_some());
        let retries = if data.is_none() || read { 3 } else { 1 };
        let mut data = data;
        let mut last_error = SdhciError::Timeout;
        for attempt in 0..retries {
            match self.command_once(
                index,
                argument,
                command_flags,
                data.as_deref_mut(),
                block_size,
            ) {
                Ok(response) => {
                    if command_flags & SDHCI_CMD_RESP_MASK as u16
                        == SDHCI_CMD_RESP_SHORT_BUSY as u16
                    {
                        self.wait_busy().inspect_err(|error| {
                            log::warn!("sdhci: CMD{index} busy response failed: {error:?}");
                        })?;
                    }
                    return Ok(response);
                }
                Err(error) => {
                    last_error = error;
                    // Reads and response-only commands are idempotent. Never
                    // replay a potentially accepted write command.
                    let _ = self.reset(SDHCI_RESET_CMD as u8 | SDHCI_RESET_DATA as u8);
                    self.io.write32(SDHCI_INT_STATUS as usize, u32::MAX);
                    if self.dma_inflight {
                        // The reset path cannot prove that a broken SDMA
                        // engine has stopped bus mastering; retain the buffer.
                        if let Some(dma) = self.dma.as_mut() {
                            dma.release = None;
                        }
                        let _quarantined = self.dma.take();
                        self.dma_inflight = false;
                    }
                    if attempt + 1 < retries {
                        self.io.delay_us(1_000);
                    } else {
                        log::warn!(
                            "sdhci: CMD{index} failed after {retries} attempt(s): {error:?}"
                        );
                    }
                }
            }
        }
        Err(last_error)
    }

    fn command_once(
        &mut self,
        index: u8,
        argument: u32,
        command_flags: u16,
        data: Option<&mut [u8]>,
        block_size: usize,
    ) -> Result<SdhciResponse, SdhciError> {
        if !self.card_present() {
            return Err(SdhciError::NoCard);
        }
        let (transfer, blocks) = if let Some(buffer) = data.as_ref() {
            if block_size == 0
                || !buffer.len().is_multiple_of(block_size)
                || buffer.len() / block_size > u16::MAX as usize
            {
                return Err(SdhciError::InvalidTransfer);
            }
            let count = buffer.len() / block_size;
            let mut mode = SDHCI_TRNS_BLK_CNT_EN as u16;
            if count > 1 {
                mode |= SDHCI_TRNS_MULTI as u16;
            }
            if command_flags & SDHCI_CMD_DATA as u16 != 0
                && (matches!(index, 8 | SD_CMD_READ_SINGLE | SD_CMD_READ_MULTIPLE)
                    || matches!(index, SD_CMD_SWITCH_FUNC | 19 | 21))
            {
                mode |= SDHCI_TRNS_READ as u16;
            }
            (Some(mode), count as u16)
        } else {
            (None, 0)
        };
        let inhibit = SDHCI_CMD_INHIBIT
            | if transfer.is_some() {
                SDHCI_DAT_INHIBIT
            } else {
                0
            };
        for _ in 0..self.timeout_polls {
            if self.io.read32(SDHCI_PRESENT_STATE as usize) & inhibit == 0 {
                break;
            }
            self.io.delay_us(10);
        }
        if self.io.read32(SDHCI_PRESENT_STATE as usize) & inhibit != 0 {
            return Err(SdhciError::Timeout);
        }
        let data_len = data.as_ref().map_or(0, |buffer| buffer.len());
        let use_sdma = transfer.is_some()
            && self.version >= SDHCI_SPEC_200 as u8
            && self.quirks & SDHCI_QUIRK_BROKEN_DMA == 0
            && (self.capabilities & SDHCI_CAN_DO_DMA != 0
                || self.quirks & SDHCI_QUIRK_FORCE_DMA != 0)
            && self.dma.as_ref().is_some_and(|dma| {
                data_len <= dma.len
                    && data_len <= sdma_bounce_buffer_size(SDHCI_BLKSZ_SDMA_BNDRY_512K)
                    && dma
                        .bus
                        .checked_add(data_len as u64)
                        .is_some_and(|end| end <= u64::from(u32::MAX) + 1)
            });
        let read_transfer = matches!(index, 8 | SD_CMD_READ_SINGLE | SD_CMD_READ_MULTIPLE)
            || (matches!(index, SD_CMD_SWITCH_FUNC | 19 | 21) && data.is_some());
        if use_sdma {
            let dma = self
                .dma
                .as_ref()
                .expect("SDMA predicate checked allocation");
            if !read_transfer {
                if let Some(buffer) = data.as_ref() {
                    // SAFETY: the caller buffer and DMA bounce are disjoint
                    // owned regions, and `data_len <= dma.len` was checked.
                    unsafe {
                        core::ptr::copy_nonoverlapping(buffer.as_ptr(), dma.cpu.as_ptr(), data_len);
                    }
                }
            }
            self.io.write32(SDHCI_DMA_ADDRESS as usize, dma.bus as u32);
            let mut host_control = self.io.read8(SDHCI_HOST_CONTROL as usize);
            host_control &= !(SDHCI_CTRL_DMA_MASK as u8);
            host_control |= SDHCI_CTRL_SDMA as u8;
            self.io.write8(SDHCI_HOST_CONTROL as usize, host_control);
            fence(Ordering::Release);
            self.dma_inflight = true;
        }
        self.io.write32(SDHCI_INT_STATUS as usize, u32::MAX);
        if let (Some(mut mode), Some(_buffer)) = (transfer, data.as_ref()) {
            let boundary = if use_sdma {
                SDHCI_BLKSZ_SDMA_BNDRY_512K
            } else {
                SDHCI_BLKSZ_SDMA_BNDRY_4K
            };
            if use_sdma {
                mode |= SDHCI_TRNS_DMA as u16;
            }
            self.io.write16(
                SDHCI_BLOCK_SIZE as usize,
                make_block_size(boundary, block_size as u32) as u16,
            );
            self.io.write16(SDHCI_BLOCK_COUNT as usize, blocks);
            self.io.write16(SDHCI_TRANSFER_MODE as usize, mode);
        }
        self.io.write32(SDHCI_ARGUMENT as usize, argument);
        self.io.write16(
            SDHCI_COMMAND_FLAGS as usize,
            ((index as u16) << 8) | command_flags,
        );
        let mut response = SdhciResponse::default();
        self.wait_status(SDHCI_INT_RESPONSE)?;
        if command_flags & SDHCI_CMD_RESP_MASK as u16 == SDHCI_CMD_RESP_LONG as u16 {
            // R2 response words are stored in reverse register order and the
            // controller strips the wire CRC byte. Reconstruct the standard
            // 128-bit response layout expected by mmc_get_bits().
            let mut extra = 0u32;
            for n in 0..4 {
                let value = self.io.read32(SDHCI_RESPONSE as usize + n * 4);
                response.0[3 - n] = if self.quirks & SDHCI_QUIRK_DONT_SHIFT_RESPONSE != 0 {
                    value
                } else {
                    (value << 8) | extra
                };
                extra = value >> 24;
            }
        } else {
            response.0[0] = self.io.read32(SDHCI_RESPONSE as usize);
        }
        if let Some(buffer) = data {
            if use_sdma {
                self.wait_dma_data_end(data_len)?;
                fence(Ordering::Acquire);
                if read_transfer {
                    let dma = self.dma.as_ref().expect("SDMA region remains owned");
                    // SAFETY: DATA_END retires device DMA before the copy.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            dma.cpu.as_ptr(),
                            buffer.as_mut_ptr(),
                            data_len,
                        );
                    }
                }
            } else {
                let mut offset = 0usize;
                while offset < buffer.len() {
                    self.wait_status(if read_transfer {
                        SDHCI_INT_DATA_AVAIL
                    } else {
                        SDHCI_INT_SPACE_AVAIL
                    })?;
                    let block_end = (offset + block_size).min(buffer.len());
                    while offset < block_end {
                        let end = (offset + 4).min(block_end);
                        if read_transfer {
                            let word = self.io.read32(SDHCI_BUFFER as usize).to_le_bytes();
                            buffer[offset..end].copy_from_slice(&word[..end - offset]);
                        } else {
                            let mut bytes = [0u8; 4];
                            bytes[..end - offset].copy_from_slice(&buffer[offset..end]);
                            self.io
                                .write32(SDHCI_BUFFER as usize, u32::from_le_bytes(bytes));
                        }
                        offset = end;
                    }
                }
                self.wait_status(SDHCI_INT_DATA_END)?;
            }
        }
        Ok(response)
    }

    // upstream: sdhci.c SDHCI_INT_DMA_END transfer-boundary handling
    fn wait_dma_data_end(&mut self, data_len: usize) -> Result<(), SdhciError> {
        let boundary = sdma_bounce_buffer_size(SDHCI_BLKSZ_SDMA_BNDRY_512K);
        let mut offset = 0usize;
        for _ in 0..self.timeout_polls {
            let status = self.io.read32(SDHCI_INT_STATUS as usize);
            let errors = status & SDHCI_INT_ERROR_MASK;
            if errors != 0 {
                self.io.write32(SDHCI_INT_STATUS as usize, status);
                return Err(SdhciError::Controller(errors));
            }
            if status & SDHCI_INT_DATA_END != 0 {
                self.io
                    .write32(SDHCI_INT_STATUS as usize, SDHCI_INT_DATA_END);
                self.dma_inflight = false;
                return Ok(());
            }
            if status & SDHCI_INT_DMA_END != 0 {
                self.io
                    .write32(SDHCI_INT_STATUS as usize, SDHCI_INT_DMA_END);
                offset = offset.saturating_add(boundary);
                if offset >= data_len {
                    continue;
                }
                let dma = self.dma.as_ref().ok_or(SdhciError::InvalidTransfer)?;
                let address = dma
                    .bus
                    .checked_add(offset as u64)
                    .ok_or(SdhciError::InvalidTransfer)?;
                let address = u32::try_from(address).map_err(|_| SdhciError::InvalidTransfer)?;
                self.io.write32(SDHCI_DMA_ADDRESS as usize, address);
            }
            self.io.delay_us(10);
        }
        Err(SdhciError::Timeout)
    }
}

// upstream: sdhci.c sdhci_set_clock() divider selection
fn calculate_clock_divider(base_hz: u32, target_hz: u32, version: u8) -> (u32, u32) {
    if version >= SDHCI_SPEC_300 as u8 {
        let divisor = if target_hz >= base_hz {
            1
        } else {
            let mut divisor = 2;
            while divisor < SDHCI_300_MAX_DIVIDER && base_hz / divisor > target_hz {
                divisor += 2;
            }
            divisor.min(SDHCI_300_MAX_DIVIDER)
        };
        let encoded = if divisor == 1 { 0 } else { divisor >> 1 };
        (divisor, encoded)
    } else {
        let mut divisor = 1;
        while divisor < SDHCI_200_MAX_DIVIDER && base_hz / divisor > target_hz {
            divisor <<= 1;
        }
        divisor = divisor.min(SDHCI_200_MAX_DIVIDER);
        (divisor, divisor >> 1)
    }
}

const SD_CMD_GO_IDLE: u8 = 0;
const MMC_CMD_SEND_OP_COND: u8 = 1;
const MMC_CMD_SWITCH: u8 = 6;
const SD_CMD_ALL_SEND_CID: u8 = 2;
const SD_CMD_SEND_RELATIVE_ADDR: u8 = 3;
const SD_CMD_SEND_CSD: u8 = 9;
const SD_CMD_SELECT_CARD: u8 = 7;
const SD_CMD_STOP_TRANSMISSION: u8 = 12;
const SD_CMD_SEND_STATUS: u8 = 13;
const SD_CMD_SWITCH_FUNC: u8 = 6;
const SD_CMD_SET_BLOCKLEN: u8 = 16;
const SD_CMD_READ_SINGLE: u8 = 17;
const SD_CMD_READ_MULTIPLE: u8 = 18;
const SD_CMD_ERASE_START: u8 = 32;
const SD_CMD_ERASE_END: u8 = 33;
const SD_CMD_ERASE: u8 = 38;
const SD_CMD_WRITE_SINGLE: u8 = 24;
const SD_CMD_WRITE_MULTIPLE: u8 = 25;
const SD_CMD_APP: u8 = 55;
const SD_ACMD_OP_COND: u8 = 41;
const SD_ACMD_SET_BUS_WIDTH: u8 = 6;
const RSP_NONE: u16 = SDHCI_CMD_RESP_NONE as u16;
const SD_R1: u16 = SDHCI_CMD_RESP_SHORT as u16 | SDHCI_CMD_CRC as u16 | SDHCI_CMD_INDEX as u16;
const SD_R1B: u16 =
    SDHCI_CMD_RESP_SHORT_BUSY as u16 | SDHCI_CMD_CRC as u16 | SDHCI_CMD_INDEX as u16;
const SD_R2: u16 = SDHCI_CMD_RESP_LONG as u16;
const SD_R3: u16 = SDHCI_CMD_RESP_SHORT as u16;
const SD_DATA: u16 = SDHCI_CMD_DATA as u16;
const SD_OCR_READY: u32 = 1 << 31;
const SD_OCR_CCS: u32 = 1 << 30;
const SD_OCR_VOLTAGE: u32 = 0x00ff_8000;
const MMC_R1_STATUS_ERRORS: u32 = 0xfff9_8000;

// upstream: mmc.c mmc_send_if_cond()
fn mmc_send_if_cond<I: SdhciIo>(host: &mut SdhciHost<I>) -> bool {
    host.command(8, 0x1aa, SD_R1, None, 0)
        .is_ok_and(|response| response.0[0] & 0xfff == 0x1aa)
}

// upstream: mmc.c mmc_send_app_op_cond()
fn mmc_send_app_op_cond<I: SdhciIo>(
    host: &mut SdhciHost<I>,
    argument: u32,
) -> Result<u32, SdhciError> {
    host.application_command(0, SD_ACMD_OP_COND, argument, SD_R3)
        .map(|response| response.0[0])
}

// upstream: mmc.c mmc_send_op_cond()
fn mmc_send_op_cond<I: SdhciIo>(host: &mut SdhciHost<I>, argument: u32) -> Result<u32, SdhciError> {
    host.command(MMC_CMD_SEND_OP_COND, argument, SD_R3, None, 0)
        .map(|response| response.0[0])
}

// upstream: mmc.c mmc_idle_cards()
fn mmc_idle_cards<I: SdhciIo>(
    host: &mut SdhciHost<I>,
    version2: bool,
) -> Result<(bool, u32), SdhciError> {
    for _ in 0..100 {
        let argument = SD_OCR_VOLTAGE | if version2 { SD_OCR_CCS } else { 0 };
        match mmc_send_app_op_cond(host, argument) {
            Ok(ocr) if ocr & SD_OCR_READY != 0 => return Ok((false, ocr)),
            Ok(_) => host.io.delay_us(10_000),
            Err(_) => break,
        }
    }
    host.command(SD_CMD_GO_IDLE, 0, RSP_NONE, None, 0)?;
    for _ in 0..100 {
        match mmc_send_op_cond(host, SD_OCR_VOLTAGE | SD_OCR_CCS) {
            Ok(ocr) if ocr & SD_OCR_READY != 0 => return Ok((true, ocr)),
            Ok(_) => host.io.delay_us(10_000),
            Err(error) => return Err(error),
        }
    }
    Err(SdhciError::Timeout)
}

// upstream: mmc.c mmc_send_relative_addr()
fn mmc_send_relative_addr<I: SdhciIo>(
    host: &mut SdhciHost<I>,
    mmc: bool,
) -> Result<u16, SdhciError> {
    if mmc {
        host.command(SD_CMD_SEND_RELATIVE_ADDR, 1 << 16, SD_R1, None, 0)?;
        Ok(1)
    } else {
        Ok((host
            .command(SD_CMD_SEND_RELATIVE_ADDR, 0, SD_R1, None, 0)?
            .0[0]
            >> 16) as u16)
    }
}

// upstream: mmc.c mmc_send_csd()
fn mmc_send_csd<I: SdhciIo>(
    host: &mut SdhciHost<I>,
    rca: u16,
) -> Result<SdhciResponse, SdhciError> {
    host.command(SD_CMD_SEND_CSD, u32::from(rca) << 16, SD_R2, None, 0)
}

// upstream: mmc.c mmc_all_send_cid()
fn mmc_all_send_cid<I: SdhciIo>(host: &mut SdhciHost<I>) -> Result<SdhciResponse, SdhciError> {
    host.command(SD_CMD_ALL_SEND_CID, 0, SD_R2, None, 0)
}

// upstream: mmc.c mmc_select_card()
fn mmc_select_card<I: SdhciIo>(host: &mut SdhciHost<I>, rca: u16) -> Result<(), SdhciError> {
    host.command(SD_CMD_SELECT_CARD, u32::from(rca) << 16, SD_R1B, None, 0)
        .map(|_| ())
}

// upstream: mmc.c mmc_set_card_bus_width()
fn mmc_set_card_bus_width<I: SdhciIo>(
    host: &mut SdhciHost<I>,
    mmc: bool,
    rca: u16,
    width: u8,
) -> Result<(), SdhciError> {
    let configured_width = match (mmc, width) {
        (false, 1) => {
            host.application_command(rca, SD_ACMD_SET_BUS_WIDTH, 0, SD_R1)?;
            1
        }
        (false, 4) => {
            host.application_command(rca, SD_ACMD_SET_BUS_WIDTH, 2, SD_R1)?;
            4
        }
        (true, 1) => {
            host.mmc_switch(183, 0)?;
            1
        }
        (true, 4) => {
            host.mmc_switch(183, 1)?;
            4
        }
        (true, 8) => {
            host.mmc_switch(183, 2)?;
            8
        }
        _ => return Err(SdhciError::InvalidTransfer),
    };
    host.set_bus_width(configured_width);
    Ok(())
}

// upstream: mmc.c mmc_send_status()
fn mmc_send_status<I: SdhciIo>(host: &mut SdhciHost<I>, rca: u16) -> Result<u32, SdhciError> {
    host.command(SD_CMD_SEND_STATUS, u32::from(rca) << 16, SD_R1, None, 0)
        .map(|response| response.0[0])
}

// upstream: mmc.c mmc_decode_csd() capacity fields
/// SD card block device initialized through the generic SDHCI command path.
pub struct SdhciDisk<I: SdhciIo> {
    host: SdhciHost<I>,
    rca: u16,
    cid: MmcCid,
    csd: MmcCsd,
    sectors: u64,
    high_capacity: bool,
    erase_group_sectors: u32,
    ext_csd: Option<MmcExtCsd>,
    active_partition: u8,
    read_only: bool,
}

impl<I: SdhciIo> SdhciDisk<I> {
    /// Initializes an SD memory card and reads its CSD capacity.
    // upstream: mmc.c mmc_idle_cards(), mmc_send_if_cond(), mmc_send_app_op_cond(), mmc_send_op_cond(), mmc_all_send_cid(), mmc_send_relative_addr(), mmc_send_csd(), mmc_select_card()
    pub fn attach(mut host: SdhciHost<I>) -> Result<Self, SdhciError> {
        host.initialize()?;
        host.command(SD_CMD_GO_IDLE, 0, RSP_NONE, None, 0)?;
        let version2 = mmc_send_if_cond(&mut host);
        let (mmc, ocr) = mmc_idle_cards(&mut host, version2)?;
        let raw_cid = mmc_all_send_cid(&mut host)?;
        let high_capacity = ocr & SD_OCR_CCS != 0;
        let rca = mmc_send_relative_addr(&mut host, mmc)?;
        let csd = mmc_send_csd(&mut host, rca)?;
        mmc_select_card(&mut host, rca)?;
        let csd_info = decode_csd(csd, mmc).ok_or(SdhciError::InvalidTransfer)?;
        let mut sectors = csd_info.capacity_bytes / 512;
        let mut erase_group_sectors = csd_info.erase_block_sectors;
        let mut ext_csd_bytes = [0u8; 512];
        let mut ext_csd = None;
        if mmc && high_capacity {
            host.command(8, 0, SD_R1 | SD_DATA, Some(&mut ext_csd_bytes), 512)?;
            let parsed = parse_ext_csd(&ext_csd_bytes);
            let ext_sectors = parsed.sectors;
            if ext_sectors != 0 {
                sectors = u64::from(ext_sectors);
            }
            if parsed.erase_group_sectors != 0 {
                erase_group_sectors = parsed.erase_group_sectors;
            }
            ext_csd = Some(parsed);
        }
        if sectors == 0 {
            return Err(SdhciError::InvalidTransfer);
        }
        if !high_capacity {
            host.command(SD_CMD_SET_BLOCKLEN, 512, SD_R1, None, 0)?;
        }
        if mmc && high_capacity {
            let bus_width = if host.capabilities & SDHCI_CAN_DO_8BITBUS != 0 {
                8
            } else {
                4
            };
            mmc_set_card_bus_width(&mut host, true, rca, bus_width)?;
            // EXT_CSD[185] (HS_TIMING) value 1 selects legacy MMC high speed.
            // Only use it when the card advertises a 26/52 MHz timing mode.
            let card_type = ext_csd.map_or(0, |csd| csd.card_type);
            if card_type & 0x03 != 0 {
                host.mmc_switch(185, 1)?;
                let target = if card_type & 0x02 != 0 {
                    52_000_000
                } else {
                    26_000_000
                };
                host.set_high_speed(target.min(host.base_clock_hz))?;
            }
            // PARTITION_CONFIG is intentionally retained for future boot/RPMB
            // child devices; the current registry exposes only user area.
            let _partition_config = ext_csd.map_or(0, |csd| csd.partition_config);
            let _partition_support = ext_csd.map_or(0, |csd| csd.partition_support);
        }
        if host.clock_hz == 0 {
            let target = host.base_clock_hz.min(25_000_000);
            host.set_clock(target)?;
        }
        let cid = if mmc {
            decode_mmc_cid(raw_cid, ext_csd.is_some())
        } else {
            decode_sd_cid(raw_cid)
        };
        let write_protected = host.card_write_protected();
        Ok(Self {
            host,
            rca,
            cid,
            csd: csd_info,
            sectors,
            high_capacity,
            erase_group_sectors: erase_group_sectors.max(1),
            ext_csd,
            active_partition: 0,
            read_only: write_protected,
        })
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only |= read_only;
    }

    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub const fn ext_csd(&self) -> Option<MmcExtCsd> {
        self.ext_csd
    }

    pub const fn cid(&self) -> MmcCid {
        self.cid
    }

    pub const fn csd(&self) -> MmcCsd {
        self.csd
    }

    // upstream: mmcsd.c mmcsd_attach() user/boot partition publication
    pub fn into_partition_devices(
        mut self,
        read_only: bool,
        disk_index: usize,
    ) -> alloc::vec::Vec<SdhciPartitionDisk<I>> {
        self.read_only |= read_only;
        let metadata = self.ext_csd;
        let shared = Arc::new(Mutex::new(self));
        let mut partitions = alloc::vec![SdhciPartitionDisk {
            shared: shared.clone(),
            access: 0,
            name: alloc::format!("mmcblk{disk_index}"),
            sectors: metadata.map_or(shared.lock().sectors, |csd| u64::from(csd.sectors)),
            read_only,
        }];
        if let Some(metadata) = metadata {
            if metadata.boot_sectors != 0 {
                partitions.push(SdhciPartitionDisk {
                    shared: shared.clone(),
                    access: 1,
                    name: alloc::format!("mmcblk{disk_index}boot0"),
                    sectors: u64::from(metadata.boot_sectors),
                    read_only,
                });
                partitions.push(SdhciPartitionDisk {
                    shared,
                    access: 2,
                    name: alloc::format!("mmcblk{disk_index}boot1"),
                    sectors: u64::from(metadata.boot_sectors),
                    read_only,
                });
            }
        }
        partitions
    }

    fn select_partition(&mut self, access: u8) -> Result<(), SdhciError> {
        if access == self.active_partition {
            return Ok(());
        }
        let metadata = self.ext_csd.ok_or(SdhciError::InvalidTransfer)?;
        if access > 2 || (access != 0 && metadata.boot_sectors == 0) {
            return Err(SdhciError::InvalidTransfer);
        }
        let config = (metadata.partition_config & !0x07) | access;
        self.host.mmc_switch(179, config)?;
        self.active_partition = access;
        Ok(())
    }

    // upstream: mmcsd.c mmcsd_rw() card-address conversion
    fn card_address(&self, lba: u64) -> Result<u32, SdhciError> {
        let address = if self.high_capacity {
            lba
        } else {
            lba.checked_mul(512).ok_or(SdhciError::InvalidTransfer)?
        };
        u32::try_from(address).map_err(|_| SdhciError::InvalidTransfer)
    }

    // upstream: mmc.c mmc_wait_for_command() ready-for-data polling
    fn wait_ready(&mut self) -> Result<(), SdhciError> {
        for _ in 0..1000 {
            let status = mmc_send_status(&mut self.host, self.rca)?;
            if status & MMC_R1_STATUS_ERRORS != 0 {
                return Err(SdhciError::Controller(status & MMC_R1_STATUS_ERRORS));
            }
            if status & (1 << 8) != 0 && status & (0xf << 9) == 4 << 9 {
                return Ok(());
            }
            self.host.io.delay_us(1000);
        }
        Err(SdhciError::Timeout)
    }

    // upstream: mmcsd.c mmcsd_rw() CMD17/CMD18/CMD24/CMD25 block transaction
    fn transfer(&mut self, lba: u64, data: &mut [u8], write: bool) -> Result<(), SdhciError> {
        if data.is_empty()
            || !data.len().is_multiple_of(512)
            || lba
                .checked_add((data.len() / 512) as u64)
                .is_none_or(|end| end > self.sectors)
        {
            return Err(SdhciError::InvalidTransfer);
        }
        if data.len() > 512 && !self.host.allows_multi_block() {
            for (index, sector) in data.chunks_exact_mut(512).enumerate() {
                self.transfer(lba + index as u64, sector, write)?;
            }
            return Ok(());
        }
        let multiple = data.len() > 512;
        let command = if write && multiple {
            SD_CMD_WRITE_MULTIPLE
        } else if write {
            SD_CMD_WRITE_SINGLE
        } else if multiple {
            SD_CMD_READ_MULTIPLE
        } else {
            SD_CMD_READ_SINGLE
        };
        let transfer = self.host.command(
            command,
            self.card_address(lba)?,
            SD_R1 | SD_DATA,
            Some(data),
            512,
        );
        if multiple {
            // Auto CMD12 is not enabled on this host, so issue the generic
            // SD/MMC stop-transmission sequence after a multi-block request.
            let stop = self
                .host
                .command(SD_CMD_STOP_TRANSMISSION, 0, SD_R1B, None, 0);
            transfer?;
            stop?;
        } else {
            transfer?;
        }
        if write {
            self.wait_ready()?;
        }
        Ok(())
    }
}

/// Serialized user-area or eMMC boot-area view over one SDHCI/MMC controller.
pub struct SdhciPartitionDisk<I: SdhciIo> {
    shared: Arc<Mutex<SdhciDisk<I>>>,
    access: u8,
    name: alloc::string::String,
    sectors: u64,
    read_only: bool,
}

impl<I: SdhciIo> crate::BaseDriverOps for SdhciPartitionDisk<I> {
    fn device_name(&self) -> &str {
        &self.name
    }

    fn device_type(&self) -> crate::DeviceType {
        crate::DeviceType::Block
    }
}

impl<I: SdhciIo> crate::BlockDriverOps for SdhciPartitionDisk<I> {
    fn num_blocks(&self) -> u64 {
        self.sectors
    }

    fn block_size(&self) -> usize {
        512
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read_block(&mut self, block: u64, output: &mut [u8]) -> crate::DevResult {
        if !output.len().is_multiple_of(512)
            || block
                .checked_add((output.len() / 512) as u64)
                .is_none_or(|end| end > self.sectors)
        {
            return Err(crate::DevError::InvalidParam);
        }
        let mut disk = self.shared.lock();
        disk.select_partition(self.access)
            .map_err(map_sdhci_error)?;
        for (index, chunk) in output.chunks_mut(512 * 128).enumerate() {
            disk.transfer(block + (index * 128) as u64, chunk, false)
                .map_err(map_sdhci_error)?;
        }
        Ok(())
    }

    fn write_block(&mut self, block: u64, input: &[u8]) -> crate::DevResult {
        if self.read_only {
            return Err(crate::DevError::Unsupported);
        }
        if !input.len().is_multiple_of(512)
            || block
                .checked_add((input.len() / 512) as u64)
                .is_none_or(|end| end > self.sectors)
        {
            return Err(crate::DevError::InvalidParam);
        }
        let mut disk = self.shared.lock();
        disk.select_partition(self.access)
            .map_err(map_sdhci_error)?;
        for (index, chunk) in input.chunks(512 * 128).enumerate() {
            let mut write_data = alloc::vec::Vec::from(chunk);
            disk.transfer(block + (index * 128) as u64, &mut write_data, true)
                .map_err(map_sdhci_error)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> crate::DevResult {
        let mut disk = self.shared.lock();
        disk.select_partition(self.access)
            .map_err(map_sdhci_error)?;
        disk.wait_ready().map_err(map_sdhci_error)
    }

    fn block_capabilities(&self) -> crate::BlockCapabilities {
        crate::BlockCapabilities {
            flush: true,
            discard: self.access == 0,
            ..crate::BlockCapabilities::default()
        }
    }

    fn discard_blocks(&mut self, range: crate::BlockRange) -> crate::DevResult {
        if self.access != 0 {
            return Err(crate::DevError::Unsupported);
        }
        let mut disk = self.shared.lock();
        disk.select_partition(self.access)
            .map_err(map_sdhci_error)?;
        crate::BlockDriverOps::discard_blocks(&mut *disk, range)
    }
}

impl<I: SdhciIo> crate::BaseDriverOps for SdhciDisk<I> {
    fn device_name(&self) -> &str {
        "mmcblk0"
    }
    fn device_type(&self) -> crate::DeviceType {
        crate::DeviceType::Block
    }
}

impl<I: SdhciIo> crate::BlockDriverOps for SdhciDisk<I> {
    fn num_blocks(&self) -> u64 {
        self.sectors
    }
    fn block_size(&self) -> usize {
        512
    }
    // upstream: mmcsd.c mmcsd_rw() read path
    fn read_block(&mut self, block: u64, output: &mut [u8]) -> crate::DevResult {
        if !output.len().is_multiple_of(512)
            || block
                .checked_add((output.len() / 512) as u64)
                .is_none_or(|end| end > self.sectors)
        {
            return Err(crate::DevError::InvalidParam);
        }
        for (index, chunk) in output.chunks_mut(512 * 128).enumerate() {
            self.transfer(block + (index * 128) as u64, chunk, false)
                .map_err(map_sdhci_error)?;
        }
        Ok(())
    }
    // upstream: mmcsd.c mmcsd_rw() write path
    fn write_block(&mut self, block: u64, input: &[u8]) -> crate::DevResult {
        if self.read_only {
            return Err(crate::DevError::Unsupported);
        }
        if !input.len().is_multiple_of(512)
            || block
                .checked_add((input.len() / 512) as u64)
                .is_none_or(|end| end > self.sectors)
        {
            return Err(crate::DevError::InvalidParam);
        }
        for (index, chunk) in input.chunks(512 * 128).enumerate() {
            let mut write_data = alloc::vec::Vec::from(chunk);
            self.transfer(block + (index * 128) as u64, &mut write_data, true)
                .map_err(map_sdhci_error)?;
        }
        Ok(())
    }
    fn flush(&mut self) -> crate::DevResult {
        self.wait_ready().map_err(map_sdhci_error)
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn block_capabilities(&self) -> crate::BlockCapabilities {
        crate::BlockCapabilities {
            flush: true,
            discard: self.erase_group_sectors != 0,
            ..crate::BlockCapabilities::default()
        }
    }

    // upstream: mmcsd.c mmcsd_delete() + mmc.c mmc_sd_erase()
    fn discard_blocks(&mut self, range: crate::BlockRange) -> crate::DevResult {
        if self.read_only {
            return Err(crate::DevError::Unsupported);
        }
        if range.blocks == 0
            || !range
                .start
                .is_multiple_of(u64::from(self.erase_group_sectors))
            || !range
                .blocks
                .is_multiple_of(u64::from(self.erase_group_sectors))
            || !(crate::BlockGeometry {
                block_size: 512,
                blocks: self.sectors,
            })
            .contains(range)
        {
            return Err(crate::DevError::InvalidParam);
        }
        let end = range
            .start
            .checked_add(range.blocks)
            .and_then(|end| end.checked_sub(1))
            .ok_or(crate::DevError::InvalidParam)?;
        self.host
            .command(
                SD_CMD_ERASE_START,
                self.card_address(range.start).map_err(map_sdhci_error)?,
                SD_R1,
                None,
                0,
            )
            .map_err(map_sdhci_error)?;
        self.host
            .command(
                SD_CMD_ERASE_END,
                self.card_address(end).map_err(map_sdhci_error)?,
                SD_R1,
                None,
                0,
            )
            .map_err(map_sdhci_error)?;
        self.host
            .command(SD_CMD_ERASE, 0, SD_R1B, None, 0)
            .map_err(map_sdhci_error)?;
        self.host.wait_busy().map_err(map_sdhci_error)?;
        self.wait_ready().map_err(map_sdhci_error)
    }
}

// upstream: mmc.c mmc_get_bits()
fn response_bits(response: SdhciResponse, lsb: u32, width: u32) -> u32 {
    if width == 0 || width > 32 || lsb >= 128 || lsb + width > 128 {
        return 0;
    }
    let raw = u128::from(response.0[3])
        | (u128::from(response.0[2]) << 32)
        | (u128::from(response.0[1]) << 64)
        | (u128::from(response.0[0]) << 96);
    ((raw >> lsb) & ((1u128 << width) - 1)) as u32
}

// upstream: mmc.c mmc_decode_cid_sd()
fn decode_sd_cid(response: SdhciResponse) -> MmcCid {
    let mut cid = MmcCid {
        manufacturer_id: response_bits(response, 120, 8) as u8,
        oem_id: response_bits(response, 104, 16) as u16,
        product_revision: response_bits(response, 56, 8) as u8,
        serial_number: response_bits(response, 24, 32),
        manufacturing_year: (2000 + response_bits(response, 12, 8)) as u16,
        manufacturing_month: response_bits(response, 8, 4) as u8,
        ..MmcCid::default()
    };
    for (index, byte) in cid.product_name[..5].iter_mut().enumerate() {
        *byte = response_bits(response, 96 - (index as u32 * 8), 8) as u8;
    }
    cid
}

// upstream: mmc.c mmc_decode_cid_mmc()
fn decode_mmc_cid(response: SdhciResponse, ext_csd_v4_41_or_later: bool) -> MmcCid {
    let mut cid = MmcCid {
        manufacturer_id: response_bits(response, 120, 8) as u8,
        oem_id: response_bits(response, 104, 8) as u16,
        product_revision: response_bits(response, 48, 8) as u8,
        serial_number: response_bits(response, 16, 32),
        manufacturing_month: response_bits(response, 12, 4) as u8,
        manufacturing_year: response_bits(response, 8, 4) as u16
            + if ext_csd_v4_41_or_later { 2013 } else { 1997 },
        ..MmcCid::default()
    };
    for (index, byte) in cid.product_name.iter_mut().enumerate() {
        *byte = response_bits(response, 96 - (index as u32 * 8), 8) as u8;
    }
    cid
}

fn map_sdhci_error(error: SdhciError) -> crate::DevError {
    match error {
        SdhciError::NoCard => crate::DevError::Io,
        SdhciError::InvalidTransfer => crate::DevError::InvalidParam,
        SdhciError::Timeout | SdhciError::Controller(_) => crate::DevError::Io,
        SdhciError::UnsupportedClock => crate::DevError::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{BaseDriverOps, BlockDriverOps};

    static DMA_RELEASES: AtomicUsize = AtomicUsize::new(0);

    unsafe fn record_dma_release(_: NonNull<u8>, _: usize) {
        DMA_RELEASES.fetch_add(1, Ordering::Relaxed);
    }

    struct MockIo {
        registers: [u32; 64],
        command: u16,
        argument: u32,
        block_count: u16,
        transfer_mode: u16,
        dma_address: u32,
        data_blocks: u16,
        command_attempts: u8,
        failed_command_attempts: u8,
    }

    impl Default for MockIo {
        fn default() -> Self {
            Self {
                registers: [0; 64],
                command: 0,
                argument: 0,
                block_count: 0,
                transfer_mode: 0,
                dma_address: 0,
                data_blocks: 0,
                command_attempts: 0,
                failed_command_attempts: 0,
            }
        }
    }

    impl SdhciIo for MockIo {
        fn read8(&mut self, offset: usize) -> u8 {
            (self.registers[offset / 4] >> ((offset % 4) * 8)) as u8
        }
        fn read16(&mut self, offset: usize) -> u16 {
            if offset == SDHCI_CLOCK_CONTROL as usize {
                return SDHCI_CLOCK_INT_STABLE as u16;
            }
            (self.registers[offset / 4] >> ((offset % 4) * 8)) as u16
        }
        fn read32(&mut self, offset: usize) -> u32 {
            if offset == SDHCI_INT_STATUS as usize {
                let mut status = self.registers[offset / 4];
                if self.data_blocks != 0 {
                    status |= SDHCI_INT_DATA_AVAIL;
                } else if self.transfer_mode & SDHCI_TRNS_DMA as u16 != 0 {
                    status |= SDHCI_INT_DATA_END;
                } else if self.transfer_mode & SDHCI_TRNS_BLK_CNT_EN as u16 != 0
                    && self.block_count != 0
                {
                    status |= SDHCI_INT_DATA_END;
                }
                return status;
            }
            self.registers[offset / 4]
        }
        fn write8(&mut self, offset: usize, value: u8) {
            if offset == SDHCI_SOFTWARE_RESET as usize {
                let _ = value;
                return;
            }
            self.registers[offset / 4] &= !(0xff << ((offset % 4) * 8));
            self.registers[offset / 4] |= u32::from(value) << ((offset % 4) * 8);
        }
        fn write16(&mut self, offset: usize, value: u16) {
            if offset == SDHCI_COMMAND_FLAGS as usize {
                self.command = value;
                self.command_attempts += 1;
                if value & (SDHCI_CMD_DATA as u16) != 0
                    && self.transfer_mode & SDHCI_TRNS_DMA as u16 == 0
                {
                    self.data_blocks = self.block_count;
                }
                if self.failed_command_attempts != 0 {
                    self.failed_command_attempts -= 1;
                    self.registers[SDHCI_INT_STATUS as usize / 4] = 0;
                } else {
                    self.registers[SDHCI_INT_STATUS as usize / 4] = SDHCI_INT_RESPONSE;
                }
                return;
            }
            if offset == SDHCI_BLOCK_COUNT as usize {
                self.block_count = value;
            }
            if offset == SDHCI_TRANSFER_MODE as usize {
                self.transfer_mode = value;
            }
            if offset == SDHCI_ARGUMENT as usize {
                self.argument = u32::from(value);
                return;
            }
            self.registers[offset / 4] &= !(0xffff << ((offset % 4) * 8));
            self.registers[offset / 4] |= u32::from(value) << ((offset % 4) * 8);
        }
        fn write32(&mut self, offset: usize, value: u32) {
            if offset == SDHCI_INT_STATUS as usize {
                self.registers[offset / 4] &= !value;
                if value & SDHCI_INT_DATA_AVAIL != 0 && self.data_blocks != 0 {
                    self.data_blocks -= 1;
                }
            } else if offset == SDHCI_ARGUMENT as usize {
                self.argument = value;
            } else if offset == SDHCI_DMA_ADDRESS as usize {
                self.dma_address = value;
            } else {
                self.registers[offset / 4] = value;
            }
        }
        fn delay_us(&mut self, _: u32) {}
    }

    #[test]
    fn block_size_and_sdma_boundary_match_upstream_encoding() {
        assert_eq!(make_block_size(5, 512), 0x5000 | 512);
        assert_eq!(sdma_bounce_buffer_size(0), 4096);
        assert_eq!(sdma_bounce_buffer_size(5), 131_072);
    }

    #[test]
    fn clock_divider_matches_spec_2_power_of_two_and_spec_3_even_steps() {
        assert_eq!(calculate_clock_divider(50_000_000, 400_000, 1), (128, 64));
        assert_eq!(calculate_clock_divider(50_000_000, 400_000, 2), (126, 63));
        assert_eq!(calculate_clock_divider(50_000_000, 50_000_000, 3), (1, 0));
    }

    #[test]
    fn voltage_and_tuning_refuse_unadvertised_host_modes() {
        let host = SdhciHost::new(MockIo::default(), 50 << SDHCI_CLOCK_BASE_SHIFT, 0, 3);
        let mut host = host;
        assert_eq!(
            host.switch_signal_voltage_18v(),
            Err(SdhciError::UnsupportedClock)
        );
        assert_eq!(
            host.execute_tuning(19, 4),
            Err(SdhciError::UnsupportedClock)
        );
    }

    #[test]
    fn host_register_offsets_and_masks_are_stable() {
        assert_eq!(SDHCI_SOFTWARE_RESET, 0x2f);
        assert_eq!(SDHCI_INT_STATUS, 0x30);
        assert_eq!(SDHCI_CAPABILITIES2, 0x44);
        assert_eq!(SDHCI_INT_CMD_MASK, 0x000f_0001);
        assert_eq!(SDHCI_SLOTTYPE_MASK, 0xc000_0000);
    }

    #[test]
    fn set_power_uses_ocr_voltage_and_preserves_enable_handshake() {
        let mut host = SdhciHost::new(MockIo::default(), 0, 0, SDHCI_SPEC_300 as u8);
        host.set_power(MMC_OCR_330_340).unwrap();
        assert_eq!(
            host.io.read8(SDHCI_POWER_CONTROL as usize),
            (SDHCI_POWER_330 | SDHCI_POWER_ON) as u8
        );
        host.set_power(0).unwrap();
        assert_eq!(host.io.read8(SDHCI_POWER_CONTROL as usize), 0);
    }

    #[test]
    fn response_bit_ranges_use_the_specified_lsb_numbering() {
        let response = SdhciResponse([0, 0, 0x0123_4567, 0x89ab_cdef]);
        assert_eq!(response_bits(response, 0, 32), 0x89ab_cdef);
        assert_eq!(response_bits(response, 32, 32), 0x0123_4567);
        assert_eq!(response_bits(response, 28, 8), 0x78);
    }

    #[test]
    fn ext_csd_decodes_capacity_partition_and_erase_metadata() {
        let mut bytes = [0u8; 512];
        bytes[212..216].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        bytes[196] = 0x13;
        bytes[179] = 0x48;
        bytes[160] = 3;
        bytes[226] = 4;
        bytes[168] = 2;
        bytes[224] = 7;
        assert_eq!(
            parse_ext_csd(&bytes),
            MmcExtCsd {
                sectors: 0x1234_5678,
                card_type: 0x13,
                partition_config: 0x48,
                partition_support: 3,
                boot_sectors: 1024,
                rpmb_sectors: 512,
                erase_group_sectors: 7168,
            }
        );
    }

    #[test]
    fn sd_and_mmc_cid_fields_follow_source_bit_ranges() {
        let mut raw = 0u128;
        raw |= 0x12u128 << 120;
        raw |= 0x3456u128 << 104;
        for (index, byte) in b"ABCDEF".iter().enumerate() {
            raw |= u128::from(*byte) << (96 - index * 8);
        }
        raw |= 0x21u128 << 56;
        raw |= 0x1234_5678u128 << 24;
        raw |= 3u128 << 12;
        raw |= 4u128 << 8;
        let response = SdhciResponse([
            (raw >> 96) as u32,
            (raw >> 64) as u32,
            (raw >> 32) as u32,
            raw as u32,
        ]);
        let sd = decode_sd_cid(response);
        assert_eq!(sd.manufacturer_id, 0x12);
        assert_eq!(sd.oem_id, 0x3456);
        assert_eq!(&sd.product_name[..5], b"ABCDE");
        assert_eq!(sd.serial_number, 0x1234_5678);
        assert_eq!((sd.manufacturing_year, sd.manufacturing_month), (2003, 4));
        let mut raw_mmc = 0u128;
        raw_mmc |= 0x12u128 << 120;
        raw_mmc |= 0x56u128 << 104;
        for (index, byte) in b"ABCDEF".iter().enumerate() {
            raw_mmc |= u128::from(*byte) << (96 - index * 8);
        }
        raw_mmc |= 0x21u128 << 48;
        raw_mmc |= 0x1234_5678u128 << 16;
        raw_mmc |= 4u128 << 12;
        raw_mmc |= 3u128 << 8;
        let mmc_response = SdhciResponse([
            (raw_mmc >> 96) as u32,
            (raw_mmc >> 64) as u32,
            (raw_mmc >> 32) as u32,
            raw_mmc as u32,
        ]);
        let mmc = decode_mmc_cid(mmc_response, true);
        assert_eq!(mmc.oem_id, 0x56);
        assert_eq!(&mmc.product_name, b"ABCDEF");
        assert_eq!(mmc.manufacturing_year, 2016);
    }

    #[test]
    fn card_identity_strings_match_mmcsd_format() {
        let cid = MmcCid {
            manufacturer_id: 3,
            oem_id: u16::from_be_bytes(*b"TN"),
            product_name: *b"SD01G\0",
            product_revision: 0x80,
            serial_number: 0x0028_f959,
            manufacturing_year: 2008,
            manufacturing_month: 8,
        };
        let (id, serial) = format_card_id(cid, false, false);
        assert_eq!(id, "SD SD01G 8.0 SN 0028F959 MFG 08/2008 by 3 TN");
        assert_eq!(serial, "0028F959");
    }

    #[test]
    fn csd_decoder_matches_sd_structure_capacity_and_transfer_fields() {
        let mut raw = 0u128;
        raw |= 1u128 << 126;
        raw |= 0x12345u128 << 48;
        raw |= 12u128 << 99;
        raw |= 1u128 << 96;
        raw |= 1u128 << 46;
        let response = SdhciResponse([
            (raw >> 96) as u32,
            (raw >> 64) as u32,
            (raw >> 32) as u32,
            raw as u32,
        ]);
        let csd = decode_csd(response, false).unwrap();
        assert_eq!(csd.structure, 1);
        assert_eq!(csd.capacity_bytes, (0x12345 + 1) * 512 * 1024);
        assert_eq!(csd.erase_block_sectors, 1);
        assert_eq!(csd.transfer_rate_hz, MMC_EXP[1] * 10_000 * MMC_MANT[12]);
    }

    #[test]
    fn host_reset_clock_and_command_response_are_bounded() {
        let mut io = MockIo::default();
        io.registers[SDHCI_PRESENT_STATE as usize / 4] = SDHCI_CARD_PRESENT;
        io.registers[SDHCI_RESPONSE as usize / 4] = 0x1234_5678;
        let caps = 50 << SDHCI_CLOCK_BASE_SHIFT;
        let mut host = SdhciHost::new(io, caps, 0, SDHCI_SPEC_300 as u8);
        host.initialize().unwrap();
        assert!(host.clock_hz() <= 400_000);
        let response = host
            .command(8, 0x1aa, SDHCI_CMD_RESP_SHORT as u16, None, 0)
            .unwrap();
        assert_eq!(response.0[0], 0x1234_5678);
        assert_eq!(
            host.io_mut().command,
            (8 << 8) | SDHCI_CMD_RESP_SHORT as u16
        );
        assert_eq!(host.io_mut().argument, 0x1aa);
    }

    #[test]
    fn write_protect_pin_is_reported_by_generic_ro_callback() {
        let mut io = MockIo::default();
        io.registers[SDHCI_PRESENT_STATE as usize / 4] = SDHCI_CARD_PRESENT;
        let mut host = SdhciHost::new(io, 50 << SDHCI_CLOCK_BASE_SHIFT, 0, 3);
        assert!(host.card_write_protected());
        host.io_mut().write32(
            SDHCI_PRESENT_STATE as usize,
            SDHCI_CARD_PRESENT | SDHCI_WRITE_PROTECT,
        );
        assert!(!host.card_write_protected());
    }

    #[test]
    fn dont_shift_response_quirk_preserves_raw_r2_register_words() {
        let mut io = MockIo::default();
        io.registers[SDHCI_PRESENT_STATE as usize / 4] = SDHCI_CARD_PRESENT;
        io.registers[SDHCI_RESPONSE as usize / 4] = 0x1020_3040;
        io.registers[SDHCI_RESPONSE as usize / 4 + 1] = 0x5060_7080;
        io.registers[SDHCI_RESPONSE as usize / 4 + 2] = 0x90a0_b0c0;
        io.registers[SDHCI_RESPONSE as usize / 4 + 3] = 0xd0e0_f000;
        let host = SdhciHost::new_with_quirks(
            io,
            50 << SDHCI_CLOCK_BASE_SHIFT,
            0,
            3,
            SDHCI_QUIRK_DONT_SHIFT_RESPONSE,
        );
        let mut host = host;
        let response = host
            .command(SD_CMD_ALL_SEND_CID, 0, SD_R2, None, 0)
            .unwrap();
        assert_eq!(
            response.0,
            [0xd0e0_f000, 0x90a0_b0c0, 0x5060_7080, 0x1020_3040]
        );
    }

    #[test]
    fn multiblock_read_sets_count_mode_and_consumes_each_pio_block() {
        let mut io = MockIo::default();
        io.registers[SDHCI_PRESENT_STATE as usize / 4] = SDHCI_CARD_PRESENT;
        io.registers[SDHCI_RESPONSE as usize / 4] = 0xfeed_beef;
        let mut host = SdhciHost::new(io, 50 << SDHCI_CLOCK_BASE_SHIFT, 0, 3);
        let mut blocks = [0u8; 1024];
        host.command(
            SD_CMD_READ_MULTIPLE,
            4,
            SD_R1 | SD_DATA,
            Some(&mut blocks),
            512,
        )
        .unwrap();
        assert_eq!(host.io_mut().block_count, 2);
        assert_eq!(
            host.io_mut().transfer_mode,
            (SDHCI_TRNS_BLK_CNT_EN | SDHCI_TRNS_MULTI | SDHCI_TRNS_READ) as u16
        );
        assert_eq!(host.io_mut().data_blocks, 0);
    }

    #[test]
    fn sd_switch_function_data_is_transferred_from_card_to_host() {
        let mut io = MockIo::default();
        io.registers[SDHCI_PRESENT_STATE as usize / 4] = SDHCI_CARD_PRESENT;
        let mut host = SdhciHost::new(io, 50 << SDHCI_CLOCK_BASE_SHIFT, 0, 3);
        let mut status = [0u8; 64];
        host.switch_sd_function(0, 0, 1, &mut status).unwrap();
        assert_ne!(host.io_mut().transfer_mode & SDHCI_TRNS_READ as u16, 0);
        assert_eq!(host.io_mut().argument, 0x00ff_fff1);
        assert!(matches!(
            host.switch_sd_function(2, 0, 1, &mut status),
            Err(SdhciError::InvalidTransfer)
        ));
    }

    #[test]
    fn sdma_uses_owned_bounce_memory_and_programs_dma_registers() {
        let mut io = MockIo::default();
        io.registers[SDHCI_PRESENT_STATE as usize / 4] = SDHCI_CARD_PRESENT;
        let mut bounce = alloc::boxed::Box::new([0u8; 1024]);
        let cpu = NonNull::new(bounce.as_mut_ptr()).unwrap();
        // SAFETY: the boxed test buffer is stable and remains alive while the
        // host owns this borrowed no-release region.
        let dma = unsafe { SdhciDmaRegion::from_raw_parts(cpu, 0x1000, 1024, 0, None) };
        let mut host = SdhciHost::new(io, (50 << SDHCI_CLOCK_BASE_SHIFT) | SDHCI_CAN_DO_DMA, 0, 3)
            .with_dma_region(dma);
        let mut input = [0xa5; 512];
        host.command(
            SD_CMD_WRITE_SINGLE,
            1,
            SD_R1 | SD_DATA,
            Some(&mut input),
            512,
        )
        .unwrap();
        assert_ne!(host.io_mut().transfer_mode & SDHCI_TRNS_DMA as u16, 0);
        assert_eq!(host.io_mut().dma_address, 0x1000);
        assert_eq!(&bounce[..512], &input);
    }

    #[test]
    fn uncertain_sdma_timeout_quarantines_allocation() {
        DMA_RELEASES.store(0, Ordering::Relaxed);
        let mut io = MockIo::default();
        io.registers[SDHCI_PRESENT_STATE as usize / 4] = SDHCI_CARD_PRESENT;
        io.failed_command_attempts = 1;
        let mut bounce = alloc::boxed::Box::new([0u8; 1024]);
        let cpu = NonNull::new(bounce.as_mut_ptr()).unwrap();
        // SAFETY: the test owns this stable allocation through the host call.
        let dma = unsafe {
            SdhciDmaRegion::from_raw_parts(cpu, 0x1000, 1024, 1, Some(record_dma_release))
        };
        let mut host = SdhciHost::new(io, (50 << SDHCI_CLOCK_BASE_SHIFT) | SDHCI_CAN_DO_DMA, 0, 3)
            .with_dma_region(dma);
        let mut input = [0x5a; 512];
        assert_eq!(
            host.command(
                SD_CMD_WRITE_SINGLE,
                1,
                SD_R1 | SD_DATA,
                Some(&mut input),
                512,
            ),
            Err(SdhciError::Timeout)
        );
        drop(host);
        assert_eq!(DMA_RELEASES.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn idempotent_command_retries_after_controller_reset() {
        let mut io = MockIo::default();
        io.registers[SDHCI_PRESENT_STATE as usize / 4] = SDHCI_CARD_PRESENT;
        io.failed_command_attempts = 1;
        let mut host = SdhciHost::new(io, 50 << SDHCI_CLOCK_BASE_SHIFT, 0, 3);
        host.command(SD_CMD_SEND_STATUS, 0, SD_R1, None, 0).unwrap();
        assert_eq!(host.io_mut().command_attempts, 2);
    }

    #[test]
    fn read_only_card_rejects_write_before_issuing_a_command() {
        let host = SdhciHost::new(MockIo::default(), 50 << SDHCI_CLOCK_BASE_SHIFT, 0, 3);
        let mut disk = SdhciDisk {
            host,
            rca: 1,
            cid: MmcCid::default(),
            csd: MmcCsd::default(),
            sectors: 16,
            high_capacity: true,
            erase_group_sectors: 1,
            ext_csd: None,
            active_partition: 0,
            read_only: true,
        };
        assert!(crate::BlockDriverOps::is_read_only(&disk));
        assert!(matches!(
            crate::BlockDriverOps::write_block(&mut disk, 0, &[0x55; 512]),
            Err(crate::DevError::Unsupported)
        ));
        assert_eq!(disk.host.io_mut().command, 0);
    }

    #[test]
    fn emmc_boot_areas_are_published_as_separate_read_only_views() {
        let host = SdhciHost::new(MockIo::default(), 50 << SDHCI_CLOCK_BASE_SHIFT, 0, 3);
        let disk = SdhciDisk {
            host,
            rca: 1,
            cid: MmcCid::default(),
            csd: MmcCsd::default(),
            sectors: 10_000,
            high_capacity: true,
            erase_group_sectors: 1,
            ext_csd: Some(MmcExtCsd {
                sectors: 10_000,
                card_type: 0,
                partition_config: 0,
                partition_support: 1,
                boot_sectors: 4096,
                rpmb_sectors: 2048,
                erase_group_sectors: 1024,
            }),
            active_partition: 0,
            read_only: false,
        };
        let areas = disk.into_partition_devices(true, 0);
        assert_eq!(areas.len(), 3);
        assert_eq!(areas[0].device_name(), "mmcblk0");
        assert_eq!(areas[1].device_name(), "mmcblk0boot0");
        assert_eq!(areas[2].device_name(), "mmcblk0boot1");
        assert_eq!(areas[1].num_blocks(), 4096);
        assert!(areas.iter().all(crate::BlockDriverOps::is_read_only));
    }

    #[test]
    fn mmc_status_error_is_reported_instead_of_being_retried_as_not_ready() {
        let mut io = MockIo::default();
        io.registers[SDHCI_PRESENT_STATE as usize / 4] = SDHCI_CARD_PRESENT;
        io.registers[SDHCI_RESPONSE as usize / 4] = 1 << 22;
        let host = SdhciHost::new(io, 50 << SDHCI_CLOCK_BASE_SHIFT, 0, 3);
        let mut disk = SdhciDisk {
            host,
            rca: 1,
            cid: MmcCid::default(),
            csd: MmcCsd::default(),
            sectors: 10,
            high_capacity: true,
            erase_group_sectors: 1,
            ext_csd: None,
            active_partition: 0,
            read_only: false,
        };
        assert_eq!(disk.wait_ready(), Err(SdhciError::Controller(1 << 22)));
    }
}
