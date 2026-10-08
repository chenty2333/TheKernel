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
    sync::atomic::{Ordering, fence},
};

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};

use super::{
    ata::{AtaRequest, setup_register_fis},
    controller::{AhciController, AhciIo, PortState},
    regs::{
        AHCI_CAP_64BIT, AHCI_CAP_SNCQ, AHCI_MAX_SLOTS, AHCI_P_CI, AHCI_P_CLB, AHCI_P_CLBU,
        AHCI_P_FB, AHCI_P_FBU, AHCI_P_IS, AHCI_P_IX_TFE, AHCI_P_SACT, AHCI_P_SERR, AHCI_P_TFD,
        AHCI_PRD_IPC, AHCI_PRD_MAX, ATA_S_ERROR,
    },
};
use crate::{
    BlockAsyncOp, BlockCapabilities, BlockCompletion, BlockCompletionDrain, BlockCompletionOwner,
    BlockCompletionStatus, BlockDriverOps, BlockQueueCaps, BlockQueueRequest, BlockRange,
    BlockRequestHandle, BlockSegmentDirection, BlockSubmitReport,
};

const COMMAND_LIST_BYTES: usize = 32 * AHCI_MAX_SLOTS;
const RECEIVED_FIS_BYTES: usize = 256;
const COMMAND_TABLE_HEADER_BYTES: usize = 128;
const PRD_BYTES: usize = 16;
const COMMAND_TIMEOUT_POLLS: usize = 50_000;
const POLL_DELAY_US: u32 = 100;
const MAX_ASYNC_SEGMENTS: usize = 16;

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
    pub ncq: bool,
    pub ncq_queue_depth: u8,
    pub trim: bool,
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
    RequestPending,
    DeviceError(u32),
    OutOfRange,
    InvalidRequest,
}

#[derive(Clone, Copy)]
struct AsyncSegment {
    address: usize,
    length: usize,
}

impl AsyncSegment {
    const EMPTY: Self = Self {
        address: 0,
        length: 0,
    };
}

#[derive(Clone, Copy)]
struct PendingAsync {
    handle: BlockRequestHandle,
    op: BlockAsyncOp,
    bytes: usize,
    segments: [AsyncSegment; MAX_ASYNC_SEGMENTS],
    segment_count: usize,
    polls: usize,
}

// Fixed-size segment metadata keeps async completion allocation-free; this
// intentionally stores one bounded request alongside the small terminal
// completion record.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy)]
enum AsyncState {
    Idle,
    InFlight(PendingAsync),
    Complete(BlockCompletion),
}

