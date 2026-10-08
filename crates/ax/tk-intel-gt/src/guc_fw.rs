// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_fw.c,
// intel_guc.c, and intel_huc.c: Gen12 uC transfer/status and GuC MMIO
// command functions; register reads are supplied by the GtIo caller.
// Copyright © 2014-2019 Intel Corporation.
// Copyright © 2016-2019 Intel Corporation. Full grant: LICENSE-MIT.

use crate::{Error, GtIo, uc::FirmwareImage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterWrite {
    pub offset: u32,
    pub value: u32,
}

const DMA_ADDR_0_LOW: u32 = 0xc300;
const DMA_ADDR_0_HIGH: u32 = 0xc304;
const DMA_ADDR_1_LOW: u32 = 0xc308;
const DMA_ADDR_1_HIGH: u32 = 0xc30c;
const DMA_COPY_SIZE: u32 = 0xc310;
const DMA_CTRL: u32 = 0xc314;
const START_DMA: u32 = 1;
const UOS_MOVE: u32 = 1 << 4;
const HUC_UKERNEL: u32 = 1 << 9;
const DMA_ADDRESS_SPACE_WOPCM: u32 = 7 << 16;
const HUC_STATUS2: u32 = 0xd3b0;
const HUC_FW_VERIFIED: u32 = 1 << 7;
const GEN11_GUC_SEND_BASE: u32 = 0x190240;
const GEN11_GUC_HOST_INTERRUPT: u32 = 0x1901f0;
const GEN11_GUC_SEND_COUNT: usize = 4;
const GUC_SEND_TRIGGER: u32 = 1;
const HXG_ORIGIN_GUC: u32 = 1 << 31;
const HXG_TYPE_MASK: u32 = 7 << 28;
const HXG_TYPE_RESPONSE_SUCCESS: u32 = 7 << 28;
const HXG_TYPE_NO_RESPONSE_BUSY: u32 = 3 << 28;
const HXG_TYPE_NO_RESPONSE_RETRY: u32 = 5 << 28;
const HXG_TYPE_RESPONSE_FAILURE: u32 = 6 << 28;
const ACTION_AUTHENTICATE_HUC: u32 = 0x4000;
const UOS_RSA_SCRATCH: u32 = 0xc200;
const UOS_RSA_SCRATCH_COUNT: usize = 64;

// upstream: intel_guc.c guc_send_reg()
fn send_reg_offset(index: usize) -> Result<u32, Error> {
    if index >= GEN11_GUC_SEND_COUNT {
        return Err(Error::Refused);
    }
    Ok(GEN11_GUC_SEND_BASE + index as u32 * 4)
}

// upstream: intel_guc.c intel_guc_notify()
pub fn notify(io: &impl GtIo) -> Result<(), Error> {
    io.write(GEN11_GUC_HOST_INTERRUPT, GUC_SEND_TRIGGER)
}

// upstream: intel_uc_fw.c uc_fw_xfer()
/// Caller must keep the firmware bytes and their GGTT binding alive until this
/// returns success and hold GT forcewake. An uncertain completion quarantines
/// the owner because DMA may continue.
pub fn firmware_dma_xfer(
    io: &impl GtIo,
    source_ggtt: u64,
    destination: u32,
    byte_count: u32,
    flags: u32,
) -> Result<(), Error> {
    firmware_dma_xfer_with_timeout(io, source_ggtt, destination, byte_count, flags, 100_000)
}

fn firmware_dma_xfer_with_timeout(
    io: &impl GtIo,
    source_ggtt: u64,
    destination: u32,
    byte_count: u32,
    flags: u32,
    timeout_us: u64,
) -> Result<(), Error> {
    if source_ggtt >> 48 != 0 || byte_count == 0 || !matches!(flags, UOS_MOVE | HUC_UKERNEL) {
        return Err(Error::Refused);
    }
    let source_hi = (source_ggtt >> 32) as u32;
    for (offset, value) in [
        (DMA_ADDR_0_LOW, source_ggtt as u32),
        (DMA_ADDR_0_HIGH, source_hi),
        (DMA_ADDR_1_LOW, destination),
        (DMA_ADDR_1_HIGH, DMA_ADDRESS_SPACE_WOPCM),
        (DMA_COPY_SIZE, byte_count),
    ] {
        io.write(offset, value)?;
    }

    // A failed write can have landed. In either case, attempt to stop/retire
    // the transfer and retain the source if the DMA completion is ambiguous.
    let start = io.write(DMA_CTRL, crate::masked_enable(flags | START_DMA));
    let wait = crate::wait(io, DMA_CTRL, START_DMA, 0, timeout_us);
    let disable = io.write(DMA_CTRL, crate::masked_disable(flags));
    if disable.is_err() || wait.is_err() {
        return Err(Error::Quarantined);
    }
    start?;
    wait?;
    Ok(())
}

// upstream: intel_guc_fw.c guc_xfer_rsa_mmio()
// The ADL-N MMIO path is the fixed 256-byte key; larger signatures require
// the upstream GGTT-pinned RSA VMA path and are refused until implemented.
fn rsa_words(image: &FirmwareImage) -> Result<[u32; UOS_RSA_SCRATCH_COUNT], Error> {
    const RSA_BYTES: usize = UOS_RSA_SCRATCH_COUNT * 4;
    if image.css.rsa_bytes != RSA_BYTES {
        return Err(Error::Refused);
    }
    let start = image
        .css
        .header_bytes
        .checked_add(image.css.microcode_bytes)
        .ok_or(Error::Refused)?;
    let end = start.checked_add(RSA_BYTES).ok_or(Error::Refused)?;
    let signature = image.bytes.get(start..end).ok_or(Error::Refused)?;
    let mut words = [0; UOS_RSA_SCRATCH_COUNT];
    for (word, bytes) in words.iter_mut().zip(signature.chunks_exact(4)) {
        *word = u32::from_le_bytes(bytes.try_into().map_err(|_| Error::Refused)?);
    }
    Ok(words)
}

// upstream: intel_guc_fw.c intel_guc_fw_upload()
/// Caller supplies a pinned firmware GGTT image and owns forcewake. This
/// Gen12.0 path transfers the RSA key, copies CSS+uKernel to WOPCM, then waits
/// for the GuC boot status; no submission queues are enabled here.
pub fn guc_upload(
    io: &impl GtIo,
    source_ggtt: u64,
    image: &FirmwareImage,
) -> Result<u32, LoadError> {
    if image.kind != crate::uc::Kind::GuC {
        return Err(LoadError::Io(Error::Refused));
    }
    for write in gen12_prepare_xfer() {
        io.write(write.offset, write.value).map_err(LoadError::Io)?;
    }
    let rsa = rsa_words(image).map_err(LoadError::Io)?;
    for (index, word) in rsa.into_iter().enumerate() {
        let offset = UOS_RSA_SCRATCH + (index as u32) * 4;
        io.write(offset, word).map_err(LoadError::Io)?;
    }
    let code_bytes = image
        .css
        .header_bytes
        .checked_add(image.css.microcode_bytes)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(LoadError::Io(Error::Refused))?;
    firmware_dma_xfer(io, source_ggtt, 0x2000, code_bytes, UOS_MOVE).map_err(LoadError::Io)?;
    wait_ucode(io)
}

// upstream: intel_huc_fw.c intel_huc_fw_upload()
/// Caller must retain the firmware GGTT mapping through DMA completion.
pub fn huc_upload(
    io: &impl GtIo,
    source_ggtt: u64,
    image: &FirmwareImage,
    loaded_by_gsc: bool,
) -> Result<(), Error> {
    if loaded_by_gsc || image.kind != crate::uc::Kind::HuC {
        return Err(Error::Refused);
    }
    let code_bytes = image
        .css
        .header_bytes
        .checked_add(image.css.microcode_bytes)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(Error::Refused)?;
    firmware_dma_xfer(io, source_ggtt, 0, code_bytes, HUC_UKERNEL)
}

// upstream: intel_huc.c intel_huc_is_authenticated()
pub fn huc_is_authenticated(status2: u32) -> bool {
    status2 & HUC_FW_VERIFIED == HUC_FW_VERIFIED
}

// upstream: intel_huc.c intel_huc_wait_for_auth_complete()
pub fn wait_huc_auth(io: &impl GtIo) -> Result<u32, Error> {
    for _ in 0..3 {
        let start = io.now_us();
        loop {
            let status = io.read(HUC_STATUS2)?;
            if huc_is_authenticated(status) {
                return Ok(status);
            }
            if io.now_us().saturating_sub(start) >= 1_000_000 {
                break;
            }
            io.delay_us(2);
        }
    }
    Err(Error::Timeout(HUC_STATUS2))
}

// upstream: intel_guc.c intel_guc_send_mmio()
/// Gen11+ four-dword MMIO transport. The caller serializes the send path and
/// owns forcewake.
pub fn send_mmio(
    io: &impl GtIo,
    request: &[u32],
    mut response_buf: Option<&mut [u32]>,
) -> Result<u32, Error> {
    if request.is_empty()
        || request.len() > GEN11_GUC_SEND_COUNT
        || request[0] & HXG_ORIGIN_GUC != 0
        || request[0] & HXG_TYPE_MASK != 0
    {
        return Err(Error::Refused);
    }
    loop {
        for (index, word) in request.iter().copied().enumerate() {
            io.write(send_reg_offset(index)?, word)?;
        }
        let _posted = io.read(send_reg_offset(request.len() - 1)?)?;
        notify(io)?;
        let mut response = crate::wait(
            io,
            GEN11_GUC_SEND_BASE,
            HXG_ORIGIN_GUC,
            HXG_ORIGIN_GUC,
            10_000,
        )?;
        let busy_start = io.now_us();
        while response & HXG_TYPE_MASK == HXG_TYPE_NO_RESPONSE_BUSY {
            if io.now_us().saturating_sub(busy_start) >= 1_000_000 {
                return Err(Error::Timeout(GEN11_GUC_SEND_BASE));
            }
            io.delay_us(1);
            response = io.read(GEN11_GUC_SEND_BASE)?;
            if response & HXG_ORIGIN_GUC == 0 {
                return Err(Error::Unavailable(GEN11_GUC_SEND_BASE));
            }
        }
        match response & HXG_TYPE_MASK {
            HXG_TYPE_RESPONSE_SUCCESS => {
                if let Some(response_buf) = response_buf.as_deref_mut() {
                    let count = response_buf.len().min(GEN11_GUC_SEND_COUNT);
                    if count == 0 {
                        return Err(Error::Refused);
                    }
                    response_buf[0] = response;
                    for (index, word) in response_buf.iter_mut().enumerate().take(count).skip(1) {
                        *word = io.read(GEN11_GUC_SEND_BASE + index as u32 * 4)?;
                    }
                    return Ok(count as u32);
                }
                return Ok(response & 0x0fff_ffff);
            }
            HXG_TYPE_RESPONSE_FAILURE => return Err(Error::Refused),
            HXG_TYPE_NO_RESPONSE_RETRY => continue,
            _ => return Err(Error::Unavailable(GEN11_GUC_SEND_BASE)),
        }
    }
}

// upstream: intel_guc.c intel_guc_auth_huc()
/// Ask the running GuC to authenticate HuC firmware's RSA data in GGTT.
pub fn authenticate_huc(io: &impl GtIo, rsa_offset: u32) -> Result<u32, Error> {
    send_mmio(io, &[ACTION_AUTHENTICATE_HUC, rsa_offset], None)
}

// upstream: intel_guc_fw.c guc_prepare_xfer()
/// Gen12.0 branch: shim cache/clock policy must precede DMA, then enable
/// doorbells. Older Gen12 SRAM/MIA settings are retained verbatim.
pub const fn gen12_prepare_xfer() -> [RegisterWrite; 2] {
    [
        RegisterWrite {
            offset: 0xc064, // GUC_SHIM_CONTROL
            value: (1 << 1) | (1 << 9) | (1 << 10) | (1 << 15) | (1 << 0) | (1 << 2),
        },
        RegisterWrite {
            offset: 0x13816c, // GEN9_GT_PM_CONFIG
            value: 1,         // GT_DOORBELL_ENABLE
        },
    ]
}

// upstream: intel_guc_fw.c guc_load_done()
/// `None` means GuC remains in a transitional state; `Some(false)` is a
/// terminal firmware/BootROM failure, and `Some(true)` is READY.
pub fn load_done(status: u32) -> Option<bool> {
    let ukernel = (status >> 8) & 0xff;
    let bootrom = (status >> 1) & 0x7f;
    match ukernel {
        0xf0 => return Some(true),
        0x02 | 0x03 | 0x04 | 0x07 | 0x60 | 0x70 | 0x71 | 0x73 | 0x74 | 0x75 => {
            return Some(false);
        }
        _ => {}
    }
    match bootrom {
        0x13 | 0x50 | 0x73 | 0x74 | 0x75 | 0x77 | 0x79 | 0x7a | 0x7e | 0x2b => Some(false),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadError {
    Io(Error),
    Firmware(u32),
    Timeout { status: u32, attempts: u8 },
}

// upstream: intel_guc_fw.c guc_wait_ucode()
pub fn wait_ucode(io: &impl GtIo) -> Result<u32, LoadError> {
    wait_ucode_with(io, 1_000_000, 3)
}

fn wait_ucode_with(io: &impl GtIo, wait_us: u64, attempts: u8) -> Result<u32, LoadError> {
    let mut status = 0;
    for attempt in 0..attempts {
        let start = io.now_us();
        loop {
            status = io.read(0xc000).map_err(LoadError::Io)?; // GUC_STATUS
            match load_done(status) {
                Some(true) => return Ok(status),
                Some(false) => return Err(LoadError::Firmware(status)),
                None => {}
            }
            if io.now_us().saturating_sub(start) >= wait_us {
                break;
            }
            io.delay_us(1);
        }
        if attempt + 1 < attempts {
            continue;
        }
    }
    Err(LoadError::Timeout { status, attempts })
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    struct ScriptedIo {
        statuses: &'static [u32],
        index: Cell<usize>,
        time: Cell<u64>,
    }
    impl GtIo for ScriptedIo {
        fn read(&self, offset: u32) -> Result<u32, Error> {
            if offset != 0xc000 {
                return Err(Error::Unavailable(offset));
            }
            let index = self.index.get();
            self.index.set(index.saturating_add(1));
            Ok(self.statuses[index.min(self.statuses.len() - 1)])
        }
        fn write(&self, _offset: u32, _value: u32) -> Result<(), Error> {
            Err(Error::Refused)
        }
        fn now_us(&self) -> u64 {
            self.time.get()
        }
        fn delay_us(&self, micros: u32) {
            self.time
                .set(self.time.get().saturating_add(u64::from(micros)));
        }
    }

    #[test]
    fn load_poll_distinguishes_ready_terminal_errors_and_pending() {
        assert_eq!(load_done(0xf0 << 8), Some(true));
        assert_eq!(load_done(0x02 << 8), Some(false));
        assert_eq!(load_done(0x50 << 1), Some(false));
        assert_eq!(load_done(0x30 << 8), None);
        assert_eq!(load_done(0), None);
    }

    #[test]
    fn gen12_guc_transfer_order_and_flags_match_upstream() {
        let writes = gen12_prepare_xfer();
        assert_eq!(writes[0].offset, 0xc064);
        assert_eq!(writes[0].value, 0x8607);
        assert_eq!(writes[1].offset, 0x13816c);
        assert_eq!(writes[1].value, 1);
    }

    #[test]
    fn wait_poll_retries_transitional_status_and_stops_on_result() {
        let ready = ScriptedIo {
            statuses: &[0, 0xf0 << 8],
            index: Cell::new(0),
            time: Cell::new(0),
        };
        assert_eq!(wait_ucode_with(&ready, 2, 3), Ok(0xf0 << 8));

        let failed = ScriptedIo {
            statuses: &[0x02 << 8],
            index: Cell::new(0),
            time: Cell::new(0),
        };
        assert_eq!(
            wait_ucode_with(&failed, 2, 3),
            Err(LoadError::Firmware(0x02 << 8))
        );

        let pending = ScriptedIo {
            statuses: &[0],
            index: Cell::new(0),
            time: Cell::new(0),
        };
        assert_eq!(
            wait_ucode_with(&pending, 2, 3),
            Err(LoadError::Timeout {
                status: 0,
                attempts: 3
            })
        );
    }

    struct DmaIo {
        writes: core::cell::RefCell<std::vec::Vec<(u32, u32)>>,
        time: core::cell::Cell<u64>,
        stuck: bool,
        fail_start: bool,
    }

    struct MmioIo<'a> {
        writes: core::cell::RefCell<std::vec::Vec<(u32, u32)>>,
        responses: &'a [&'a [u32]],
        notifications: core::cell::Cell<usize>,
        response_index: core::cell::Cell<usize>,
        time: core::cell::Cell<u64>,
    }
    impl GtIo for MmioIo<'_> {
        fn read(&self, offset: u32) -> Result<u32, Error> {
            if offset == GEN11_GUC_SEND_BASE {
                let notification = self.notifications.get().saturating_sub(1);
                let responses = self.responses[notification.min(self.responses.len() - 1)];
                let index = self.response_index.get();
                self.response_index.set(index.saturating_add(1));
                return Ok(responses[index.min(responses.len() - 1)]);
            }
            if (GEN11_GUC_SEND_BASE + 4..GEN11_GUC_SEND_BASE + 16).contains(&offset) {
                return Ok(0);
            }
            Err(Error::Unavailable(offset))
        }
        fn write(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.writes.borrow_mut().push((offset, value));
            if offset == GEN11_GUC_HOST_INTERRUPT {
                self.notifications.set(self.notifications.get() + 1);
                self.response_index.set(0);
            }
            Ok(())
        }
        fn now_us(&self) -> u64 {
            self.time.get()
        }
        fn delay_us(&self, micros: u32) {
            self.time
                .set(self.time.get().saturating_add(u64::from(micros)));
        }
    }
    impl GtIo for DmaIo {
        fn read(&self, offset: u32) -> Result<u32, Error> {
            if offset == 0xc000 {
                return Ok(0xf0 << 8);
            }
            if offset == HUC_STATUS2 {
                return Ok(HUC_FW_VERIFIED);
            }
            if offset == DMA_CTRL {
                return Ok(u32::from(self.stuck));
            }
            Err(Error::Unavailable(offset))
        }
        fn write(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.writes.borrow_mut().push((offset, value));
            if offset == DMA_CTRL && value & START_DMA != 0 && self.fail_start {
                return Err(Error::Unavailable(offset));
            }
            Ok(())
        }
        fn now_us(&self) -> u64 {
            self.time.get()
        }
        fn delay_us(&self, micros: u32) {
            self.time
                .set(self.time.get().saturating_add(u64::from(micros)));
        }
    }

    #[test]
    fn firmware_dma_programs_source_destination_and_retires_ctrl() {
        let io = DmaIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            time: core::cell::Cell::new(0),
            stuck: false,
            fail_start: false,
        };
        assert_eq!(
            firmware_dma_xfer_with_timeout(&io, 0x1234_5678_9abc, 0x2000, 0x4000, UOS_MOVE, 10),
            Ok(())
        );
        assert_eq!(
            *io.writes.borrow(),
            [
                (DMA_ADDR_0_LOW, 0x5678_9abc),
                (DMA_ADDR_0_HIGH, 0x1234),
                (DMA_ADDR_1_LOW, 0x2000),
                (DMA_ADDR_1_HIGH, DMA_ADDRESS_SPACE_WOPCM),
                (DMA_COPY_SIZE, 0x4000),
                (DMA_CTRL, crate::masked_enable(UOS_MOVE | START_DMA)),
                (DMA_CTRL, crate::masked_disable(UOS_MOVE)),
            ]
        );
    }

    #[test]
    fn firmware_dma_refuses_unaddressable_and_ambiguous_transfers() {
        let io = DmaIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            time: core::cell::Cell::new(0),
            stuck: false,
            fail_start: false,
        };
        assert_eq!(
            firmware_dma_xfer_with_timeout(&io, 1 << 48, 0x2000, 0x1000, UOS_MOVE, 10),
            Err(Error::Refused)
        );
        assert!(io.writes.borrow().is_empty());

        let stuck = DmaIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            time: core::cell::Cell::new(0),
            stuck: true,
            fail_start: false,
        };
        assert_eq!(
            firmware_dma_xfer_with_timeout(&stuck, 0x1000, 0, 0x1000, HUC_UKERNEL, 2),
            Err(Error::Quarantined)
        );
    }

    fn huc_fixture() -> FirmwareImage {
        let mut bytes = std::vec![0; 512];
        for (offset, value) in [
            (4, 160u32),
            (24, 192),
            (28, 64),
            (32, 64),
            (36, 0),
            (64, 7 << 16 | 9 << 8 | 3),
            (120, 0),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let css = crate::uc::parse_css(&bytes, 2 * 1024 * 1024).unwrap();
        FirmwareImage {
            kind: crate::uc::Kind::HuC,
            blob: crate::uc::candidates(crate::uc::Platform::AlderLakeN, crate::uc::Kind::HuC)[0],
            css,
            bytes,
        }
    }

    #[test]
    fn guc_upload_orders_rsa_dma_and_waits_for_ready() {
        let mut image = huc_fixture();
        image.kind = crate::uc::Kind::GuC;
        image.blob =
            crate::uc::candidates(crate::uc::Platform::AlderLakeN, crate::uc::Kind::GuC)[0];
        image.css.version = (70, 12, 1);
        image.bytes[64..68].copy_from_slice(&(70u32 << 16 | 12 << 8 | 1).to_le_bytes());
        image.css.rsa_bytes = 256;
        for (index, word) in image.bytes[256..512].chunks_exact_mut(4).enumerate() {
            word.copy_from_slice(&(index as u32).to_le_bytes());
        }
        let io = DmaIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            time: core::cell::Cell::new(0),
            stuck: false,
            fail_start: false,
        };
        assert_eq!(guc_upload(&io, 0x10_0000, &image), Ok(0xf0 << 8));
        let writes = io.writes.borrow();
        assert_eq!(writes[0], (0xc064, 0x8607));
        assert_eq!(writes[1], (0x13816c, 1));
        assert_eq!(writes[2], (UOS_RSA_SCRATCH, 0));
        assert_eq!(writes[65], (UOS_RSA_SCRATCH + 63 * 4, 63));
        assert_eq!(writes[66], (DMA_ADDR_0_LOW, 0x10_0000));
        assert_eq!(
            writes[71],
            (DMA_CTRL, crate::masked_enable(UOS_MOVE | START_DMA))
        );
    }

    #[test]
    fn huc_upload_uses_zero_destination_and_refuses_gsc_owned_firmware() {
        let image = huc_fixture();
        let io = DmaIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            time: core::cell::Cell::new(0),
            stuck: false,
            fail_start: false,
        };
        assert_eq!(huc_upload(&io, 0x2000, &image, true), Err(Error::Refused));
        assert!(io.writes.borrow().is_empty());
        huc_upload(&io, 0x2000, &image, false).unwrap();
        assert_eq!(io.writes.borrow()[2], (DMA_ADDR_1_LOW, 0));
        assert_eq!(
            io.writes.borrow()[5],
            (DMA_CTRL, crate::masked_enable(HUC_UKERNEL | START_DMA))
        );
    }

    #[test]
    fn huc_authentication_status_requires_the_verified_bit() {
        assert!(!huc_is_authenticated(0));
        assert!(!huc_is_authenticated(HUC_FW_VERIFIED >> 1));
        assert!(huc_is_authenticated(HUC_FW_VERIFIED));
        assert!(huc_is_authenticated(HUC_FW_VERIFIED | 0x1234));
    }

    #[test]
    fn huc_auth_wait_reads_verified_status() {
        let io = DmaIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            time: core::cell::Cell::new(0),
            stuck: false,
            fail_start: false,
        };
        assert_eq!(wait_huc_auth(&io), Ok(HUC_FW_VERIFIED));
    }

    #[test]
    fn gen11_mmio_send_handles_busy_retry_success_and_failure() {
        assert_eq!(send_reg_offset(0), Ok(GEN11_GUC_SEND_BASE));
        assert_eq!(send_reg_offset(3), Ok(GEN11_GUC_SEND_BASE + 12));
        assert_eq!(send_reg_offset(4), Err(Error::Refused));
        let busy_response = HXG_ORIGIN_GUC | HXG_TYPE_NO_RESPONSE_BUSY;
        let success = HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS | 0x1234;
        let busy_values = [busy_response, busy_response, success];
        let busy_responses = [&busy_values[..]];
        let busy = MmioIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            responses: &busy_responses,
            notifications: core::cell::Cell::new(0),
            response_index: core::cell::Cell::new(0),
            time: core::cell::Cell::new(0),
        };
        assert_eq!(send_mmio(&busy, &[0x4000, 0x1234], None), Ok(0x1234));
        assert_eq!(busy.notifications.get(), 1);
        assert_eq!(busy.writes.borrow()[0], (GEN11_GUC_SEND_BASE, 0x4000));
        assert_eq!(
            busy.writes.borrow()[2],
            (GEN11_GUC_HOST_INTERRUPT, GUC_SEND_TRIGGER)
        );

        let retry_values = [HXG_ORIGIN_GUC | HXG_TYPE_NO_RESPONSE_RETRY];
        let retry_success = [success];
        let retry_responses = [&retry_values[..], &retry_success[..]];
        let retry = MmioIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            responses: &retry_responses,
            notifications: core::cell::Cell::new(0),
            response_index: core::cell::Cell::new(0),
            time: core::cell::Cell::new(0),
        };
        assert_eq!(send_mmio(&retry, &[0x4000, 0x1234], None), Ok(0x1234));
        assert_eq!(retry.notifications.get(), 2);

        let failure_values = [HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_FAILURE];
        let failure_responses = [&failure_values[..]];
        let failure = MmioIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            responses: &failure_responses,
            notifications: core::cell::Cell::new(0),
            response_index: core::cell::Cell::new(0),
            time: core::cell::Cell::new(0),
        };
        assert_eq!(send_mmio(&failure, &[0x4000], None), Err(Error::Refused));
    }

    #[test]
    fn huc_authentication_sends_upstream_action_and_rsa_offset() {
        let response = [HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS | 0x55];
        let responses = [&response[..]];
        let io = MmioIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            responses: &responses,
            notifications: core::cell::Cell::new(0),
            response_index: core::cell::Cell::new(0),
            time: core::cell::Cell::new(0),
        };
        assert_eq!(authenticate_huc(&io, 0x1234_0000), Ok(0x55));
        assert_eq!(
            io.writes.borrow()[..2],
            [
                (GEN11_GUC_SEND_BASE, 0x4000),
                (GEN11_GUC_SEND_BASE + 4, 0x1234_0000)
            ]
        );
    }

    #[test]
    fn mmio_send_copies_only_the_available_response_registers() {
        let response = [HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS | 0x55];
        let responses = [&response[..]];
        let io = MmioIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            responses: &responses,
            notifications: core::cell::Cell::new(0),
            response_index: core::cell::Cell::new(0),
            time: core::cell::Cell::new(0),
        };
        let mut copied = [u32::MAX; 6];
        assert_eq!(send_mmio(&io, &[0x4000], Some(&mut copied)), Ok(4));
        assert_eq!(copied[0], response[0]);
        assert_eq!(&copied[1..4], &[0, 0, 0]);
        assert_eq!(copied[4..], [u32::MAX; 2]);
    }
}
