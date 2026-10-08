//! Single-port ATA disk command path over AHCI DMA descriptors.
//!
//! The path translates the ATA parts of FreeBSD's `ahci_setup_fis()` and
//! `ahci_execute_transaction()` to `BlockDriverOps`. CAM CCB scheduling and
//! async callouts are not copied; the HBA command engine is polled synchronously
//! and serializes one request through slot zero. NCQ FIS encoding is provided
//! by `ata`, but NCQ queue admission/recovery is a later stage.

use core::{
    mem::ManuallyDrop,
    ptr::{self, NonNull},
};

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};

use super::{
    ata::{AtaRequest, setup_register_fis},
    controller::{AhciController, AhciIo, PortState},
    regs::{
        AHCI_CAP_64BIT, AHCI_MAX_SLOTS, AHCI_P_CI, AHCI_P_CLB, AHCI_P_CLBU, AHCI_P_FB, AHCI_P_FBU,
        AHCI_P_IS, AHCI_P_IX_TFE, AHCI_P_SERR, AHCI_P_TFD, AHCI_PRD_IPC, AHCI_PRD_MAX, ATA_S_ERROR,
    },
};
use crate::{BlockCapabilities, BlockDriverOps};

const COMMAND_LIST_BYTES: usize = 32 * AHCI_MAX_SLOTS;
const RECEIVED_FIS_BYTES: usize = 256;
const COMMAND_TABLE_HEADER_BYTES: usize = 128;
const PRD_BYTES: usize = 16;
const COMMAND_TIMEOUT_POLLS: usize = 50_000;
const POLL_DELAY_US: u32 = 100;

/// DMA owner for an AHCI memory region. Platform code supplies a coherent,
/// physically addressable allocation and its release callback.
pub struct DmaRegion {
    cpu: NonNull<u8>,
    bus: u64,
    len: usize,
    pages: usize,
    release: Option<unsafe fn(NonNull<u8>, usize)>,
}

// SAFETY: the DMA allocation remains pinned and exclusively owned by its
// workspace; AHCI disk access is serialized through `&mut self`, and the
// platform-provided release callback is valid from the owner’s drop context.
unsafe impl Send for DmaRegion {}
// SAFETY: shared access to a region only reads its immutable address metadata;
// the disk's mutable APIs serialize all CPU/device memory operations.
unsafe impl Sync for DmaRegion {}

impl DmaRegion {
    /// Constructs an owner for a DMA allocation made by the platform.
    ///
    /// # Safety
    /// The virtual range must be writable for `len` bytes, coherent with the
    /// device, and remain pinned at `bus` until `release` runs. `pages` and the
    /// callback must identify the exact allocation.
    pub unsafe fn from_raw_parts(
        cpu: NonNull<u8>,
        bus: u64,
        len: usize,
        pages: usize,
        release: unsafe fn(NonNull<u8>, usize),
    ) -> Self {
        Self {
            cpu,
            bus,
            len,
            pages,
            release: Some(release),
        }
    }

    /// Creates a non-owning region over a caller-managed DMA allocation.
    ///
    /// # Safety
    /// The caller must retain the allocation and meet the DMA safety contract
    /// until the region is dropped and the HBA has stopped using it.
    pub unsafe fn borrowed(cpu: NonNull<u8>, bus: u64, len: usize) -> Self {
        Self {
            cpu,
            bus,
            len,
            pages: 0,
            release: None,
        }
    }
}

impl Drop for DmaRegion {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            // SAFETY: the unsafe constructor binds this callback to this exact
            // allocation, and the workspace owner drops only after HBA stop.
            unsafe { release(self.cpu, self.pages) };
        }
    }
}

/// The per-port command list, received-FIS area, command table, and persistent
/// bounce buffer. Keeping DMA buffers in the disk object protects them through
/// request timeout and controller reset paths.
pub struct PortWorkspace {
    command_list: DmaRegion,
    received_fis: DmaRegion,
    command_table: DmaRegion,
    bounce: DmaRegion,
}

