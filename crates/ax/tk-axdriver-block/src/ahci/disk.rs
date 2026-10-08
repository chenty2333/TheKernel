//! Single-port ATA disk command path over AHCI DMA descriptors.
//!
//! The path translates the ATA parts of FreeBSD's `ahci_setup_fis()` and
//! `ahci_execute_transaction()` to `BlockDriverOps`. CAM CCB scheduling and
//! async callouts are not copied; the HBA command engine is polled synchronously
//! and serializes one request through slot zero. NCQ FIS encoding is provided
//! by `ata`, but NCQ queue admission/recovery is a later stage.

extern crate alloc;

use alloc::{string::String, sync::Arc};
use core::{
    mem::ManuallyDrop,
    ptr::{self, NonNull},
    sync::atomic::{Ordering, fence},
};

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
use spin::Mutex;

use super::{
    ata::{AtaRequest, setup_register_fis},
    controller::{AhciController, AhciIo, PortState},
    regs::{
        AHCI_CAP_64BIT, AHCI_CAP_SNCQ, AHCI_GHC, AHCI_GHC_IE, AHCI_MAX_SLOTS, AHCI_P_CI,
        AHCI_P_CLB, AHCI_P_CLBU, AHCI_P_FB, AHCI_P_FBU, AHCI_P_IE, AHCI_P_IS, AHCI_P_IX_CPD,
        AHCI_P_IX_DHR, AHCI_P_IX_HBD, AHCI_P_IX_HBF, AHCI_P_IX_IF, AHCI_P_IX_OF, AHCI_P_IX_SDB,
        AHCI_P_IX_TFE, AHCI_P_SACT, AHCI_P_SERR, AHCI_P_SSTS, AHCI_P_TFD, AHCI_PRD_IPC,
        AHCI_PRD_MAX, ATA_S_ERROR, ATA_SS_DET_MASK, ATA_SS_DET_PHY_ONLINE,
    },
};
use crate::{
    BlockAsyncOp, BlockCapabilities, BlockCompletion, BlockCompletionDrain,
    BlockCompletionNotifier, BlockCompletionOwner, BlockCompletionStatus, BlockDriverOps,
    BlockPhysicalRequest, BlockPhysicalSegment, BlockPhysicalSgOutcome, BlockQueueCaps,
    BlockQueueRequest, BlockRange, BlockRequestHandle, BlockSegmentDirection, BlockSubmitReport,
};

