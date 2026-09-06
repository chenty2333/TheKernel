//! VirtIO GPU 2D protocol driver.
//!
//! This layer owns its queues and command buffers.  DMA returned by
//! `allocate_backing` is driver-owned; physical backing passed to
//! `attach_backing` remains pinned and owned by the caller until detached.

#![allow(missing_docs)] // The protocol fields mirror the VirtIO wire format.

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    vec::Vec,
};

use bitflags::bitflags;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::{
    hal::Hal,
    queue::VirtQueue,
    transport::{SharedMemoryRegion, Transport},
    volatile::{volread, volwrite, ReadOnly, Volatile, WriteOnly},
    Error, Result, PAGE_SIZE,
};

/// Control work is deliberately bounded.  Each accepted asynchronous command
/// owns one request/response pair until its exact used-ring entry is reaped.
const QUEUE_SIZE: u16 = 8;
const MAX_PENDING_CONTROL: usize = QUEUE_SIZE as usize;
/// A present batch owns a resource token while the scanout/transfer/flush
/// sequence is between controlq commands.  Keep the externally visible
/// batches bounded independently of the used-ring entries.
const MAX_PENDING_PRESENTS: usize = QUEUE_SIZE as usize;
const MAX_RESOURCES: usize = 128;
const MAX_CONTEXTS: usize = 32;
const MAX_CONTEXT_RESOURCES: usize = 128;
const MAX_SG_ENTRIES: usize = 8192;
/// Bound untrusted host/client control payload allocation while allowing virgl
/// capsets and command streams far larger than a page.
const MAX_CONTROL_PAYLOAD: usize = 1024 * 1024;
const SUPPORTED_FEATURES: Features = Features::RING_EVENT_IDX
    .union(Features::RING_INDIRECT_DESC)
    .union(Features::VIRGL)
    .union(Features::RESOURCE_UUID)
    .union(Features::RESOURCE_BLOB)
    .union(Features::CONTEXT_INIT);

/// An allocated nonzero VirtIO resource ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ResourceId(u32);
impl ResourceId {
    pub const fn get(self) -> u32 {
        self.0
    }
    pub const fn from_raw(value: u32) -> Self {
        Self(value)
    }
}