impl PortWorkspace {
    /// Validates all required AHCI alignment and minimum-size constraints.
    pub fn new(
        command_list: DmaRegion,
        received_fis: DmaRegion,
        command_table: DmaRegion,
        bounce: DmaRegion,
    ) -> Result<Self, AhciDiskError> {
        if command_list.len < COMMAND_LIST_BYTES
            || command_list.bus as usize & 0x3ff != 0
            || command_list.cpu.as_ptr() as usize & 0x3ff != 0
            || received_fis.len < RECEIVED_FIS_BYTES
            || received_fis.bus as usize & 0xff != 0
            || received_fis.cpu.as_ptr() as usize & 0xff != 0
            || command_table.len < COMMAND_TABLE_HEADER_BYTES + PRD_BYTES
            || command_table.bus as usize & 0x7f != 0
            || command_table.cpu.as_ptr() as usize & 0x7f != 0
            || bounce.len < 512
            || bounce.len > AHCI_PRD_MAX
            || bounce.bus as usize & 1 != 0
            || bounce.cpu.as_ptr() as usize & 1 != 0
        {
            return Err(AhciDiskError::InvalidWorkspace);
        }
        // SAFETY: each allocation is owned by this value and is large enough
        // for the zeroing extent validated above.
        unsafe {
            ptr::write_bytes(command_list.cpu.as_ptr(), 0, COMMAND_LIST_BYTES);
            ptr::write_bytes(received_fis.cpu.as_ptr(), 0, RECEIVED_FIS_BYTES);
            ptr::write_bytes(command_table.cpu.as_ptr(), 0, command_table.len);
            ptr::write_bytes(bounce.cpu.as_ptr(), 0, bounce.len);
        }
        Ok(Self {
            command_list,
            received_fis,
            command_table,
            bounce,
        })
    }

    /// Available bytes in the request bounce buffer.
    pub const fn bounce_capacity(&self) -> usize {
        self.bounce.len
    }
}

/// ATA IDENTIFY-derived disk geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtaGeometry {
    pub block_size: usize,
    pub blocks: u64,
    pub lba48: bool,
}

/// AHCI disk attach/command failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AhciDiskError {
    InvalidWorkspace,
    UnsupportedAddressWidth,
    PortDidNotStop,
    NoDevice,
    IdentifyFailed,
    InvalidIdentifyData,
    CommandTimeout,
    DeviceError(u32),
    OutOfRange,
    InvalidRequest,
}

/// A single-port disk issuing serialized ATA commands through AHCI slot 0.
pub struct AhciDisk<I: AhciIo> {
    controller: AhciController<I>,
    port: PortState,
    workspace: ManuallyDrop<PortWorkspace>,
    geometry: AtaGeometry,
    poisoned: bool,
    workspace_live: bool,
}