const COMMAND_LIST_BYTES: usize = 32 * AHCI_MAX_SLOTS;
const RECEIVED_FIS_BYTES: usize = 256;
const COMMAND_TABLE_HEADER_BYTES: usize = 128;
const COMMAND_TABLE_SLOT_BYTES: usize = 4096;
const PRD_BYTES: usize = 16;
const COMMAND_TIMEOUT_POLLS: usize = 50_000;
const READ_LOG_TIMEOUT_POLLS: usize = 10_000;
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
    command_table_slots: usize,
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
            || command_table.len < COMMAND_TABLE_SLOT_BYTES
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
        let command_table_slots =
            (command_table.len / COMMAND_TABLE_SLOT_BYTES).min(AHCI_MAX_SLOTS);
        Ok(Self {
            command_list,
            received_fis,
            command_table,
            command_table_slots,
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
    DmaMayStillBeActive,
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
    owner: BlockCompletionOwner,
    cookie: u64,
    op: BlockAsyncOp,
    bytes: usize,
    segments: [AsyncSegment; MAX_ASYNC_SEGMENTS],
    segment_count: usize,
    polls: usize,
}

#[derive(Clone, Copy)]
struct PendingPhysical {
    handle: BlockRequestHandle,
    cookie: u64,
    bytes: usize,
    polls: usize,
}

#[derive(Clone, Copy)]
struct PhysicalSlot {
    pending: Option<PendingPhysical>,
    completion: Option<BlockCompletion>,
}

impl PhysicalSlot {
    const EMPTY: Self = Self {
        pending: None,
        completion: None,
    };
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
    pmp_port: u8,
    workspace: ManuallyDrop<PortWorkspace>,
    geometry: AtaGeometry,
    identity_digest: u64,
    name: String,
    ncq: bool,
    poisoned: bool,
    ncq_error_tag: Option<u8>,
    workspace_live: bool,
    async_state: AsyncState,
    physical_slots: [PhysicalSlot; AHCI_MAX_SLOTS],
    next_async_handle: u64,
    irq_enabled: bool,
}

impl<I: AhciIo> AhciDisk<I> {
    fn has_physical_work(&self) -> bool {
        self.physical_slots
            .iter()
            .any(|slot| slot.pending.is_some() || slot.completion.is_some())
    }

    fn physical_slots_mask(&self) -> u32 {
        self.physical_slots
            .iter()
            .enumerate()
            .fold(0u32, |mask, (slot, state)| {
                if state.pending.is_some() || state.completion.is_some() {
                    mask | (1u32 << slot)
                } else {
                    mask
                }
            })
    }

    fn physical_slot_range(&self) -> core::ops::Range<usize> {
        if self.ncq {
            let end = (1 + usize::from(self.geometry.ncq_queue_depth))
                .min(self.workspace().command_table_slots)
                .min(AHCI_MAX_SLOTS);
            1..end
        } else {
            0..self.workspace().command_table_slots.min(1)
        }
    }

    /// Initializes one SATA port and identifies an ATA disk.
    ///
    /// Workspace allocations are retained by the returned driver. On a failed
    /// attach, command/FIS DMA is stopped before the allocations are released;
    /// if stop cannot prove DMA quiescence they are deliberately leaked.
    // upstream: ahci.c ahci_ch_attach()
    pub fn attach(
        controller: AhciController<I>,
        port: PortState,
        workspace: PortWorkspace,
    ) -> Result<Self, AhciDiskError> {
        Self::attach_target(controller, port, workspace, 0, false, true)
    }

    /// Constructs one target-selected command path. This does not enumerate or
    /// share the port workspace with sibling target views; the PCI frontend
    /// currently publishes only target zero until that port registry exists.
    // upstream: ahci.c ahci_ch_attach() PMP target variant
    pub fn attach_pmp_port(
        controller: AhciController<I>,
        port: PortState,
        workspace: PortWorkspace,
        pmp_port: u8,
    ) -> Result<Self, AhciDiskError> {
        Self::attach_target(controller, port, workspace, pmp_port, true, true)
    }

    /// Initializes the shared DMA/command engine for PMP IDENTIFY discovery.
    pub fn attach_pmp_controller(
        controller: AhciController<I>,
        port: PortState,
        workspace: PortWorkspace,
    ) -> Result<Self, AhciDiskError> {
        Self::attach_target(controller, port, workspace, 0, true, false)
    }

    fn attach_target(
        mut controller: AhciController<I>,
        mut port: PortState,
        workspace: PortWorkspace,
        pmp_port: u8,
        pmp_present: bool,
        identify_target: bool,
    ) -> Result<Self, AhciDiskError> {
        if pmp_port >= 16 {
            return Err(AhciDiskError::InvalidRequest);
        }
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
        port.pm_present = pmp_present;
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
        controller.ahci_start(&mut port, pmp_present);
        let irq_enabled = controller.io_mut().has_interrupt();
        if irq_enabled {
            controller.io_mut().write32(
                base + AHCI_P_IE,
                AHCI_P_IX_DHR
                    | AHCI_P_IX_SDB
                    | AHCI_P_IX_TFE
                    | AHCI_P_IX_OF
                    | AHCI_P_IX_IF
                    | AHCI_P_IX_HBD
                    | AHCI_P_IX_HBF
                    | AHCI_P_IX_CPD,
            );
            let ghc = controller.io_mut().read32(AHCI_GHC);
            controller.io_mut().write32(AHCI_GHC, ghc | AHCI_GHC_IE);
        }
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
            pmp_port,
            workspace: ManuallyDrop::new(workspace),
            geometry: AtaGeometry {
                block_size: 512,
                blocks: 0,
                lba48: false,
                ncq: false,
                ncq_queue_depth: 0,
                trim: false,
            },
            identity_digest: 0,
            name: String::from("ahci"),
            ncq: false,
            poisoned: false,
            ncq_error_tag: None,
            workspace_live: true,
            async_state: AsyncState::Idle,
            physical_slots: [PhysicalSlot::EMPTY; AHCI_MAX_SLOTS],
            next_async_handle: 1,
            irq_enabled,
        };
        if !identify_target {
            return Ok(disk);
        }
        let mut identify = [0u8; 512];
        if disk
            .execute(AtaRequest::Identify { pmp_port }, identify.len())
            .is_err()
        {
            return Err(disk.attach_failure(AhciDiskError::IdentifyFailed));
        }
        // SAFETY: IDENTIFY completed and the persistent bounce buffer holds
        // the full 512-byte device response.
        unsafe {
            ptr::copy_nonoverlapping(
                disk.workspace().bounce.cpu.as_ptr(),
                identify.as_mut_ptr(),
                identify.len(),
            )
        };
        match parse_identify(&identify) {
            Some(geometry) if disk.workspace().bounce.len >= geometry.block_size => {
                disk.ncq = geometry.ncq
                    && controller_capabilities_for_ncq(disk.controller.capabilities)
                    && disk.port.quirks & super::regs::AHCI_Q_NONCQ == 0;
                disk.geometry = geometry;
                disk.identity_digest = identify_digest(&identify);
            }
            Some(_) => return Err(disk.attach_failure(AhciDiskError::InvalidWorkspace)),
            None => return Err(disk.attach_failure(AhciDiskError::InvalidIdentifyData)),
        }
        Ok(disk)
    }

    /// Ensure the original ATA medium is online. A removed disk returns an
    /// I/O error; when a medium is reinserted, IDENTIFY must match both the
    /// saved geometry and stable serial/model/capacity fields before the old
    /// block-device identity may resume I/O.
    // upstream: ahci.c ahci_sata_connect()
    fn ensure_connected(&mut self) -> Result<(), AhciDiskError> {
        if self.poisoned {
            return Err(AhciDiskError::CommandTimeout);
        }
        let base = self.port.register_base();
        let status = self
            .controller
            .io_mut()
            .read32(base + super::regs::AHCI_P_SSTS);
        let serr = self
            .controller
            .io_mut()
            .read32(base + super::regs::AHCI_P_SERR);
        if status & super::regs::ATA_SS_DET_MASK == super::regs::ATA_SS_DET_PHY_ONLINE
            && status & super::regs::ATA_SS_SPD_MASK != super::regs::ATA_SS_SPD_NO_SPEED
            && status & super::regs::ATA_SS_IPM_MASK == super::regs::ATA_SS_IPM_ACTIVE
            && serr & super::regs::ATA_SE_PHY_CHANGED == 0
        {
            return Ok(());
        }
        if !self.controller.ahci_sata_phy_reset(&mut self.port) {
            return Err(AhciDiskError::NoDevice);
        }
        let mut identify = [0u8; 512];
        self.execute(
            AtaRequest::Identify {
                pmp_port: self.pmp_port,
            },
            identify.len(),
        )?;
        // SAFETY: the identify transaction finished before copying from bounce.
        unsafe {
            ptr::copy_nonoverlapping(
                self.workspace().bounce.cpu.as_ptr(),
                identify.as_mut_ptr(),
                identify.len(),
            )
        };
        if parse_identify(&identify) != Some(self.geometry)
            || identify_digest(&identify) != self.identity_digest
        {
            // Do not let a replacement disk inherit the stale device node.
            self.poisoned = true;
            return Err(AhciDiskError::NoDevice);
        }
        Ok(())
    }

    /// Override the registry name assigned by the PCI controller enumerator.
    pub fn set_device_name(&mut self, name: String) {
        self.name = name;
    }

    /// Identify one PMP device behind this shared AHCI port without changing
    /// the currently published target's geometry or fingerprint.
    // upstream: ahci.c ahci_ata_probe() PMP target selection
    pub fn probe_pmp_target(&mut self, pmp_port: u8) -> Result<(AtaGeometry, u64), AhciDiskError> {
        if !self.port.pm_present || pmp_port >= 15 || self.has_physical_work() {
            return Err(AhciDiskError::InvalidRequest);
        }
        let previous = self.pmp_port;
        self.pmp_port = pmp_port;
        let result = (|| {
            let mut identify = [0u8; 512];
            self.execute(AtaRequest::Identify { pmp_port }, identify.len())?;
            // SAFETY: IDENTIFY completed and its DMA data is quiescent.
            unsafe {
                ptr::copy_nonoverlapping(
                    self.workspace().bounce.cpu.as_ptr(),
                    identify.as_mut_ptr(),
                    identify.len(),
                )
            };
            let geometry = parse_identify(&identify).ok_or(AhciDiskError::IdentifyFailed)?;
            Ok((geometry, identify_digest(&identify)))
        })();
        self.pmp_port = previous;
        result
    }

    /// Geometry captured by ATA IDENTIFY DEVICE.
    pub const fn geometry(&self) -> AtaGeometry {
        self.geometry
    }

    pub const fn identity_digest(&self) -> u64 {
        self.identity_digest
    }

    pub const fn pmp_port(&self) -> u8 {
        self.pmp_port
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

    /// Build a direct physical scatter-gather PRDT without copying through the
    /// bounce buffer. The caller owns/pins every segment through completion.
    // upstream: ahci.c ahci_dmasetprd() and ahci_dmasetupc_cb()
    fn build_physical_sg_command(
        &mut self,
        slot: usize,
        request: AtaRequest,
        segments: &[BlockPhysicalSegment],
    ) -> Result<bool, AhciDiskError> {
        if slot >= self.workspace().command_table_slots
            || segments.is_empty()
            || segments.len() > crate::MAX_PHYSICAL_COALESCED_SG
        {
            return Err(AhciDiskError::InvalidRequest);
        }
        let (fis, attributes) =
            setup_register_fis(request).map_err(|_| AhciDiskError::InvalidRequest)?;
        if !attributes.data_transfer {
            return Err(AhciDiskError::InvalidRequest);
        }
        let mut bytes = 0usize;
        let mut prd_count = 0usize;
        for segment in segments {
            if segment.len == 0 || segment.paddr & 1 != 0 {
                return Err(AhciDiskError::InvalidRequest);
            }
            let end = segment
                .paddr
                .checked_add(segment.len)
                .ok_or(AhciDiskError::InvalidRequest)?;
            if self.controller.capabilities & AHCI_CAP_64BIT == 0 && end > u32::MAX as usize + 1 {
                return Err(AhciDiskError::UnsupportedAddressWidth);
            }
            bytes = bytes
                .checked_add(segment.len)
                .ok_or(AhciDiskError::InvalidRequest)?;
            prd_count = prd_count
                .checked_add(segment.len.div_ceil(AHCI_PRD_MAX))
                .ok_or(AhciDiskError::InvalidRequest)?;
        }
        if bytes
            != usize::from(attributes.sectors)
                .checked_mul(self.geometry.block_size)
                .ok_or(AhciDiskError::InvalidRequest)?
        {
            return Err(AhciDiskError::InvalidRequest);
        }
        let table_capacity =
            COMMAND_TABLE_SLOT_BYTES.saturating_sub(COMMAND_TABLE_HEADER_BYTES) / PRD_BYTES;
        if prd_count == 0 || prd_count > table_capacity {
            return Err(AhciDiskError::InvalidRequest);
        }
        let table_offset = slot * COMMAND_TABLE_SLOT_BYTES;
        let ws = self.workspace_mut();
        // SAFETY: the command table/list are aligned owned DMA memory and the
        // PRDT capacity and caller-pinned ranges were validated above.
        unsafe {
            let table = ws.command_table.cpu.as_ptr().add(table_offset);
            ptr::write_bytes(table, 0, COMMAND_TABLE_SLOT_BYTES);
            ptr::copy_nonoverlapping(fis.as_ptr(), table, fis.len());
            let mut prd_index = 0usize;
            for segment in segments {
                let mut address = segment.paddr;
                let mut remaining = segment.len;
                while remaining != 0 {
                    let len = remaining.min(AHCI_PRD_MAX);
                    let prd =
                        table.add(COMMAND_TABLE_HEADER_BYTES + prd_index * PRD_BYTES) as *mut u32;
                    ptr::write_unaligned(prd.add(0), address as u32);
                    ptr::write_unaligned(prd.add(1), (address as u64 >> 32) as u32);
                    ptr::write_unaligned(prd.add(2), 0);
                    let final_prd = prd_index + 1 == prd_count;
                    ptr::write_unaligned(
                        prd.add(3),
                        ((len - 1) as u32 & (AHCI_PRD_MAX as u32 - 1))
                            | if final_prd { AHCI_PRD_IPC } else { 0 },
                    );
                    address += len;
                    remaining -= len;
                    prd_index += 1;
                }
            }
            let header = ws.command_list.cpu.as_ptr().add(slot * 32);
            ptr::write_unaligned(
                header as *mut u16,
                5 | if attributes.device_reads_buffer {
                    1 << 6
                } else {
                    0
                },
            );
            ptr::write_unaligned(header.add(2) as *mut u16, prd_count as u16);
            ptr::write_unaligned(header.add(4) as *mut u32, 0);
            ptr::write_unaligned(
                header.add(8) as *mut u64,
                ws.command_table.bus + table_offset as u64,
            );
        }
        let base = self.port.register_base();
        self.controller.io_mut().write32(base + AHCI_P_IS, u32::MAX);
        self.controller
            .io_mut()
            .write32(base + AHCI_P_SERR, u32::MAX);
        Ok(attributes.tag.is_some())
    }

    fn publish_command(&mut self, ncq: bool) {
        self.publish_slot_mask(1, ncq);
    }

    fn publish_slot_mask(&mut self, mask: u32, ncq: bool) {
        // Publish coherent command writes before handing these slots to the
        // HBA. The x86 platform's DMA pages are cache coherent.
        fence(Ordering::Release);
        let base = self.port.register_base();
        if ncq {
            self.controller.io_mut().write32(base + AHCI_P_SACT, mask);
        }
        self.controller.io_mut().write32(base + AHCI_P_CI, mask);
    }

    // upstream: ahci.c ahci_ch_intr_main()
    fn sample_command(&mut self, ncq: bool) -> Option<Result<(), AhciDiskError>> {
        self.sample_slot(0, ncq)
    }

    fn sample_slot(&mut self, slot: usize, ncq: bool) -> Option<Result<(), AhciDiskError>> {
        let Some(slot_bit) = 1u32.checked_shl(slot as u32) else {
            return Some(Err(AhciDiskError::InvalidRequest));
        };
        let base = self.port.register_base();
        let command_active = self.controller.io_mut().read32(base + AHCI_P_CI) & slot_bit != 0;
        let ncq_active = ncq && self.controller.io_mut().read32(base + AHCI_P_SACT) & slot_bit != 0;
        if command_active || ncq_active {
            return None;
        }
        // The HBA clears CI/SACT only after completing DMA; order subsequent
        // CPU reads of the persistent bounce buffer after that observation.
        fence(Ordering::Acquire);
        let status = self.controller.io_mut().read32(base + AHCI_P_TFD);
        let interrupt = self.controller.io_mut().read32(base + AHCI_P_IS);
        // With AHCI_Q_NOCCS the upstream marks all running slots as failed;
        // this driver has exactly one possible running slot (slot zero), so
        // the condition below fails that entire running set without reading
        // the unreliable CCS field.
        let transport_error = interrupt
            & (AHCI_P_IX_TFE
                | super::regs::AHCI_P_IX_OF
                | super::regs::AHCI_P_IX_IF
                | super::regs::AHCI_P_IX_HBD
                | super::regs::AHCI_P_IX_HBF)
            != 0;
        let no_ccs_running_slot_failed =
            self.port.quirks & super::regs::AHCI_Q_NOCCS != 0 && transport_error;
        if status & ATA_S_ERROR != 0 || transport_error || no_ccs_running_slot_failed {
            Some(Err(AhciDiskError::DeviceError(status)))
        } else {
            Some(Ok(()))
        }
    }

    fn timeout_command(&mut self) -> bool {
        let fis_stopped = self.controller.ahci_stop_fr(&self.port);
        let command_stopped = self.controller.ahci_stop(&mut self.port);
        if fis_stopped && command_stopped {
            if !self.recover_port() {
                self.poisoned = true;
            }
        } else {
            self.poisoned = true;
        }
        fis_stopped && command_stopped
    }

    fn recover_port(&mut self) -> bool {
        if !self.controller.ahci_clo(&self.port) {
            // FreeBSD logs CLO failure but continues the reset/restart path.
            log::warn!("ahci: CLO timed out during reset; continuing");
        }
        if !self.controller.ahci_sata_phy_reset(&mut self.port) {
            return false;
        }
        self.controller.ahci_start_fr(&self.port);
        self.controller.ahci_start(&mut self.port, true);
        true
    }

    /// FreeBSD `ahci_execute_transaction()`'s serialized completion path.
    // upstream: ahci.c ahci_execute_transaction()
    fn execute(&mut self, request: AtaRequest, data_len: usize) -> Result<(), AhciDiskError> {
        if !matches!(self.async_state, AsyncState::Idle) || self.has_physical_work() {
            return Err(AhciDiskError::RequestPending);
        }
        let ncq = self.build_command(request, data_len)?;
        self.execute_built(ncq)
    }

    fn execute_built(&mut self, ncq: bool) -> Result<(), AhciDiskError> {
        if !matches!(self.async_state, AsyncState::Idle) || self.has_physical_work() {
            return Err(AhciDiskError::RequestPending);
        }
        self.publish_command(ncq);
        self.execute_published(ncq)
    }

    fn execute_published(&mut self, ncq: bool) -> Result<(), AhciDiskError> {
        for _ in 0..COMMAND_TIMEOUT_POLLS {
            let observed = self.controller.io_mut().interrupt_generation();
            if let Some(result) = self.sample_command(ncq) {
                if let Err(error @ AhciDiskError::DeviceError(_)) = result {
                    if !self.recover_command_error(ncq) {
                        self.poisoned = true;
                        return Err(AhciDiskError::DmaMayStillBeActive);
                    }
                    return Err(error);
                }
                return result;
            }
            self.wait_for_progress(observed);
        }
        if self.timeout_command() {
            Err(AhciDiskError::CommandTimeout)
        } else {
            Err(AhciDiskError::DmaMayStillBeActive)
        }
    }

    fn wait_for_progress(&mut self, observed: Option<u64>) {
        if let Some(observed) = observed {
            self.controller
                .io_mut()
                .wait_for_interrupt(observed, u64::from(POLL_DELAY_US));
        } else {
            self.controller.io_mut().delay_us(POLL_DELAY_US);
        }
    }

    /// FreeBSD keeps an NCQ error victim on hold while issuing READ LOG EXT,
    /// then restarts the channel before releasing it. This driver has one slot,
    /// so it reads the error log and reinitializes the engine before returning
    /// the exact failed request; no unrelated queued victim exists.
    // upstream: ahci.c ahci_end_transaction() + ahci_issue_recovery()
    fn recover_command_error(&mut self, ncq: bool) -> bool {
        if ncq {
            self.ncq_error_tag = self.read_ncq_error_log();
        } else {
            self.ncq_error_tag = None;
        }
        let _ = self.controller.ahci_stop_fr(&self.port);
        if !self.controller.ahci_stop(&mut self.port) {
            return false;
        }
        self.recover_port()
    }

    fn read_ncq_error_log(&mut self) -> Option<u8> {
        if self.poisoned {
            return None;
        }
        let Ok(ncq) = self.build_command(
            AtaRequest::ReadLogExt {
                pmp_port: self.pmp_port,
            },
            512,
        ) else {
            return None;
        };
        debug_assert!(!ncq);
        self.publish_command(false);
        for _ in 0..READ_LOG_TIMEOUT_POLLS {
            let observed = self.controller.io_mut().interrupt_generation();
            if let Some(result) = self.sample_command(false) {
                if result.is_err() {
                    return None;
                }
                let log = self.workspace().bounce.cpu.as_ptr();
                // SAFETY: READ LOG EXT completed and DMA is quiescent.
                let status = unsafe { ptr::read_volatile(log) };
                return ncq_error_log_tag(status);
            }
            self.wait_for_progress(observed);
        }
        self.timeout_command();
        None
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

    // upstream: ahci.c ahci_timeout() + ahci_end_transaction()
    fn reap_async(&mut self) -> bool {
        let mut any_physical_completion = false;
        let mut physical_error = None;
        let mut physical_timeout = false;
        for slot in 0..AHCI_MAX_SLOTS {
            let Some(mut pending) = self.physical_slots[slot].pending else {
                continue;
            };
            if let Some(result) = self.sample_slot(slot, self.ncq) {
                match result {
                    Ok(()) => {
                        self.physical_slots[slot].pending = None;
                        self.physical_slots[slot].completion = Some(BlockCompletion {
                            handle: pending.handle,
                            owner: BlockCompletionOwner::Physical,
                            cookie: pending.cookie,
                            status: BlockCompletionStatus::Success,
                            bytes: pending.bytes as u32,
                        });
                        any_physical_completion = true;
                    }
                    Err(AhciDiskError::DeviceError(status)) => {
                        physical_error = Some(status as u8);
                        break;
                    }
                    Err(_) => {
                        physical_error = Some(0xff);
                        break;
                    }
                }
                continue;
            }
            pending.polls += 1;
            if pending.polls < COMMAND_TIMEOUT_POLLS {
                self.physical_slots[slot].pending = Some(pending);
            } else {
                physical_timeout = true;
                break;
            }
        }
        if let Some(status) = physical_error {
            let recovered = self.recover_command_error(self.ncq);
            if !recovered {
                self.poisoned = true;
            }
            self.finish_physical_after_port_reset(
                if recovered {
                    BlockCompletionStatus::DeviceError(status)
                } else {
                    BlockCompletionStatus::Quarantined
                },
                if recovered { self.ncq_error_tag } else { None },
            );
            return true;
        }
        if physical_timeout {
            let stopped = self.timeout_command();
            self.finish_physical_after_port_reset(
                if stopped {
                    BlockCompletionStatus::DeviceError(0xff)
                } else {
                    BlockCompletionStatus::Quarantined
                },
                None,
            );
            return true;
        }

        let AsyncState::InFlight(mut pending) = self.async_state else {
            return any_physical_completion;
        };
        if let Some(result) = self.sample_command(self.ncq && pending.op != BlockAsyncOp::Flush) {
            let status = match result {
                Ok(()) => {
                    if pending.owner == BlockCompletionOwner::Ordinary
                        && pending.op == BlockAsyncOp::Read
                    {
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
                owner: pending.owner,
                cookie: pending.cookie,
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
        let quiesced = self.timeout_command();
        self.async_state = AsyncState::Complete(BlockCompletion {
            handle: pending.handle,
            owner: pending.owner,
            cookie: pending.cookie,
            status: if quiesced {
                BlockCompletionStatus::DeviceError(0xff)
            } else {
                BlockCompletionStatus::Quarantined
            },
            bytes: 0,
        });
        true
    }

    fn finish_physical_after_port_reset(
        &mut self,
        status: BlockCompletionStatus,
        failed_tag: Option<u8>,
    ) {
        for (slot_index, slot) in self.physical_slots.iter_mut().enumerate() {
            if let Some(pending) = slot.pending.take() {
                slot.completion = Some(BlockCompletion {
                    handle: pending.handle,
                    owner: BlockCompletionOwner::Physical,
                    cookie: pending.cookie,
                    status: if failed_tag.is_some_and(|tag| tag != slot_index as u8)
                        && matches!(status, BlockCompletionStatus::DeviceError(_))
                    {
                        BlockCompletionStatus::DeviceError(0xff)
                    } else {
                        status
                    },
                    bytes: 0,
                });
            }
        }
    }

    fn transfer_read(
        &mut self,
        request: AtaRequest,
        buffer: &mut [u8],
    ) -> Result<(), AhciDiskError> {
        if buffer.is_empty() {
            return Err(AhciDiskError::InvalidRequest);
        }
        self.ensure_connected()?;
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
        self.ensure_connected()?;
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
        self.controller
            .io_mut()
            .write32(self.port.register_base() + AHCI_P_IE, 0);
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
        &self.name
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

    fn media_presence(&mut self) -> Option<bool> {
        Some(
            self.controller
                .io_mut()
                .read32(self.port.register_base() + AHCI_P_SSTS)
                & ATA_SS_DET_MASK
                == ATA_SS_DET_PHY_ONLINE,
        )
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

    // upstream: ahci.c ahci_execute_transaction() physical DMA request path
    unsafe fn read_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        self.transfer_physical_sg(block_id, segments, false)
    }

    // upstream: ahci.c ahci_execute_transaction() physical DMA request path
    unsafe fn write_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        self.transfer_physical_sg(block_id, segments, true)
    }

    // upstream: ahci.c ahci_begin_transaction() physical descriptor publication
    unsafe fn submit_physical_batch(
        &mut self,
        requests: &mut [BlockPhysicalRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        if requests.is_empty() {
            return Ok(BlockSubmitReport::default());
        }
        if requests.len() > crate::MAX_PHYSICAL_BATCH_REQUESTS {
            return Err(DevError::InvalidParam);
        }
        if !matches!(self.async_state, AsyncState::Idle) || self.has_physical_work() {
            return Ok(BlockSubmitReport {
                submitted: 0,
                bytes: 0,
                queue_full: true,
            });
        }
        let range = self.physical_slot_range();
        let mut occupied = self.physical_slots_mask();
        let base = self.port.register_base();
        occupied |= self.controller.io_mut().read32(base + AHCI_P_CI);
        occupied |= self.controller.io_mut().read32(base + AHCI_P_SACT);
        if !matches!(self.async_state, AsyncState::Idle) {
            occupied |= 1;
        }
        let mut free_slots = [usize::MAX; AHCI_MAX_SLOTS];
        let mut slot_count = 0usize;
        for slot in range {
            if occupied & (1u32 << slot) == 0 {
                free_slots[slot_count] = slot;
                slot_count += 1;
            }
        }
        let submitted = requests.len().min(slot_count);
        if submitted == 0 {
            return Ok(BlockSubmitReport {
                submitted: 0,
                bytes: 0,
                queue_full: true,
            });
        }
        self.ensure_connected().map_err(map_error)?;
        let mut bytes_submitted = 0usize;
        let mut mask = 0u32;
        let mut pending = [None; AHCI_MAX_SLOTS];
        let mut prepared_ids = [None; crate::MAX_PHYSICAL_BATCH_REQUESTS];
        for (index, request) in requests.iter_mut().take(submitted).enumerate() {
            if request.op == BlockAsyncOp::Flush
                || request.segments.is_empty()
                || request.segments.len() > crate::MAX_PHYSICAL_COALESCED_SG
            {
                return Err(DevError::InvalidParam);
            }
            let bytes = request
                .segments
                .iter()
                .try_fold(0usize, |sum, segment| sum.checked_add(segment.len))
                .ok_or(DevError::InvalidParam)?;
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
            let sectors = u16::try_from(blocks).map_err(|_| DevError::InvalidParam)?;
            let slot = free_slots[index];
            let ata = self.data_request_tagged(
                request.block_id,
                sectors,
                request.op == BlockAsyncOp::Write,
                slot as u8,
            );
            let ncq = match self.build_physical_sg_command(slot, ata, request.segments) {
                Ok(ncq) => ncq,
                Err(AhciDiskError::UnsupportedAddressWidth) => {
                    return Err(DevError::Unsupported);
                }
                Err(error) => return Err(map_error(error)),
            };
            debug_assert_eq!(ncq, self.ncq);
            let raw = self.next_async_handle.max(1);
            self.next_async_handle = raw.wrapping_add(1).max(1);
            let handle = BlockRequestHandle { raw };
            let cookie = raw;
            prepared_ids[index] = Some((handle, cookie));
            pending[slot] = Some(PendingPhysical {
                handle,
                cookie,
                bytes,
                polls: 0,
            });
            mask |= 1u32 << slot;
            bytes_submitted = bytes_submitted
                .checked_add(bytes)
                .ok_or(DevError::InvalidParam)?;
        }
        for slot in 0..AHCI_MAX_SLOTS {
            if let Some(pending) = pending[slot] {
                self.physical_slots[slot] = PhysicalSlot {
                    pending: Some(pending),
                    completion: None,
                };
            }
        }
        for (request, ids) in requests.iter_mut().take(submitted).zip(prepared_ids) {
            if let Some((handle, cookie)) = ids {
                request.handle = Some(handle);
                request.cookie = Some(cookie);
            }
        }
        // Publish every handle/cookie and slot owner before one CI doorbell.
        self.publish_slot_mask(mask, self.ncq);
        Ok(BlockSubmitReport {
            submitted,
            bytes: bytes_submitted,
            queue_full: submitted < requests.len(),
        })
    }

    fn async_queue_caps(&self) -> Option<BlockQueueCaps> {
        let slots = self.physical_slot_range().count().max(1);
        (!self.poisoned).then_some(BlockQueueCaps {
            max_requests: slots,
            max_descriptors: slots
                * ((COMMAND_TABLE_SLOT_BYTES - COMMAND_TABLE_HEADER_BYTES) / PRD_BYTES),
            supports_indirect: false,
            supports_event_idx: false,
            default_depth: 1,
        })
    }

    // upstream: ahci.c ahci_begin_transaction()
    fn submit_async_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        if requests.is_empty() {
            return Ok(BlockSubmitReport::default());
        }
        if !matches!(self.async_state, AsyncState::Idle) || self.has_physical_work() {
            return Ok(BlockSubmitReport {
                submitted: 0,
                bytes: 0,
                queue_full: true,
            });
        }
        self.ensure_connected().map_err(map_error)?;
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
                (
                    AtaRequest::FlushCacheExt {
                        pmp_port: self.pmp_port,
                    },
                    0,
                )
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
            owner: BlockCompletionOwner::Ordinary,
            cookie: handle.raw,
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
        let mut completed = 0usize;
        if let AsyncState::Complete(completion) = self.async_state {
            output[completed] = completion;
            completed += 1;
            self.async_state = AsyncState::Idle;
        }
        for slot in &mut self.physical_slots {
            if completed == output.len() {
                break;
            }
            if let Some(completion) = slot.completion.take() {
                output[completed] = completion;
                completed += 1;
            }
        }
        let continuation = self
            .physical_slots
            .iter()
            .any(|slot| slot.completion.is_some());
        Ok(BlockCompletionDrain {
            completed,
            continuation,
        })
    }

    fn poll_async_complete(&mut self, budget: usize) -> DevResult<usize> {
        if budget == 0 {
            return Ok(0);
        }
        self.reap_async();
        let mut completed = 0usize;
        if matches!(self.async_state, AsyncState::Complete(_)) {
            self.async_state = AsyncState::Idle;
            completed += 1;
        }
        for slot in &mut self.physical_slots {
            if completed == budget {
                break;
            }
            if slot.completion.take().is_some() {
                completed += 1;
            }
        }
        Ok(completed)
    }

    fn install_completion_notifier(
        &mut self,
        notifier: Option<BlockCompletionNotifier>,
        context: usize,
    ) -> DevResult {
        if !self
            .controller
            .io_mut()
            .install_completion_notifier(notifier, context)
        {
            return Err(DevError::Unsupported);
        }
        Ok(())
    }

    fn enable_irq(&mut self) -> DevResult {
        if !self.controller.io_mut().has_interrupt() {
            return Err(DevError::Unsupported);
        }
        let base = self.port.register_base();
        self.controller.io_mut().write32(
            base + AHCI_P_IE,
            AHCI_P_IX_DHR
                | AHCI_P_IX_SDB
                | AHCI_P_IX_TFE
                | AHCI_P_IX_OF
                | AHCI_P_IX_IF
                | AHCI_P_IX_HBD
                | AHCI_P_IX_HBF
                | AHCI_P_IX_CPD,
        );
        let ghc = self.controller.io_mut().read32(AHCI_GHC);
        self.controller
            .io_mut()
            .write32(AHCI_GHC, ghc | AHCI_GHC_IE);
        self.irq_enabled = true;
        Ok(())
    }

    fn disable_irq(&mut self) -> DevResult {
        let base = self.port.register_base();
        self.controller.io_mut().write32(base + AHCI_P_IE, 0);
        self.irq_enabled = false;
        Ok(())
    }

    fn is_irq_enabled(&self) -> bool {
        self.irq_enabled
    }

    fn handle_irq(&mut self) -> DevResult<usize> {
        Ok(usize::from(self.reap_async()))
    }

    fn wait_async_all(&mut self, handles: &[BlockRequestHandle]) -> DevResult {
        let mut failed = false;
        for handle in handles {
            loop {
                if let Some(slot) = self
                    .physical_slots
                    .iter_mut()
                    .find(|slot| slot.completion.is_some_and(|c| c.handle == *handle))
                {
                    let completion = slot.completion.take().expect("completion was matched");
                    failed |= completion.status != BlockCompletionStatus::Success;
                    break;
                }
                if self
                    .physical_slots
                    .iter()
                    .any(|slot| slot.pending.is_some_and(|p| p.handle == *handle))
                {
                    let observed = self.controller.io_mut().interrupt_generation();
                    if !self.reap_async() {
                        self.wait_for_progress(observed);
                    }
                    continue;
                }
                match self.async_state {
                    AsyncState::InFlight(pending) if pending.handle == *handle => {
                        let observed = self.controller.io_mut().interrupt_generation();
                        if !self.reap_async() {
                            self.wait_for_progress(observed);
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
        self.ensure_connected().map_err(map_error)?;
        self.execute(
            AtaRequest::FlushCacheExt {
                pmp_port: self.pmp_port,
            },
            0,
        )
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
        self.ensure_connected().map_err(map_error)?;
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
        self.data_request_tagged(lba, sectors, write, 0)
    }

    fn data_request_tagged(&self, lba: u64, sectors: u16, write: bool, tag: u8) -> AtaRequest {
        if self.ncq {
            AtaRequest::Fpdma {
                lba,
                sectors,
                write,
                tag,
                pmp_port: self.pmp_port,
            }
        } else {
            AtaRequest::DmaExt {
                lba,
                sectors,
                write,
                pmp_port: self.pmp_port,
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

fn identify_digest(bytes: &[u8; 512]) -> u64 {
    // ATA word ranges: serial (10..20), model (27..47), capacity (100..104),
    // and logical-sector-size fields (106..119). This is an identity guard,
    // not a cryptographic authenticity check.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes[20..40]
        .iter()
        .chain(bytes[54..94].iter())
        .chain(bytes[200..208].iter())
        .chain(bytes[212..238].iter())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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

impl<I: AhciIo> AhciDisk<I> {
    fn transfer_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
        write: bool,
    ) -> DevResult<BlockPhysicalSgOutcome> {
        if !matches!(self.async_state, AsyncState::Idle) || self.has_physical_work() {
            return Ok(BlockPhysicalSgOutcome::NotSubmitted);
        }
        let mut bytes = 0usize;
        for segment in segments {
            bytes = bytes
                .checked_add(segment.len)
                .ok_or(DevError::InvalidParam)?;
        }
        if bytes == 0 || !bytes.is_multiple_of(self.geometry.block_size) {
            return Err(DevError::InvalidParam);
        }
        let blocks = (bytes / self.geometry.block_size) as u64;
        if block_id
            .checked_add(blocks)
            .is_none_or(|end| end > self.geometry.blocks)
        {
            return Err(DevError::InvalidParam);
        }
        let sectors = match u16::try_from(blocks) {
            Ok(sectors) => sectors,
            Err(_) => return Ok(BlockPhysicalSgOutcome::NotSubmitted),
        };
        self.ensure_connected().map_err(map_error)?;
        let request = self.data_request(block_id, sectors, write);
        let ncq = match self.build_physical_sg_command(0, request, segments) {
            Ok(ncq) => ncq,
            Err(AhciDiskError::UnsupportedAddressWidth) => {
                return Ok(BlockPhysicalSgOutcome::NotSubmitted);
            }
            Err(error) => return Err(map_error(error)),
        };
        match self.execute_built(ncq) {
            Ok(()) => Ok(BlockPhysicalSgOutcome::Completed),
            Err(AhciDiskError::DmaMayStillBeActive) => Ok(BlockPhysicalSgOutcome::Quarantined),
            Err(error) => Err(map_error(error)),
        }
    }
}

/// One synchronously serialized block view for a PMP target that shares the
/// command/FIS engine and DMA workspace owned by a single AHCI port driver.
pub struct AhciPmpTargetDisk<I: AhciIo> {
    shared: Arc<Mutex<AhciDisk<I>>>,
    pmp_port: u8,
    geometry: AtaGeometry,
    identity_digest: u64,
    name: String,
}

impl<I: AhciIo> AhciPmpTargetDisk<I> {
    /// Creates an independent device node over a target discovered by the
    /// parent PMP port's IDENTIFY scan.
    pub fn new(
        shared: Arc<Mutex<AhciDisk<I>>>,
        pmp_port: u8,
        geometry: AtaGeometry,
        identity_digest: u64,
        name: String,
    ) -> Self {
        Self {
            shared,
            pmp_port,
            geometry,
            identity_digest,
            name,
        }
    }

    pub const fn pmp_port(&self) -> u8 {
        self.pmp_port
    }

    fn with_target<R>(&self, f: impl FnOnce(&mut AhciDisk<I>) -> R) -> R {
        let mut disk = self.shared.lock();
        let old_port = core::mem::replace(&mut disk.pmp_port, self.pmp_port);
        let old_geometry = core::mem::replace(&mut disk.geometry, self.geometry);
        let old_identity = core::mem::replace(&mut disk.identity_digest, self.identity_digest);
        let target_ncq = self.geometry.ncq
            && controller_capabilities_for_ncq(disk.controller.capabilities)
            && disk.port.quirks & super::regs::AHCI_Q_NONCQ == 0;
        let old_ncq = core::mem::replace(&mut disk.ncq, target_ncq);
        let result = f(&mut disk);
        disk.pmp_port = old_port;
        disk.geometry = old_geometry;
        disk.identity_digest = old_identity;
        disk.ncq = old_ncq;
        result
    }
}

impl<I: AhciIo> BaseDriverOps for AhciPmpTargetDisk<I> {
    fn device_name(&self) -> &str {
        &self.name
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }
}

impl<I: AhciIo> BlockDriverOps for AhciPmpTargetDisk<I> {
    fn num_blocks(&self) -> u64 {
        self.geometry.blocks
    }

    fn block_size(&self) -> usize {
        self.geometry.block_size
    }

    fn media_presence(&mut self) -> Option<bool> {
        self.with_target(BlockDriverOps::media_presence)
    }

    fn read_block(&mut self, block_id: u64, output: &mut [u8]) -> DevResult {
        self.with_target(|disk| BlockDriverOps::read_block(disk, block_id, output))
    }

    fn write_block(&mut self, block_id: u64, input: &[u8]) -> DevResult {
        self.with_target(|disk| BlockDriverOps::write_block(disk, block_id, input))
    }

    fn flush(&mut self) -> DevResult {
        self.with_target(BlockDriverOps::flush)
    }

    fn block_capabilities(&self) -> BlockCapabilities {
        BlockCapabilities {
            flush: true,
            discard: self.geometry.trim,
            ..BlockCapabilities::default()
        }
    }

    fn discard_blocks(&mut self, range: BlockRange) -> DevResult {
        self.with_target(|disk| BlockDriverOps::discard_blocks(disk, range))
    }
}

fn map_error(error: AhciDiskError) -> DevError {
    match error {
        AhciDiskError::InvalidWorkspace | AhciDiskError::UnsupportedAddressWidth => {
            DevError::InvalidParam
        }
        AhciDiskError::CommandTimeout
        | AhciDiskError::DmaMayStillBeActive
        | AhciDiskError::DeviceError(_) => DevError::Io,
        AhciDiskError::RequestPending => DevError::ResourceBusy,
        AhciDiskError::NoDevice => DevError::Io,
        _ => DevError::BadState,
    }
}

// upstream: ahci.c ahci_process_read_log() NQ bit and failing NCQ tag
const fn ncq_error_log_tag(status: u8) -> Option<u8> {
    if status & 0x80 == 0 {
        Some(status & 0x1f)
    } else {
        None
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
    fn physical_sg_builds_one_prd_per_pinned_range() {
        with_fake_disk(|disk, _| {
            let segments = [
                BlockPhysicalSegment {
                    paddr: 0x8000,
                    len: 512,
                },
                BlockPhysicalSegment {
                    paddr: 0x9000,
                    len: 512,
                },
            ];
            let request = disk.data_request(4, 2, false);
            assert!(
                !disk
                    .build_physical_sg_command(0, request, &segments)
                    .unwrap()
            );
            let ws = disk.workspace();
            // SAFETY: the descriptor list was just initialized in the live
            // test-owned DMA page and is read using aligned AHCI dword fields.
            unsafe {
                let header = ws.command_list.cpu.as_ptr();
                assert_eq!(ptr::read_unaligned(header.add(2).cast::<u16>()), 2);
                let prd = ws
                    .command_table
                    .cpu
                    .as_ptr()
                    .add(COMMAND_TABLE_HEADER_BYTES)
                    .cast::<u32>();
                assert_eq!(ptr::read_unaligned(prd), 0x8000);
                assert_eq!(ptr::read_unaligned(prd.add(1)), 0);
                assert_eq!(ptr::read_unaligned(prd.add(3)), 511);
                assert_eq!(ptr::read_unaligned(prd.add(4)), 0x9000);
                assert_eq!(ptr::read_unaligned(prd.add(7)), 511 | AHCI_PRD_IPC);
            }
        });
    }

    #[test]
    fn media_presence_tracks_sata_phy_detection() {
        with_fake_disk(|disk, _| {
            assert_eq!(disk.media_presence(), Some(true));
            disk.controller.io_mut().registers
                [(AHCI_OFFSET + super::super::regs::AHCI_P_SSTS) / 4] = 0;
            assert_eq!(disk.media_presence(), Some(false));
        });
    }

    #[test]
    fn physical_batch_returns_typed_completion_after_prd_submission() {
        with_fake_disk(|disk, _| {
            disk.controller.io_mut().auto_complete = true;
            let segments = [
                BlockPhysicalSegment {
                    paddr: 0x8000,
                    len: 512,
                },
                BlockPhysicalSegment {
                    paddr: 0x9000,
                    len: 512,
                },
            ];
            let mut requests = [BlockPhysicalRequest {
                block_id: 2,
                op: BlockAsyncOp::Read,
                segments: &segments,
                handle: None,
                cookie: None,
            }];
            // SAFETY: the test's fixed physical addresses are used only by the
            // auto-completing fake HBA and are never dereferenced.
            let report = unsafe { disk.submit_physical_batch(&mut requests).unwrap() };
            assert_eq!(report.submitted, 1);
            assert_eq!(report.bytes, 1024);
            assert!(requests[0].handle.is_some());
            assert_eq!(
                requests[0].cookie,
                requests[0].handle.map(|handle| handle.raw)
            );
            assert!(disk.physical_slots[0].pending.is_some());
            let mut completion = [BlockCompletion {
                handle: BlockRequestHandle::default(),
                owner: BlockCompletionOwner::Ordinary,
                cookie: 0,
                status: BlockCompletionStatus::Success,
                bytes: 0,
            }];
            let drain = disk.drain_async_completions(&mut completion).unwrap();
            assert_eq!(drain.completed, 1);
            assert_eq!(completion[0].owner, BlockCompletionOwner::Physical);
            assert_eq!(completion[0].status, BlockCompletionStatus::Success);
            assert_eq!(completion[0].bytes, 1024);
        });
    }

    #[test]
    fn ncq_error_log_tag_obeys_nq_flag_and_tag_mask() {
        assert_eq!(ncq_error_log_tag(0x65), Some(5));
        assert_eq!(ncq_error_log_tag(0x85), None);
    }

    #[test]
    fn pmp_probe_identify_targets_requested_multiplier_port() {
        with_fake_disk(|disk, _| {
            disk.port.pm_present = true;
            disk.controller.io_mut().auto_complete = true;
            assert_eq!(disk.probe_pmp_target(3), Err(AhciDiskError::IdentifyFailed));
            assert_eq!(disk.pmp_port, 0);
            // SAFETY: the command table is owned and the FIS was populated by
            // the completed fake-HBA IDENTIFY transaction.
            let target = unsafe { ptr::read(disk.workspace().command_table.cpu.as_ptr().add(1)) };
            assert_eq!(target, 0x83);
        });
    }

    #[test]
    fn pmp_target_block_view_selects_its_fis_port_and_restores_shared_state() {
        with_fake_disk(|disk, _| {
            disk.controller.io_mut().auto_complete = true;
            let geometry = disk.geometry;
            // Move the disk into shared ownership for the duration of the
            // wrapper call, then restore it before the fake backing pages go
            // out of scope. The original local is made inert to avoid a
            // second workspace drop.
            let owned = unsafe { ptr::read(disk as *const AhciDisk<FakeIo>) };
            disk.workspace_live = false;
            let shared = Arc::new(Mutex::new(owned));
            let mut target =
                AhciPmpTargetDisk::new(shared.clone(), 2, geometry, 0, String::from("sdb"));
            let mut output = [0u8; 512];
            assert!(BlockDriverOps::read_block(&mut target, 0, &mut output).is_ok());
            drop(target);
            let shared = Arc::try_unwrap(shared)
                .ok()
                .expect("wrapper released its shared port reference")
                .into_inner();
            unsafe { ptr::write(disk, shared) };
            let shared = disk;
            assert_eq!(shared.pmp_port, 0);
            // SAFETY: the original driver and table allocation remain alive.
            let target_byte =
                unsafe { ptr::read(shared.workspace().command_table.cpu.as_ptr().add(1)) };
            assert_eq!(target_byte, 0x82);
        });
    }

    #[test]
    fn ncq_physical_batch_publishes_distinct_slots_and_drains_all_completions() {
        with_fake_disk(|disk, _| {
            disk.ncq = true;
            disk.geometry.ncq = true;
            disk.geometry.ncq_queue_depth = 4;
            disk.controller.io_mut().auto_complete = true;
            assert_eq!(
                BlockDriverOps::async_queue_caps(disk).unwrap().max_requests,
                4
            );
            let first_segments = [BlockPhysicalSegment {
                paddr: 0x8000,
                len: 512,
            }];
            let second_segments = [BlockPhysicalSegment {
                paddr: 0x9000,
                len: 512,
            }];
            let mut requests = [
                BlockPhysicalRequest {
                    block_id: 2,
                    op: BlockAsyncOp::Read,
                    segments: &first_segments,
                    handle: None,
                    cookie: None,
                },
                BlockPhysicalRequest {
                    block_id: 9,
                    op: BlockAsyncOp::Write,
                    segments: &second_segments,
                    handle: None,
                    cookie: None,
                },
            ];
            // SAFETY: both ranges are only recorded by the fake HBA.
            let report = unsafe { disk.submit_physical_batch(&mut requests).unwrap() };
            assert_eq!(report.submitted, 2);
            assert_eq!(report.bytes, 1024);
            assert_eq!(requests[0].handle, Some(BlockRequestHandle { raw: 1 }));
            assert_eq!(requests[1].handle, Some(BlockRequestHandle { raw: 2 }));
            let command_list = disk.workspace().command_list.cpu.as_ptr();
            // SAFETY: the test owns the 1 KiB-aligned command list and slots
            // one and two were populated by the submission above.
            unsafe {
                assert_eq!(
                    ptr::read_unaligned(command_list.add(32 + 8) as *const u64),
                    0x4000
                );
                assert_eq!(
                    ptr::read_unaligned(command_list.add(64 + 8) as *const u64),
                    0x5000
                );
            }
            let mut completions = [BlockCompletion {
                handle: BlockRequestHandle::default(),
                owner: BlockCompletionOwner::Ordinary,
                cookie: 0,
                status: BlockCompletionStatus::DeviceError(0),
                bytes: 0,
            }; 2];
            let drain = disk.drain_async_completions(&mut completions).unwrap();
            assert_eq!(drain.completed, 2);
            assert!(!drain.continuation);
            assert!(completions.iter().all(|c| {
                c.owner == BlockCompletionOwner::Physical
                    && c.status == BlockCompletionStatus::Success
                    && c.bytes == 512
            }));
        });
    }

    #[test]
    fn identify_digest_changes_with_serial_model_or_capacity() {
        let first = [0u8; 512];
        let mut second = first;
        second[20] = 1;
        assert_ne!(identify_digest(&first), identify_digest(&second));
        second = first;
        second[54] = 1;
        assert_ne!(identify_digest(&first), identify_digest(&second));
        second = first;
        second[200] = 1;
        assert_ne!(identify_digest(&first), identify_digest(&second));
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
        writes: [(usize, u32); 128],
        write_count: usize,
        auto_complete: bool,
    }

    impl Default for FakeIo {
        fn default() -> Self {
            Self {
                registers: [0; 128],
                writes: [(0, 0); 128],
                write_count: 0,
                auto_complete: false,
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
            } else if offset == AHCI_OFFSET + AHCI_P_CI && self.auto_complete {
                self.registers[offset / 4] = 0;
                self.registers[(AHCI_OFFSET + AHCI_P_SACT) / 4] = 0;
            } else {
                self.registers[offset / 4] = value;
            }
        }

        fn delay_us(&mut self, _micros: u32) {}
    }

    fn with_fake_disk(f: impl FnOnce(&mut AhciDisk<FakeIo>, *mut u8)) {
        let mut pages = [Page([0; 4096]), Page([0; 4096]), Page([0; 4096])];
        let mut table_pages: [Page; AHCI_MAX_SLOTS] = core::array::from_fn(|_| Page([0; 4096]));
        // SAFETY: these aligned DMA regions remain live in this stack frame
        // until the closure and disk teardown finish.
        let command_list = unsafe {
            DmaRegion::borrowed(NonNull::new(pages[0].0.as_mut_ptr()).unwrap(), 0x1000, 4096)
        };
        let received_fis = unsafe {
            DmaRegion::borrowed(NonNull::new(pages[1].0.as_mut_ptr()).unwrap(), 0x2000, 4096)
        };
        let command_table = unsafe {
            DmaRegion::borrowed(
                NonNull::new(table_pages[0].0.as_mut_ptr()).unwrap(),
                0x3000,
                COMMAND_TABLE_SLOT_BYTES * AHCI_MAX_SLOTS,
            )
        };
        let bounce_ptr = pages[2].0.as_mut_ptr();
        let bounce =
            unsafe { DmaRegion::borrowed(NonNull::new(bounce_ptr).unwrap(), 0x4000, 4096) };
        let workspace =
            PortWorkspace::new(command_list, received_fis, command_table, bounce).unwrap();
        let disk = AhciDisk {
            controller: AhciController::new(
                FakeIo {
                    registers: {
                        let mut regs = [0; 128];
                        regs[(AHCI_OFFSET + super::super::regs::AHCI_P_SSTS) / 4] =
                            super::super::regs::ATA_SS_DET_PHY_ONLINE
                                | super::super::regs::ATA_SS_SPD_GEN1
                                | super::super::regs::ATA_SS_IPM_ACTIVE;
                        regs
                    },
                    ..FakeIo::default()
                },
                AHCI_CAP_64BIT,
                0,
                0,
            ),
            port: PortState::new(0),
            pmp_port: 0,
            workspace: ManuallyDrop::new(workspace),
            geometry: AtaGeometry {
                block_size: 512,
                blocks: 128,
                lba48: true,
                ncq: false,
                ncq_queue_depth: 0,
                trim: false,
            },
            identity_digest: 0,
            name: String::from("sda"),
            ncq: false,
            poisoned: false,
            ncq_error_tag: None,
            workspace_live: true,
            async_state: AsyncState::Idle,
            physical_slots: [PhysicalSlot::EMPTY; AHCI_MAX_SLOTS],
            next_async_handle: 1,
            irq_enabled: false,
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

    #[test]
    fn async_timeout_completes_with_device_error_when_dma_stop_is_proven() {
        with_fake_disk(|disk, _| {
            let mut output = [0u8; 512];
            let segment = crate::BlockSegment::from_read_buf(&mut output);
            let mut requests = [BlockQueueRequest {
                op: BlockAsyncOp::Read,
                block_id: 3,
                segments: &[segment],
                handle: None,
            }];
            assert_eq!(disk.submit_async_batch(&mut requests).unwrap().submitted, 1);
            let mut completed = false;
            for _ in 0..=COMMAND_TIMEOUT_POLLS {
                if disk.reap_async() {
                    completed = true;
                    break;
                }
            }
            assert!(completed);
            let AsyncState::Complete(completion) = disk.async_state else {
                panic!("timeout completion was not published");
            };
            assert_eq!(completion.handle, requests[0].handle.unwrap());
            assert_eq!(completion.status, BlockCompletionStatus::DeviceError(0xff));
        });
    }
}