/// A single-port disk issuing serialized ATA commands through AHCI slot 0.
pub struct AhciDisk<I: AhciIo> {
    controller: AhciController<I>,
    port: PortState,
    workspace: ManuallyDrop<PortWorkspace>,
    geometry: AtaGeometry,
    ncq: bool,
    poisoned: bool,
    workspace_live: bool,
    async_state: AsyncState,
    next_async_handle: u64,
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
                ncq: false,
                ncq_queue_depth: 0,
                trim: false,
            },
            ncq: false,
            poisoned: false,
            workspace_live: true,
            async_state: AsyncState::Idle,
            next_async_handle: 1,
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
                disk.ncq = geometry.ncq
                    && controller_capabilities_for_ncq(disk.controller.capabilities)
                    && disk.port.quirks & super::regs::AHCI_Q_NONCQ == 0;
                disk.geometry = geometry;
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

    /// Translate one ATA request into slot-zero DMA descriptors before publication.
    // upstream: ahci.c ahci_setup_fis() + ahci_dmasetprd()
    fn build_command(
        &mut self,
        request: AtaRequest,
        data_len: usize,
    ) -> Result<bool, AhciDiskError> {
        if self.poisoned {
            return Err(AhciDiskError::CommandTimeout);
        }
        let (fis, attributes) =
            setup_register_fis(request).map_err(|_| AhciDiskError::InvalidRequest)?;
        if attributes.data_transfer && (data_len == 0 || data_len > self.workspace().bounce.len) {
            return Err(AhciDiskError::InvalidRequest);
        }
        let ws = self.workspace_mut();
        // SAFETY: the command table/list are owned, aligned DMA allocations;
        // this driver admits one request at a time through slot zero.
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
            let header = ws.command_list.cpu.as_ptr();
            ptr::write_unaligned(
                header as *mut u16,
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
        Ok(attributes.tag.is_some())
    }

    fn publish_command(&mut self, ncq: bool) {
        // Publish coherent command/bounce writes before handing slot zero to
        // the HBA. The x86 platform's DMA pages are cache coherent.
        fence(Ordering::Release);
        let base = self.port.register_base();
        if ncq {
            self.controller.io_mut().write32(base + AHCI_P_SACT, 1);
        }
        self.controller.io_mut().write32(base + AHCI_P_CI, 1);
    }

    fn sample_command(&mut self, ncq: bool) -> Option<Result<(), AhciDiskError>> {
        let base = self.port.register_base();
        let command_active = self.controller.io_mut().read32(base + AHCI_P_CI) & 1 != 0;
        let ncq_active = ncq && self.controller.io_mut().read32(base + AHCI_P_SACT) & 1 != 0;
        if command_active || ncq_active {
            return None;
        }
        // The HBA clears CI/SACT only after completing DMA; order subsequent
        // CPU reads of the persistent bounce buffer after that observation.
        fence(Ordering::Acquire);
        let status = self.controller.io_mut().read32(base + AHCI_P_TFD);
        let interrupt = self.controller.io_mut().read32(base + AHCI_P_IS);
        if status & ATA_S_ERROR != 0 || interrupt & AHCI_P_IX_TFE != 0 {
            Some(Err(AhciDiskError::DeviceError(status)))
        } else {
            Some(Ok(()))
        }
    }

    fn timeout_command(&mut self) -> bool {
        self.poisoned = true;
        let fis_stopped = self.controller.ahci_stop_fr(&self.port);
        let command_stopped = self.controller.ahci_stop(&mut self.port);
        fis_stopped && command_stopped
    }

    /// FreeBSD `ahci_execute_transaction()`'s serialized completion path.
    // upstream: ahci.c ahci_execute_transaction()
    fn execute(&mut self, request: AtaRequest, data_len: usize) -> Result<(), AhciDiskError> {
        if !matches!(self.async_state, AsyncState::Idle) {
            return Err(AhciDiskError::RequestPending);
        }
        let ncq = self.build_command(request, data_len)?;
        self.publish_command(ncq);
        for _ in 0..COMMAND_TIMEOUT_POLLS {
            if let Some(result) = self.sample_command(ncq) {
                if let Err(error @ AhciDiskError::DeviceError(_)) = result {
                    if !self.recover_command_error(ncq) {
                        self.poisoned = true;
                    }
                    return Err(error);
                }
                return result;
            }
            self.controller.io_mut().delay_us(POLL_DELAY_US);
        }
        let _ = self.timeout_command();
        Err(AhciDiskError::CommandTimeout)
    }

    /// FreeBSD keeps an NCQ error victim on hold while issuing READ LOG EXT,
    /// then restarts the channel before releasing it. This driver has one slot,
    /// so it reads the error log and reinitializes the engine before returning
    /// the exact failed request; no unrelated queued victim exists.
    // upstream: ahci.c ahci_process_read_log()
    fn recover_command_error(&mut self, ncq: bool) -> bool {
        if ncq && !self.read_ncq_error_log() {
            return false;
        }
        if !self.controller.ahci_stop(&mut self.port) {
            return false;
        }
        if !self.controller.ahci_clo(&self.port) {
            return false;
        }
        self.controller.ahci_start(&mut self.port, false);
        self.controller
            .ahci_wait_ready(&self.port, 1_000, 0)
            .is_ok()
    }

    fn read_ncq_error_log(&mut self) -> bool {
        if self.poisoned {
            return false;
        }
        let Ok(ncq) = self.build_command(AtaRequest::ReadLogExt { pmp_port: 0 }, 512) else {
            return false;
        };
        debug_assert!(!ncq);
        self.publish_command(false);
        for _ in 0..COMMAND_TIMEOUT_POLLS {
            if let Some(result) = self.sample_command(false) {
                return result.is_ok();
            }
            self.controller.io_mut().delay_us(POLL_DELAY_US);
        }
        self.timeout_command();
        false
    }

    fn copy_segments_to_bounce(&mut self, segments: &[AsyncSegment], len: usize) {
        let mut offset = 0;
        let bounce = self.workspace().bounce.cpu.as_ptr();
        for segment in segments {
            if segment.length == 0 {
                continue;
            }
            // SAFETY: submission validates total length, source buffers are
            // live for this call, and the DMA bounce range is owned by `self`.
            unsafe {
                ptr::copy_nonoverlapping(
                    segment.address as *const u8,
                    bounce.add(offset),
                    segment.length,
                );
            }
            offset += segment.length;
        }
        debug_assert_eq!(offset, len);
    }

    fn copy_bounce_to_segments(&mut self, pending: PendingAsync) {
        let mut offset = 0;
        let bounce = self.workspace().bounce.cpu.as_ptr();
        for segment in pending.segments.iter().take(pending.segment_count) {
            if segment.length == 0 {
                continue;
            }
            // SAFETY: BlockDriverOps keeps each caller segment valid until its
            // request handle completes; copying happens only after CI/SACT clear.
            unsafe {
                ptr::copy_nonoverlapping(
                    bounce.add(offset),
                    segment.address as *mut u8,
                    segment.length,
                );
            }
            offset += segment.length;
        }
        debug_assert_eq!(offset, pending.bytes);
    }

    fn reap_async(&mut self) -> bool {
        let AsyncState::InFlight(mut pending) = self.async_state else {
            return false;
        };
        if let Some(result) = self.sample_command(self.ncq && pending.op != BlockAsyncOp::Flush) {
            let status = match result {
                Ok(()) => {
                    if pending.op == BlockAsyncOp::Read {
                        self.copy_bounce_to_segments(pending);
                    }
                    BlockCompletionStatus::Success
                }
                Err(AhciDiskError::DeviceError(status)) => {
                    let ncq = self.ncq && pending.op != BlockAsyncOp::Flush;
                    if !self.recover_command_error(ncq) {
                        self.poisoned = true;
                    }
                    BlockCompletionStatus::DeviceError(status as u8)
                }
                Err(_) => BlockCompletionStatus::DeviceError(0xff),
            };
            self.async_state = AsyncState::Complete(BlockCompletion {
                handle: pending.handle,
                owner: BlockCompletionOwner::Ordinary,
                cookie: pending.handle.raw,
                status,
                bytes: if status == BlockCompletionStatus::Success {
                    pending.bytes as u32
                } else {
                    0
                },
            });
            return true;
        }
        pending.polls += 1;
        if pending.polls < COMMAND_TIMEOUT_POLLS {
            self.async_state = AsyncState::InFlight(pending);
            return false;
        }
        self.timeout_command();
        let quiesced =
            self.controller.ahci_stop_fr(&self.port) && self.controller.ahci_stop(&mut self.port);
        self.async_state = AsyncState::Complete(BlockCompletion {
            handle: pending.handle,
            owner: BlockCompletionOwner::Ordinary,
            cookie: pending.handle.raw,
            status: if quiesced {
                BlockCompletionStatus::DeviceError(0xff)
            } else {
                BlockCompletionStatus::Quarantined
            },
            bytes: 0,
        });
        true
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
        if !buf.len().is_multiple_of(self.geometry.block_size) {
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
            let request = self.data_request(lba, sectors, true);
            self.transfer_write(request, &buf[offset..offset + count])
                .map_err(map_error)?;
            offset += count;
            lba = lba
                .checked_add(u64::from(sectors))
                .ok_or(DevError::InvalidParam)?;
        }
        Ok(())
    }

    fn async_queue_caps(&self) -> Option<BlockQueueCaps> {
        (!self.poisoned).then_some(BlockQueueCaps {
            max_requests: 1,
            max_descriptors: 1,
            supports_indirect: false,
            supports_event_idx: false,
            default_depth: 1,
        })
    }

    fn submit_async_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        if requests.is_empty() {
            return Ok(BlockSubmitReport::default());
        }
        if !matches!(self.async_state, AsyncState::Idle) {
            return Ok(BlockSubmitReport {
                submitted: 0,
                bytes: 0,
                queue_full: true,
            });
        }
        let request = &mut requests[0];
        if request.segments.len() > MAX_ASYNC_SEGMENTS {
            return Err(DevError::InvalidParam);
        }
        let mut segments = [AsyncSegment::EMPTY; MAX_ASYNC_SEGMENTS];
        let mut segment_count = 0;
        let mut bytes = 0usize;
        for segment in request.segments {
            bytes = bytes
                .checked_add(segment.len)
                .ok_or(DevError::InvalidParam)?;
            if segment.len != 0 {
                if segment.addr == 0 {
                    return Err(DevError::InvalidParam);
                }
                segments[segment_count] = AsyncSegment {
                    address: segment.addr,
                    length: segment.len,
                };
                segment_count += 1;
            }
        }
        if bytes > self.workspace().bounce.len {
            return Err(DevError::InvalidParam);
        }
        let (ata, data_len) = match request.op {
            BlockAsyncOp::Read | BlockAsyncOp::Write => {
                if bytes == 0 || !bytes.is_multiple_of(self.geometry.block_size) {
                    return Err(DevError::InvalidParam);
                }
                let blocks = (bytes / self.geometry.block_size) as u64;
                if request
                    .block_id
                    .checked_add(blocks)
                    .is_none_or(|end| end > self.geometry.blocks)
                {
                    return Err(DevError::InvalidParam);
                }
                let direction = if request.op == BlockAsyncOp::Read {
                    BlockSegmentDirection::DeviceToMemory
                } else {
                    BlockSegmentDirection::MemoryToDevice
                };
                if request
                    .segments
                    .iter()
                    .any(|segment| segment.len != 0 && segment.direction != direction)
                {
                    return Err(DevError::InvalidParam);
                }
                let sectors = u16::try_from(blocks).map_err(|_| DevError::InvalidParam)?;
                let ata =
                    self.data_request(request.block_id, sectors, request.op == BlockAsyncOp::Write);
                (ata, bytes)
            }
            BlockAsyncOp::Flush => {
                if bytes != 0 {
                    return Err(DevError::InvalidParam);
                }
                (AtaRequest::FlushCacheExt { pmp_port: 0 }, 0)
            }
        };
        if request.op == BlockAsyncOp::Write {
            self.copy_segments_to_bounce(&segments, bytes);
        }
        let ncq = self.build_command(ata, data_len).map_err(map_error)?;
        let raw = self.next_async_handle.max(1);
        self.next_async_handle = raw.wrapping_add(1).max(1);
        let handle = BlockRequestHandle { raw };
        let pending = PendingAsync {
            handle,
            op: request.op,
            bytes,
            segments,
            segment_count,
            polls: 0,
        };
        // Publish ownership before ringing CI so even an immediate completion
        // has a valid higher-level request owner.
        self.async_state = AsyncState::InFlight(pending);
        self.publish_command(ncq);
        request.handle = Some(handle);
        Ok(BlockSubmitReport {
            submitted: 1,
            bytes,
            queue_full: requests.len() > 1,
        })
    }

    fn drain_async_completions(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        if output.is_empty() {
            return Ok(BlockCompletionDrain::default());
        }
        self.reap_async();
        if let AsyncState::Complete(completion) = self.async_state {
            output[0] = completion;
            self.async_state = AsyncState::Idle;
            Ok(BlockCompletionDrain {
                completed: 1,
                continuation: false,
            })
        } else {
            Ok(BlockCompletionDrain::default())
        }
    }

    fn poll_async_complete(&mut self, budget: usize) -> DevResult<usize> {
        if budget == 0 {
            return Ok(0);
        }
        self.reap_async();
        if matches!(self.async_state, AsyncState::Complete(_)) {
            self.async_state = AsyncState::Idle;
            Ok(1)
        } else {
            Ok(0)
        }
    }

    fn wait_async_all(&mut self, handles: &[BlockRequestHandle]) -> DevResult {
        let mut failed = false;
        for handle in handles {
            loop {
                match self.async_state {
                    AsyncState::InFlight(pending) if pending.handle == *handle => {
                        if !self.reap_async() {
                            self.controller.io_mut().delay_us(POLL_DELAY_US);
                        }
                    }
                    AsyncState::Complete(completion) if completion.handle == *handle => {
                        failed |= completion.status != BlockCompletionStatus::Success;
                        self.async_state = AsyncState::Idle;
                        break;
                    }
                    _ => return Err(DevError::InvalidParam),
                }
            }
        }
        if failed { Err(DevError::Io) } else { Ok(()) }
    }

    fn flush(&mut self) -> DevResult {
        self.execute(AtaRequest::FlushCacheExt { pmp_port: 0 }, 0)
            .map_err(map_error)
    }

    fn block_capabilities(&self) -> BlockCapabilities {
        BlockCapabilities {
            flush: true,
            discard: self.geometry.trim,
            ..BlockCapabilities::default()
        }
    }

    fn discard_blocks(&mut self, range: BlockRange) -> DevResult {
        if !self.geometry.trim {
            return Err(DevError::Unsupported);
        }
        if range
            .start
            .checked_add(range.blocks)
            .is_none_or(|end| end > self.geometry.blocks)
        {
            return Err(DevError::InvalidParam);
        }
        let mut current_lba = range.start;
        let mut remaining = range.blocks;
        while remaining != 0 {
            let mut payload = [0u8; 512];
            let mut entries = 0usize;
            while entries < 64 && remaining != 0 {
                let sectors = remaining.min(u64::from(u16::MAX)) as u16;
                let at = entries * 8;
                let lba = current_lba.to_le_bytes();
                payload[at..at + 6].copy_from_slice(&lba[..6]);
                payload[at + 6..at + 8].copy_from_slice(&sectors.to_le_bytes());
                current_lba = current_lba
                    .checked_add(u64::from(sectors))
                    .ok_or(DevError::InvalidParam)?;
                remaining -= u64::from(sectors);
                entries += 1;
            }
            // The device reads the DSM parameter list from the persistent DMA
            // bounce buffer; its ownership outlives this synchronous command.
            unsafe {
                ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    self.workspace_mut().bounce.cpu.as_ptr(),
                    payload.len(),
                )
            };
            self.execute(
                AtaRequest::DsmTrim {
                    parameter_sectors: 1,
                    pmp_port: 0,
                },
                payload.len(),
            )
            .map_err(map_error)?;
        }
        Ok(())
    }
}