impl<I: AhciIo> AhciDisk<I> {
    /// Initializes one SATA port and identifies an ATA disk.
    ///
    /// Workspace allocations are retained by the returned driver. On a failed
    /// attach, command/FIS DMA is stopped before the allocations are released;
    /// if stop cannot prove DMA quiescence they are deliberately leaked.
    pub fn attach(
        mut controller: AhciController<I>,
        mut port: PortState,
        workspace: PortWorkspace,
    ) -> Result<Self, AhciDiskError> {
        if controller.capabilities & AHCI_CAP_64BIT == 0
            && [
                workspace.command_list.bus,
                workspace.received_fis.bus,
                workspace.command_table.bus,
                workspace.bounce.bus,
            ]
            .iter()
            .any(|address| *address > u32::MAX as u64)
        {
            return Err(AhciDiskError::UnsupportedAddressWidth);
        }
        if !controller.ahci_stop_fr(&port) || !controller.ahci_stop(&mut port) {
            return Err(AhciDiskError::PortDidNotStop);
        }
        let base = port.register_base();
        controller
            .io_mut()
            .write32(base + AHCI_P_CLB, workspace.command_list.bus as u32);
        controller.io_mut().write32(
            base + AHCI_P_CLBU,
            (workspace.command_list.bus >> 32) as u32,
        );
        controller
            .io_mut()
            .write32(base + AHCI_P_FB, workspace.received_fis.bus as u32);
        controller
            .io_mut()
            .write32(base + AHCI_P_FBU, (workspace.received_fis.bus >> 32) as u32);
        controller.ahci_start_fr(&port);
        controller.ahci_start(&mut port, false);
        if !controller.ahci_sata_phy_reset(&mut port) {
            let stopped = controller.ahci_stop_fr(&port) && controller.ahci_stop(&mut port);
            if !stopped {
                core::mem::forget(workspace);
            }
            return Err(AhciDiskError::NoDevice);
        }
        let mut disk = Self {
            controller,
            port,
            workspace: ManuallyDrop::new(workspace),
            geometry: AtaGeometry {
                block_size: 512,
                blocks: 0,
                lba48: false,
            },
            poisoned: false,
            workspace_live: true,
        };
        let mut identify = [0u8; 512];
        if disk
            .transfer_read(AtaRequest::Identify { pmp_port: 0 }, &mut identify)
            .is_err()
        {
            return Err(disk.attach_failure(AhciDiskError::IdentifyFailed));
        }
        match parse_identify(&identify) {
            Some(geometry) if disk.workspace().bounce.len >= geometry.block_size => {
                disk.geometry = geometry
            }
            Some(_) => return Err(disk.attach_failure(AhciDiskError::InvalidWorkspace)),
            None => return Err(disk.attach_failure(AhciDiskError::InvalidIdentifyData)),
        }
        Ok(disk)
    }

    /// Geometry captured by ATA IDENTIFY DEVICE.
    pub const fn geometry(&self) -> AtaGeometry {
        self.geometry
    }

    fn workspace(&self) -> &PortWorkspace {
        // SAFETY: the `ManuallyDrop` value remains initialized until `Drop`.
        unsafe { &*(&self.workspace as *const ManuallyDrop<PortWorkspace> as *const PortWorkspace) }
    }

    fn workspace_mut(&mut self) -> &mut PortWorkspace {
        // SAFETY: exclusive `&mut self` access gives exclusive workspace access.
        unsafe {
            &mut *(&mut self.workspace as *mut ManuallyDrop<PortWorkspace> as *mut PortWorkspace)
        }
    }

    fn attach_failure(&mut self, error: AhciDiskError) -> AhciDiskError {
        let stopped =
            self.controller.ahci_stop_fr(&self.port) && self.controller.ahci_stop(&mut self.port);
        if stopped {
            // The workspace will be dropped as `self` returns from attach.
            // Transfer ownership out and drop it now, after DMA quiescence.
            unsafe { ManuallyDrop::drop(&mut self.workspace) };
            self.workspace_live = false;
        } else {
            // The workspace is a `ManuallyDrop` field, so returning from
            // attach will retain its allocations while the HBA may still DMA.
            self.workspace_live = false;
        }
        error
    }

