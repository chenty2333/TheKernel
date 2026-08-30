//! VirtIO GPU 2D protocol driver.
//!
//! This layer owns its queues and command buffers.  DMA returned by
//! `allocate_backing` is driver-owned; physical backing passed to
//! `attach_backing` remains pinned and owned by the caller until detached.

#![allow(missing_docs)] // The protocol fields mirror the VirtIO wire format.

use alloc::{boxed::Box, vec::Vec};

use bitflags::bitflags;
use log::error;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::{
    Error, PAGE_SIZE, Result,
    hal::{BufferDirection, Dma, Hal},
    pages,
    queue::VirtQueue,
    transport::Transport,
    volatile::{ReadOnly, Volatile, WriteOnly, volread, volwrite},
};

const QUEUE_SIZE: u16 = 2;
const MAX_RESOURCES: usize = 128;
const MAX_CONTEXTS: usize = 32;
const MAX_CONTEXT_RESOURCES: usize = 128;
const MAX_SG_ENTRIES: usize = 8192;
/// Bound untrusted host/client control payload allocation while allowing virgl
/// capsets and command streams far larger than a page.
const MAX_CONTROL_PAYLOAD: usize = 1024 * 1024;
const SUPPORTED_FEATURES: Features = Features::RING_EVENT_IDX
    .union(Features::RING_INDIRECT_DESC)
    .union(Features::VIRGL);

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
}

/// Local knowledge of a host backing attachment.
///
/// A failed control completion is not proof that the device ignored the
/// request.  Keep that ambiguity explicit until a successful detach proves
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

struct Context {
    id: ContextId,
    resources: Vec<ResourceId>,
}

