use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::AxResult;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet};
use spin::Mutex;

use super::{
    device::{DrmDevice, DrmError, DrmResult, Scanout, dumb_layout, remove_owned_framebuffers},
    gem::{DumbBuffer, DumbRequest, GemHandle, GemObject, MmapOffset},
    kms::{Framebuffer, FramebufferId, KmsResources, Mode, PageFlip},
};

pub type OpenId = u64;
const MAX_EVENTS: usize = 64;
const PAGE_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrmEvent {
    VBlank {
        sequence: u64,
        user_data: u64,
        timestamp_us: u64,
    },
    FlipComplete {
        sequence: u64,
        user_data: u64,
        timestamp_us: u64,
    },
}

pub struct DrmFile {
    device: Arc<DrmDevice>,
    id: OpenId,
    state: Mutex<FileState>,
    events: Arc<EventQueue>,
    render_node: bool,
}

struct FileState {
    next_handle: GemHandle,
    handles: BTreeMap<GemHandle, Arc<GemObject>>,
    next_syncobj: super::syncobj::SyncobjHandle,
    syncobjs: BTreeMap<super::syncobj::SyncobjHandle, Arc<super::syncobj::Syncobj>>,
    is_master: bool,
    atomic_enabled: bool,
    next_blob: u32,
    blobs: BTreeMap<u32, alloc::vec::Vec<u8>>,
    render_context: Option<u32>,
}

pub(crate) struct EventQueue {
    state: Mutex<EventState>,
    waiters: PollSet,
    closing: AtomicBool,
    in_flight: AtomicUsize,
}

struct EventState {
    events: VecDeque<DrmEvent>,
    reserved: BTreeMap<u64, u64>,
    next_reservation: u64,
    closed: bool,
}

impl EventQueue {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(EventState {
                events: VecDeque::new(),
                reserved: BTreeMap::new(),
                next_reservation: 1,
                closed: false,
            }),
            waiters: PollSet::new(),
            closing: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
        })
    }
    pub(crate) fn reserve(&self, user_data: u64) -> DrmResult<u64> {
        if self.closing.load(Ordering::Acquire) {
            return Err(DrmError::NotFound);
        }
        let mut state = self.state.lock();
        if state.closed || self.closing.load(Ordering::Acquire) {
            return Err(DrmError::NotFound);
        }
        if state
            .events
            .len()
            .checked_add(state.reserved.len())
            .ok_or(DrmError::Overflow)?
            == MAX_EVENTS
        {
            return Err(DrmError::QueueFull);
        }
        let token = state.next_reservation;
        state.next_reservation = token.wrapping_add(1).max(1);
        state.reserved.insert(token, user_data);
        Ok(token)
    }
    pub(crate) fn complete(&self, token: u64, sequence: u64, timestamp_us: u64) {
        let mut state = self.state.lock();
        let user_data = state.reserved.remove(&token);
        if !state.closed && !self.closing.load(Ordering::Acquire) {
            if let Some(user_data) = user_data {
                state.events.push_back(DrmEvent::FlipComplete {
                    sequence,
                    user_data,
                    timestamp_us,
                });
                self.waiters.wake();
            }
        }
    }
    pub(crate) fn complete_vblank(&self, token: u64, sequence: u64, timestamp_us: u64) {
        let mut state = self.state.lock();
        let user_data = state.reserved.remove(&token);
        if !state.closed && !self.closing.load(Ordering::Acquire) {
            if let Some(user_data) = user_data {
                state.events.push_back(DrmEvent::VBlank {
                    sequence,
                    user_data,
                    timestamp_us,
                });
                self.waiters.wake();
            }
        }
    }
    pub(crate) fn discard(&self, token: u64) {
        self.state.lock().reserved.remove(&token);
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }
    pub(crate) fn try_begin_delivery(&self) -> bool {
        if self.closing.load(Ordering::Acquire) {
            return false;
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        if self.closing.load(Ordering::Acquire) {
            self.end_delivery();
            return false;
        }
        true
    }
    pub(crate) fn end_delivery(&self) {
        if self.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.finish_close();
        }
    }
    fn close(&self) {
        self.begin_close();
    }
    pub(crate) fn begin_close(&self) {
        self.closing.store(true, Ordering::Release);
        self.finish_close();
    }
    pub(crate) fn finish_close(&self) {
        if !self.closing.load(Ordering::Acquire)
            || self.in_flight.load(Ordering::Acquire) != 0
        {
            return;
        }
        let mut state = self.state.lock();
        if state.closed {
            return;
        }
        state.closed = true;
        state.events.clear();
        state.reserved.clear();
        self.waiters.wake();
    }
}

