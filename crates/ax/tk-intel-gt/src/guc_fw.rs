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
const GEN11_HUC_KERNEL_LOAD_INFO: u32 = 0xc1dc;
const HUC_LOAD_SUCCESSFUL: u32 = 1 << 0;
const GEN11_GUC_SEND_BASE: u32 = 0x190240;
const GEN11_GUC_HOST_INTERRUPT: u32 = 0x1901f0;
const GEN11_GUC_SEND_COUNT: usize = 4;
const MEDIA_GUC_HOST_INTERRUPT: u32 = 0x190304;
const MEDIA_GUC_SEND_BASE: u32 = 0x190310;
const GUC_SEND_TRIGGER: u32 = 1;
const HXG_ORIGIN_GUC: u32 = 1 << 31;
const HXG_TYPE_MASK: u32 = 7 << 28;
const HXG_TYPE_RESPONSE_SUCCESS: u32 = 7 << 28;
const HXG_TYPE_NO_RESPONSE_BUSY: u32 = 3 << 28;
const HXG_TYPE_NO_RESPONSE_RETRY: u32 = 5 << 28;
const HXG_TYPE_RESPONSE_FAILURE: u32 = 6 << 28;
const ACTION_AUTHENTICATE_HUC: u32 = 0x4000;
pub const ACTION_CLIENT_SOFT_RESET: u32 = 0x5507;
const ACTION_HOST2GUC_SELF_CFG: u32 = 0x0508;
const UOS_RSA_SCRATCH: u32 = 0xc200;
const UOS_RSA_SCRATCH_COUNT: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GucSendRegs {
    pub scratch_base: u32,
    pub host_interrupt: u32,
    pub count: usize,
}

pub const GT_GUC_SEND_REGS: GucSendRegs = GucSendRegs {
    scratch_base: GEN11_GUC_SEND_BASE,
    host_interrupt: GEN11_GUC_HOST_INTERRUPT,
    count: GEN11_GUC_SEND_COUNT,
};

pub const MEDIA_GUC_SEND_REGS: GucSendRegs = GucSendRegs {
    scratch_base: MEDIA_GUC_SEND_BASE,
    host_interrupt: MEDIA_GUC_HOST_INTERRUPT,
    count: GEN11_GUC_SEND_COUNT,
};

// upstream: intel_guc.c guc_send_reg().
fn send_reg_offset(regs: GucSendRegs, index: usize) -> Result<u32, Error> {
    if index >= regs.count {
        return Err(Error::Refused);
    }
    Ok(regs.scratch_base + index as u32 * 4)
}

// upstream: intel_guc.c intel_guc_notify()
pub fn notify(io: &impl GtIo) -> Result<(), Error> {
    notify_with_regs(io, GT_GUC_SEND_REGS)
}

pub fn notify_with_regs(io: &impl GtIo, regs: GucSendRegs) -> Result<(), Error> {
    io.write(regs.host_interrupt, GUC_SEND_TRIGGER)
}

/// Suspend the GuC after device-idle coordination: issue CLIENT_SOFT_RESET
/// only when submission is active, ignore its response failure as upstream
/// does, then reset the GuC domain to sanitize firmware state.
/// upstream: intel_guc.c intel_guc_suspend().
pub fn suspend_guc(
    io: &impl GtIo,
    ready: bool,
    submission_used: bool,
    graphics_ip: (u8, u8),
    mut client_soft_reset: impl FnMut() -> Result<(), Error>,
) -> Result<(), Error> {
    if !ready {
        return Ok(());
    }
    if submission_used {
        let _ = client_soft_reset();
    }
    crate::reset::reset_guc(io, graphics_ip)
}

/// GuC has no extra resume action after sanitize/reinitialization.
/// upstream: intel_guc.c intel_guc_resume().
pub const fn resume_guc() {}

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

// upstream: intel_uc_fw.c uc_fw_xfer()
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

// upstream: intel_uc_fw.c intel_uc_fw_copy_rsa()
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