    /// Submit one ATA command through slot zero and synchronously poll CI.
    // upstream: ahci.c ahci_execute_transaction()
    fn execute(&mut self, request: AtaRequest, data_len: usize) -> Result<(), AhciDiskError> {
        if self.poisoned {
            return Err(AhciDiskError::CommandTimeout);
        }
        let (fis, attributes) =
            setup_register_fis(request).map_err(|_| AhciDiskError::InvalidRequest)?;
        if attributes.data_transfer && (data_len == 0 || data_len > self.workspace().bounce.len) {
            return Err(AhciDiskError::InvalidRequest);
        }
        let ws = self.workspace_mut();
        // SAFETY: the command table and list are owned, aligned DMA regions of
        // validated size; slot zero is the only in-flight request.
        unsafe {
            ptr::write_bytes(ws.command_table.cpu.as_ptr(), 0, ws.command_table.len);
            ptr::copy_nonoverlapping(fis.as_ptr(), ws.command_table.cpu.as_ptr(), fis.len());
            if attributes.data_transfer {
                let prd = ws
                    .command_table
                    .cpu
                    .as_ptr()
                    .add(COMMAND_TABLE_HEADER_BYTES) as *mut u32;
                ptr::write_unaligned(prd.add(0), ws.bounce.bus as u32);
                ptr::write_unaligned(prd.add(1), (ws.bounce.bus >> 32) as u32);
                ptr::write_unaligned(prd.add(2), 0);
                ptr::write_unaligned(
                    prd.add(3),
                    ((data_len - 1) as u32 & (AHCI_PRD_MAX as u32 - 1)) | AHCI_PRD_IPC,
                );
            }
            let header = ws.command_list.cpu.as_ptr() as *mut u8;
            ptr::write_unaligned(
                header.add(0) as *mut u16,
                5 | if attributes.device_reads_buffer {
                    1 << 6
                } else {
                    0
                },
            );
            ptr::write_unaligned(
                header.add(2) as *mut u16,
                u16::from(attributes.data_transfer),
            );
            ptr::write_unaligned(header.add(4) as *mut u32, 0);
            ptr::write_unaligned(header.add(8) as *mut u64, ws.command_table.bus);
        }
        let base = self.port.register_base();
        self.controller.io_mut().write32(base + AHCI_P_IS, u32::MAX);
        self.controller
            .io_mut()
            .write32(base + AHCI_P_SERR, u32::MAX);
        self.controller.io_mut().write32(base + AHCI_P_CI, 1);
        for _ in 0..COMMAND_TIMEOUT_POLLS {
            if self.controller.io_mut().read32(base + AHCI_P_CI) & 1 == 0 {
                let status = self.controller.io_mut().read32(base + AHCI_P_TFD);
                let interrupt = self.controller.io_mut().read32(base + AHCI_P_IS);
                if status & ATA_S_ERROR != 0 || interrupt & AHCI_P_IX_TFE != 0 {
                    return Err(AhciDiskError::DeviceError(status));
                }
                return Ok(());
            }
            self.controller.io_mut().delay_us(POLL_DELAY_US);
        }
        self.poisoned = true;
        let _ = self.controller.ahci_stop_fr(&self.port);
        let _ = self.controller.ahci_stop(&mut self.port);
        Err(AhciDiskError::CommandTimeout)
    }