impl DrmFile {
    pub(crate) fn new(device: Arc<DrmDevice>, id: OpenId, render_node: bool) -> Self {
        Self {
            device,
            id,
            state: Mutex::new(FileState {
                next_handle: 1,
                handles: BTreeMap::new(),
                next_syncobj: 1,
                syncobjs: BTreeMap::new(),
                is_master: false,
                atomic_enabled: false,
                next_blob: 1,
                blobs: BTreeMap::new(),
                render_context: None,
            }),
            events: EventQueue::new(),
            render_node,
        }
    }

    pub fn id(&self) -> OpenId {
        self.id
    }
    pub(crate) fn is_render_node(&self) -> bool {
        self.render_node
    }
    pub(crate) fn has_open_id(&self, id: OpenId) -> bool {
        self.device.state.lock().open_ids.contains(&id)
    }
    pub(crate) fn vblank_sequence(&self) -> u64 {
        self.device.vblank_sequence()
    }
    pub(crate) fn device_state(&self) -> spin::MutexGuard<'_, super::device::DeviceState> {
        self.device.state.lock()
    }
    pub(crate) fn adapter_present(&self, scanout: Scanout) -> DrmResult<()> {
        self.device.adapter.present(scanout)
    }
    pub(crate) fn submit_atomic(
        &self,
        generation: u64,
        next: super::atomic::State,
        fb: Option<Framebuffer>,
        user_data: Option<u64>,
        nonblock: bool,
    ) -> DrmResult<()> {
        let event = user_data
            .map(|user_data| self.events.reserve(user_data))
            .transpose()?;
        let job = super::device::AtomicCommit {
            next,
            fb,
            cancellation: Arc::clone(&self.events),
            event,
            completion: None,
        };
        let result = if nonblock {
            self.device.queue_atomic(job, generation)
        } else {
            self.device.commit_atomic(job, generation)
        };
        if result.is_err() {
            if let Some(token) = event {
                self.events.discard(token);
            }
        }
        result
    }
    pub(crate) fn atomic_enabled(&self) -> bool {
        self.state.lock().atomic_enabled
    }
    pub(crate) fn enable_atomic(&self) {
        self.state.lock().atomic_enabled = true;
    }
    pub(crate) fn create_blob(&self, bytes: alloc::vec::Vec<u8>) -> DrmResult<u32> {
        let mut state = self.state.lock();
        let id = state.next_blob;
        state.next_blob = id.checked_add(1).ok_or(DrmError::Overflow)?;
        state.blobs.insert(id, bytes);
        Ok(id)
    }
    pub(crate) fn blob(&self, id: u32) -> Option<alloc::vec::Vec<u8>> {
        self.state.lock().blobs.get(&id).cloned()
    }
    pub(crate) fn destroy_blob(&self, id: u32) -> DrmResult<()> {
        self.state
            .lock()
            .blobs
            .remove(&id)
            .map(|_| ())
            .ok_or(DrmError::NotFound)
    }

    /// Handles a DRM ioctl using the syscall's captured user memory.
    pub fn ioctl(
        &self,
        context: &crate::file::IoctlContext,
        cmd: u32,
        arg: usize,
    ) -> AxResult<usize> {
        if self.render_node {
            super::ioctl::render_dispatch(self, context, cmd, arg)
        } else {
            super::ioctl::dispatch(self, context, cmd, arg)
        }
    }

    pub fn become_master(&self) -> DrmResult<()> {
        if self.render_node {
            return Err(DrmError::PermissionDenied);
        }
        let mut device = self.device.state.lock();
        match device.master {
            Some(id) if id != self.id => Err(DrmError::Busy),
            _ => {
                device.master = Some(self.id);
                self.state.lock().is_master = true;
                Ok(())
            }
        }
    }

    pub fn drop_master(&self) {
        let mut device = self.device.state.lock();
        if device.master == Some(self.id) {
            device.master = None;
        }
        self.state.lock().is_master = false;
    }

    pub fn resources(&self) -> KmsResources {
        self.device.state.lock().resources.clone()
    }

    pub(crate) fn gamma_lut(&self, crtc_id: u32) -> DrmResult<alloc::vec::Vec<u16>> {
        let state = self.device.state.lock();
        if crtc_id != state.resources.crtc.id {
            return Err(DrmError::NotFound);
        }
        Ok(state.gamma_lut.clone())
    }

    pub(crate) fn set_gamma_lut(&self, crtc_id: u32, values: &[u16]) -> DrmResult<()> {
        self.require_master()?;
        let mut state = self.device.state.lock();
        if crtc_id != state.resources.crtc.id || values.len() != state.gamma_lut.len() {
            return Err(DrmError::Invalid);
        }
        state.gamma_lut.copy_from_slice(values);
        Ok(())
    }

    pub fn create_dumb(&self, request: DumbRequest) -> DrmResult<DumbBuffer> {
        let (pitch, size) = dumb_layout(request)?;
        let backing = self.device.adapter.create_dumb(request, pitch, size)?;
        let mmap_offset = {
            let mut device = self.device.state.lock();
            let offset = device.next_mmap_offset;
            device.next_mmap_offset = offset
                .checked_add(
                    size.checked_add(PAGE_SIZE - 1).ok_or(DrmError::Overflow)? & !(PAGE_SIZE - 1),
                )
                .ok_or(DrmError::Overflow)?;
            offset
        };
        let mut file = self.state.lock();
        let handle = file.next_handle;
        file.next_handle = handle.checked_add(1).ok_or(DrmError::Overflow)?;
        file.handles
            .insert(handle, Arc::new(GemObject::new(backing, size, mmap_offset)));
        Ok(DumbBuffer {
            handle,
            pitch,
            size,
            mmap_offset,
        })
    }

    pub fn close_handle(&self, handle: GemHandle) -> DrmResult<()> {
        self.state
            .lock()
            .handles
            .remove(&handle)
            .map(|_| ())
            .ok_or(DrmError::NotFound)
    }

    pub(crate) fn gem(&self, handle: GemHandle) -> DrmResult<Arc<GemObject>> {
        self.state
            .lock()
            .handles
            .get(&handle)
            .cloned()
            .ok_or(DrmError::NotFound)
    }

    pub(crate) fn create_render_gem(
        &self,
        backing: Arc<dyn super::GemBacking>,
        size: u64,
        resource: u32,
        meta: super::render::RenderResource,
    ) -> DrmResult<GemHandle> {
        let mmap_offset = {
            let mut device = self.device.state.lock();
            let offset = device.next_mmap_offset;
            device.next_mmap_offset = offset
                .checked_add(
                    size.checked_add(PAGE_SIZE - 1).ok_or(DrmError::Overflow)? & !(PAGE_SIZE - 1),
                )
                .ok_or(DrmError::Overflow)?;
            offset
        };
        let mut state = self.state.lock();
        let handle = state.next_handle;
        state.next_handle = handle.checked_add(1).ok_or(DrmError::Overflow)?;
        state.handles.insert(
            handle,
            Arc::new(GemObject::render(
                backing,
                size,
                mmap_offset,
                resource,
                meta,
            )),
        );
        Ok(handle)
    }

    pub(crate) fn render_resource(&self, handle: GemHandle) -> DrmResult<(u32, Arc<GemObject>)> {
        let object = self.gem(handle)?;
        object
            .render_resource
            .map(|resource| (resource, object))
            .ok_or(DrmError::Invalid)
    }

    pub(crate) fn render_adapter(&self) -> DrmResult<Arc<dyn super::render::RenderAdapter>> {
        self.device.render.clone().ok_or(DrmError::Unsupported)
    }

    pub(crate) fn render_context(&self) -> DrmResult<u32> {
        if let Some(context) = self.state.lock().render_context {
            return Ok(context);
        }
        let adapter = self.render_adapter()?;
        let context = adapter.create_context(b"thekernel-render")?;
        let mut state = self.state.lock();
        if let Some(existing) = state.render_context {
            drop(state);
            let _ = adapter.destroy_context(context);
            Ok(existing)
        } else {
            state.render_context = Some(context);
            Ok(context)
        }
    }

    /// Imports one dma-buf object, coalescing repeated imports in this OFD.
    pub(crate) fn import_gem(&self, object: Arc<GemObject>) -> DrmResult<GemHandle> {
        let mut state = self.state.lock();
        if let Some((&handle, _)) = state
            .handles
            .iter()
            .find(|(_, existing)| Arc::ptr_eq(existing, &object))
        {
            return Ok(handle);
        }
        let handle = state.next_handle;
        state.next_handle = handle.checked_add(1).ok_or(DrmError::Overflow)?;
        state.handles.insert(handle, object);
        Ok(handle)
    }

    pub(crate) fn create_syncobj(
        &self,
        signaled: bool,
    ) -> DrmResult<super::syncobj::SyncobjHandle> {
        let mut state = self.state.lock();
        let handle = state.next_syncobj;
        state.next_syncobj = handle.checked_add(1).ok_or(DrmError::Overflow)?;
        state
            .syncobjs
            .insert(handle, super::syncobj::Syncobj::new(signaled));
        Ok(handle)
    }
    pub(crate) fn syncobj(
        &self,
        handle: super::syncobj::SyncobjHandle,
    ) -> DrmResult<Arc<super::syncobj::Syncobj>> {
        self.state
            .lock()
            .syncobjs
            .get(&handle)
            .cloned()
            .ok_or(DrmError::NotFound)
    }
    pub(crate) fn destroy_syncobj(&self, handle: super::syncobj::SyncobjHandle) -> DrmResult<()> {
        self.state
            .lock()
            .syncobjs
            .remove(&handle)
            .map(|_| ())
            .ok_or(DrmError::NotFound)
    }

    pub fn mmap_object(&self, offset: MmapOffset) -> DrmResult<Arc<dyn super::GemBacking>> {
        self.state
            .lock()
            .handles
            .values()
            .find(|object| object.mmap_offset == offset)
            .map(|object| Arc::clone(&object.backing))
            .ok_or(DrmError::NotFound)
    }

    pub(crate) fn map_dumb(&self, handle: GemHandle) -> DrmResult<MmapOffset> {
        self.state
            .lock()
            .handles
            .get(&handle)
            .map(|object| object.mmap_offset)
            .ok_or(DrmError::NotFound)
    }

    pub fn add_framebuffer(
        &self,
        handle: GemHandle,
        width: u32,
        height: u32,
        pitch: u32,
        bpp: u32,
    ) -> DrmResult<FramebufferId> {
        let object = self
            .state
            .lock()
            .handles
            .get(&handle)
            .cloned()
            .ok_or(DrmError::NotFound)?;
        if width == 0
            || height == 0
            || bpp == 0
            || bpp % 8 != 0
            || pitch < width.checked_mul(bpp / 8).ok_or(DrmError::Overflow)?
            || (pitch as u64)
                .checked_mul(height as u64)
                .ok_or(DrmError::Overflow)?
                > object.size
        {
            return Err(DrmError::Invalid);
        }
        let mut device = self.device.state.lock();
        let id = device.next_framebuffer;
        device.next_framebuffer = id.checked_add(1).ok_or(DrmError::Overflow)?;
        device.framebuffers.insert(
            id,
            Framebuffer {
                owner: self.id,
                handle,
                object,
                width,
                height,
                pitch,
                bpp,
            },
        );
        Ok(id)
    }

    pub fn rm_framebuffer(&self, id: FramebufferId) -> DrmResult<()> {
        let mut device = self.device.state.lock();
        let fb = device.framebuffers.get(&id).ok_or(DrmError::NotFound)?;
        if fb.owner != self.id {
            return Err(DrmError::PermissionDenied);
        }
        if device.resources.crtc.framebuffer == Some(id) {
            return Err(DrmError::Busy);
        }
        if device.pending_fb_pins.contains_key(&id) {
            return Err(DrmError::Busy);
        }
        device.advance_atomic_generation()?;
        device.framebuffers.remove(&id);
        Ok(())
    }

    pub(crate) fn framebuffer(&self, id: FramebufferId) -> DrmResult<Framebuffer> {
        self.device
            .state
            .lock()
            .framebuffers
            .get(&id)
            .cloned()
            .ok_or(DrmError::NotFound)
    }

    pub(crate) fn set_legacy_property(
        &self,
        object: u32,
        object_type: u32,
        property: u32,
        value: u64,
    ) -> DrmResult<()> {
        self.require_master()?;
        let mut state = self.device.state.lock();
        if object_type != super::uapi::DRM_MODE_OBJECT_CONNECTOR
            || object != state.resources.connector.id
            || property != super::property::CONNECTOR_CRTC_ID
            || value > u32::MAX as u64
        {
            return Err(DrmError::Invalid);
        }
        let crtc_id = value as u32;
        if crtc_id != 0 && crtc_id != state.resources.crtc.id {
            return Err(DrmError::Invalid);
        }
        if crtc_id == 0 && state.resources.crtc.framebuffer.is_some() {
            return Err(DrmError::Busy);
        }
        state.atomic.connector_crtc = crtc_id;
        Ok(())
    }

    pub fn set_crtc(&self, framebuffer: FramebufferId, mode: Mode) -> DrmResult<()> {
        self.present(framebuffer, mode, None)
    }
    pub fn page_flip(&self, request: PageFlip) -> DrmResult<()> {
        let mode = self
            .device
            .state
            .lock()
            .resources
            .crtc
            .mode
            .ok_or(DrmError::Invalid)?;
        self.present(
            request.framebuffer,
            mode,
            request.event.then_some(request.user_data),
        )
    }

    fn present(&self, framebuffer: FramebufferId, mode: Mode, event: Option<u64>) -> DrmResult<()> {
        self.require_master()?;
        let fb = {
            let device = self.device.state.lock();
            let fb = device
                .framebuffers
                .get(&framebuffer)
                .filter(|fb| fb.owner == self.id)
                .cloned()
                .ok_or(DrmError::NotFound)?;
            fb
        };
        if mode.width != fb.width || mode.height != fb.height {
            return Err(DrmError::Invalid);
        }
        let (generation, mut next) = {
            let state = self.device.state.lock();
            (state.atomic_generation, state.atomic_tail)
        };
        next.active = true;
        next.mode = Some(mode);
        next.fb = framebuffer;
        next.src_w = fb.width.checked_shl(16).ok_or(DrmError::Overflow)?;
        next.src_h = fb.height.checked_shl(16).ok_or(DrmError::Overflow)?;
        next.crtc_w = fb.width;
        next.crtc_h = fb.height;
        self.submit_atomic(generation, next, Some(fb), event, false)
    }

    pub fn wait_vblank(&self, user_data: u64) -> DrmResult<u64> {
        self.wait_vblank_request(user_data, true)
    }

    pub(crate) fn wait_vblank_request(&self, user_data: u64, event: bool) -> DrmResult<u64> {
        if event {
            let token = self.events.reserve(user_data)?;
            let sequence = self.device.vblank_sequence();
            let queued = super::device::VblankEvent {
                target: sequence.saturating_add(1),
                token,
                queue: Arc::clone(&self.events),
            };
            if let Err(error) = self.device.queue_vblank_event(queued) {
                self.events.discard(token);
                return Err(error);
            }
            return Ok(sequence);
        }
        self.device.wait_for_vblank()
    }
    pub(crate) fn wait_vblank_target(
        &self,
        target: u64,
        user_data: u64,
        event: bool,
    ) -> DrmResult<u64> {
        let current = self.device.vblank_sequence();
        let target = if event {
            target.max(current.saturating_add(1))
        } else {
            target
        };
        if event {
            let token = self.events.reserve(user_data)?;
            let queued = super::device::VblankEvent {
                target,
                token,
                queue: Arc::clone(&self.events),
            };
            if let Err(error) = self.device.queue_vblank_event(queued) {
                self.events.discard(token);
                return Err(error);
            }
            return Ok(current);
        }
        self.device.wait_for_vblank_at_least(target)
    }
    pub fn dequeue_event(&self) -> Option<DrmEvent> {
        self.events.state.lock().events.pop_front()
    }

    /// Serializes whole Linux `drm_event_vblank` records; never splits one.
    pub fn read_events(&self, dst: &mut crate::file::IoDst) -> AxResult<usize> {
        let mut written = 0;
        let mut state = self.events.state.lock();
        while let Some(event) = state.events.front().copied() {
            let bytes = event.to_linux_bytes();
            if dst.remaining_mut() < bytes.len() {
                if written == 0 {
                    return Err(axerrno::AxError::InvalidInput);
                }
                break;
            }
            dst.write(&bytes)?;
            state.events.pop_front();
            written += bytes.len();
        }
        Ok(written)
    }

    pub fn poll_events(&self) -> IoEvents {
        if self.events.state.lock().events.is_empty() {
            IoEvents::empty()
        } else {
            IoEvents::READABLE
        }
    }

    pub fn register_events<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let mut prepared = axpoll::PreparedPollRegistration::try_new(
            events.intersects(IoEvents::READABLE | IoEvents::ERROR) as usize,
        )?;
        if events.intersects(IoEvents::READABLE | IoEvents::ERROR) {
            prepared.arm(&self.events.waiters, context.waker())?;
        }
        prepared.commit()
    }

    pub fn prepare_mmap(
        &self,
        request: crate::file::FileMmapRequest,
    ) -> AxResult<Option<crate::file::PreparedFileMmap>> {
        let object = match self
            .state
            .lock()
            .handles
            .values()
            .find(|object| object.mmap_offset == request.offset())
            .cloned()
        {
            Some(object) => object,
            None => return Ok(None),
        };
        let pages = object
            .backing
            .shared_pages()
            .map_err(axerrno::AxError::from)?;
        crate::file::FixedSharedMmapRegion::try_new(
            object.mmap_offset,
            pages,
            crate::file::FileMmapProtection::READ | crate::file::FileMmapProtection::WRITE,
        )?
        .prepare(request)
    }

    fn push_event(&self, event: DrmEvent) -> DrmResult<()> {
        if self.events.is_closed() {
            return Err(DrmError::NotFound);
        }
        let mut state = self.events.state.lock();
        if state.closed || self.events.is_closed() {
            return Err(DrmError::NotFound);
        }
        if state
            .events
            .len()
            .checked_add(state.reserved.len())
            .ok_or(DrmError::Overflow)?
            == MAX_EVENTS
        {
            return Err(DrmError::QueueFull);
        }
        state.events.push_back(event);
        self.events.waiters.wake();
        Ok(())
    }
    pub(crate) fn require_master(&self) -> DrmResult<()> {
        if self.render_node {
            return Err(DrmError::PermissionDenied);
        }
        let device_is_master = self.device.state.lock().master == Some(self.id);
        if device_is_master && self.state.lock().is_master {
            Ok(())
        } else {
            Err(DrmError::PermissionDenied)
        }
    }
}