// upstream: intel_guc_fw.c guc_xfer_rsa_mmio()
fn guc_xfer_rsa_mmio(io: &impl GtIo, image: &FirmwareImage) -> Result<(), Error> {
    let rsa = rsa_words(image)?;
    for (index, word) in rsa.into_iter().enumerate() {
        io.write(UOS_RSA_SCRATCH + index as u32 * 4, word)?;
    }
    Ok(())
}

// upstream: intel_guc_fw.c guc_xfer_rsa_vma()
fn guc_xfer_rsa_vma(io: &impl GtIo, source_ggtt: u64, image: &FirmwareImage) -> Result<(), Error> {
    let rsa_offset = image
        .css
        .header_bytes
        .checked_add(image.css.microcode_bytes)
        .and_then(|offset| u64::try_from(offset).ok()?.checked_add(source_ggtt))
        .and_then(|offset| u32::try_from(offset).ok())
        .ok_or(Error::Refused)?;
    io.write(UOS_RSA_SCRATCH, rsa_offset)
}

// upstream: intel_guc_fw.c guc_xfer_rsa()
fn guc_xfer_rsa(io: &impl GtIo, source_ggtt: u64, image: &FirmwareImage) -> Result<(), Error> {
    if image.css.rsa_bytes > UOS_RSA_SCRATCH_COUNT * 4 {
        guc_xfer_rsa_vma(io, source_ggtt, image)
    } else {
        guc_xfer_rsa_mmio(io, image)
    }
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
    image
        .change_status(crate::uc::FirmwareStatus::Loadable)
        .map_err(|_| LoadError::Io(Error::Refused))?;
    let result = (|| {
        for write in gen12_prepare_xfer() {
            io.write(write.offset, write.value).map_err(LoadError::Io)?;
        }
        guc_xfer_rsa(io, source_ggtt, image).map_err(LoadError::Io)?;
        let code_bytes = image
            .css
            .header_bytes
            .checked_add(image.css.microcode_bytes)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(LoadError::Io(Error::Refused))?;
        firmware_dma_xfer(io, source_ggtt, 0x2000, code_bytes, UOS_MOVE).map_err(LoadError::Io)?;
        image
            .change_status(crate::uc::FirmwareStatus::Transferred)
            .map_err(|_| LoadError::Io(Error::Refused))?;
        wait_ucode(io)
    })();
    match result {
        Ok(status) => {
            if image
                .change_status(crate::uc::FirmwareStatus::Running)
                .is_err()
            {
                let _ = image.change_status(crate::uc::FirmwareStatus::LoadFail);
                return Err(LoadError::Io(Error::Refused));
            }
            Ok(status)
        }
        Err(error) => {
            let _ = image.change_status(crate::uc::FirmwareStatus::LoadFail);
            Err(error)
        }
    }
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
    image
        .change_status(crate::uc::FirmwareStatus::Loadable)
        .map_err(|_| Error::Refused)?;
    let result = (|| {
        let code_bytes = image
            .css
            .header_bytes
            .checked_add(image.css.microcode_bytes)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(Error::Refused)?;
        firmware_dma_xfer(io, source_ggtt, 0, code_bytes, HUC_UKERNEL)
    })();
    match result {
        Ok(()) => image
            .change_status(crate::uc::FirmwareStatus::Transferred)
            .map_err(|_| Error::Refused),
        Err(error) => {
            let _ = image.change_status(crate::uc::FirmwareStatus::LoadFail);
            Err(error)
        }
    }
}

// upstream: intel_huc.c intel_huc_is_authenticated()
pub fn huc_is_authenticated(status: u32) -> bool {
    status & HUC_LOAD_SUCCESSFUL == HUC_LOAD_SUCCESSFUL
}

// upstream: intel_huc.c intel_huc_wait_for_auth_complete()
pub fn wait_huc_auth(io: &impl GtIo) -> Result<u32, Error> {
    for _ in 0..3 {
        let start = io.now_us();
        loop {
            let status = io.read(GEN11_HUC_KERNEL_LOAD_INFO)?;
            if huc_is_authenticated(status) {
                return Ok(status);
            }
            if io.now_us().saturating_sub(start) >= 1_000_000 {
                break;
            }
            io.delay_us(2);
        }
    }
    Err(Error::Timeout(GEN11_HUC_KERNEL_LOAD_INFO))
}