    fn transfer_read(
        &mut self,
        request: AtaRequest,
        buffer: &mut [u8],
    ) -> Result<(), AhciDiskError> {
        if buffer.is_empty() {
            return Err(AhciDiskError::InvalidRequest);
        }
        self.execute(request, buffer.len())?;
        // SAFETY: copy only after the command completion proves DMA has ended.
        unsafe {
            ptr::copy_nonoverlapping(
                self.workspace().bounce.cpu.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        Ok(())
    }

    fn transfer_write(&mut self, request: AtaRequest, buffer: &[u8]) -> Result<(), AhciDiskError> {
        if buffer.is_empty() {
            return Err(AhciDiskError::InvalidRequest);
        }
        // SAFETY: the bounce allocation is writable and `buffer.len()` is
        // bounded by the previously validated workspace capacity.
        unsafe {
            ptr::copy_nonoverlapping(
                buffer.as_ptr(),
                self.workspace_mut().bounce.cpu.as_ptr(),
                buffer.len(),
            )
        };
        self.execute(request, buffer.len())
    }
}

impl<I: AhciIo> Drop for AhciDisk<I> {
    fn drop(&mut self) {
        if !self.workspace_live {
            return;
        }
        let fis_stopped = self.controller.ahci_stop_fr(&self.port);
        let command_stopped = self.controller.ahci_stop(&mut self.port);
        if fis_stopped && command_stopped {
            // SAFETY: both engines stopped, so DMA regions can be released.
            unsafe { ManuallyDrop::drop(&mut self.workspace) };
            self.workspace_live = false;
        } else {
            // The device may retain access to every address in the workspace.
            // The `ManuallyDrop` field intentionally leaks it rather than
            // recycling DMA memory.
            self.workspace_live = false;
        }
    }
}

impl<I: AhciIo> BaseDriverOps for AhciDisk<I> {
    fn device_name(&self) -> &str {
        "ahci"
    }
    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
}

impl<I: AhciIo> BlockDriverOps for AhciDisk<I> {
    fn num_blocks(&self) -> u64 {
        self.geometry.blocks
    }
    fn block_size(&self) -> usize {
        self.geometry.block_size
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult {
        self.transfer_blocks(block_id, buf, false)
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        if buf.is_empty() {
            return Ok(());
        }
        if buf.len() % self.geometry.block_size != 0 {
            return Err(DevError::InvalidParam);
        }
        let blocks = (buf.len() / self.geometry.block_size) as u64;
        if block_id
            .checked_add(blocks)
            .is_none_or(|end| end > self.geometry.blocks)
        {
            return Err(DevError::InvalidParam);
        }
        let mut offset = 0;
        let mut lba = block_id;
        let max_bytes =
            self.workspace().bounce.len / self.geometry.block_size * self.geometry.block_size;
        while offset < buf.len() {
            let count = (buf.len() - offset).min(max_bytes);
            let count = count - count % self.geometry.block_size;
            if count == 0 {
                return Err(DevError::InvalidParam);
            }
            let sectors = u16::try_from(count / self.geometry.block_size)
                .map_err(|_| DevError::InvalidParam)?;
            let request = AtaRequest::DmaExt {
                lba,
                sectors,
                write: true,
                pmp_port: 0,
            };
            self.transfer_write(request, &buf[offset..offset + count])
                .map_err(map_error)?;
            offset += count;
            lba = lba
                .checked_add(u64::from(sectors))
                .ok_or(DevError::InvalidParam)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> DevResult {
        self.execute(AtaRequest::FlushCacheExt { pmp_port: 0 }, 0)
            .map_err(map_error)
    }

    fn block_capabilities(&self) -> BlockCapabilities {
        BlockCapabilities {
            flush: true,
            ..BlockCapabilities::default()
        }
    }
}

impl<I: AhciIo> AhciDisk<I> {
    fn transfer_blocks(&mut self, block_id: u64, buf: &mut [u8], write: bool) -> DevResult {
        if buf.is_empty() {
            return Ok(());
        }
        if buf.len() % self.geometry.block_size != 0 {
            return Err(DevError::InvalidParam);
        }
        let blocks = (buf.len() / self.geometry.block_size) as u64;
        if block_id
            .checked_add(blocks)
            .is_none_or(|end| end > self.geometry.blocks)
        {
            return Err(DevError::InvalidParam);
        }
        let max_bytes =
            self.workspace().bounce.len / self.geometry.block_size * self.geometry.block_size;
        let mut offset = 0;
        let mut lba = block_id;
        while offset < buf.len() {
            let count = (buf.len() - offset).min(max_bytes);
            let sectors = u16::try_from(count / self.geometry.block_size)
                .map_err(|_| DevError::InvalidParam)?;
            let request = AtaRequest::DmaExt {
                lba,
                sectors,
                write,
                pmp_port: 0,
            };
            if write {
                self.transfer_write(request, &buf[offset..offset + count])
                    .map_err(map_error)?;
            } else {
                self.transfer_read(request, &mut buf[offset..offset + count])
                    .map_err(map_error)?;
            }
            offset += count;
            lba += u64::from(sectors);
        }
        Ok(())
    }
}

/// Parses ATA IDENTIFY words into a supported LBA48 block geometry.
pub fn parse_identify(bytes: &[u8; 512]) -> Option<AtaGeometry> {
    let word = |index: usize| u16::from_le_bytes([bytes[index * 2], bytes[index * 2 + 1]]);
    let lba48 = word(83) & (1 << 10) != 0;
    if !lba48 {
        return None;
    }
    let blocks = u64::from(word(100))
        | (u64::from(word(101)) << 16)
        | (u64::from(word(102)) << 32)
        | (u64::from(word(103)) << 48);
    if blocks == 0 {
        return None;
    }
    let sector_size =
        if word(106) & (1 << 14) != 0 && word(106) & (1 << 15) == 0 && word(106) & (1 << 12) != 0 {
            (u32::from(word(117)) | (u32::from(word(118)) << 16)).checked_mul(2)? as usize
        } else {
            512
        };
    if sector_size < 512 || !sector_size.is_power_of_two() {
        return None;
    }
    Some(AtaGeometry {
        block_size: sector_size,
        blocks,
        lba48,
    })
}

fn map_error(error: AhciDiskError) -> DevError {
    match error {
        AhciDiskError::InvalidWorkspace | AhciDiskError::UnsupportedAddressWidth => {
            DevError::InvalidParam
        }
        AhciDiskError::CommandTimeout | AhciDiskError::DeviceError(_) => DevError::Io,
        AhciDiskError::NoDevice => DevError::Io,
        _ => DevError::BadState,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(align(4096))]
    struct Page([u8; 4096]);

    #[test]
    fn identify_requires_lba48_and_decodes_logical_sector_size() {
        let mut bytes = [0u8; 512];
        let set_word = |bytes: &mut [u8; 512], index: usize, value: u16| {
            bytes[index * 2..index * 2 + 2].copy_from_slice(&value.to_le_bytes());
        };
        set_word(&mut bytes, 83, 1 << 10);
        set_word(&mut bytes, 100, 0x5678);
        set_word(&mut bytes, 101, 0x1234);
        set_word(&mut bytes, 106, (1 << 14) | (1 << 12));
        set_word(&mut bytes, 117, 2048);
        let geometry = parse_identify(&bytes).unwrap();
        assert_eq!(geometry.block_size, 4096);
        assert_eq!(geometry.blocks, 0x1234_5678);
        assert!(geometry.lba48);
        set_word(&mut bytes, 83, 0);
        assert_eq!(parse_identify(&bytes), None);
    }

    #[test]
    fn workspace_accepts_aligned_regions_and_rejects_bad_sizes() {
        let command_list = Page([0; 4096]);
        let received_fis = Page([0; 4096]);
        let command_table = Page([0; 4096]);
        let bounce = Page([0; 4096]);
        let regions = unsafe {
            (
                DmaRegion::borrowed(NonNull::from(&command_list.0[0]), 0x1000, 4096),
                DmaRegion::borrowed(NonNull::from(&received_fis.0[0]), 0x2000, 4096),
                DmaRegion::borrowed(NonNull::from(&command_table.0[0]), 0x3000, 4096),
                DmaRegion::borrowed(NonNull::from(&bounce.0[0]), 0x4000, 4096),
            )
        };
        assert!(PortWorkspace::new(regions.0, regions.1, regions.2, regions.3).is_ok());
        let short = unsafe { DmaRegion::borrowed(NonNull::from(&bounce.0[0]), 0x4000, 64) };
        let command_list =
            unsafe { DmaRegion::borrowed(NonNull::from(&command_list.0[0]), 0x1000, 4096) };
        let received_fis =
            unsafe { DmaRegion::borrowed(NonNull::from(&received_fis.0[0]), 0x2000, 4096) };
        let command_table =
            unsafe { DmaRegion::borrowed(NonNull::from(&command_table.0[0]), 0x3000, 4096) };
        assert!(matches!(
            PortWorkspace::new(command_list, received_fis, command_table, short),
            Err(AhciDiskError::InvalidWorkspace)
        ));
    }
}