impl<I: AhciIo> AhciDisk<I> {
    fn data_request(&self, lba: u64, sectors: u16, write: bool) -> AtaRequest {
        if self.ncq {
            AtaRequest::Fpdma {
                lba,
                sectors,
                write,
                tag: 0,
                pmp_port: 0,
            }
        } else {
            AtaRequest::DmaExt {
                lba,
                sectors,
                write,
                pmp_port: 0,
            }
        }
    }

    fn transfer_blocks(&mut self, block_id: u64, buf: &mut [u8], write: bool) -> DevResult {
        if buf.is_empty() {
            return Ok(());
        }
        if !buf.len().is_multiple_of(self.geometry.block_size) {
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
            let request = self.data_request(lba, sectors, write);
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

const fn controller_capabilities_for_ncq(capabilities: u32) -> bool {
    capabilities & AHCI_CAP_SNCQ != 0
}

/// Parses ATA IDENTIFY words into LBA48 geometry and advertised features.
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
    let ncq = word(76) & (1 << 8) != 0;
    let ncq_queue_depth = if ncq { (word(75) & 0x1f) as u8 + 1 } else { 0 };
    let trim = word(169) & 1 != 0;
    Some(AtaGeometry {
        block_size: sector_size,
        blocks,
        lba48,
        ncq,
        ncq_queue_depth,
        trim,
    })
}

fn map_error(error: AhciDiskError) -> DevError {
    match error {
        AhciDiskError::InvalidWorkspace | AhciDiskError::UnsupportedAddressWidth => {
            DevError::InvalidParam
        }
        AhciDiskError::CommandTimeout | AhciDiskError::DeviceError(_) => DevError::Io,
        AhciDiskError::RequestPending => DevError::ResourceBusy,
        AhciDiskError::NoDevice => DevError::Io,
        _ => DevError::BadState,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::regs::{AHCI_OFFSET, AHCI_P_CI},
        *,
    };

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
        set_word(&mut bytes, 76, 1 << 8);
        set_word(&mut bytes, 75, 7);
        set_word(&mut bytes, 169, 1);
        let geometry = parse_identify(&bytes).unwrap();
        assert_eq!(geometry.block_size, 4096);
        assert_eq!(geometry.blocks, 0x1234_5678);
        assert!(geometry.lba48);
        assert!(geometry.ncq);
        assert_eq!(geometry.ncq_queue_depth, 8);
        assert!(geometry.trim);
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

    struct FakeIo {
        registers: [u32; 128],
        writes: [(usize, u32); 32],
        write_count: usize,
    }

    impl Default for FakeIo {
        fn default() -> Self {
            Self {
                registers: [0; 128],
                writes: [(0, 0); 32],
                write_count: 0,
            }
        }
    }

    impl AhciIo for FakeIo {
        fn read32(&mut self, offset: usize) -> u32 {
            self.registers[offset / 4]
        }

        fn write32(&mut self, offset: usize, value: u32) {
            self.writes[self.write_count] = (offset, value);
            self.write_count += 1;
            if offset == AHCI_OFFSET + super::super::regs::AHCI_P_IS
                || offset == AHCI_OFFSET + super::super::regs::AHCI_P_SERR
            {
                self.registers[offset / 4] = 0;
            } else {
                self.registers[offset / 4] = value;
            }
        }

        fn delay_us(&mut self, _micros: u32) {}
    }

    fn with_fake_disk(f: impl FnOnce(&mut AhciDisk<FakeIo>, *mut u8)) {
        let mut pages = [
            Page([0; 4096]),
            Page([0; 4096]),
            Page([0; 4096]),
            Page([0; 4096]),
        ];
        // SAFETY: these four aligned pages remain live in this stack frame
        // until the closure and disk teardown finish.
        let command_list = unsafe {
            DmaRegion::borrowed(NonNull::new(pages[0].0.as_mut_ptr()).unwrap(), 0x1000, 4096)
        };
        let received_fis = unsafe {
            DmaRegion::borrowed(NonNull::new(pages[1].0.as_mut_ptr()).unwrap(), 0x2000, 4096)
        };
        let command_table = unsafe {
            DmaRegion::borrowed(NonNull::new(pages[2].0.as_mut_ptr()).unwrap(), 0x3000, 4096)
        };
        let bounce_ptr = pages[3].0.as_mut_ptr();
        let bounce =
            unsafe { DmaRegion::borrowed(NonNull::new(bounce_ptr).unwrap(), 0x4000, 4096) };
        let workspace =
            PortWorkspace::new(command_list, received_fis, command_table, bounce).unwrap();
        let disk = AhciDisk {
            controller: AhciController::new(FakeIo::default(), AHCI_CAP_64BIT, 0, 0),
            port: PortState::new(0),
            workspace: ManuallyDrop::new(workspace),
            geometry: AtaGeometry {
                block_size: 512,
                blocks: 128,
                lba48: true,
                ncq: false,
                ncq_queue_depth: 0,
                trim: false,
            },
            ncq: false,
            poisoned: false,
            workspace_live: true,
            async_state: AsyncState::Idle,
            next_async_handle: 1,
        };
        let mut disk = disk;
        f(&mut disk, bounce_ptr);
    }

    #[test]
    fn async_read_uses_owned_dma_until_completion_then_copies_to_caller() {
        with_fake_disk(|disk, bounce| {
            let mut output = [0u8; 512];
            let segment = crate::BlockSegment::from_read_buf(&mut output);
            let flush_segments = [];
            let mut requests = [
                BlockQueueRequest {
                    op: BlockAsyncOp::Read,
                    block_id: 3,
                    segments: &[segment],
                    handle: None,
                },
                BlockQueueRequest {
                    op: BlockAsyncOp::Flush,
                    block_id: 0,
                    segments: &flush_segments,
                    handle: None,
                },
            ];
            let report = disk.submit_async_batch(&mut requests).unwrap();
            assert_eq!(report.submitted, 1);
            assert!(report.queue_full);
            assert_eq!(requests[1].handle, None);
            assert_eq!(requests[0].handle, Some(BlockRequestHandle { raw: 1 }));
            assert_eq!(
                disk.controller.io_mut().registers[(AHCI_OFFSET + AHCI_P_CI) / 4],
                1
            );
            assert_eq!(output, [0; 512]);
            // Simulate device DMA into the persistent bounce region and then
            // the HBA clearing CI before task-context completion drain.
            unsafe { ptr::write_bytes(bounce, 0x5a, 512) };
            disk.controller.io_mut().registers[(AHCI_OFFSET + AHCI_P_CI) / 4] = 0;
            let mut completions = [BlockCompletion {
                handle: BlockRequestHandle::default(),
                owner: BlockCompletionOwner::Ordinary,
                cookie: 0,
                status: BlockCompletionStatus::DeviceError(0),
                bytes: 0,
            }];
            let drained = disk.drain_async_completions(&mut completions).unwrap();
            assert_eq!(drained.completed, 1);
            assert_eq!(completions[0].status, BlockCompletionStatus::Success);
            assert_eq!(completions[0].bytes, 512);
            assert_eq!(output, [0x5a; 512]);
        });
    }

    #[test]
    fn async_write_copies_before_publication_and_wait_retires_handle() {
        with_fake_disk(|disk, bounce| {
            let source = [0x37u8; 512];
            let segment = crate::BlockSegment::from_write_buf(&source);
            let mut requests = [BlockQueueRequest {
                op: BlockAsyncOp::Write,
                block_id: 5,
                segments: &[segment],
                handle: None,
            }];
            let report = disk.submit_async_batch(&mut requests).unwrap();
            assert_eq!(report.submitted, 1);
            assert_eq!(
                disk.controller.io_mut().registers[(AHCI_OFFSET + AHCI_P_CI) / 4],
                1
            );
            assert_eq!(unsafe { core::slice::from_raw_parts(bounce, 512) }, &source);
            let handle = requests[0].handle.unwrap();
            disk.controller.io_mut().registers[(AHCI_OFFSET + AHCI_P_CI) / 4] = 0;
            assert!(disk.wait_async_all(&[handle]).is_ok());
            assert!(matches!(disk.async_state, AsyncState::Idle));
        });
    }
}