// upstream: intel_guc.c intel_guc_send_mmio()
/// Gen11+ four-dword MMIO transport. The caller serializes the send path and
/// owns forcewake.
pub fn send_mmio(
    io: &impl GtIo,
    request: &[u32],
    response_buf: Option<&mut [u32]>,
) -> Result<u32, Error> {
    send_mmio_with_regs(io, GT_GUC_SEND_REGS, request, response_buf)
}

/// upstream: intel_guc.c intel_guc_send_mmio() with GT-specific scratch and
/// host-interrupt register selection (`GEN11_SOFT_SCRATCH` vs `MEDIA_*`).
pub fn send_mmio_with_regs(
    io: &impl GtIo,
    regs: GucSendRegs,
    request: &[u32],
    mut response_buf: Option<&mut [u32]>,
) -> Result<u32, Error> {
    if request.is_empty()
        || request.len() > regs.count
        || request[0] & HXG_ORIGIN_GUC != 0
        || request[0] & HXG_TYPE_MASK != 0
    {
        return Err(Error::Refused);
    }
    loop {
        for (index, word) in request.iter().copied().enumerate() {
            io.write(send_reg_offset(regs, index)?, word)?;
        }
        let _posted = io.read(send_reg_offset(regs, request.len() - 1)?)?;
        notify_with_regs(io, regs)?;
        let mut response = crate::wait(
            io,
            regs.scratch_base,
            HXG_ORIGIN_GUC,
            HXG_ORIGIN_GUC,
            10_000,
        )?;
        let busy_start = io.now_us();
        while response & HXG_TYPE_MASK == HXG_TYPE_NO_RESPONSE_BUSY {
            if io.now_us().saturating_sub(busy_start) >= 1_000_000 {
                return Err(Error::Timeout(regs.scratch_base));
            }
            io.delay_us(1);
            response = io.read(regs.scratch_base)?;
            if response & HXG_ORIGIN_GUC == 0 {
                return Err(Error::Unavailable(regs.scratch_base));
            }
        }
        match response & HXG_TYPE_MASK {
            HXG_TYPE_RESPONSE_SUCCESS => {
                if let Some(response_buf) = response_buf.as_deref_mut() {
                    let count = response_buf.len().min(regs.count);
                    if count == 0 {
                        return Err(Error::Refused);
                    }
                    response_buf[0] = response;
                    for (index, word) in response_buf.iter_mut().enumerate().take(count).skip(1) {
                        *word = io.read(send_reg_offset(regs, index)?)?;
                    }
                    return Ok(count as u32);
                }
                return Ok(response & 0x0fff_ffff);
            }
            HXG_TYPE_RESPONSE_FAILURE => return Err(Error::Refused),
            HXG_TYPE_NO_RESPONSE_RETRY => continue,
            _ => return Err(Error::Unavailable(regs.scratch_base)),
        }
    }
}

// upstream: intel_guc.c intel_guc_auth_huc()
/// Ask the running GuC to authenticate HuC firmware's RSA data in GGTT.
pub fn authenticate_huc(io: &impl GtIo, rsa_offset: u32) -> Result<u32, Error> {
    authenticate_huc_with_regs(io, GT_GUC_SEND_REGS, rsa_offset)
}

pub fn authenticate_huc_with_regs(
    io: &impl GtIo,
    regs: GucSendRegs,
    rsa_offset: u32,
) -> Result<u32, Error> {
    send_mmio_with_regs(io, regs, &[ACTION_AUTHENTICATE_HUC, rsa_offset], None)
}

// upstream: intel_guc.c __guc_action_self_cfg()
pub fn self_config(io: &impl GtIo, key: u16, len: u16, value: u64) -> Result<(), Error> {
    self_config_with_regs(io, GT_GUC_SEND_REGS, key, len, value)
}