/// A synchronous VirtIO GPU 2D device. Each control command waits for and
/// validates its completion before the resource state is changed.
pub struct VirtIOGpu<H: Hal, T: Transport> {
    transport: T,
    control_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    cursor_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    queue_buf_send: Box<[u8]>,
    queue_buf_recv: Box<[u8]>,
    resources: Vec<Resource>,
    next_resource_id: u32,
    frame_buffer: Option<(ResourceId, Dma<H>)>,
    frame_rect: Option<Rect>,
    frame_scanout: Option<u32>,
    virgl: bool,
    contexts: Vec<Context>,
    next_context_id: u32,
    next_fence_id: u64,
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
        transport.finish_init();
        Ok(Self {
            transport,
            control_queue,
            cursor_queue,
            queue_buf_send: FromZeroes::new_box_slice_zeroed(PAGE_SIZE),
            queue_buf_recv: FromZeroes::new_box_slice_zeroed(PAGE_SIZE),
            resources: Vec::new(),
            next_resource_id: 1,
            frame_buffer: None,
            frame_rect: None,
            frame_scanout: None,
            virgl: features.contains(Features::VIRGL),
            contexts: Vec::new(),
            next_context_id: 1,
            next_fence_id: 1,
        })
    }
    pub const fn virgl_supported(&self) -> bool {
        self.virgl
    }

    fn require_virgl(&self) -> Result {
        self.virgl.then_some(()).ok_or(Error::Unsupported)
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
    /// Return and acknowledge config events. `EVENT_DISPLAY` requires querying display_info again.
    pub fn config_events(&mut self) -> Result<u32> {
        let cfg = self.transport.config_space::<Config>()?;
        let events = unsafe { volread!(cfg, events_read) };
        if events != 0 {
            unsafe { volwrite!(cfg, events_clear, events) };
        }
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

    /// Create an unbacked B8G8R8A8 resource; IDs are dynamically allocated.
    pub fn create_2d(&mut self, width: u32, height: u32) -> Result<ResourceId> {
        self.retry_uncertain_resources();
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
        let id = ResourceId(self.next_resource_id);
        self.next_resource_id = self
            .next_resource_id
            .checked_add(1)
            .ok_or(Error::DmaError)?;
        // Reserve before the wire command.  If allocation fails there is no
        // host resource to leak and no cleanup command whose own failure
        // would leave the two sides out of sync.
        self.resources.try_reserve(1).map_err(|_| Error::DmaError)?;
        let pending = Resource {
            id,
            width,
            height,
            backing: BackingState::Detached,
            lifecycle: ResourceLifecycle::CreateUncertain,
            backing_bytes: Some(u64::from(width) * u64::from(height) * 4),
        };
        // Keep the ID before sending CREATE. An invalid/lost completion does
        // not prove the host did not allocate it.
        self.resources.push(pending);
        let rsp: CtrlHeader = match self.request(ResourceCreate2D {
            header: CtrlHeader::with_type(Command::RESOURCE_CREATE_2D),
            resource_id: id.0,
            format: Format::B8G8R8A8UNORM,
            width,
            height,
        }) {
            Ok(rsp) => rsp,
            Err(error) => return Err(error),
        };
        if let Err(error) = rsp.check_type(Command::OK_NODATA) {
            return Err(error);
        }
        self.resource_mut(id)?.lifecycle = ResourceLifecycle::Live;
        Ok(id)
    }
    /// Allocate contiguous driver-owned backing sized for a 32-bit ARGB resource.
    pub fn allocate_backing(&self, id: ResourceId) -> Result<Dma<H>> {
        let r = self.resource(id)?;
        if r.lifecycle != ResourceLifecycle::Live {
            return Err(Error::NotReady);
        }
        let bytes = r
            .width
            .checked_mul(r.height)
            .and_then(|n| n.checked_mul(4))
            .ok_or(Error::InvalidParam)?;
        Dma::new(pages(bytes as usize), BufferDirection::DriverToDevice)
    }
    /// Attach caller-owned pinned physical backing. It must remain valid until detach completes.
    pub fn attach_backing(&mut self, id: ResourceId, paddr: u64, length: u32) -> Result {
        self.attach_backing_entries(id, &[(paddr, length)])
    }
    /// Attach caller-owned pinned physical backing. Entries remain valid until detach completes.
    pub fn attach_backing_entries(&mut self, id: ResourceId, entries: &[(u64, u32)]) -> Result {
        let resource = self.resource(id)?;
        let required = resource.backing_bytes;
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
            || required.is_some_and(|bytes| total < bytes)
            || resource.backing != BackingState::Detached
            || resource.lifecycle != ResourceLifecycle::Live
        {
            return Err(Error::InvalidParam);
        }
        let header = ResourceAttachBackingHeader {
            header: CtrlHeader::with_type(Command::RESOURCE_ATTACH_BACKING),
            resource_id: id.0,
            nr_entries: u32::try_from(entries.len()).map_err(|_| Error::InvalidParam)?,
        };
        let mut request = Vec::new();
        request
            .try_reserve_exact(
                core::mem::size_of::<ResourceAttachBackingHeader>()
                    .checked_add(
                        entries
                            .len()
                            .checked_mul(core::mem::size_of::<MemEntry>())
                            .ok_or(Error::InvalidParam)?,
                    )
                    .ok_or(Error::InvalidParam)?,
            )
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
        // Once the request is submitted, any error may mean the host consumed
        // it but the guest did not receive (or validate) the completion.
        // Preserve that DMA hazard until DETACH_BACKING completes.
        let rsp: CtrlHeader = match self.request_bytes(&request) {
            Ok(rsp) => rsp,
            Err(error) => {
                self.resource_mut(id)?.backing = BackingState::Uncertain;
                return Err(error);
            }
        };
        if let Err(error) = rsp.check_type(Command::OK_NODATA) {
            self.resource_mut(id)?.backing = BackingState::Uncertain;
            return Err(error);
        }
        self.resource_mut(id)?.backing = BackingState::Attached;
        Ok(())
    }
    pub fn detach_backing(&mut self, id: ResourceId) -> Result {
        if self.resource(id)?.backing == BackingState::Detached {
            return Err(Error::InvalidParam);
        }
        let rsp: CtrlHeader = self.request(ResourceDetachBacking {
            header: CtrlHeader::with_type(Command::RESOURCE_DETACH_BACKING),
            resource_id: id.0,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)?;
        self.resource_mut(id)?.backing = BackingState::Detached;
        Ok(())
    }
    /// Unreference only detached resources, preventing DMA use-after-free.
    pub fn unref(&mut self, id: ResourceId) -> Result {
        let lifecycle = self.resource(id)?.lifecycle;
        if self.resource(id)?.backing != BackingState::Detached
            || self.contexts.iter().any(|c| c.resources.contains(&id))
        {
            return Err(Error::NotReady);
        }
        let response: Result<CtrlHeader> = self.request(ResourceUnref {
            header: CtrlHeader::with_type(Command::RESOURCE_UNREF),
            resource_id: id.0,
            _padding: 0,
        });
        let result = response.and_then(|rsp| rsp.check_type(Command::OK_NODATA));
        match result {
            Ok(()) => self.forget_resource(id),
            Err(error) if lifecycle == ResourceLifecycle::UnrefUncertain => {
                // The first unref may already have reached the host.  A
                // second bounded attempt that reports unknown/bad completion
                // cannot distinguish the two cases; retire the local token
                // instead of pinning one of the 128 slots forever. This can
                // leak only host metadata: backing was proven detached.
                error!(
                    "virtio-gpu unref completion remained uncertain for resource {}; retiring \
                     local token: {:?}",
                    id.get(),
                    error
                );
                self.forget_resource(id);
            }
            Err(error) => {
                self.resource_mut(id)?.lifecycle = ResourceLifecycle::UnrefUncertain;
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn set_scanout(
        &mut self,
        rect: Rect,
        scanout: u32,
        resource: Option<ResourceId>,
    ) -> Result {
        if scanout >= 16 {
            return Err(Error::InvalidParam);
        }
        if let Some(id) = resource {
            let r = self.resource(id)?;
            if r.lifecycle != ResourceLifecycle::Live
                || r.backing != BackingState::Attached
                || !rect.fits(r.width, r.height)
            {
                return Err(Error::InvalidParam);
            }
        }
        let rsp: CtrlHeader = self.request(SetScanout {
            header: CtrlHeader::with_type(Command::SET_SCANOUT),
            rect,
            scanout_id: scanout,
            resource_id: resource.map_or(0, ResourceId::get),
        })?;
        rsp.check_type(Command::OK_NODATA)
    }
    pub fn transfer_to_host(&mut self, id: ResourceId, rect: Rect) -> Result {
        let r = self.resource(id)?;
        if r.lifecycle != ResourceLifecycle::Live
            || r.backing != BackingState::Attached
            || !rect.fits(r.width, r.height)
        {
            return Err(Error::InvalidParam);
        }
        let rsp: CtrlHeader = self.request(TransferToHost2D {
            header: CtrlHeader::with_type(Command::TRANSFER_TO_HOST_2D),
            rect,
            offset: 0,
            resource_id: id.0,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }
    pub fn resource_flush(&mut self, id: ResourceId, rect: Rect) -> Result {
        let r = self.resource(id)?;
        if r.lifecycle != ResourceLifecycle::Live || !rect.fits(r.width, r.height) {
            return Err(Error::InvalidParam);
        }
        let rsp: CtrlHeader = self.request(ResourceFlush {
            header: CtrlHeader::with_type(Command::RESOURCE_FLUSH),
            rect,
            resource_id: id.0,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    pub fn capset_info(&mut self, index: u32) -> Result<CapsetInfo> {
        self.require_virgl()?;
        let fence = self.next_fence()?;
        let rsp: RespCapsetInfo = self.request(GetCapsetInfo {
            header: CtrlHeader::fenced(Command::GET_CAPSET_INFO, 0, fence),
            index,
            _padding: 0,
        })?;
        rsp.header.check_fence(Command::OK_CAPSET_INFO, 0, fence)?;
        Ok(CapsetInfo {
            id: rsp.capset_id,
            max_version: rsp.capset_max_version,
            max_size: rsp.capset_max_size,
        })
    }
    pub fn capset(&mut self, id: u32, version: u32, data: &mut [u8]) -> Result<usize> {
        self.require_virgl()?;
        let response_len = core::mem::size_of::<CtrlHeader>()
            .checked_add(data.len())
            .ok_or(Error::InvalidParam)?;
        if response_len > MAX_CONTROL_PAYLOAD {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        let req = GetCapset {
            header: CtrlHeader::fenced(Command::GET_CAPSET, 0, fence),
            capset_id: id,
            capset_version: version,
        };
        let mut response = Vec::new();
        response
            .try_reserve_exact(response_len)
            .map_err(|_| Error::DmaError)?;
        response.resize(response_len, 0);
        let written = self.request_into(req.as_bytes(), &mut response)? as usize;
        if written < response_len {
            return Err(Error::IoError);
        }
        let header = CtrlHeader::read_from_prefix(&response).ok_or(Error::IoError)?;
        header.check_fence(Command::OK_CAPSET, 0, fence)?;
        data.copy_from_slice(&response[core::mem::size_of::<CtrlHeader>()..]);
        Ok(data.len())
    }
    pub fn create_context(&mut self, name: &[u8]) -> Result<ContextId> {
        self.require_virgl()?;
        if name.len() > 64 || self.contexts.len() == MAX_CONTEXTS {
            return Err(Error::InvalidParam);
        }
        let id = ContextId(self.next_context_id);
        self.next_context_id = self.next_context_id.checked_add(1).ok_or(Error::DmaError)?;
        self.contexts.try_reserve(1).map_err(|_| Error::DmaError)?;
        let fence = self.next_fence()?;
        let mut req = CtxCreate {
            header: CtrlHeader::fenced(Command::CTX_CREATE, id.0, fence),
            nlen: name.len() as u32,
            context_init: 0,
            debug_name: [0; 64],
        };
        req.debug_name[..name.len()].copy_from_slice(name);
        let rsp: CtrlHeader = self.request(req)?;
        rsp.check_fence(Command::OK_NODATA, id.0, fence)?;
        self.contexts.push(Context {
            id,
            resources: Vec::new(),
        });
        Ok(id)
    }
    pub fn destroy_context(&mut self, id: u32) -> Result {
        let index = self
            .contexts
            .iter()
            .position(|c| c.id.0 == id)
            .ok_or(Error::InvalidParam)?;
        let fence = self.next_fence()?;
        let rsp: CtrlHeader = self.request(CtxDestroy {
            header: CtrlHeader::fenced(Command::CTX_DESTROY, id, fence),
        })?;
        rsp.check_fence(Command::OK_NODATA, id, fence)?;
        self.contexts.remove(index);
        Ok(())
    }
    pub fn context_attach_resource(&mut self, context: u32, resource: u32) -> Result {
        let id = ResourceId(resource);
        if self.resource(id)?.lifecycle != ResourceLifecycle::Live {
            return Err(Error::NotReady);
        }
        let c = self
            .contexts
            .iter()
            .find(|c| c.id.0 == context)
            .ok_or(Error::InvalidParam)?;
        if c.resources.len() == MAX_CONTEXT_RESOURCES || c.resources.contains(&id) {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        let rsp: CtrlHeader = self.request(CtxResource {
            header: CtrlHeader::fenced(Command::CTX_ATTACH_RESOURCE, context, fence),
            resource_id: resource,
            _padding: 0,
        })?;
        rsp.check_fence(Command::OK_NODATA, context, fence)?;
        self.contexts
            .iter_mut()
            .find(|c| c.id.0 == context)
            .ok_or(Error::IoError)?
            .resources
            .push(id);
        Ok(())
    }
    pub fn context_detach_resource(&mut self, context: u32, resource: u32) -> Result {
        let id = ResourceId(resource);
        let c = self
            .contexts
            .iter()
            .find(|c| c.id.0 == context)
            .ok_or(Error::InvalidParam)?;
        if !c.resources.contains(&id) {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        let rsp: CtrlHeader = self.request(CtxResource {
            header: CtrlHeader::fenced(Command::CTX_DETACH_RESOURCE, context, fence),
            resource_id: resource,
            _padding: 0,
        })?;
        rsp.check_fence(Command::OK_NODATA, context, fence)?;
        let c = self
            .contexts
            .iter_mut()
            .find(|c| c.id.0 == context)
            .ok_or(Error::IoError)?;
        c.resources.retain(|r| *r != id);
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn create_3d(
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
    ) -> Result<ResourceId> {
        self.require_virgl()?;
        self.retry_uncertain_resources();
        if width == 0
            || height == 0
            || depth == 0
            || array_size == 0
            || self.resources.len() == MAX_RESOURCES
        {
            return Err(Error::InvalidParam);
        }
        let id = ResourceId(self.next_resource_id);
        self.next_resource_id = self
            .next_resource_id
            .checked_add(1)
            .ok_or(Error::DmaError)?;
        self.resources.try_reserve(1).map_err(|_| Error::DmaError)?;
        let fence = self.next_fence()?;
        self.resources.push(Resource {
            id,
            width,
            height,
            backing: BackingState::Detached,
            lifecycle: ResourceLifecycle::CreateUncertain,
            backing_bytes: None,
        });
        let rsp: CtrlHeader = match self.request(ResourceCreate3D {
            header: CtrlHeader::fenced(Command::RESOURCE_CREATE_3D, 0, fence),
            resource_id: id.0,
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
        }) {
            Ok(rsp) => rsp,
            Err(error) => return Err(error),
        };
        if let Err(error) = rsp.check_fence(Command::OK_NODATA, 0, fence) {
            return Err(error);
        }
        self.resource_mut(id)?.lifecycle = ResourceLifecycle::Live;
        Ok(id)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn transfer_3d(
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
    ) -> Result {
        self.require_virgl()?;
        if self.resource(ResourceId(resource))?.lifecycle != ResourceLifecycle::Live {
            return Err(Error::NotReady);
        }
        if width == 0
            || height == 0
            || depth == 0
            || !self
                .contexts
                .iter()
                .any(|c| c.id.0 == context && c.resources.contains(&ResourceId(resource)))
        {
            return Err(Error::InvalidParam);
        }
        let fence = self.next_fence()?;
        let command = if to_host {
            Command::TRANSFER_TO_HOST_3D
        } else {
            Command::TRANSFER_FROM_HOST_3D
        };
        let rsp: CtrlHeader = self.request(TransferHost3D {
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
            resource_id: resource,
            level,
            stride,
            layer_stride,
        })?;
        rsp.check_fence(Command::OK_NODATA, context, fence)
    }
    pub fn submit_3d(&mut self, context: u32, commands: &[u8], resources: &[u32]) -> Result {
        self.require_virgl()?;
        if commands.is_empty() || !self.contexts.iter().any(|c| c.id.0 == context) {
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
            header: CtrlHeader::fenced(Command::SUBMIT_3D, context, fence),
            size: u32::try_from(commands.len()).map_err(|_| Error::InvalidParam)?,
            _padding: 0,
        };
        let mut request = Vec::new();
        request
            .try_reserve_exact(bytes)
            .map_err(|_| Error::DmaError)?;
        request.extend_from_slice(header.as_bytes());
        request.extend_from_slice(commands);
        let rsp: CtrlHeader = self.request_bytes(&request)?;
        rsp.check_fence(Command::OK_NODATA, context, fence)
    }

    /// Existing framebuffer interface implemented through the dynamic API.
    pub fn setup_framebuffer(&mut self) -> Result<&mut [u8]> {
        if self.frame_buffer.is_some() {
            return Err(Error::AlreadyUsed);
        }
        let (scanout, mode) = self
            .display_info()?
            .first_enabled()
            .ok_or(Error::NotReady)?;
        let id = self.create_2d(mode.rect.width, mode.rect.height)?;
        let dma = match self.allocate_backing(id) {
            Ok(dma) => dma,
            Err(e) => {
                if let Err(cleanup) = self.unref(id) {
                    error!(
                        "virtio-gpu failed to unref unbacked framebuffer: {:?}",
                        cleanup
                    );
                }
                return Err(e);
            }
        };
        let bytes = mode
            .rect
            .width
            .checked_mul(mode.rect.height)
            .and_then(|n| n.checked_mul(4))
            .ok_or(Error::InvalidParam)?;
        if let Err(e) = self.attach_backing(id, dma.paddr() as u64, bytes) {
            // An attach completion error can be ambiguous.  This helper
            // detaches first and deliberately leaks `dma` if that proof of
            // quiescence cannot be obtained.
            self.release_owned_backing(id, dma);
            return Err(e);
        }
        if let Err(e) = self.set_scanout(mode.rect, scanout, Some(id)) {
            self.release_owned_backing(id, dma);
            return Err(e);
        }
        self.frame_rect = Some(mode.rect);
        self.frame_scanout = Some(scanout);
        self.frame_buffer = Some((id, dma));
        let (_, dma) = self.frame_buffer.as_ref().ok_or(Error::NotReady)?;
        Ok(unsafe { dma.raw_slice().as_mut() })
    }
    pub fn flush(&mut self) -> Result {
        let id = self.frame_buffer.as_ref().ok_or(Error::NotReady)?.0;
        let rect = self.frame_rect.ok_or(Error::NotReady)?;
        self.transfer_to_host(id, rect)?;
        self.resource_flush(id, rect)
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
    /// Complete at most two UNREF attempts for every orphaned create/unref
    /// token before allocating a new resource.  The resource limit is 128, so
    /// this is bounded and cannot turn one bad completion into an unbounded
    /// wait loop.
    fn retry_uncertain_resources(&mut self) {
        let mut index = 0;
        while index < self.resources.len() {
            let (id, lifecycle) = {
                let resource = &self.resources[index];
                (resource.id, resource.lifecycle)
            };
            if lifecycle == ResourceLifecycle::Live {
                index += 1;
                continue;
            }
            let _ = self.unref(id);
            if self.resource(id).is_ok() {
                // The first attempt changed CreateUncertain to
                // UnrefUncertain. A second completion, including UNKNOWN_ID,
                // retires the local token conservatively.
                let _ = self.unref(id);
            }
            if self.resource(id).is_ok() {
                index += 1;
            }
        }
    }
    /// Releases owned backing only after a successful detach. On a completion
    /// failure the backing is intentionally leaked rather than exposed to DMA
    /// use-after-free; the transport reset in Drop owns the device afterward.
    fn release_owned_backing(&mut self, id: ResourceId, dma: Dma<H>) {
        match self.detach_backing(id) {
            Ok(()) => {
                if let Err(err) = self.unref(id) {
                    error!("virtio-gpu failed to unref detached framebuffer: {:?}", err);
                }
                // `dma` drops only after detach completion has been validated.
            }
            Err(err) => {
                error!(
                    "virtio-gpu failed to detach framebuffer; retaining DMA: {:?}",
                    err
                );
                core::mem::forget(dma);
            }
        }
    }
    fn request<Req: AsBytes, Rsp: FromBytes>(&mut self, req: Req) -> Result<Rsp> {
        self.request_bytes(req.as_bytes())
    }
    fn request_bytes<Rsp: FromBytes>(&mut self, request: &[u8]) -> Result<Rsp> {
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
    fn request_into(&mut self, request: &[u8], response: &mut [u8]) -> Result<u32> {
        if request.is_empty() || response.is_empty() {
            return Err(Error::InvalidParam);
        }
        if request.len() <= self.queue_buf_send.len() {
            self.queue_buf_send[..request.len()].copy_from_slice(request);
            self.control_queue.add_notify_wait_pop(
                &[&self.queue_buf_send[..request.len()]],
                &mut [response],
                &mut self.transport,
            )
        } else {
            // One dynamically allocated request plus one response still fits
            // the two-entry control queue without requiring indirect descs.
            // Both slices live through `pop_used`.
            self.control_queue
                .add_notify_wait_pop(&[request], &mut [response], &mut self.transport)
        }
    }
}
impl<H: Hal, T: Transport> Drop for VirtIOGpu<H, T> {
    fn drop(&mut self) {
        // Contexts are stopped before queue teardown.  A failed detach/destroy
        // deliberately leaves ownership intact; callers retaining backing then
        // cannot be raced by host DMA through a live context.
        while let Some(context) = self.contexts.last().map(|c| c.id.0) {
            let resources = self
                .contexts
                .last()
                .map(|c| c.resources.clone())
                .unwrap_or_default();
            let mut detached = true;
            for resource in resources {
                if let Err(err) = self.context_detach_resource(context, resource.0) {
                    error!("virtio-gpu failed to detach context resource: {:?}", err);
                    detached = false;
                    break;
                }
            }
            if !detached {
                break;
            }
            if let Err(err) = self.destroy_context(context) {
                error!("virtio-gpu failed to destroy context: {:?}", err);
                break;
            }
        }
        // The compatibility framebuffer owns DMA. A failed detach leaves the
        // host entitled to access it, so leaking is the only safe bounded
        // failure: freeing it would turn a device fault into a host DMA UAF.
        if let Some((id, dma)) = self.frame_buffer.take() {
            let scanout = self.frame_scanout.unwrap_or(0);
            if let Err(err) = self.set_scanout(Rect::default(), scanout, None) {
                error!(
                    "virtio-gpu failed to disable framebuffer scanout: {:?}",
                    err
                );
            }
            self.release_owned_backing(id, dma);
        }
        // Retain the cursor queue until it has been unset as well.
        let _ = &self.cursor_queue;
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
bitflags! { #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)] struct Features: u64 { const VIRGL = 1 << 0; const RING_INDIRECT_DESC = 1 << 28; const RING_EVENT_IDX = 1 << 29; } }
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
    const CTX_CREATE: Self = Self(0x200);
    const CTX_DESTROY: Self = Self(0x201);
    const CTX_ATTACH_RESOURCE: Self = Self(0x202);
    const CTX_DETACH_RESOURCE: Self = Self(0x203);
    const RESOURCE_CREATE_3D: Self = Self(0x204);
    const TRANSFER_TO_HOST_3D: Self = Self(0x205);
    const TRANSFER_FROM_HOST_3D: Self = Self(0x206);
    const SUBMIT_3D: Self = Self(0x207);
    const OK_NODATA: Self = Self(0x1100);
    const OK_DISPLAY_INFO: Self = Self(0x1101);
    const OK_CAPSET_INFO: Self = Self(0x1102);
    const OK_CAPSET: Self = Self(0x1103);
}
#[repr(C)]
#[derive(AsBytes, Debug, Clone, Copy, FromBytes, FromZeroes)]
struct CtrlHeader {
    hdr_type: Command,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    _padding: u32,
}
impl CtrlHeader {
    fn with_type(hdr_type: Command) -> Self {
        Self {
            hdr_type,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            _padding: 0,
        }
    }
    fn fenced(hdr_type: Command, ctx_id: u32, fence_id: u64) -> Self {
        Self {
            hdr_type,
            flags: 1,
            fence_id,
            ctx_id,
            _padding: 0,
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
            DeviceType, Transport,
            fake::{FakeTransport, QueueStatus, State},
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

    #[test]
    fn fake_transport_accepts_large_sg_and_capset_payloads() {
        use alloc::{boxed::Box, sync::Arc, vec};
        use core::ptr::NonNull;
        use std::{sync::Mutex, thread};

        use crate::{
            hal::fake::FakeHal,
            transport::{
                DeviceType,
                fake::{FakeTransport, QueueStatus, State},
            },
        };

        let config = unsafe {
            NonNull::new_unchecked(Box::into_raw(Box::new(core::mem::zeroed::<Config>())))
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..State::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::GPU,
            max_queue_size: 2,
            device_features: Features::VIRGL.bits(),
            config_space: config,
            state: state.clone(),
        };
        let worker_state = state.clone();
        let worker = thread::spawn(move || {
            for _ in 0..3 {
                State::wait_until_queue_notified(&worker_state, QUEUE_TRANSMIT);
                worker_state
                    .lock()
                    .unwrap()
                    .read_write_queue::<2>(QUEUE_TRANSMIT, |input| {
                        let mut hdr = CtrlHeader::read_from_prefix(&input).unwrap();
                        hdr.hdr_type = if hdr.hdr_type == Command::GET_CAPSET {
                            Command::OK_CAPSET
                        } else {
                            Command::OK_NODATA
                        };
                        if hdr.hdr_type == Command::OK_CAPSET {
                            let mut response = vec![0x5a; 24 + 5000];
                            response[..24].copy_from_slice(hdr.as_bytes());
                            response
                        } else {
                            hdr.as_bytes().to_vec()
                        }
                    });
            }
        });
        let mut gpu = VirtIOGpu::<FakeHal, _>::new(transport).unwrap();
        let resource = gpu.create_2d(1024, 768).unwrap();
        let entries = vec![(0x1000, 4096); 768];
        gpu.attach_backing_entries(resource, &entries).unwrap();
        let mut capset = vec![0; 5000];
        assert_eq!(gpu.capset(1, 1, &mut capset).unwrap(), capset.len());
        assert!(capset.iter().all(|value| *value == 0x5a));
        drop(gpu);
        worker.join().unwrap();
        unsafe {
            drop(Box::from_raw(config.as_ptr()));
        }
    }

    #[test]
    fn failed_detach_retains_resource_for_safe_retry() {
        use alloc::{boxed::Box, sync::Arc, vec};
        use core::ptr::NonNull;
        use std::{sync::Mutex, thread};

        use crate::{
            hal::fake::FakeHal,
            transport::{
                DeviceType,
                fake::{FakeTransport, QueueStatus, State},
            },
        };
        let config = unsafe {
            NonNull::new_unchecked(Box::into_raw(Box::new(core::mem::zeroed::<Config>())))
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..State::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::GPU,
            max_queue_size: 2,
            device_features: 0,
            config_space: config,
            state: state.clone(),
        };
        let worker_state = state.clone();
        let worker = thread::spawn(move || {
            for _ in 0..3 {
                State::wait_until_queue_notified(&worker_state, QUEUE_TRANSMIT);
                worker_state
                    .lock()
                    .unwrap()
                    .read_write_queue::<2>(QUEUE_TRANSMIT, |input| {
                        let header = CtrlHeader::read_from_prefix(&input).unwrap();
                        CtrlHeader::with_type(
                            if header.hdr_type == Command::RESOURCE_DETACH_BACKING {
                                Command(0x1200)
                            } else {
                                Command::OK_NODATA
                            },
                        )
                        .as_bytes()
                        .to_vec()
                    });
            }
        });
        let mut gpu = VirtIOGpu::<FakeHal, _>::new(transport).unwrap();
        let resource = gpu.create_2d(1, 1).unwrap();
        gpu.attach_backing(resource, 0x1000, 4096).unwrap();
        assert_eq!(gpu.detach_backing(resource), Err(Error::IoError));
        // A failed detach does not clear local attachment state and unref
        // therefore cannot free memory that the host may still DMA.
        assert_eq!(gpu.unref(resource), Err(Error::NotReady));
        drop(gpu);
        worker.join().unwrap();
        unsafe {
            drop(Box::from_raw(config.as_ptr()));
        }
    }

    #[test]
    fn failed_attach_is_uncertain_until_detach_completes() {
        use alloc::{boxed::Box, sync::Arc, vec};
        use core::ptr::NonNull;
        use std::{sync::Mutex, thread};

        use crate::{
            hal::fake::FakeHal,
            transport::{
                DeviceType,
                fake::{FakeTransport, QueueStatus, State},
            },
        };
        let config = unsafe {
            NonNull::new_unchecked(Box::into_raw(Box::new(core::mem::zeroed::<Config>())))
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..State::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::GPU,
            max_queue_size: 2,
            device_features: 0,
            config_space: config,
            state: state.clone(),
        };
        let worker_state = state.clone();
        let worker = thread::spawn(move || {
            for _ in 0..4 {
                State::wait_until_queue_notified(&worker_state, QUEUE_TRANSMIT);
                worker_state
                    .lock()
                    .unwrap()
                    .read_write_queue::<2>(QUEUE_TRANSMIT, |input| {
                        let header = CtrlHeader::read_from_prefix(&input).unwrap();
                        CtrlHeader::with_type(
                            if header.hdr_type == Command::RESOURCE_ATTACH_BACKING {
                                // A protocol error may arrive after the host has
                                // consumed the descriptors; the guest must not
                                // unref caller pages until a detach succeeds.
                                Command(0x1200)
                            } else {
                                Command::OK_NODATA
                            },
                        )
                        .as_bytes()
                        .to_vec()
                    });
            }
        });
        let mut gpu = VirtIOGpu::<FakeHal, _>::new(transport).unwrap();
        let resource = gpu.create_2d(1, 1).unwrap();
        assert_eq!(
            gpu.attach_backing(resource, 0x1000, 4096),
            Err(Error::IoError)
        );
        assert_eq!(gpu.unref(resource), Err(Error::NotReady));
        gpu.detach_backing(resource).unwrap();
        gpu.unref(resource).unwrap();
        drop(gpu);
        worker.join().unwrap();
        unsafe {
            drop(Box::from_raw(config.as_ptr()));
        }
    }

    #[test]
    fn failed_attach_detach_and_unref_keep_retry_token() {
        use alloc::{boxed::Box, sync::Arc, vec};
        use core::ptr::NonNull;
        use std::{sync::Mutex, thread};

        use crate::{
            hal::fake::FakeHal,
            transport::{
                DeviceType,
                fake::{FakeTransport, QueueStatus, State},
            },
        };
        let config = unsafe {
            NonNull::new_unchecked(Box::into_raw(Box::new(core::mem::zeroed::<Config>())))
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..State::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::GPU,
            max_queue_size: 2,
            device_features: 0,
            config_space: config,
            state: state.clone(),
        };
        let worker_state = state.clone();
        let worker = thread::spawn(move || {
            let mut detach_count = 0;
            let mut unref_count = 0;
            for _ in 0..6 {
                State::wait_until_queue_notified(&worker_state, QUEUE_TRANSMIT);
                worker_state
                    .lock()
                    .unwrap()
                    .read_write_queue::<2>(QUEUE_TRANSMIT, |input| {
                        let header = CtrlHeader::read_from_prefix(&input).unwrap();
                        let response = match header.hdr_type {
                            Command::RESOURCE_ATTACH_BACKING => Some(Command(0x1200)),
                            Command::RESOURCE_DETACH_BACKING => {
                                detach_count += 1;
                                (detach_count == 1).then_some(Command(0x1200))
                            }
                            Command::RESOURCE_UNREF => {
                                unref_count += 1;
                                (unref_count == 1).then_some(Command(0x1200))
                            }
                            _ => None,
                        }
                        .unwrap_or(Command::OK_NODATA);
                        CtrlHeader::with_type(response).as_bytes().to_vec()
                    });
            }
        });
        let mut gpu = VirtIOGpu::<FakeHal, _>::new(transport).unwrap();
        let resource = gpu.create_2d(1, 1).unwrap();
        assert_eq!(
            gpu.attach_backing(resource, 0x1000, 4096),
            Err(Error::IoError)
        );
        assert_eq!(gpu.detach_backing(resource), Err(Error::IoError));
        assert_eq!(gpu.unref(resource), Err(Error::NotReady));
        gpu.detach_backing(resource).unwrap();
        assert_eq!(gpu.unref(resource), Err(Error::IoError));
        gpu.unref(resource).unwrap();
        drop(gpu);
        worker.join().unwrap();
        unsafe {
            drop(Box::from_raw(config.as_ptr()));
        }
    }

    #[test]
    fn failed_create_2d_response_keeps_and_retires_cleanup_token() {
        use alloc::{boxed::Box, sync::Arc, vec};
        use core::ptr::NonNull;
        use std::{sync::Mutex, thread};

        use crate::{
            hal::fake::FakeHal,
            transport::{
                DeviceType,
                fake::{FakeTransport, QueueStatus, State},
            },
        };
        let config = unsafe {
            NonNull::new_unchecked(Box::into_raw(Box::new(core::mem::zeroed::<Config>())))
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..State::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::GPU,
            max_queue_size: 2,
            device_features: 0,
            config_space: config,
            state: state.clone(),
        };
        let worker_state = state.clone();
        let worker = thread::spawn(move || {
            let mut creates = 0;
            for _ in 0..3 {
                State::wait_until_queue_notified(&worker_state, QUEUE_TRANSMIT);
                worker_state
                    .lock()
                    .unwrap()
                    .read_write_queue::<2>(QUEUE_TRANSMIT, |input| {
                        let header = CtrlHeader::read_from_prefix(&input).unwrap();
                        let response = if header.hdr_type == Command::RESOURCE_CREATE_2D {
                            creates += 1;
                            (creates == 1).then_some(Command(0x1200))
                        } else {
                            None
                        }
                        .unwrap_or(Command::OK_NODATA);
                        CtrlHeader::with_type(response).as_bytes().to_vec()
                    });
            }
        });
        let mut gpu = VirtIOGpu::<FakeHal, _>::new(transport).unwrap();
        assert_eq!(gpu.create_2d(1, 1), Err(Error::IoError));
        assert_eq!(gpu.resources.len(), 1);
        assert_eq!(
            gpu.resources[0].lifecycle,
            ResourceLifecycle::CreateUncertain
        );
        // The next create reaps the unknown create via UNREF before taking a
        // new slot, so a bad host completion cannot fill all 128 slots.
        assert_eq!(gpu.create_2d(1, 1).unwrap().get(), 2);
        assert_eq!(gpu.resources.len(), 1);
        drop(gpu);
        worker.join().unwrap();
        unsafe { drop(Box::from_raw(config.as_ptr())) };
    }

    #[test]
    fn failed_create_3d_fence_keeps_and_retires_cleanup_token() {
        use alloc::{boxed::Box, sync::Arc, vec};
        use core::ptr::NonNull;
        use std::{sync::Mutex, thread};

        use crate::{
            hal::fake::FakeHal,
            transport::{
                DeviceType,
                fake::{FakeTransport, QueueStatus, State},
            },
        };
        let config = unsafe {
            NonNull::new_unchecked(Box::into_raw(Box::new(core::mem::zeroed::<Config>())))
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..State::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::GPU,
            max_queue_size: 2,
            device_features: Features::VIRGL.bits(),
            config_space: config,
            state: state.clone(),
        };
        let worker_state = state.clone();
        let worker = thread::spawn(move || {
            let mut creates = 0;
            for _ in 0..3 {
                State::wait_until_queue_notified(&worker_state, QUEUE_TRANSMIT);
                worker_state
                    .lock()
                    .unwrap()
                    .read_write_queue::<2>(QUEUE_TRANSMIT, |input| {
                        let mut response = CtrlHeader::read_from_prefix(&input).unwrap();
                        if response.hdr_type == Command::RESOURCE_CREATE_3D {
                            creates += 1;
                            response.hdr_type = if creates == 1 {
                                Command(0x1200)
                            } else {
                                Command::OK_NODATA
                            };
                        } else {
                            response.hdr_type = Command::OK_NODATA;
                        }
                        response.as_bytes().to_vec()
                    });
            }
        });
        let mut gpu = VirtIOGpu::<FakeHal, _>::new(transport).unwrap();
        let create = |gpu: &mut VirtIOGpu<FakeHal, _>| gpu.create_3d(2, 1, 0, 1, 1, 1, 1, 0, 0, 0);
        assert_eq!(create(&mut gpu), Err(Error::IoError));
        assert_eq!(gpu.resources.len(), 1);
        assert_eq!(
            gpu.resources[0].lifecycle,
            ResourceLifecycle::CreateUncertain
        );
        assert_eq!(create(&mut gpu).unwrap().get(), 2);
        assert_eq!(gpu.resources.len(), 1);
        drop(gpu);
        worker.join().unwrap();
        unsafe { drop(Box::from_raw(config.as_ptr())) };
    }

    #[test]
    fn unref_completion_then_unknown_retry_retires_local_slot() {
        use alloc::{boxed::Box, sync::Arc, vec};
        use core::ptr::NonNull;
        use std::{sync::Mutex, thread};

        use crate::{
            hal::fake::FakeHal,
            transport::{
                DeviceType,
                fake::{FakeTransport, QueueStatus, State},
            },
        };
        let config = unsafe {
            NonNull::new_unchecked(Box::into_raw(Box::new(core::mem::zeroed::<Config>())))
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default(), QueueStatus::default()],
            ..State::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::GPU,
            max_queue_size: 2,
            device_features: 0,
            config_space: config,
            state: state.clone(),
        };
        let worker_state = state.clone();
        let worker = thread::spawn(move || {
            for _ in 0..3 {
                State::wait_until_queue_notified(&worker_state, QUEUE_TRANSMIT);
                worker_state
                    .lock()
                    .unwrap()
                    .read_write_queue::<2>(QUEUE_TRANSMIT, |input| {
                        let header = CtrlHeader::read_from_prefix(&input).unwrap();
                        CtrlHeader::with_type(if header.hdr_type == Command::RESOURCE_UNREF {
                            // Model a host that completed the first UNREF but
                            // lost its response, then reports UNKNOWN_ID on
                            // the single bounded retry.
                            Command(0x1200)
                        } else {
                            Command::OK_NODATA
                        })
                        .as_bytes()
                        .to_vec()
                    });
            }
        });
        let mut gpu = VirtIOGpu::<FakeHal, _>::new(transport).unwrap();
        let id = gpu.create_2d(1, 1).unwrap();
        assert_eq!(gpu.unref(id), Err(Error::IoError));
        assert_eq!(gpu.resources.len(), 1);
        assert_eq!(
            gpu.resources[0].lifecycle,
            ResourceLifecycle::UnrefUncertain
        );
        // A second error is terminal for the local token: no forever-retry.
        assert_eq!(gpu.unref(id), Ok(()));
        assert!(gpu.resources.is_empty());
        drop(gpu);
        worker.join().unwrap();
        unsafe { drop(Box::from_raw(config.as_ptr())) };
    }
}