/// A non-empty rectangle expressed in resource pixels.
#[repr(C)]
#[derive(AsBytes, Debug, Copy, Clone, Default, Eq, PartialEq, FromBytes, FromZeroes)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
impl Rect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
    fn fits(self, width: u32, height: u32) -> bool {
        self.width != 0
            && self.height != 0
            && self.x.checked_add(self.width).is_some_and(|v| v <= width)
            && self.y.checked_add(self.height).is_some_and(|v| v <= height)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Scanout {
    pub rect: Rect,
    pub enabled: bool,
    pub flags: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayInfo {
    pub scanouts: [Scanout; 16],
}
impl DisplayInfo {
    pub fn first_enabled(&self) -> Option<(u32, Scanout)> {
        self.scanouts
            .iter()
            .enumerate()
            .find(|(_, s)| s.enabled)
            .map(|(i, s)| (i as u32, *s))
    }
}

struct Resource {
    id: ResourceId,
    width: u32,
    height: u32,
    backing: BackingState,
    lifecycle: ResourceLifecycle,
    backing_bytes: Option<u64>,
    kind: ResourceKind,
    uuid: Option<[u8; 16]>,
    mapped: bool,
    map_offset: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceKind {
    Legacy,
    GuestBlob,
    Host3dBlob,
    Host3dGuestBlob,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlobMem {
    Guest,
    Host3d,
    Host3dGuest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobResource {
    pub mem: BlobMem,
    pub flags: u32,
    pub size: u64,
    pub blob_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextInit {
    pub capset_id: u32,
    pub num_rings: u32,
    pub poll_rings_mask: u64,
    pub debug_name: [u8; 64],
    pub debug_name_len: u8,
}

impl Default for ContextInit {
    fn default() -> Self {
        Self {
            capset_id: 0,
            num_rings: 1,
            poll_rings_mask: 0,
            debug_name: [0; 64],
            debug_name_len: 0,
        }
    }
}

/// Local knowledge of a host backing attachment.
///
/// A failed control completion is not proof that the device ignored the
/// request. Keep that ambiguity explicit until a successful detach proves
/// the host can no longer DMA the caller's pages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackingState {
    Detached,
    Attached,
    Uncertain,
}

/// Local knowledge of the resource object itself.  Creation and destruction
/// commands can be consumed by the host even when their completion is lost.
/// Uncertain tokens are cleanup-only: they may be unrefed, but never used for
/// display or rendering commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceLifecycle {
    Live,
    CreateUncertain,
    UnrefUncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextId(u32);
impl ContextId {
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapsetInfo {
    pub id: u32,
    pub max_version: u32,
    pub max_size: u32,
}

/// A monotonic fence allocated for an asynchronous control submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuSubmission {
    pub fence_id: u64,
}

/// One terminal control completion.  The request and response DMA owners have
/// been reclaimed before this record is exposed.
#[derive(Debug)]
pub struct GpuCompletion {
    pub fence_id: u64,
    pub result: Result,
    pub data: GpuCompletionData,
}

/// Response payload retained with a terminal control completion.  Do not add
/// borrowed protocol storage here: `PendingControl` owns its DMA buffers
/// until the used entry is consumed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GpuCompletionData {
    None,
    MapInfo {
        aperture_offset: u64,
        aperture_base: u64,
        physical_base: u64,
        cache_policy: u32,
    },
    Uuid([u8; 16]),
    CapsetInfo {
        id: u32,
        max_version: u32,
        max_size: u32,
    },
    Capset(Vec<u8>),
}

struct PendingControl {
    token: u16,
    fence_id: u64,
    context: u32,
    operation: PendingControlOperation,
    request: Box<[u8]>,
    response: Box<[u8]>,
}

/// State transition held with the exact controlq DMA owners. A resource or
/// context identifier is never made live merely because its descriptor was
/// published: completion is the only publication boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingControlOperation {
    Submit3d,
    CreateResource(ResourceId),
    AttachBacking(ResourceId),
    DetachBacking(ResourceId),
    UnrefResource(ResourceId),
    SetScanout,
    Transfer2d(ResourceId),
    Flush(ResourceId),
    CreateContext(ContextId),
    DestroyContext(ContextId),
    AttachContextResource {
        context: ContextId,
        resource: ResourceId,
    },
    DetachContextResource {
        context: ContextId,
        resource: ResourceId,
    },
    Transfer3d {
        context: ContextId,
        resource: ResourceId,
    },
    CreateBlob(ResourceId),
    MapBlob(ResourceId),
    UnmapBlob(ResourceId),
    AssignUuid(ResourceId),
    CapsetInfo,
    Capset {
        bytes: usize,
    },
    DestroyUncertainContext(ContextId),
}

impl PendingControlOperation {
    fn response_len(self) -> usize {
        match self {
            Self::MapBlob(_) => core::mem::size_of::<RespMapBlob>(),
            Self::AssignUuid(_) => core::mem::size_of::<RespResourceUuid>(),
            Self::CapsetInfo => core::mem::size_of::<RespCapsetInfo>(),
            Self::Capset { bytes } => core::mem::size_of::<CtrlHeader>() + bytes,
            _ => core::mem::size_of::<CtrlHeader>(),
        }
    }
}
struct PendingCursor {
    token: u16,
    fence_id: u64,
    request: Box<[u8]>,
}

/// One externally visible presentation is three strictly ordered controlq
/// commands.  Only `fence_id` is visible above this layer; `in_flight` is an
/// internal command fence and is never completed to the caller.
struct PresentBatch {
    fence_id: u64,
    resource: ResourceId,
    visible: Rect,
    damage: Rect,
    blob_layout: Option<BlobScanoutLayout>,
    stage: PresentStage,
    in_flight: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentStage {
    SetScanout,
    SetScanoutBlob,
    TransferToHost,
    Flush,
}

#[derive(Clone, Copy)]
struct BlobScanoutLayout {
    framebuffer_width: u32,
    framebuffer_height: u32,
    format: u32,
    stride: u32,
    offset: u32,
}

struct Context {
    id: ContextId,
    resources: Vec<ResourceId>,
    rings: u32,
}

/// VirtIO GPU device with a bounded asynchronous control submission path.
/// Synchronous protocol helpers are serialized with that path; callers must
/// reap submitted work before issuing a command that changes shared state.
pub struct VirtIOGpu<H: Hal, T: Transport> {
    transport: T,
    control_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    cursor_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    // Synchronous config probes use these fixed buffers before normal DRM
    // control traffic begins; mutable GPU operations use owned batches below.
    queue_buf_send: Box<[u8]>,
    queue_buf_recv: Box<[u8]>,
    resources: Vec<Resource>,
    next_resource_id: u32,
    virgl: bool,
    resource_uuid: bool,
    resource_blob: bool,
    context_init: bool,
    hostmem: Option<SharedMemoryRegion>,
    contexts: Vec<Context>,
    pending_contexts: Vec<Context>,
    failed_contexts: Vec<ContextId>,
    next_context_id: u32,
    next_fence_id: u64,
    pending_control: Vec<PendingControl>,
    pending_presents: Vec<PresentBatch>,
    terminal_control: Vec<GpuCompletion>,
    control_faulted: bool,
    /// token -> retained cursor DMA owner.  Queue tokens are unique while a
    /// descriptor is outstanding, so lookup/removal never scans the queue.
    pending_cursor: BTreeMap<u16, PendingCursor>,
    terminal_cursor: VecDeque<GpuCompletion>,
    cursor_faulted: bool,
}

impl<H: Hal, T: Transport> VirtIOGpu<H, T> {
    pub fn new(mut transport: T) -> Result<Self> {
        let features = transport.begin_init(SUPPORTED_FEATURES);
        let control_queue = VirtQueue::new(
            &mut transport,
            QUEUE_TRANSMIT,
            features.contains(Features::RING_INDIRECT_DESC),
            features.contains(Features::RING_EVENT_IDX),
        )?;
        let cursor_queue = VirtQueue::new(
            &mut transport,
            QUEUE_CURSOR,
            features.contains(Features::RING_INDIRECT_DESC),
            features.contains(Features::RING_EVENT_IDX),
        )?;
        let hostmem = transport.shared_memory_region(1);
        transport.finish_init();
        Ok(Self {
            transport,
            control_queue,
            cursor_queue,
            queue_buf_send: FromZeroes::new_box_slice_zeroed(PAGE_SIZE),
            queue_buf_recv: FromZeroes::new_box_slice_zeroed(PAGE_SIZE),
            resources: Vec::new(),
            next_resource_id: 1,
            virgl: features.contains(Features::VIRGL),
            resource_uuid: features.contains(Features::RESOURCE_UUID),
            resource_blob: features.contains(Features::RESOURCE_BLOB),
            context_init: features.contains(Features::CONTEXT_INIT),
            hostmem,
            contexts: Vec::new(),
            pending_contexts: Vec::new(),
            failed_contexts: Vec::new(),
            next_context_id: 1,
            next_fence_id: 1,
            pending_control: Vec::new(),
            pending_presents: Vec::new(),
            terminal_control: Vec::new(),
            control_faulted: false,
            pending_cursor: BTreeMap::new(),
            terminal_cursor: VecDeque::new(),
            cursor_faulted: false,
        })
    }
    pub const fn virgl_supported(&self) -> bool {
        self.virgl
    }
    pub const fn resource_uuid_supported(&self) -> bool {
        self.resource_uuid
    }
    pub const fn resource_blob_supported(&self) -> bool {
        self.resource_blob
    }
    pub const fn context_init_supported(&self) -> bool {
        self.context_init
    }
    pub const fn hostmem_supported(&self) -> bool {
        self.hostmem.is_some()
    }
    pub fn hostmem_len(&self) -> Option<u64> {
        match self.hostmem {
            Some(region) => Some(region.len as u64),
            None => None,
        }
    }

    /// A present batch is a resource-lifetime owner even while it is waiting
    /// for a free descriptor between protocol stages.
    pub fn present_pending(&self, resource: ResourceId) -> bool {
        self.pending_presents
            .iter()
            .any(|batch| batch.resource == resource)
    }

    /// Returns the immutable requested size of a live blob resource.  This is
    /// used only to validate a MAP_BLOB aperture span before it becomes a
    /// kernel physical-page vector.
    pub fn blob_size(&self, id: ResourceId) -> Result<u64> {
        let resource = self.resource(id)?;
        match resource.kind {
            ResourceKind::GuestBlob | ResourceKind::Host3dBlob | ResourceKind::Host3dGuestBlob => {
                resource.backing_bytes.ok_or(Error::InvalidParam)
            }
            ResourceKind::Legacy => Err(Error::InvalidParam),
        }
    }

    pub fn blob_mapped(&self, id: ResourceId) -> Result<bool> {
        Ok(self.resource(id)?.mapped)
    }

    fn require_virgl(&self) -> Result {
        (self.virgl || self.resource_blob)
            .then_some(())
            .ok_or(Error::Unsupported)
    }
    fn next_fence(&mut self) -> Result<u64> {
        let id = self.next_fence_id;
        // Never reuse an ID after wrap: an old completion must not validate a
        // new command, even though this synchronous queue has no normal
        // outstanding fence window.
        self.next_fence_id = self.next_fence_id.checked_add(1).ok_or(Error::NotReady)?;
        Ok(id)
    }
    pub fn ack_interrupt(&mut self) -> bool {
        self.transport.ack_interrupt()
    }
    /// Read config events without acknowledgement.  Callers must retain
    /// EVENT_DISPLAY until GET_DISPLAY_INFO has completed successfully.
    pub fn read_config_events(&mut self) -> Result<u32> {
        let cfg = self.transport.config_space::<Config>()?;
        Ok(unsafe { volread!(cfg, events_read) })
    }
    pub fn ack_config_events(&mut self, events: u32) -> Result<()> {
        if events != 0 {
            let cfg = self.transport.config_space::<Config>()?;
            unsafe { volwrite!(cfg, events_clear, events) };
        }
        Ok(())
    }
    pub fn config_events(&mut self) -> Result<u32> {
        let events = self.read_config_events()?;
        self.ack_config_events(events)?;
        Ok(events)
    }
    pub fn display_info(&mut self) -> Result<DisplayInfo> {
        let rsp: RespDisplayInfo =
            self.request(CtrlHeader::with_type(Command::GET_DISPLAY_INFO))?;
        rsp.header.check_type(Command::OK_DISPLAY_INFO)?;
        let mut scanouts = [Scanout::default(); 16];
        for (out, mode) in scanouts.iter_mut().zip(rsp.pmodes.iter()) {
            *out = Scanout {
                rect: mode.rect,
                enabled: mode.enabled != 0,
                flags: mode.flags,
            };
        }
        Ok(DisplayInfo { scanouts })
    }
    pub fn resolution(&mut self) -> Result<(u32, u32)> {
        let (_, s) = self
            .display_info()?
            .first_enabled()
            .ok_or(Error::NotReady)?;
        Ok((s.rect.width, s.rect.height))
    }

    /// Replace the sole scanout cursor image through cursorq.  Cursor work is
    /// deliberately independent of controlq: render completions must never
    /// delay pointer motion, and a pending render command must not let this
    /// method consume a controlq used entry.
    pub fn update_cursor(
        &mut self,
        id: ResourceId,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        x: i32,
        y: i32,
    ) -> Result<GpuSubmission> {
        let resource = self.resource(id)?;
        if resource.lifecycle != ResourceLifecycle::Live
            || resource.backing != BackingState::Attached
            || width != 64
            || height != 64
            || width > resource.width
            || height > resource.height
            || hot_x >= width
            || hot_y >= height
        {
            return Err(Error::InvalidParam);
        }
        self.submit_cursor(UpdateCursor {
            header: CtrlHeader::with_type(Command::UPDATE_CURSOR),
            pos: CursorPos {
                scanout_id: 0,
                x,
                y,
                _padding: 0,
            },
            resource_id: id.0,
            hot_x,
            hot_y,
            _padding: 0,
        })
    }

    /// Move the cursor on cursorq without changing its resource.
    pub fn move_cursor(&mut self, x: i32, y: i32) -> Result<GpuSubmission> {
        self.submit_cursor(MoveCursor {
            header: CtrlHeader::with_type(Command::MOVE_CURSOR),
            pos: CursorPos {
                scanout_id: 0,
                x,
                y,
                _padding: 0,
            },
        })
    }

    /// Queue GET_CAPSET_INFO.  Its typed response is available only through
    /// the matching terminal completion.
    pub fn submit_capset_info(&mut self, index: u32) -> Result<GpuSubmission> {
        self.require_virgl()?;
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::CapsetInfo,
            GetCapsetInfo {
                header: CtrlHeader::fenced(Command::GET_CAPSET_INFO, 0, fence),
                index,
                _padding: 0,
            }
            .as_bytes()
            .to_vec(),
        )
    }

    /// Queue GET_CAPSET with an exact, bounded response payload.
    pub fn submit_capset(&mut self, id: u32, version: u32, bytes: usize) -> Result<GpuSubmission> {
        self.require_virgl()?;
        if core::mem::size_of::<CtrlHeader>()
            .checked_add(bytes)
            .ok_or(Error::InvalidParam)?
            > MAX_CONTROL_PAYLOAD
        {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::Capset { bytes },
            GetCapset {
                header: CtrlHeader::fenced(Command::GET_CAPSET, 0, fence),
                capset_id: id,
                capset_version: version,
            }
            .as_bytes()
            .to_vec(),
        )
    }
    pub fn submit_assign_uuid(&mut self, id: ResourceId) -> Result<GpuSubmission> {
        if !self.resource_uuid || self.resource(id)?.lifecycle != ResourceLifecycle::Live {
            return Err(Error::Unsupported);
        }
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::AssignUuid(id),
            ResourceAssignUuid {
                header: CtrlHeader::fenced(Command::RESOURCE_ASSIGN_UUID, 0, fence),
                resource_id: id.0,
                _padding: 0,
            }
            .as_bytes()
            .to_vec(),
        )
    }
    pub fn submit_map_blob(&mut self, id: ResourceId, offset: u64) -> Result<GpuSubmission> {
        if !self.resource_blob
            || self.hostmem.is_none()
            || self.resource(id)?.lifecycle != ResourceLifecycle::Live
            || self.resource(id)?.mapped
        {
            return Err(Error::Unsupported);
        }
        let bytes = (self
            .resource(id)?
            .backing_bytes
            .ok_or(Error::InvalidParam)?
            + PAGE_SIZE as u64
            - 1)
            & !(PAGE_SIZE as u64 - 1);
        let aperture = self.hostmem.ok_or(Error::Unsupported)?;
        if offset % PAGE_SIZE as u64 != 0
            || offset
                .checked_add(bytes)
                .is_none_or(|end| end > aperture.len as u64)
        {
            return Err(Error::InvalidParam);
        }
        self.resource_mut(id)?.map_offset = Some(offset);
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::MapBlob(id),
            ResourceMapBlob {
                header: CtrlHeader::fenced(Command::RESOURCE_MAP_BLOB, 0, fence),
                resource_id: id.0,
                _padding: 0,
                offset,
            }
            .as_bytes()
            .to_vec(),
        )
    }
    pub fn submit_unmap_blob(&mut self, id: ResourceId) -> Result<GpuSubmission> {
        if !self.resource_blob || !self.resource(id)?.mapped {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::UnmapBlob(id),
            ResourceUnmapBlob {
                header: CtrlHeader::fenced(Command::RESOURCE_UNMAP_BLOB, 0, fence),
                resource_id: id.0,
                _padding: 0,
            }
            .as_bytes()
            .to_vec(),
        )
    }
    pub fn submit_present_blob(
        &mut self,
        id: ResourceId,
        source_x: u32,
        source_y: u32,
        width: u32,
        height: u32,
        framebuffer_width: u32,
        framebuffer_height: u32,
        format: u32,
        stride: u32,
        offset: u32,
        damage: Option<Rect>,
    ) -> Result<GpuSubmission> {
        if !self.resource_blob
            || self.resource(id)?.lifecycle != ResourceLifecycle::Live
            || width == 0
            || height == 0
            || framebuffer_width == 0
            || framebuffer_height == 0
            || source_x
                .checked_add(width)
                .is_none_or(|end| end > framebuffer_width)
            || source_y
                .checked_add(height)
                .is_none_or(|end| end > framebuffer_height)
            || !matches!(format, 1 | 2)
            || stride
                < framebuffer_width
                    .checked_mul(4)
                    .ok_or(Error::InvalidParam)?
        {
            return Err(Error::InvalidParam);
        }
        let end = u64::from(offset)
            .checked_add(
                u64::from(stride)
                    .checked_mul(u64::from(framebuffer_height))
                    .ok_or(Error::InvalidParam)?,
            )
            .ok_or(Error::InvalidParam)?;
        if end
            > self
                .resource(id)?
                .backing_bytes
                .ok_or(Error::InvalidParam)?
        {
            return Err(Error::InvalidParam);
        }
        if self.control_faulted || self.pending_presents.len() == MAX_PENDING_PRESENTS {
            return Err(Error::NotReady);
        }
        self.pending_presents
            .try_reserve(1)
            .map_err(|_| Error::DmaError)?;
        self.terminal_control
            .try_reserve(MAX_PENDING_PRESENTS)
            .map_err(|_| Error::DmaError)?;
        let fence_id = self.next_fence()?;
        let visible = Rect::new(source_x, source_y, width, height);
        let damage = damage.unwrap_or(visible);
        if !damage.fits(framebuffer_width, framebuffer_height) {
            return Err(Error::InvalidParam);
        }
        self.pending_presents.push(PresentBatch {
            fence_id,
            resource: id,
            visible,
            damage,
            blob_layout: Some(BlobScanoutLayout {
                framebuffer_width,
                framebuffer_height,
                format,
                stride,
                offset,
            }),
            stage: PresentStage::SetScanoutBlob,
            in_flight: None,
        });
        self.service_present_batches();
        Ok(GpuSubmission { fence_id })
    }
    /// Submit virgl work without waiting for the host.  The returned fence is
    /// terminal only after [`drain_control_completions`] reports it.  Request
    /// and response buffers remain owned by the driver throughout that
    /// interval, so neither borrowed command bytes nor a stack response can
    /// be released while the device may still DMA them.
    pub fn submit_3d(
        &mut self,
        context: u32,
        ring_idx: u32,
        commands: &[u8],
        resources: &[u32],
    ) -> Result<GpuSubmission> {
        self.require_virgl()?;
        if self.control_faulted || self.pending_control.len() == MAX_PENDING_CONTROL {
            return Err(Error::NotReady);
        }
        // Reserve metadata before publishing descriptors. A metadata failure
        // must not leave a DMA request with no retained owner.
        self.pending_control
            .try_reserve(1)
            .map_err(|_| Error::DmaError)?;
        self.terminal_control
            .try_reserve(MAX_PENDING_CONTROL)
            .map_err(|_| Error::DmaError)?;
        if commands.is_empty()
            || !self
                .contexts
                .iter()
                .any(|c| c.id.0 == context && ring_idx < c.rings)
        {
            return Err(Error::InvalidParam);
        }
        if resources.iter().any(|r| {
            !self
                .contexts
                .iter()
                .any(|c| c.id.0 == context && c.resources.contains(&ResourceId(*r)))
        }) {
            return Err(Error::InvalidParam);
        }
        let bytes = core::mem::size_of::<CmdSubmit>()
            .checked_add(commands.len())
            .ok_or(Error::InvalidParam)?;
        if bytes > MAX_CONTROL_PAYLOAD {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        let header = CmdSubmit {
            header: CtrlHeader::fenced_ring(Command::SUBMIT_3D, context, fence, ring_idx),
            size: u32::try_from(commands.len()).map_err(|_| Error::InvalidParam)?,
            _padding: 0,
        };
        let mut request = Vec::new();
        request
            .try_reserve_exact(bytes)
            .map_err(|_| Error::DmaError)?;
        request.extend_from_slice(header.as_bytes());
        request.extend_from_slice(commands);
        self.enqueue_control_submission(fence, context, PendingControlOperation::Submit3d, request)
    }

    /// Publish a fully owned controlq request.  The vector is moved into the
    /// pending record before descriptor publication, which is the sole place
    /// where controlq DMA ownership crosses from the caller to the device.
    fn enqueue_control_submission(
        &mut self,
        fence_id: u64,
        context: u32,
        operation: PendingControlOperation,
        request: Vec<u8>,
    ) -> Result<GpuSubmission> {
        if self.control_faulted || self.pending_control.len() == MAX_PENDING_CONTROL {
            return Err(Error::NotReady);
        }
        if request.is_empty() || request.len() > MAX_CONTROL_PAYLOAD {
            return Err(Error::InvalidParam);
        }
        let response_len = operation.response_len();
        if response_len > MAX_CONTROL_PAYLOAD {
            return Err(Error::InvalidParam);
        }
        self.pending_control
            .try_reserve(1)
            .map_err(|_| Error::DmaError)?;
        self.terminal_control
            .try_reserve(MAX_PENDING_CONTROL)
            .map_err(|_| Error::DmaError)?;
        let mut pending = PendingControl {
            token: 0,
            fence_id,
            context,
            operation,
            request: request.into_boxed_slice(),
            response: FromZeroes::new_box_slice_zeroed(response_len),
        };
        let inputs = [pending.request.as_ref()];
        let mut outputs = [pending.response.as_mut()];
        // SAFETY: `pending` is moved into `pending_control` before the chain
        // is published and remains there until its matching `pop_used`.
        let token = unsafe { self.control_queue.add_unpublished(&inputs, &mut outputs) }?;
        pending.token = token;
        drop(outputs);
        self.pending_control.push(pending);
        self.control_queue.publish_unpublished(token);
        if self.control_queue.should_notify() {
            self.transport.notify(QUEUE_TRANSMIT);
        }
        Ok(GpuSubmission { fence_id })
    }

    /// Reserve a resource identity and publish CREATE_2D without making the
    /// identity usable. The resource stays `CreateUncertain` until the
    /// matching control completion commits it to `Live`.
    pub fn submit_create_2d(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<(ResourceId, GpuSubmission)> {
        if width == 0
            || height == 0
            || self.resources.len() == MAX_RESOURCES
            || width
                .checked_mul(height)
                .and_then(|n| n.checked_mul(4))
                .is_none()
        {
            return Err(Error::InvalidParam);
        }
        self.resources.try_reserve(1).map_err(|_| Error::DmaError)?;
        let id = ResourceId(self.next_resource_id);
        self.next_resource_id = self
            .next_resource_id
            .checked_add(1)
            .ok_or(Error::DmaError)?;
        let fence = self.next_fence()?;
        self.resources.push(Resource {
            id,
            width,
            height,
            backing: BackingState::Detached,
            lifecycle: ResourceLifecycle::CreateUncertain,
            backing_bytes: Some(u64::from(width) * u64::from(height) * 4),
            kind: ResourceKind::Legacy,
            uuid: None,
            mapped: false,
            map_offset: None,
        });
        let request = ResourceCreate2D {
            header: CtrlHeader::fenced(Command::RESOURCE_CREATE_2D, 0, fence),
            resource_id: id.0,
            format: Format::B8G8R8A8UNORM,
            width,
            height,
        }
        .as_bytes()
        .to_vec();
        match self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::CreateResource(id),
            request,
        ) {
            Ok(submission) => Ok((id, submission)),
            Err(error) => {
                // Descriptor publication failed before a lower DMA owner was
                // installed; remove the guest-only reservation immediately.
                self.forget_resource(id);
                Err(error)
            }
        }
    }

    /// Reserve a context ID and publish CTX_CREATE. A failed completion
    /// schedules a fenced destroy for the never-reused ID (or resets the
    /// queue if that cleanup cannot be owned), so an uncertain host context
    /// can never be confused with a later client context.
    pub fn submit_create_context(
        &mut self,
        name: &[u8],
        init: ContextInit,
    ) -> Result<(ContextId, GpuSubmission)> {
        self.require_virgl()?;
        let init = ContextInit {
            num_rings: if init.num_rings == 0 {
                1
            } else {
                init.num_rings
            },
            ..init
        };
        if name.len() > 64
            || init.debug_name_len as usize > 64
            || init.num_rings > 256
            || init.poll_rings_mask >> init.num_rings != 0
            || (!self.context_init
                && init
                    != ContextInit {
                        num_rings: 1,
                        ..ContextInit::default()
                    })
            || self.contexts.len() + self.pending_contexts.len() == MAX_CONTEXTS
        {
            return Err(Error::InvalidParam);
        }
        self.pending_contexts
            .try_reserve(1)
            .map_err(|_| Error::DmaError)?;
        let id = ContextId(self.next_context_id);
        self.next_context_id = self.next_context_id.checked_add(1).ok_or(Error::DmaError)?;
        let fence = self.next_fence()?;
        let mut request = CtxCreate {
            header: CtrlHeader::fenced(Command::CTX_CREATE, id.get(), fence),
            nlen: name.len() as u32,
            context_init: init.capset_id & 0xff,
            debug_name: [0; 64],
        };
        let debug = if init.debug_name_len != 0 {
            &init.debug_name[..init.debug_name_len as usize]
        } else {
            name
        };
        request.nlen = debug.len() as u32;
        request.debug_name[..debug.len()].copy_from_slice(debug);
        self.pending_contexts.push(Context {
            id,
            resources: Vec::new(),
            rings: init.num_rings,
        });
        match self.enqueue_control_submission(
            fence,
            id.get(),
            PendingControlOperation::CreateContext(id),
            request.as_bytes().to_vec(),
        ) {
            Ok(submission) => Ok((id, submission)),
            Err(error) => {
                self.pending_contexts.retain(|entry| entry.id != id);
                Err(error)
            }
        }
    }

    /// Asynchronous CTX_DESTROY.  The context remains locally live until the
    /// exact used-ring completion commits its removal.
    pub fn submit_destroy_context(&mut self, context: u32) -> Result<GpuSubmission> {
        let id = self
            .contexts
            .iter()
            .find(|entry| entry.id.get() == context)
            .map(|entry| entry.id)
            .ok_or(Error::InvalidParam)?;
        if !self
            .contexts
            .iter()
            .any(|entry| entry.id == id && entry.resources.is_empty())
        {
            return Err(Error::NotReady);
        }
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            context,
            PendingControlOperation::DestroyContext(id),
            CtxDestroy {
                header: CtrlHeader::fenced(Command::CTX_DESTROY, context, fence),
            }
            .as_bytes()
            .to_vec(),
        )
    }

    pub fn submit_context_attach_resource(
        &mut self,
        context: u32,
        resource: u32,
    ) -> Result<GpuSubmission> {
        let resource = ResourceId(resource);
        if self.resource(resource)?.lifecycle != ResourceLifecycle::Live {
            return Err(Error::NotReady);
        }
        let context_id = {
            let entry = self
                .contexts
                .iter_mut()
                .find(|entry| entry.id.get() == context)
                .ok_or(Error::InvalidParam)?;
            if entry.resources.len() == MAX_CONTEXT_RESOURCES || entry.resources.contains(&resource)
            {
                return Err(Error::InvalidParam);
            }
            // Reserve before publishing; completion may not allocate.
            entry
                .resources
                .try_reserve(1)
                .map_err(|_| Error::DmaError)?;
            entry.id
        };
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            context,
            PendingControlOperation::AttachContextResource {
                context: context_id,
                resource,
            },
            CtxResource {
                header: CtrlHeader::fenced(Command::CTX_ATTACH_RESOURCE, context, fence),
                resource_id: resource.get(),
                _padding: 0,
            }
            .as_bytes()
            .to_vec(),
        )
    }

    pub fn submit_context_detach_resource(
        &mut self,
        context: u32,
        resource: u32,
    ) -> Result<GpuSubmission> {
        let resource = ResourceId(resource);
        let context_id = {
            let entry = self
                .contexts
                .iter()
                .find(|entry| entry.id.get() == context)
                .ok_or(Error::InvalidParam)?;
            if !entry.resources.contains(&resource) {
                return Err(Error::InvalidParam);
            }
            entry.id
        };
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            context,
            PendingControlOperation::DetachContextResource {
                context: context_id,
                resource,
            },
            CtxResource {
                header: CtrlHeader::fenced(Command::CTX_DETACH_RESOURCE, context, fence),
                resource_id: resource.get(),
                _padding: 0,
            }
            .as_bytes()
            .to_vec(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit_create_3d(
        &mut self,
        target: u32,
        format: u32,
        bind: u32,
        width: u32,
        height: u32,
        depth: u32,
        array_size: u32,
        last_level: u32,
        nr_samples: u32,
        flags: u32,
    ) -> Result<(ResourceId, GpuSubmission)> {
        self.require_virgl()?;
        if width == 0
            || height == 0
            || depth == 0
            || array_size == 0
            || self.resources.len() == MAX_RESOURCES
        {
            return Err(Error::InvalidParam);
        }
        self.resources.try_reserve(1).map_err(|_| Error::DmaError)?;
        let id = ResourceId(self.next_resource_id);
        self.next_resource_id = self
            .next_resource_id
            .checked_add(1)
            .ok_or(Error::DmaError)?;
        let fence = self.next_fence()?;
        self.resources.push(Resource {
            id,
            width,
            height,
            backing: BackingState::Detached,
            lifecycle: ResourceLifecycle::CreateUncertain,
            backing_bytes: None,
            kind: ResourceKind::Legacy,
            uuid: None,
            mapped: false,
            map_offset: None,
        });
        let request = ResourceCreate3D {
            header: CtrlHeader::fenced(Command::RESOURCE_CREATE_3D, 0, fence),
            resource_id: id.get(),
            target,
            format,
            bind,
            width,
            height,
            depth,
            array_size,
            last_level,
            nr_samples,
            flags,
            _padding: 0,
        }
        .as_bytes()
        .to_vec();
        match self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::CreateResource(id),
            request,
        ) {
            Ok(submission) => Ok((id, submission)),
            Err(error) => {
                self.forget_resource(id);
                Err(error)
            }
        }
    }

    pub fn submit_create_blob(
        &mut self,
        blob: BlobResource,
        entries: &[(u64, u32)],
    ) -> Result<(ResourceId, GpuSubmission)> {
        if !self.resource_blob || blob.size == 0 || self.resources.len() == MAX_RESOURCES {
            return Err(Error::InvalidParam);
        }
        self.resources.try_reserve(1).map_err(|_| Error::DmaError)?;
        let id = ResourceId(self.next_resource_id);
        self.next_resource_id = self
            .next_resource_id
            .checked_add(1)
            .ok_or(Error::DmaError)?;
        let kind = match blob.mem {
            BlobMem::Guest => ResourceKind::GuestBlob,
            BlobMem::Host3d => ResourceKind::Host3dBlob,
            BlobMem::Host3dGuest => ResourceKind::Host3dGuestBlob,
        };
        let fence = self.next_fence()?;
        if entries.len() > MAX_SG_ENTRIES
            || matches!(blob.mem, BlobMem::Host3d) && !entries.is_empty()
            || !matches!(blob.mem, BlobMem::Host3d) && entries.is_empty()
        {
            return Err(Error::InvalidParam);
        }
        // CREATE_BLOB embeds its SG list; it is not ATTACH_BACKING and stays
        // detachable so UNMAP followed by UNREF has a valid lifecycle.
        self.resources.push(Resource {
            id,
            width: 0,
            height: 0,
            backing: BackingState::Detached,
            lifecycle: ResourceLifecycle::CreateUncertain,
            backing_bytes: Some(blob.size),
            kind,
            uuid: None,
            mapped: false,
            map_offset: None,
        });
        let header = ResourceCreateBlob {
            header: CtrlHeader::fenced(Command::RESOURCE_CREATE_BLOB, 0, fence),
            resource_id: id.get(),
            blob_mem: match blob.mem {
                BlobMem::Guest => 1,
                BlobMem::Host3d => 2,
                BlobMem::Host3dGuest => 3,
            },
            blob_flags: blob.flags,
            nr_entries: entries.len() as u32,
            blob_id: blob.blob_id,
            size: blob.size,
        };
        let mut request = header.as_bytes().to_vec();
        for &(addr, length) in entries {
            if addr == 0 || length == 0 {
                self.forget_resource(id);
                return Err(Error::InvalidParam);
            }
            request.extend_from_slice(
                MemEntry {
                    addr,
                    length,
                    _padding: 0,
                }
                .as_bytes(),
            );
        }
        match self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::CreateBlob(id),
            request,
        ) {
            Ok(submission) => Ok((id, submission)),
            Err(error) => {
                self.forget_resource(id);
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit_transfer_3d(
        &mut self,
        context: u32,
        resource: u32,
        x: u32,
        y: u32,
        z: u32,
        width: u32,
        height: u32,
        depth: u32,
        offset: u64,
        level: u32,
        stride: u32,
        layer_stride: u32,
        to_host: bool,
    ) -> Result<GpuSubmission> {
        self.require_virgl()?;
        let resource = ResourceId(resource);
        if self.resource(resource)?.lifecycle != ResourceLifecycle::Live
            || !self
                .contexts
                .iter()
                .any(|entry| entry.id.get() == context && entry.resources.contains(&resource))
        {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        let command = if to_host {
            Command::TRANSFER_TO_HOST_3D
        } else {
            Command::TRANSFER_FROM_HOST_3D
        };
        if width == 0 || height == 0 || depth == 0 {
            return Err(Error::InvalidParam);
        }
        let request = TransferHost3D {
            header: CtrlHeader::fenced(command, context, fence),
            box_: Box3D {
                x,
                y,
                z,
                w: width,
                h: height,
                d: depth,
            },
            offset,
            resource_id: resource.get(),
            level,
            stride,
            layer_stride,
        }
        .as_bytes()
        .to_vec();
        self.enqueue_control_submission(
            fence,
            context,
            PendingControlOperation::Transfer3d {
                context: ContextId(context),
                resource,
            },
            request,
        )
    }

    pub fn submit_attach_backing_entries(
        &mut self,
        id: ResourceId,
        entries: &[(u64, u32)],
    ) -> Result<GpuSubmission> {
        let resource = self.resource(id)?;
        let total = entries
            .iter()
            .try_fold(0u64, |total, (paddr, length)| {
                (*paddr != 0 && *length != 0)
                    .then(|| total.checked_add(*length as u64))
                    .flatten()
            })
            .ok_or(Error::InvalidParam)?;
        if entries.is_empty()
            || entries.len() > MAX_SG_ENTRIES
            || resource.backing_bytes.is_some_and(|bytes| total < bytes)
            || resource.backing != BackingState::Detached
            || resource.lifecycle != ResourceLifecycle::Live
        {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        let header = ResourceAttachBackingHeader {
            header: CtrlHeader::fenced(Command::RESOURCE_ATTACH_BACKING, 0, fence),
            resource_id: id.get(),
            nr_entries: u32::try_from(entries.len()).map_err(|_| Error::InvalidParam)?,
        };
        let bytes = core::mem::size_of::<ResourceAttachBackingHeader>()
            .checked_add(
                entries
                    .len()
                    .checked_mul(core::mem::size_of::<MemEntry>())
                    .ok_or(Error::InvalidParam)?,
            )
            .ok_or(Error::InvalidParam)?;
        let mut request = Vec::new();
        request
            .try_reserve_exact(bytes)
            .map_err(|_| Error::DmaError)?;
        request.extend_from_slice(header.as_bytes());
        for &(addr, length) in entries {
            request.extend_from_slice(
                MemEntry {
                    addr,
                    length,
                    _padding: 0,
                }
                .as_bytes(),
            );
        }
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::AttachBacking(id),
            request,
        )
    }

    pub fn submit_detach_backing(&mut self, id: ResourceId) -> Result<GpuSubmission> {
        if self.resource(id)?.backing == BackingState::Detached {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::DetachBacking(id),
            ResourceDetachBacking {
                header: CtrlHeader::fenced(Command::RESOURCE_DETACH_BACKING, 0, fence),
                resource_id: id.get(),
                _padding: 0,
            }
            .as_bytes()
            .to_vec(),
        )
    }

    pub fn submit_unref(&mut self, id: ResourceId) -> Result<GpuSubmission> {
        let resource = self.resource(id)?;
        if resource.backing != BackingState::Detached
            || resource.mapped
            || self
                .contexts
                .iter()
                .any(|context| context.resources.contains(&id))
        {
            return Err(Error::NotReady);
        }
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::UnrefResource(id),
            ResourceUnref {
                header: CtrlHeader::fenced(Command::RESOURCE_UNREF, 0, fence),
                resource_id: id.get(),
                _padding: 0,
            }
            .as_bytes()
            .to_vec(),
        )
    }

    pub fn submit_set_scanout(
        &mut self,
        rect: Rect,
        scanout: u32,
        resource: Option<ResourceId>,
    ) -> Result<GpuSubmission> {
        if scanout >= 16 {
            return Err(Error::InvalidParam);
        }
        if let Some(id) = resource {
            let resource = self.resource(id)?;
            if resource.lifecycle != ResourceLifecycle::Live
                || resource.backing != BackingState::Attached
                || !rect.fits(resource.width, resource.height)
            {
                return Err(Error::InvalidParam);
            }
        }
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::SetScanout,
            SetScanout {
                header: CtrlHeader::fenced(Command::SET_SCANOUT, 0, fence),
                rect,
                scanout_id: scanout,
                resource_id: resource.map_or(0, ResourceId::get),
            }
            .as_bytes()
            .to_vec(),
        )
    }

    pub fn submit_transfer_to_host(&mut self, id: ResourceId, rect: Rect) -> Result<GpuSubmission> {
        let resource = self.resource(id)?;
        if resource.lifecycle != ResourceLifecycle::Live
            || resource.backing != BackingState::Attached
            || !rect.fits(resource.width, resource.height)
        {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::Transfer2d(id),
            TransferToHost2D {
                header: CtrlHeader::fenced(Command::TRANSFER_TO_HOST_2D, 0, fence),
                rect,
                offset: 0,
                resource_id: id.get(),
                _padding: 0,
            }
            .as_bytes()
            .to_vec(),
        )
    }

    pub fn submit_resource_flush(&mut self, id: ResourceId, rect: Rect) -> Result<GpuSubmission> {
        let resource = self.resource(id)?;
        if resource.lifecycle != ResourceLifecycle::Live
            || !rect.fits(resource.width, resource.height)
        {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        self.enqueue_control_submission(
            fence,
            0,
            PendingControlOperation::Flush(id),
            ResourceFlush {
                header: CtrlHeader::fenced(Command::RESOURCE_FLUSH, 0, fence),
                rect,
                resource_id: id.get(),
                _padding: 0,
            }
            .as_bytes()
            .to_vec(),
        )
    }

    /// Submit one complete 2D presentation.  Its single externally visible
    /// fence becomes terminal only after SET_SCANOUT, TRANSFER_TO_HOST_2D,
    /// and RESOURCE_FLUSH have each completed successfully.  The individual
    /// command fences remain private so a caller cannot mistake a configured
    /// scanout for pixels that have reached the host.
    pub fn submit_present(
        &mut self,
        resource: ResourceId,
        visible: Rect,
        damage: Rect,
    ) -> Result<GpuSubmission> {
        if self.control_faulted || self.pending_presents.len() == MAX_PENDING_PRESENTS {
            return Err(Error::NotReady);
        }
        let entry = self.resource(resource)?;
        if entry.lifecycle != ResourceLifecycle::Live
            || entry.backing != BackingState::Attached
            || !visible.fits(entry.width, entry.height)
            || !damage.fits(entry.width, entry.height)
        {
            return Err(Error::InvalidParam);
        }
        // Reserve all terminal-record capacity before the batch gains a
        // resource lifetime pin. Reset may need one result for every queued
        // batch in addition to ordinary control submissions.
        self.pending_presents
            .try_reserve(1)
            .map_err(|_| Error::DmaError)?;
        self.terminal_control
            .try_reserve(MAX_PENDING_PRESENTS)
            .map_err(|_| Error::DmaError)?;
        let fence_id = self.next_fence()?;
        self.pending_presents.push(PresentBatch {
            fence_id,
            resource,
            visible,
            damage,
            blob_layout: None,
            stage: PresentStage::SetScanout,
            in_flight: None,
        });
        self.service_present_batches();
        Ok(GpuSubmission { fence_id })
    }

    /// Try to advance every batch whose previous command has completed.  A
    /// full control queue leaves the batch intact for the next bounded drain;
    /// no later stage is ever published before its exact predecessor result.
    fn service_present_batches(&mut self) {
        let mut index = 0;
        while index < self.pending_presents.len() {
            if self.pending_presents[index].in_flight.is_some() {
                index += 1;
                continue;
            }
            let (resource, visible, damage, layout, stage) = {
                let batch = &self.pending_presents[index];
                (
                    batch.resource,
                    batch.visible,
                    batch.damage,
                    batch.blob_layout,
                    batch.stage,
                )
            };
            let submission = match stage {
                PresentStage::SetScanout => self.submit_set_scanout(visible, 0, Some(resource)),
                PresentStage::SetScanoutBlob => {
                    let layout = layout.expect("blob stage has layout");
                    match self.next_fence() {
                        Err(error) => Err(error),
                        Ok(fence) => self.enqueue_control_submission(
                            fence,
                            0,
                            PendingControlOperation::SetScanout,
                            SetScanoutBlob {
                                header: CtrlHeader::fenced(Command::SET_SCANOUT_BLOB, 0, fence),
                                rect: visible,
                                scanout_id: 0,
                                resource_id: resource.0,
                                width: layout.framebuffer_width,
                                height: layout.framebuffer_height,
                                format: layout.format,
                                _padding: 0,
                                strides: [layout.stride, 0, 0, 0],
                                offsets: [layout.offset, 0, 0, 0],
                            }
                            .as_bytes()
                            .to_vec(),
                        ),
                    }
                }
                PresentStage::TransferToHost => self.submit_transfer_to_host(resource, damage),
                PresentStage::Flush => self.submit_resource_flush(resource, damage),
            };
            match submission {
                Ok(submission) => {
                    self.pending_presents[index].in_flight = Some(submission.fence_id);
                    index += 1;
                }
                Err(Error::NotReady) | Err(Error::QueueFull) => {
                    // Backpressure is not failure: the completion worker will
                    // retry after it reaps another controlq entry.
                    index += 1;
                }
                Err(error) => {
                    let batch = self.pending_presents.swap_remove(index);
                    self.terminal_control.push(GpuCompletion {
                        fence_id: batch.fence_id,
                        result: Err(error),
                        data: GpuCompletionData::None,
                    });
                }
            }
        }
    }

    /// Consume a private stage completion. Returns true when `fence_id`
    /// belonged to a present batch, in which case the caller must not expose
    /// the intermediate result through the public completion ring.
    fn complete_present_stage(&mut self, fence_id: u64, result: Result) -> bool {
        let Some(index) = self
            .pending_presents
            .iter()
            .position(|batch| batch.in_flight == Some(fence_id))
        else {
            return false;
        };
        if let Err(error) = result {
            let batch = self.pending_presents.swap_remove(index);
            self.terminal_control.push(GpuCompletion {
                fence_id: batch.fence_id,
                result: Err(error),
                data: GpuCompletionData::None,
            });
            return true;
        }
        let batch = &mut self.pending_presents[index];
        batch.in_flight = None;
        match batch.stage {
            PresentStage::SetScanout => batch.stage = PresentStage::TransferToHost,
            PresentStage::SetScanoutBlob => batch.stage = PresentStage::Flush,
            PresentStage::TransferToHost => batch.stage = PresentStage::Flush,
            PresentStage::Flush => {
                let batch = self.pending_presents.swap_remove(index);
                self.terminal_control.push(GpuCompletion {
                    fence_id: batch.fence_id,
                    result: Ok(()),
                    data: GpuCompletionData::None,
                });
            }
        }
        true
    }

    /// Reap at most `out.len()` host completions.  This is the only normal
    /// path that releases asynchronous control request/response DMA owners.
    pub fn drain_control_completions(&mut self, out: &mut [GpuCompletion]) -> Result<usize> {
        // A prior stage may be waiting solely for a descriptor released by an
        // unrelated completion. Retrying here keeps that transition bounded
        // and does not require a second worker or a busy submission loop.
        self.service_present_batches();
        let mut completed = 0;
        while completed < out.len() && !self.terminal_control.is_empty() {
            out[completed] = self.terminal_control.swap_remove(0);
            completed += 1;
        }
        if completed == out.len() {
            return Ok(completed);
        }
        while completed < out.len() {
            let Some(token) = self.control_queue.peek_used() else {
                break;
            };
            let Some(index) = self.pending_control.iter().position(|p| p.token == token) else {
                self.fault_control_queue();
                return Err(Error::IoError);
            };
            let pop = {
                let pending = &mut self.pending_control[index];
                let inputs = [pending.request.as_ref()];
                let mut outputs = [pending.response.as_mut()];
                // SAFETY: these are the exact retained buffers installed for
                // this token, and `peek_used` selected the same chain.
                unsafe { self.control_queue.pop_used(token, &inputs, &mut outputs) }
            };
            let used = match pop {
                Ok(used) => used,
                Err(error) => {
                    self.fault_control_queue();
                    return Err(error);
                }
            };
            let (fence_id, operation, mut result, data) = {
                let pending = &self.pending_control[index];
                let (result, data) = match self.decode_control_response(pending, used) {
                    Ok(data) => (Ok(()), data),
                    Err(error) => (Err(error), GpuCompletionData::None),
                };
                (pending.fence_id, pending.operation, result, data)
            };
            self.pending_control.swap_remove(index);
            if result.is_ok() {
                if let Err(error) = self.commit_control_operation(operation, &data) {
                    result = Err(error);
                }
            } else {
                self.abort_control_operation(operation);
            }
            if self.complete_present_stage(fence_id, result) {
                // A stage completion is private to its PresentBatch. Advance
                // only after the exact successful completion has committed;
                // a queue-full retry remains retained in the batch.
                self.service_present_batches();
                while completed < out.len() && !self.terminal_control.is_empty() {
                    out[completed] = self.terminal_control.swap_remove(0);
                    completed += 1;
                }
            } else {
                out[completed] = GpuCompletion {
                    fence_id,
                    result,
                    data,
                };
                completed += 1;
            }
        }
        Ok(completed)
    }

    /// Commit only the local state transition proven by the matching used
    /// entry. Commands are allowed to reserve IDs before publication, but
    /// nothing may observe those IDs as live while the request remains in
    /// `pending_control`.
    fn decode_control_response(
        &self,
        pending: &PendingControl,
        used: u32,
    ) -> Result<GpuCompletionData> {
        if used < pending.operation.response_len() as u32 {
            return Err(Error::IoError);
        }
        match pending.operation {
            PendingControlOperation::MapBlob(resource) => {
                let response =
                    RespMapBlob::read_from_prefix(&pending.response).ok_or(Error::IoError)?;
                response.header.check_fence(
                    Command::OK_MAP_INFO,
                    pending.context,
                    pending.fence_id,
                )?;
                let offset = self.resource(resource)?.map_offset.ok_or(Error::IoError)?;
                let base = self.hostmem.ok_or(Error::Unsupported)?.virt_base.as_ptr() as usize;
                let aperture_base = base
                    .checked_add(offset as usize)
                    .ok_or(Error::InvalidParam)? as u64;
                let physical_base = self
                    .hostmem
                    .ok_or(Error::Unsupported)?
                    .phys_base
                    .checked_add(offset as usize)
                    .ok_or(Error::InvalidParam)? as u64;
                if !matches!(response.map_info & 0x0f, 0 | 1 | 2 | 3) {
                    return Err(Error::IoError);
                }
                Ok(GpuCompletionData::MapInfo {
                    aperture_offset: offset,
                    aperture_base,
                    physical_base,
                    cache_policy: response.map_info,
                })
            }
            PendingControlOperation::AssignUuid(_) => {
                let response =
                    RespResourceUuid::read_from_prefix(&pending.response).ok_or(Error::IoError)?;
                response.header.check_fence(
                    Command::OK_RESOURCE_UUID,
                    pending.context,
                    pending.fence_id,
                )?;
                Ok(GpuCompletionData::Uuid(response.uuid))
            }
            PendingControlOperation::CapsetInfo => {
                let response =
                    RespCapsetInfo::read_from_prefix(&pending.response).ok_or(Error::IoError)?;
                response.header.check_fence(
                    Command::OK_CAPSET_INFO,
                    pending.context,
                    pending.fence_id,
                )?;
                Ok(GpuCompletionData::CapsetInfo {
                    id: response.capset_id,
                    max_version: response.capset_max_version,
                    max_size: response.capset_max_size,
                })
            }
            PendingControlOperation::Capset { bytes } => {
                let header =
                    CtrlHeader::read_from_prefix(&pending.response).ok_or(Error::IoError)?;
                header.check_fence(Command::OK_CAPSET, pending.context, pending.fence_id)?;
                let start = core::mem::size_of::<CtrlHeader>();
                let end = start.checked_add(bytes).ok_or(Error::IoError)?;
                Ok(GpuCompletionData::Capset(
                    pending.response[start..end].to_vec(),
                ))
            }
            _ => {
                let header =
                    CtrlHeader::read_from_prefix(&pending.response).ok_or(Error::IoError)?;
                header.check_fence(Command::OK_NODATA, pending.context, pending.fence_id)?;
                Ok(GpuCompletionData::None)
            }
        }
    }

    fn commit_control_operation(
        &mut self,
        operation: PendingControlOperation,
        data: &GpuCompletionData,
    ) -> Result {
        match operation {
            PendingControlOperation::Submit3d
            | PendingControlOperation::SetScanout
            | PendingControlOperation::Transfer2d(_)
            | PendingControlOperation::Flush(_)
            | PendingControlOperation::Transfer3d { .. } => Ok(()),
            PendingControlOperation::CreateResource(resource)
            | PendingControlOperation::CreateBlob(resource) => {
                self.resource_mut(resource)?.lifecycle = ResourceLifecycle::Live;
                Ok(())
            }
            PendingControlOperation::AttachBacking(resource) => {
                self.resource_mut(resource)?.backing = BackingState::Attached;
                Ok(())
            }
            PendingControlOperation::DetachBacking(resource) => {
                self.resource_mut(resource)?.backing = BackingState::Detached;
                Ok(())
            }
            PendingControlOperation::UnrefResource(resource) => {
                self.forget_resource(resource);
                Ok(())
            }
            PendingControlOperation::CreateContext(context) => {
                let index = self
                    .pending_contexts
                    .iter()
                    .position(|entry| entry.id == context)
                    .ok_or(Error::IoError)?;
                self.contexts.try_reserve(1).map_err(|_| Error::DmaError)?;
                self.contexts.push(self.pending_contexts.swap_remove(index));
                Ok(())
            }
            PendingControlOperation::DestroyUncertainContext(context) => {
                self.failed_contexts
                    .retain(|candidate| *candidate != context);
                Ok(())
            }
            PendingControlOperation::DestroyContext(context) => {
                let index = self
                    .contexts
                    .iter()
                    .position(|entry| entry.id == context)
                    .ok_or(Error::IoError)?;
                self.contexts.swap_remove(index);
                Ok(())
            }
            PendingControlOperation::AttachContextResource { context, resource } => {
                let entry = self
                    .contexts
                    .iter_mut()
                    .find(|entry| entry.id == context)
                    .ok_or(Error::IoError)?;
                entry.resources.push(resource);
                Ok(())
            }
            PendingControlOperation::DetachContextResource { context, resource } => {
                let entry = self
                    .contexts
                    .iter_mut()
                    .find(|entry| entry.id == context)
                    .ok_or(Error::IoError)?;
                entry.resources.retain(|candidate| *candidate != resource);
                Ok(())
            }
            PendingControlOperation::MapBlob(resource) => {
                if !matches!(data, GpuCompletionData::MapInfo { .. }) {
                    return Err(Error::IoError);
                }
                let entry = self.resource_mut(resource)?;
                entry.mapped = true;
                Ok(())
            }
            PendingControlOperation::UnmapBlob(resource) => {
                let entry = self.resource_mut(resource)?;
                entry.mapped = false;
                entry.map_offset = None;
                Ok(())
            }
            PendingControlOperation::AssignUuid(resource) => {
                let GpuCompletionData::Uuid(uuid) = data else {
                    return Err(Error::IoError);
                };
                self.resource_mut(resource)?.uuid = Some(*uuid);
                Ok(())
            }
            PendingControlOperation::CapsetInfo | PendingControlOperation::Capset { .. } => Ok(()),
        }
    }

    /// Preserve cleanup ownership after a failed context-create completion.
    /// The host may have created the context despite a lost/error response,
    /// so submit a fenced destroy using a distinct never-reused identity. If
    /// the cleanup request cannot be published, the control queue is reset
    /// rather than allowing later work to overlap an ambiguous context.
    fn abort_control_operation(&mut self, operation: PendingControlOperation) {
        match operation {
            PendingControlOperation::AttachBacking(resource) => {
                if let Ok(resource) = self.resource_mut(resource) {
                    resource.backing = BackingState::Uncertain;
                }
                return;
            }
            PendingControlOperation::UnrefResource(resource) => {
                if let Ok(resource) = self.resource_mut(resource) {
                    resource.lifecycle = ResourceLifecycle::UnrefUncertain;
                }
                return;
            }
            PendingControlOperation::CreateContext(context) => self.abort_created_context(context),
            PendingControlOperation::MapBlob(resource) => {
                if let Ok(resource) = self.resource_mut(resource) {
                    resource.map_offset = None;
                }
            }
            _ => return,
        }
    }

    fn abort_created_context(&mut self, context: ContextId) {
        self.pending_contexts.retain(|entry| entry.id != context);
        if self.failed_contexts.try_reserve(1).is_err() {
            self.fault_control_queue();
            return;
        }
        self.failed_contexts.push(context);
        let fence = match self.next_fence() {
            Ok(fence) => fence,
            Err(_) => {
                self.fault_control_queue();
                return;
            }
        };
        let request = CtxDestroy {
            header: CtrlHeader::fenced(Command::CTX_DESTROY, context.get(), fence),
        }
        .as_bytes()
        .to_vec();
        if self
            .enqueue_control_submission(
                fence,
                context.get(),
                PendingControlOperation::DestroyUncertainContext(context),
                request,
            )
            .is_err()
        {
            self.fault_control_queue();
        }
    }

    /// A malformed token or failed descriptor recycle invalidates controlq
    /// ownership. Reset ends host DMA, then keeps a terminal error record for
    /// every request until callers have consumed it.
    fn fault_control_queue(&mut self) {
        if self.control_faulted {
            return;
        }
        let mut records: [GpuCompletion; MAX_PENDING_CONTROL] =
            core::array::from_fn(|_| GpuCompletion {
                fence_id: 0,
                result: Ok(()),
                data: GpuCompletionData::None,
            });
        let count = self.reset_control(&mut records);
        self.terminal_control
            .extend(records.into_iter().take(count));
    }

    /// Fence every outstanding request with a terminal error after a device
    /// reset.  The caller supplies bounded storage for those records.  Queue
    /// descriptors are retained until transport reset makes device DMA
    /// impossible; this method intentionally marks the instance unusable
    /// rather than pretending it can resume with stale resource state.
    pub fn reset_control(&mut self, out: &mut [GpuCompletion]) -> usize {
        self.transport
            .set_status(crate::transport::DeviceStatus::empty());
        self.transport.mark_reset_complete();
        self.control_faulted = true;
        while !self.pending_control.is_empty() {
            let mut pending = self.pending_control.swap_remove(0);
            let inputs = [pending.request.as_ref()];
            let mut outputs = [pending.response.as_mut()];
            // SAFETY: transport reset completed before descriptor recycling,
            // so the device can no longer access these exact owners.
            unsafe {
                self.control_queue
                    .discard_quiesced(pending.token, &inputs, &mut outputs)
            };
            if !self.complete_present_stage(pending.fence_id, Err(Error::IoError)) {
                self.terminal_control.push(GpuCompletion {
                    fence_id: pending.fence_id,
                    result: Err(Error::IoError),
                    data: GpuCompletionData::None,
                });
            }
        }
        // A batch can be between stages when reset happens. It has no live
        // descriptor, but still owns its resource token and its public fence
        // until this terminal error is published.
        for batch in core::mem::take(&mut self.pending_presents) {
            self.terminal_control.push(GpuCompletion {
                fence_id: batch.fence_id,
                result: Err(Error::IoError),
                data: GpuCompletionData::None,
            });
        }
        let count = core::cmp::min(out.len(), self.terminal_control.len());
        for slot in out.iter_mut().take(count) {
            *slot = self.terminal_control.swap_remove(0);
        }
        count
    }

    fn resource(&self, id: ResourceId) -> Result<&Resource> {
        self.resources
            .iter()
            .find(|r| r.id == id)
            .ok_or(Error::InvalidParam)
    }
    fn resource_mut(&mut self, id: ResourceId) -> Result<&mut Resource> {
        self.resources
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or(Error::InvalidParam)
    }
    fn forget_resource(&mut self, id: ResourceId) {
        if let Some(index) = self.resources.iter().position(|resource| resource.id == id) {
            self.resources.swap_remove(index);
        }
    }
    /// Synchronously issue an immutable display/config query. Mutable GPU
    /// operations must use `submit_*` and `drain_control_completions`.
    fn request<Req: AsBytes, Rsp: FromBytes>(&mut self, req: Req) -> Result<Rsp> {
        self.request_bytes(req.as_bytes())
    }
    fn request_bytes<Rsp: FromBytes>(&mut self, request: &[u8]) -> Result<Rsp> {
        // This is reserved for immutable display/config reads and must never
        // consume a completion owned by an asynchronous submission.
        if self.control_faulted || !self.pending_control.is_empty() {
            return Err(Error::NotReady);
        }
        if request.len() > MAX_CONTROL_PAYLOAD
            || core::mem::size_of::<Rsp>() > self.queue_buf_recv.len()
        {
            return Err(Error::InvalidParam);
        }
        let written = if request.len() <= self.queue_buf_send.len() {
            self.queue_buf_send[..request.len()].copy_from_slice(request);
            self.control_queue.add_notify_wait_pop(
                &[&self.queue_buf_send[..request.len()]],
                &mut [&mut self.queue_buf_recv],
                &mut self.transport,
            )?
        } else {
            self.control_queue.add_notify_wait_pop(
                &[request],
                &mut [&mut self.queue_buf_recv],
                &mut self.transport,
            )?
        } as usize;
        if written < core::mem::size_of::<Rsp>() {
            return Err(Error::IoError);
        }
        Rsp::read_from_prefix(&self.queue_buf_recv).ok_or(Error::IoError)
    }

    /// Publish one cursorq command while retaining its exact DMA owner until
    /// the matching used token is drained.  cursorq never aliases controlq's
    /// request or completion ownership.
    fn submit_cursor<Req: AsBytes>(&mut self, request: Req) -> Result<GpuSubmission> {
        let request = request.as_bytes();
        if self.cursor_faulted {
            return Err(Error::NotReady);
        }
        if self.pending_cursor.len() == MAX_PENDING_CONTROL {
            return Err(Error::QueueFull);
        }
        if request.len() > MAX_CONTROL_PAYLOAD {
            return Err(Error::InvalidParam);
        }
        self.terminal_cursor
            .try_reserve(MAX_PENDING_CONTROL)
            .map_err(|_| Error::DmaError)?;
        let fence_id = self.next_fence()?;
        let mut pending = PendingCursor {
            token: 0,
            fence_id,
            request: request.into(),
        };
        let inputs = [pending.request.as_ref()];
        let mut outputs: [&mut [u8]; 0] = [];
        let token = unsafe { self.cursor_queue.add_unpublished(&inputs, &mut outputs) }?;
        pending.token = token;
        self.pending_cursor.insert(token, pending);
        self.cursor_queue.publish_unpublished(token);
        if self.cursor_queue.should_notify() {
            self.transport.notify(QUEUE_CURSOR);
        }
        Ok(GpuSubmission { fence_id })
    }

    pub fn drain_cursor_completions(&mut self, out: &mut [GpuCompletion]) -> Result<usize> {
        let mut completed = 0;
        while completed < out.len() && !self.terminal_cursor.is_empty() {
            out[completed] = self
                .terminal_cursor
                .pop_front()
                .expect("nonempty cursor terminal queue");
            completed += 1;
        }
        while completed < out.len() {
            let Some(token) = self.cursor_queue.peek_used() else {
                break;
            };
            let Some(pending) = self.pending_cursor.get(&token) else {
                self.fault_cursor_queue();
                return Err(Error::IoError);
            };
            let result = {
                let inputs = [pending.request.as_ref()];
                let mut outputs: [&mut [u8]; 0] = [];
                unsafe { self.cursor_queue.pop_used(token, &inputs, &mut outputs) }.map(|_| ())
            };
            let pending = self
                .pending_cursor
                .remove(&token)
                .expect("cursor token disappeared during drain");
            out[completed] = GpuCompletion {
                fence_id: pending.fence_id,
                result,
                data: GpuCompletionData::None,
            };
            completed += 1;
        }
        Ok(completed)
    }

    fn fault_cursor_queue(&mut self) {
        if self.cursor_faulted {
            return;
        }
        self.cursor_faulted = true;
        self.transport
            .set_status(crate::transport::DeviceStatus::empty());
        self.transport.mark_reset_complete();
        while let Some((_, pending)) = self.pending_cursor.pop_first() {
            let inputs = [pending.request.as_ref()];
            let mut outputs: [&mut [u8]; 0] = [];
            unsafe {
                self.cursor_queue
                    .discard_quiesced(pending.token, &inputs, &mut outputs)
            };
            self.terminal_cursor.push_back(GpuCompletion {
                fence_id: pending.fence_id,
                result: Err(Error::IoError),
                data: GpuCompletionData::None,
            });
        }
    }
    pub fn reset_cursor(&mut self, out: &mut [GpuCompletion]) -> usize {
        self.fault_cursor_queue();
        let count = core::cmp::min(out.len(), self.terminal_cursor.len());
        for slot in out.iter_mut().take(count) {
            *slot = self
                .terminal_cursor
                .pop_front()
                .expect("cursor reset terminal missing");
        }
        count
    }
}
impl<H: Hal, T: Transport> Drop for VirtIOGpu<H, T> {
    fn drop(&mut self) {
        if !self.pending_control.is_empty() || !self.pending_cursor.is_empty() {
            // Published descriptors may still be DMA-visible. Reset before
            // queue teardown; asynchronous resource cleanup is owned by the
            // caller before dropping the driver.
            let mut completions: [GpuCompletion; MAX_PENDING_CONTROL] =
                core::array::from_fn(|_| GpuCompletion {
                    fence_id: 0,
                    result: Ok(()),
                    data: GpuCompletionData::None,
                });
            let _ = self.reset_control(&mut completions);
            let _ = self.reset_cursor(&mut completions);
        }
        self.transport.queue_unset(QUEUE_TRANSMIT);
        self.transport.queue_unset(QUEUE_CURSOR);
    }
}
#[repr(C)]
struct Config {
    events_read: ReadOnly<u32>,
    events_clear: WriteOnly<u32>,
    _num_scanouts: Volatile<u32>,
}
pub const EVENT_DISPLAY: u32 = 1;
bitflags! { #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)] struct Features: u64 { const VIRGL = 1 << 0; const RESOURCE_UUID = 1 << 2; const RESOURCE_BLOB = 1 << 3; const CONTEXT_INIT = 1 << 4; const RING_INDIRECT_DESC = 1 << 28; const RING_EVENT_IDX = 1 << 29; } }
#[repr(transparent)]
#[derive(AsBytes, Clone, Copy, Debug, Eq, PartialEq, FromBytes, FromZeroes)]
struct Command(u32);
impl Command {
    const GET_DISPLAY_INFO: Self = Self(0x100);
    const RESOURCE_CREATE_2D: Self = Self(0x101);
    const RESOURCE_UNREF: Self = Self(0x102);
    const SET_SCANOUT: Self = Self(0x103);
    const RESOURCE_FLUSH: Self = Self(0x104);
    const TRANSFER_TO_HOST_2D: Self = Self(0x105);
    const RESOURCE_ATTACH_BACKING: Self = Self(0x106);
    const RESOURCE_DETACH_BACKING: Self = Self(0x107);
    const GET_CAPSET_INFO: Self = Self(0x108);
    const GET_CAPSET: Self = Self(0x109);
    const RESOURCE_ASSIGN_UUID: Self = Self(0x10b);
    const RESOURCE_CREATE_BLOB: Self = Self(0x10c);
    const SET_SCANOUT_BLOB: Self = Self(0x10d);
    const CTX_CREATE: Self = Self(0x200);
    const CTX_DESTROY: Self = Self(0x201);
    const CTX_ATTACH_RESOURCE: Self = Self(0x202);
    const CTX_DETACH_RESOURCE: Self = Self(0x203);
    const RESOURCE_CREATE_3D: Self = Self(0x204);
    const TRANSFER_TO_HOST_3D: Self = Self(0x205);
    const TRANSFER_FROM_HOST_3D: Self = Self(0x206);
    const SUBMIT_3D: Self = Self(0x207);
    const RESOURCE_MAP_BLOB: Self = Self(0x208);
    const RESOURCE_UNMAP_BLOB: Self = Self(0x209);
    const UPDATE_CURSOR: Self = Self(0x300);
    const MOVE_CURSOR: Self = Self(0x301);
    const OK_NODATA: Self = Self(0x1100);
    const OK_DISPLAY_INFO: Self = Self(0x1101);
    const OK_CAPSET_INFO: Self = Self(0x1102);
    const OK_CAPSET: Self = Self(0x1103);
    const OK_RESOURCE_UUID: Self = Self(0x1104);
    const OK_MAP_INFO: Self = Self(0x1105);
}
#[repr(C)]
#[derive(AsBytes, Debug, Clone, Copy, FromBytes, FromZeroes)]
struct CtrlHeader {
    hdr_type: Command,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    ring_idx: u8,
    _padding: [u8; 3],
}
impl CtrlHeader {
    fn with_type(hdr_type: Command) -> Self {
        Self {
            hdr_type,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            ring_idx: 0,
            _padding: [0; 3],
        }
    }
    fn fenced(hdr_type: Command, ctx_id: u32, fence_id: u64) -> Self {
        Self {
            hdr_type,
            flags: 1,
            fence_id,
            ctx_id,
            ring_idx: 0,
            _padding: [0; 3],
        }
    }
    fn fenced_ring(hdr_type: Command, ctx_id: u32, fence_id: u64, ring_idx: u32) -> Self {
        Self {
            hdr_type,
            // VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX.
            flags: 1 | 2,
            fence_id,
            ctx_id,
            ring_idx: u8::try_from(ring_idx).unwrap_or(0),
            _padding: [0; 3],
        }
    }
    fn check_type(&self, expected: Command) -> Result {
        if self.hdr_type == expected {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }
    fn check_fence(&self, expected: Command, ctx_id: u32, fence_id: u64) -> Result {
        if self.hdr_type == expected
            && self.flags & 1 != 0
            && self.ctx_id == ctx_id
            && self.fence_id == fence_id
        {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }
}
#[repr(C)]
#[derive(Debug, FromBytes, FromZeroes)]
struct RespDisplayInfo {
    header: CtrlHeader,
    pmodes: [DisplayOne; 16],
}
#[repr(C)]
#[derive(Debug, Copy, Clone, FromBytes, FromZeroes)]
struct DisplayOne {
    rect: Rect,
    enabled: u32,
    flags: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceCreate2D {
    header: CtrlHeader,
    resource_id: u32,
    format: Format,
    width: u32,
    height: u32,
}
#[repr(u32)]
#[derive(AsBytes)]
enum Format {
    B8G8R8A8UNORM = 1,
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceAttachBackingHeader {
    header: CtrlHeader,
    resource_id: u32,
    nr_entries: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct MemEntry {
    addr: u64,
    length: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceDetachBacking {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceUnref {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct SetScanout {
    header: CtrlHeader,
    rect: Rect,
    scanout_id: u32,
    resource_id: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct SetScanoutBlob {
    header: CtrlHeader,
    rect: Rect,
    scanout_id: u32,
    resource_id: u32,
    width: u32,
    height: u32,
    format: u32,
    _padding: u32,
    strides: [u32; 4],
    offsets: [u32; 4],
}
#[repr(C)]
#[derive(AsBytes)]
struct TransferToHost2D {
    header: CtrlHeader,
    rect: Rect,
    offset: u64,
    resource_id: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceFlush {
    header: CtrlHeader,
    rect: Rect,
    resource_id: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct CursorPos {
    scanout_id: u32,
    x: i32,
    y: i32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct UpdateCursor {
    header: CtrlHeader,
    pos: CursorPos,
    resource_id: u32,
    hot_x: u32,
    hot_y: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct MoveCursor {
    header: CtrlHeader,
    pos: CursorPos,
}
#[repr(C)]
#[derive(AsBytes)]
struct GetCapsetInfo {
    header: CtrlHeader,
    index: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(FromBytes, FromZeroes)]
struct RespCapsetInfo {
    header: CtrlHeader,
    capset_id: u32,
    capset_max_version: u32,
    capset_max_size: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct GetCapset {
    header: CtrlHeader,
    capset_id: u32,
    capset_version: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct CtxCreate {
    header: CtrlHeader,
    nlen: u32,
    context_init: u32,
    debug_name: [u8; 64],
}
#[repr(C)]
#[derive(AsBytes)]
struct CtxDestroy {
    header: CtrlHeader,
}
#[repr(C)]
#[derive(AsBytes)]
struct CtxResource {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceCreate3D {
    header: CtrlHeader,
    resource_id: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct Box3D {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct TransferHost3D {
    header: CtrlHeader,
    box_: Box3D,
    offset: u64,
    resource_id: u32,
    level: u32,
    stride: u32,
    layer_stride: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct CmdSubmit {
    header: CtrlHeader,
    size: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceCreateBlob {
    header: CtrlHeader,
    resource_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    nr_entries: u32,
    blob_id: u64,
    size: u64,
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceAssignUuid {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(FromBytes, FromZeroes)]
struct RespResourceUuid {
    header: CtrlHeader,
    uuid: [u8; 16],
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceMapBlob {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
    offset: u64,
}
#[repr(C)]
#[derive(FromBytes, FromZeroes)]
struct RespMapBlob {
    header: CtrlHeader,
    map_info: u32,
    _padding: u32,
}
#[repr(C)]
#[derive(AsBytes)]
struct ResourceUnmapBlob {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
}
const QUEUE_TRANSMIT: u16 = 0;
const QUEUE_CURSOR: u16 = 1;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rects_are_bounded() {
        assert!(Rect::new(1, 2, 3, 4).fits(4, 6));
        assert!(!Rect::new(4, 0, 1, 1).fits(4, 1));
    }
    #[test]
    fn virgl_feature_and_wire_layout_match_uapi() {
        assert_eq!(Features::VIRGL.bits(), 1);
        assert_eq!(core::mem::size_of::<CtrlHeader>(), 24);
        assert_eq!(core::mem::size_of::<CtxCreate>(), 96);
        assert_eq!(core::mem::size_of::<ResourceCreate3D>(), 72);
        assert_eq!(core::mem::size_of::<TransferHost3D>(), 72);
        assert_eq!(core::mem::size_of::<CmdSubmit>(), 32);
        let header = CtrlHeader::fenced(Command::SUBMIT_3D, 7, 9);
        assert!(header.check_fence(Command::SUBMIT_3D, 7, 9).is_ok());
    }
    #[test]
    fn fake_transport_negotiates_virgl_only_when_offered() {
        use alloc::{boxed::Box, sync::Arc, vec};
        use core::ptr::NonNull;
        use std::sync::Mutex;

        use crate::transport::{
            fake::{FakeTransport, QueueStatus, State},
            DeviceType, Transport,
        };
        let config = unsafe {
            NonNull::new_unchecked(Box::into_raw(Box::new(core::mem::zeroed::<Config>())))
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..State::default()
        }));
        let mut transport = FakeTransport {
            device_type: DeviceType::GPU,
            max_queue_size: 2,
            device_features: Features::VIRGL.bits(),
            config_space: config,
            state: state.clone(),
        };
        let features = transport.begin_init(SUPPORTED_FEATURES);
        assert!(features.contains(Features::VIRGL));
        assert_eq!(
            state.lock().unwrap().driver_features & Features::VIRGL.bits(),
            Features::VIRGL.bits()
        );
        unsafe {
            drop(Box::from_raw(config.as_ptr()));
        }
    }
}