pub fn self_config_with_regs(
    io: &impl GtIo,
    regs: GucSendRegs,
    key: u16,
    len: u16,
    value: u64,
) -> Result<(), Error> {
    if !(1..=2).contains(&len) || (len == 1 && value >> 32 != 0) {
        return Err(Error::Refused);
    }
    let request = [
        ACTION_HOST2GUC_SELF_CFG,
        (u32::from(key) << 16) | u32::from(len),
        value as u32,
        (value >> 32) as u32,
    ];
    let response = send_mmio_with_regs(io, regs, &request, None)?;
    if response > 1 {
        return Err(Error::Unavailable(regs.scratch_base));
    }
    if response == 0 {
        return Err(Error::Refused);
    }
    Ok(())
}

// upstream: intel_guc.c intel_guc_self_cfg32()
pub fn self_config32(io: &impl GtIo, key: u16, value: u32) -> Result<(), Error> {
    self_config32_with_regs(io, GT_GUC_SEND_REGS, key, value)
}

// upstream: intel_guc.c intel_guc_self_cfg64()
pub fn self_config64(io: &impl GtIo, key: u16, value: u64) -> Result<(), Error> {
    self_config64_with_regs(io, GT_GUC_SEND_REGS, key, value)
}

pub fn self_config32_with_regs(
    io: &impl GtIo,
    regs: GucSendRegs,
    key: u16,
    value: u32,
) -> Result<(), Error> {
    self_config_with_regs(io, regs, key, 1, u64::from(value))
}