impl DrmEvent {
    fn to_linux_bytes(self) -> [u8; 32] {
        let (kind, sequence, user_data, timestamp_us) = match self {
            Self::VBlank {
                sequence,
                user_data,
                timestamp_us,
            } => (1u32, sequence, user_data, timestamp_us),
            Self::FlipComplete {
                sequence,
                user_data,
                timestamp_us,
            } => (2u32, sequence, user_data, timestamp_us),
        };
        let mut bytes = [0; 32];
        bytes[0..4].copy_from_slice(&kind.to_ne_bytes());
        bytes[4..8].copy_from_slice(&(32u32).to_ne_bytes());
        bytes[8..16].copy_from_slice(&user_data.to_ne_bytes());
        bytes[16..20].copy_from_slice(&((timestamp_us / 1_000_000) as u32).to_ne_bytes());
        bytes[20..24].copy_from_slice(&((timestamp_us % 1_000_000) as u32).to_ne_bytes());
        bytes[24..28].copy_from_slice(&(sequence as u32).to_ne_bytes());
        bytes
    }
}

impl Drop for DrmFile {
    fn drop(&mut self) {
        self.events.begin_close();
        self.device.cancel_file_commits(&self.events);
        self.events.finish_close();
        let mut device = self.device.state.lock();
        device.open_ids.remove(&self.id);
        if device.master == Some(self.id) {
            device.master = None;
        }
        remove_owned_framebuffers(&mut device, self.id);
        drop(device);
        if self.render_node {
            if let Some(adapter) = self.device.render.clone() {
                let (context, resources) = {
                    let mut state = self.state.lock();
                    let context = state.render_context.take();
                    let resources = state
                        .handles
                        .values()
                        .filter_map(|o| o.render_resource)
                        .collect::<alloc::vec::Vec<_>>();
                    (context, resources)
                };
                if let Some(context) = context {
                    for resource in resources {
                        let _ = adapter.detach_resource(context, resource);
                    }
                    let _ = adapter.destroy_context(context);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::*;

    struct Backing;
    impl super::super::GemBacking for Backing {
        fn shared_pages(&self) -> DrmResult<Arc<crate::mm::SharedPages>> {
            Err(DrmError::Unsupported)
        }
    }
    struct Adapter;
    impl super::super::DisplayAdapter for Adapter {
        fn create_dumb(
            &self,
            _: DumbRequest,
            _: u32,
            _: u64,
        ) -> DrmResult<Arc<dyn super::super::GemBacking>> {
            Ok(Arc::new(Backing))
        }
        fn present(&self, _: Scanout) -> DrmResult<()> {
            Ok(())
        }
    }
    fn device() -> Arc<DrmDevice> {
        DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4)
    }
    #[test]
    fn handles_are_per_open_and_drop_releases_master() {
        let dev = device();
        let first = dev.open_primary();
        let second = dev.open_primary();
        assert_ne!(first.id(), second.id());
        let dumb = first
            .create_dumb(DumbRequest {
                width: 16,
                height: 16,
                bpp: 32,
            })
            .unwrap();
        assert!(second.mmap_object(dumb.mmap_offset).is_err());
        first.become_master().unwrap();
        assert_eq!(second.become_master(), Err(DrmError::Busy));
        drop(first);
        assert!(second.become_master().is_ok());
    }
    #[test]
    fn flip_completes_in_order_and_events_are_bounded() {
        let file = device().open_primary();
        file.become_master().unwrap();
        let dumb = file
            .create_dumb(DumbRequest {
                width: 8,
                height: 8,
                bpp: 32,
            })
            .unwrap();
        let fb = file
            .add_framebuffer(dumb.handle, 8, 8, dumb.pitch, 32)
            .unwrap();
        let mode = Mode {
            width: 8,
            height: 8,
            refresh_millihz: 60_000,
        };
        file.set_crtc(fb, mode).unwrap();
        file.page_flip(PageFlip {
            framebuffer: fb,
            event: true,
            user_data: 7,
        })
        .unwrap();
        assert!(matches!(
            file.dequeue_event(),
            Some(DrmEvent::FlipComplete { user_data: 7, .. })
        ));
        for _ in 0..MAX_EVENTS {
            file.wait_vblank(0).unwrap();
        }
        assert_eq!(file.wait_vblank(0), Err(DrmError::QueueFull));
    }

    #[test]
    fn reserved_atomic_event_is_bounded_and_close_cancels_it() {
        let events = EventQueue::new();
        let token = events.reserve(9).unwrap();
        events.complete(token, 4, 1);
        assert_eq!(
            events.state.lock().events.pop_front(),
            Some(DrmEvent::FlipComplete {
                sequence: 4,
                user_data: 9,
                timestamp_us: 1,
            })
        );
        for _ in 0..MAX_EVENTS {
            events.reserve(0).unwrap();
        }
        assert_eq!(events.reserve(0), Err(DrmError::QueueFull));
        events.close();
        events.complete(token, 5, 2);
        assert!(events.state.lock().events.is_empty());
        assert!(events.is_closed());
    }

    #[test]
    fn close_defers_cleanup_to_the_last_delivery_without_blocking() {
        let events = EventQueue::new();
        let token = events.reserve(9).unwrap();
        assert!(events.try_begin_delivery());

        // `close` is also used from `DrmFile::drop`, so it must not wait for
        // this in-flight delivery.  The delivery owns final cleanup instead.
        events.close();
        assert!(events.is_closed());
        assert!(!events.state.lock().closed);
        assert_eq!(events.reserve(10), Err(DrmError::NotFound));

        events.complete(token, 4, 1);
        events.end_delivery();
        let state = events.state.lock();
        assert!(state.closed);
        assert!(state.events.is_empty());
        assert!(state.reserved.is_empty());
    }

    #[test]
    fn syncobj_handle_lifecycle_preserves_fence_identity_until_destroyed() {
        let file = device().open_primary();
        let handle = file.create_syncobj(false).unwrap();
        let object = file.syncobj(handle).unwrap();
        let original = object.fence();
        assert!(!original.is_signaled());

        object.signal();
        assert!(original.is_signaled());
        object.reset();
        assert!(!object.fence().is_signaled());
        assert!(original.is_signaled());

        file.destroy_syncobj(handle).unwrap();
        assert!(matches!(file.syncobj(handle), Err(DrmError::NotFound)));
    }

    #[test]
    fn gamma_lut_is_per_crtc_and_requires_master_to_change() {
        let file = device().open_primary();
        let original = file.gamma_lut(3).unwrap();
        let mut replacement = original.clone();
        replacement[0] = 0x1234;
        assert_eq!(
            file.set_gamma_lut(3, &replacement),
            Err(DrmError::PermissionDenied)
        );
        file.become_master().unwrap();
        file.set_gamma_lut(3, &replacement).unwrap();
        assert_eq!(file.gamma_lut(3).unwrap()[0], 0x1234);
        assert!(matches!(file.gamma_lut(99), Err(DrmError::NotFound)));
    }

    #[test]
    fn rmfb_invalidates_a_generation_snapshot_before_it_can_enqueue() {
        let file = device().open_primary();
        file.become_master().unwrap();
        let dumb = file
            .create_dumb(DumbRequest {
                width: 8,
                height: 8,
                bpp: 32,
            })
            .unwrap();
        let fb = file
            .add_framebuffer(dumb.handle, 8, 8, dumb.pitch, 32)
            .unwrap();
        let stale_generation = file.device_state().atomic_generation;
        file.rm_framebuffer(fb).unwrap();
        let state = file.device_state();
        assert_ne!(state.atomic_generation, stale_generation);
        assert!(!state.framebuffers.contains_key(&fb));
    }
}