pub fn self_config64_with_regs(
    io: &impl GtIo,
    regs: GucSendRegs,
    key: u16,
    value: u64,
) -> Result<(), Error> {
    self_config_with_regs(io, regs, key, 2, value)
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
pub enum LoadFailure {
    MissingKey,
    SignatureRejected,
    ProductionKeyRejected,
    FirmwareException,
    InvalidMmioSaveRestore,
    InvalidWorkaroundKlv,
    HardwareConfigTimeout,
    Other,
}

impl LoadFailure {
    pub fn into_error(self, status: u32) -> Error {
        match self {
            Self::HardwareConfigTimeout => Error::Timeout(0xc000),
            Self::FirmwareException | Self::Other => Error::Unavailable(status),
            Self::MissingKey
            | Self::SignatureRejected
            | Self::ProductionKeyRejected
            | Self::InvalidMmioSaveRestore
            | Self::InvalidWorkaroundKlv => Error::Refused,
        }
    }
}

// upstream: intel_guc_fw.c guc_wait_ucode()
pub fn load_failure(status: u32) -> LoadFailure {
    let bootrom = (status >> 1) & 0x7f;
    let ukernel = (status >> 8) & 0xff;
    match bootrom {
        0x13 => return LoadFailure::MissingKey,
        0x50 => return LoadFailure::SignatureRejected,
        0x2b => return LoadFailure::ProductionKeyRejected,
        _ => {}
    }
    match ukernel {
        0x70 => LoadFailure::FirmwareException,
        0x74 => LoadFailure::InvalidMmioSaveRestore,
        0x75 => LoadFailure::InvalidWorkaroundKlv,
        0x05 => LoadFailure::HardwareConfigTimeout,
        _ => LoadFailure::Other,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadError {
    Io(Error),
    Firmware { status: u32, failure: LoadFailure },
    Timeout { status: u32, attempts: u8 },
}

pub fn wait_ucode(io: &impl GtIo) -> Result<u32, LoadError> {
    wait_ucode_with(io, 1_000_000, 3)
}

// upstream: intel_guc_fw.c guc_wait_ucode()
fn wait_ucode_with(io: &impl GtIo, wait_us: u64, attempts: u8) -> Result<u32, LoadError> {
    let mut status = 0;
    for attempt in 0..attempts {
        let start = io.now_us();
        loop {
            status = io.read(0xc000).map_err(LoadError::Io)?; // GUC_STATUS
            match load_done(status) {
                Some(true) => return Ok(status),
                Some(false) => {
                    return Err(LoadError::Firmware {
                        status,
                        failure: load_failure(status),
                    });
                }
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
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};

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

    struct ResetIo {
        writes: RefCell<Vec<(u32, u32)>>,
        delayed: Cell<u32>,
    }
    impl GtIo for ResetIo {
        fn read(&self, _offset: u32) -> Result<u32, Error> {
            Ok(0)
        }
        fn write(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.writes.borrow_mut().push((offset, value));
            Ok(())
        }
        fn now_us(&self) -> u64 {
            0
        }
        fn delay_us(&self, micros: u32) {
            self.delayed.set(self.delayed.get() + micros);
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
    fn guc_load_failure_diagnostics_match_bootrom_and_ukernel_errors() {
        for (status, failure) in [
            (0x13 << 1, LoadFailure::MissingKey),
            (0x50 << 1, LoadFailure::SignatureRejected),
            (0x2b << 1, LoadFailure::ProductionKeyRejected),
            (0x70 << 8, LoadFailure::FirmwareException),
            (0x74 << 8, LoadFailure::InvalidMmioSaveRestore),
            (0x75 << 8, LoadFailure::InvalidWorkaroundKlv),
            (0x05 << 8, LoadFailure::HardwareConfigTimeout),
            (0x73 << 8, LoadFailure::Other),
        ] {
            assert_eq!(load_failure(status), failure);
        }
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
    fn suspend_guc_soft_resets_only_submission_and_always_sanitizes_ready_guc() {
        let io = ResetIo {
            writes: RefCell::new(Vec::new()),
            delayed: Cell::new(0),
        };
        let mut soft_reset_count = 0;
        suspend_guc(&io, false, true, (12, 0), || {
            soft_reset_count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(soft_reset_count, 0);
        assert!(io.writes.borrow().is_empty());

        suspend_guc(&io, true, false, (12, 0), || {
            soft_reset_count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(soft_reset_count, 0);
        assert_eq!(io.writes.borrow().len(), 2);

        suspend_guc(&io, true, true, (12, 70), || {
            soft_reset_count += 1;
            Err(Error::Refused)
        })
        .unwrap();
        assert_eq!(soft_reset_count, 1);
        assert_eq!(io.writes.borrow().len(), 3);
        assert_eq!(io.delayed.get(), 100);
        assert_eq!(ACTION_CLIENT_SOFT_RESET, 0x5507);
        resume_guc();
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
            Err(LoadError::Firmware {
                status: 0x02 << 8,
                failure: LoadFailure::Other,
            })
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
            let send_base = if offset == GEN11_GUC_SEND_BASE {
                Some(GEN11_GUC_SEND_BASE)
            } else if offset == MEDIA_GUC_SEND_BASE {
                Some(MEDIA_GUC_SEND_BASE)
            } else {
                None
            };
            if let Some(_base) = send_base {
                let notification = self.notifications.get().saturating_sub(1);
                let responses = self.responses[notification.min(self.responses.len() - 1)];
                let index = self.response_index.get();
                self.response_index.set(index.saturating_add(1));
                return Ok(responses[index.min(responses.len() - 1)]);
            }
            if (GEN11_GUC_SEND_BASE + 4..GEN11_GUC_SEND_BASE + 16).contains(&offset)
                || (MEDIA_GUC_SEND_BASE + 4..MEDIA_GUC_SEND_BASE + 16).contains(&offset)
            {
                return Ok(0);
            }
            Err(Error::Unavailable(offset))
        }
        fn write(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.writes.borrow_mut().push((offset, value));
            if offset == GEN11_GUC_HOST_INTERRUPT || offset == MEDIA_GUC_HOST_INTERRUPT {
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
            if offset == GEN11_HUC_KERNEL_LOAD_INFO {
                return Ok(HUC_LOAD_SUCCESSFUL);
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
            old_version: false,
            bytes,
            status: core::sync::atomic::AtomicU8::new(crate::uc::FirmwareStatus::Available as u8),
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
        assert_eq!(image.status(), crate::uc::FirmwareStatus::Available);
        assert_eq!(guc_upload(&io, 0x10_0000, &image), Ok(0xf0 << 8));
        assert_eq!(image.status(), crate::uc::FirmwareStatus::Running);
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
    fn guc_large_rsa_uses_the_pinned_ggtt_vma_path() {
        let mut image = huc_fixture();
        image.kind = crate::uc::Kind::GuC;
        image.css.rsa_bytes = UOS_RSA_SCRATCH_COUNT * 4 + 4;
        let io = DmaIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            time: core::cell::Cell::new(0),
            stuck: false,
            fail_start: false,
        };
        assert_eq!(guc_xfer_rsa(&io, 0x10_0000, &image), Ok(()));
        assert_eq!(*io.writes.borrow(), [(UOS_RSA_SCRATCH, 0x10_0100)]);
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
        assert_eq!(image.status(), crate::uc::FirmwareStatus::Transferred);
        assert_eq!(io.writes.borrow()[2], (DMA_ADDR_1_LOW, 0));
        assert_eq!(
            io.writes.borrow()[5],
            (DMA_CTRL, crate::masked_enable(HUC_UKERNEL | START_DMA))
        );
    }

    #[test]
    fn huc_authentication_status_requires_the_gen11_success_bit() {
        assert!(!huc_is_authenticated(0));
        assert!(!huc_is_authenticated(1 << 1));
        assert!(huc_is_authenticated(HUC_LOAD_SUCCESSFUL));
        assert!(huc_is_authenticated(HUC_LOAD_SUCCESSFUL | 0x1234));
    }

    #[test]
    fn huc_auth_wait_reads_gen11_load_info() {
        let io = DmaIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            time: core::cell::Cell::new(0),
            stuck: false,
            fail_start: false,
        };
        assert_eq!(wait_huc_auth(&io), Ok(HUC_LOAD_SUCCESSFUL));
    }

    #[test]
    fn gen11_mmio_send_handles_busy_retry_success_and_failure() {
        assert_eq!(
            send_reg_offset(GT_GUC_SEND_REGS, 0),
            Ok(GEN11_GUC_SEND_BASE)
        );
        assert_eq!(
            send_reg_offset(GT_GUC_SEND_REGS, 3),
            Ok(GEN11_GUC_SEND_BASE + 12)
        );
        assert_eq!(send_reg_offset(GT_GUC_SEND_REGS, 4), Err(Error::Refused));
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

        let media_values = [success];
        let media_responses = [&media_values[..]];
        let media = MmioIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            responses: &media_responses,
            notifications: core::cell::Cell::new(0),
            response_index: core::cell::Cell::new(0),
            time: core::cell::Cell::new(0),
        };
        assert_eq!(
            send_mmio_with_regs(&media, MEDIA_GUC_SEND_REGS, &[0x4000, 0x1234], None),
            Ok(0x1234)
        );
        assert_eq!(media.writes.borrow()[0], (MEDIA_GUC_SEND_BASE, 0x4000));
        assert_eq!(
            media.writes.borrow()[2],
            (MEDIA_GUC_HOST_INTERRUPT, GUC_SEND_TRIGGER)
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
    fn guc_self_config_encodes_klv_key_length_and_both_value_words() {
        let response = [HXG_ORIGIN_GUC | HXG_TYPE_RESPONSE_SUCCESS | 1];
        let responses = [&response[..]];
        let io = MmioIo {
            writes: core::cell::RefCell::new(std::vec::Vec::new()),
            responses: &responses,
            notifications: core::cell::Cell::new(0),
            response_index: core::cell::Cell::new(0),
            time: core::cell::Cell::new(0),
        };
        assert_eq!(self_config64(&io, 0x0903, 0x1234_5678_9abc_def0), Ok(()));
        assert_eq!(
            &io.writes.borrow()[..4],
            &[
                (GEN11_GUC_SEND_BASE, ACTION_HOST2GUC_SELF_CFG),
                (GEN11_GUC_SEND_BASE + 4, (0x0903 << 16) | 2),
                (GEN11_GUC_SEND_BASE + 8, 0x9abc_def0),
                (GEN11_GUC_SEND_BASE + 12, 0x1234_5678),
            ]
        );
        assert_eq!(self_config32(&io, 0x0904, 4096), Ok(()));
        assert_eq!(self_config(&io, 0x0904, 1, 1u64 << 32), Err(Error::Refused));
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
