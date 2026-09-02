use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet};
use spin::Mutex;

use super::{
    device::{
        CursorUpdate, DrmDevice, DrmError, DrmResult, Scanout, dumb_layout,
        remove_owned_framebuffers,
    },
    gem::{DumbBuffer, DumbRequest, GemHandle, GemObject, MmapOffset},
    kms::{Framebuffer, FramebufferId, KmsResources, Mode},
};

/// Per-request explicit synchronization.  These fences are intentionally
/// outside `atomic::State`: KMS properties reset after every commit.
pub(crate) struct AtomicSync {
    pub(crate) inputs: alloc::vec::Vec<Arc<super::fence::Fence>>,
    pub(crate) predecessors: alloc::vec::Vec<Arc<super::fence::Fence>>,
    pub(crate) completion: Option<Arc<super::fence::Fence>>,
}

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
    seat_revoked: Arc<AtomicBool>,
    seat_owned_primary: bool,
}

struct FileState {
    next_handle: GemHandle,
    handles: BTreeMap<GemHandle, Arc<GemObject>>,
    cursor_framebuffers: BTreeMap<GemHandle, FramebufferId>,
    next_syncobj: super::syncobj::SyncobjHandle,
    syncobjs: BTreeMap<super::syncobj::SyncobjHandle, Arc<super::syncobj::Syncobj>>,
    is_master: bool,
    atomic_enabled: bool,
    render_context: Option<u32>,
    render_init: super::render::ContextInit,
    render_cancelled: Arc<AtomicBool>,
}

pub(crate) struct EventQueue {
    state: Mutex<EventState>,
    waiters: PollSet,
    closing: AtomicBool,
    in_flight: AtomicUsize,
}

#[derive(Clone)]
pub(crate) struct SeatLease {
    revoked: Arc<AtomicBool>,
    events: Arc<EventQueue>,
}

impl SeatLease {
    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        self.events.begin_close();
        self.events.finish_close();
    }
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
        if !state.closed
            && !self.closing.load(Ordering::Acquire)
            && let Some(user_data) = user_data
        {
            state.events.push_back(DrmEvent::FlipComplete {
                sequence,
                user_data,
                timestamp_us,
            });
            self.waiters.wake();
        }
    }
    pub(crate) fn complete_vblank(&self, token: u64, sequence: u64, timestamp_us: u64) {
        let mut state = self.state.lock();
        let user_data = state.reserved.remove(&token);
        if !state.closed
            && !self.closing.load(Ordering::Acquire)
            && let Some(user_data) = user_data
        {
            state.events.push_back(DrmEvent::VBlank {
                sequence,
                user_data,
                timestamp_us,
            });
            self.waiters.wake();
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
        if !self.closing.load(Ordering::Acquire) || self.in_flight.load(Ordering::Acquire) != 0 {
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
    pub(crate) fn new(
        device: Arc<DrmDevice>,
        id: OpenId,
        render_node: bool,
        seat_owned_primary: bool,
    ) -> Self {
        Self {
            device,
            id,
            state: Mutex::new(FileState {
                next_handle: 1,
                handles: BTreeMap::new(),
                cursor_framebuffers: BTreeMap::new(),
                next_syncobj: 1,
                syncobjs: BTreeMap::new(),
                is_master: false,
                atomic_enabled: false,
                render_context: None,
                render_init: super::render::ContextInit::default(),
                render_cancelled: Arc::new(AtomicBool::new(false)),
            }),
            events: EventQueue::new(),
            render_node,
            seat_revoked: Arc::new(AtomicBool::new(false)),
            seat_owned_primary,
        }
    }

    pub fn id(&self) -> OpenId {
        self.id
    }
    pub(crate) fn seat_lease(&self) -> SeatLease {
        SeatLease {
            revoked: self.seat_revoked.clone(),
            events: self.events.clone(),
        }
    }
    fn require_live_seat_lease(&self) -> DrmResult<()> {
        if self.seat_owned_primary && self.seat_revoked.load(Ordering::Acquire) {
            Err(DrmError::NotFound)
        } else {
            Ok(())
        }
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
        self.device.adapter.present(scanout).map(|_| ())
    }
    pub(crate) fn update_cursor(
        &self,
        handle: GemHandle,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        x: i32,
        y: i32,
    ) -> DrmResult<()> {
        self.require_master()?;
        if width != 64 || height != 64 || hot_x >= width || hot_y >= height {
            return Err(DrmError::Invalid);
        }
        let object = self.gem(handle)?;
        if object.size < u64::from(width) * u64::from(height) * 4 {
            return Err(DrmError::Invalid);
        }
        // `object` remains owned by this frame until the lower cursor queue
        // has returned its terminal completion.
        self.device
            .adapter
            .update_cursor(CursorUpdate {
                backing: Arc::clone(&object.backing),
                width,
                height,
                hot_x,
                hot_y,
                x,
                y,
            })
            .map(|_| ())
    }
    pub(crate) fn move_cursor(&self, x: i32, y: i32) -> DrmResult<()> {
        self.require_master()?;
        self.device.adapter.move_cursor(x, y).map(|_| ())
    }
    pub(crate) fn submit_atomic(
        &self,
        generation: u64,
        next: super::atomic::State,
        fb: Option<Framebuffer>,
        user_data: Option<u64>,
        nonblock: bool,
        sync: AtomicSync,
    ) -> DrmResult<()> {
        let scanout_fence = sync.completion.clone();
        // This early check keeps event reservation unavailable to non-masters;
        // enqueue_atomic repeats the device-state check while holding its lock
        // to close the master-drop race before publication.
        if let Err(error) = self.require_master() {
            if let Some(fence) = &scanout_fence {
                fence.signal_error();
            }
            return Err(error);
        }
        let event = user_data
            .map(|user_data| self.events.reserve(user_data))
            .transpose()
            .map_err(|error| {
                if let Some(fence) = &scanout_fence {
                    fence.signal_error();
                }
                error
            })?;
        let damage = match self.device.damage_for_atomic(next, fb.as_ref()) {
            Ok(damage) => damage,
            Err(error) => {
                if let Some(token) = event {
                    self.events.discard(token);
                }
                if let Some(fence) = &scanout_fence {
                    fence.signal_error();
                }
                return Err(error);
            }
        };
        let cursor_fb = if next.cursor_fb == 0 {
            None
        } else {
            match self.framebuffer(next.cursor_fb) {
                Ok(fb) => Some(fb),
                Err(error) => {
                    if let Some(token) = event {
                        self.events.discard(token);
                    }
                    if let Some(fence) = &scanout_fence {
                        fence.signal_error();
                    }
                    return Err(error);
                }
            }
        };
        let cursor = if next.cursor_fb == 0 {
            None
        } else {
            let cursor = cursor_fb.as_ref().expect("cursor framebuffer resolved");
            Some(CursorUpdate {
                backing: Arc::clone(&cursor.object.backing),
                width: 64,
                height: 64,
                hot_x: next.cursor_hot_x,
                hot_y: next.cursor_hot_y,
                x: next.cursor_crtc_x as i32,
                y: next.cursor_crtc_y as i32,
            })
        };
        let mut reservation_predecessors = sync.predecessors;
        let reservation_rollback = match (&fb, &scanout_fence) {
            (Some(fb), Some(fence)) => {
                let predecessor = fb.object.reservation.replace(fence.clone());
                reservation_predecessors.clear();
                if let Some(predecessor) = &predecessor {
                    reservation_predecessors.push(predecessor.clone());
                }
                Some((fb.clone(), fence.clone(), predecessor))
            }
            _ => None,
        };
        let job = super::device::AtomicCommit {
            owner: self.id,
            next,
            fb,
            cancellation: Arc::clone(&self.events),
            event,
            completion: None,
            present: None,
            present_target: 0,
            damage,
            cursor,
            cursor_fb,
            cursor_submitted: false,
            cursor_fence: None,
            cursor_target: 0,
            input_fences: sync.inputs,
            reservation_predecessors,
            scanout_fence: scanout_fence.clone(),
        };
        let completion_result = if nonblock {
            self.device.queue_atomic(job, generation).map(|()| None)
        } else {
            self.device
                .queue_atomic_with_completion(job, generation)
                .map(Some)
        };
        let completion = match completion_result {
            Ok(completion) => completion,
            Err(error) => {
                if let Some((fb, fence, predecessor)) = reservation_rollback {
                    fb.object
                        .reservation
                        .restore_if_current(&fence, predecessor);
                }
                if let Some(token) = event {
                    self.events.discard(token);
                }
                if let Some(fence) = scanout_fence {
                    fence.signal_error();
                }
                return Err(error);
            }
        };
        // The reservation edge was installed immediately before admission.
        // From this point onward it remains the scanout read completion; a
        // concurrent render replace observes it as its exact predecessor.
        if let Some(completion) = completion {
            #[cfg(test)]
            for _ in 0..4 {
                if completion.is_complete() {
                    break;
                }
                self.device.advance_vblank()?;
            }
            completion.wait()
        } else {
            Ok(())
        }
    }
    /// Submit a legacy KMS request through the atomic proposal and commit
    /// path.  Inline legacy modes have no blob ID, but all object/property,
    /// ownership, framebuffer and presentation validation remains shared.
    pub(crate) fn submit_legacy_atomic(
        &self,
        changes: &[super::atomic::Change],
        mode: Option<Mode>,
        user_data: Option<u64>,
        nonblock: bool,
    ) -> DrmResult<()> {
        let (generation, _, next, fb) = super::atomic::propose_legacy(self, changes, mode)?;
        let completion = fb.as_ref().map(|_| super::fence::Fence::new(false));
        self.submit_atomic(
            generation,
            next,
            fb,
            user_data,
            nonblock,
            AtomicSync {
                inputs: alloc::vec::Vec::new(),
                predecessors: alloc::vec::Vec::new(),
                completion,
            },
        )
    }
    pub(crate) fn submit_legacy_cursor_atomic(
        &self,
        changes: &[super::atomic::Change],
        hot_x: u32,
        hot_y: u32,
    ) -> DrmResult<()> {
        let (generation, _, mut next, fb) = super::atomic::propose_legacy(self, changes, None)?;
        next.cursor_hot_x = hot_x;
        next.cursor_hot_y = hot_y;
        // Cursor-only commits still read the active primary scanout while
        // compositing in the host. Publish a real completion edge so render
        // cannot overwrite it while cursorq waits for its terminal token.
        self.submit_atomic(
            generation,
            next,
            fb,
            None,
            false,
            AtomicSync {
                inputs: alloc::vec::Vec::new(),
                predecessors: alloc::vec::Vec::new(),
                completion: Some(super::fence::Fence::new(false)),
            },
        )
    }
    pub(crate) fn atomic_enabled(&self) -> bool {
        self.state.lock().atomic_enabled
    }
    pub(crate) fn enable_atomic(&self) {
        self.state.lock().atomic_enabled = true;
    }
    pub(crate) fn create_blob(&self, bytes: alloc::vec::Vec<u8>) -> DrmResult<u32> {
        self.device.create_property_blob(bytes)
    }
    pub(crate) fn blob(&self, id: u32) -> Option<alloc::vec::Vec<u8>> {
        self.device.property_blob(id)
    }
    pub(crate) fn live_blob(&self, id: u32) -> Option<alloc::vec::Vec<u8>> {
        self.device.live_property_blob(id)
    }
    pub(crate) fn destroy_blob(&self, id: u32) -> DrmResult<()> {
        self.device.destroy_property_blob(id)
    }

    /// Handles a DRM ioctl using the syscall's captured user memory.
    pub fn ioctl(
        &self,
        context: &crate::file::IoctlContext,
        cmd: u32,
        arg: usize,
    ) -> AxResult<usize> {
        self.require_live_seat_lease().map_err(AxError::from)?;
        if self.render_node {
            super::ioctl::render_dispatch(self, context, cmd, arg)
        } else {
            super::ioctl::dispatch(self, context, cmd, arg)
        }
    }

    pub fn become_master(&self) -> DrmResult<()> {
        self.require_live_seat_lease()?;
        if self.render_node {
            return Err(DrmError::PermissionDenied);
        }
        let mut device = self.device.state.lock();
        if device.kms_suspended {
            return Err(DrmError::Busy);
        }
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
        drop(device);
        // Do not leave an already-host-complete flip eligible for publication
        // after this file loses DRM mastership.
        self.device.cancel_file_commits(&self.events);
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
        let expected = {
            let state = self.device.state.lock();
            if crtc_id != state.resources.crtc.id || values.len() != state.gamma_lut.len() {
                return Err(DrmError::Invalid);
            }
            state.gamma_lut.len()
        };
        if expected % 3 != 0 {
            return Err(DrmError::Invalid);
        }
        let mut bytes = alloc::vec::Vec::new();
        bytes
            .try_reserve_exact(expected / 3 * 8)
            .map_err(|_| DrmError::NoMemory)?;
        for color in values.chunks_exact(3) {
            bytes.extend_from_slice(&color[0].to_ne_bytes());
            bytes.extend_from_slice(&color[1].to_ne_bytes());
            bytes.extend_from_slice(&color[2].to_ne_bytes());
            bytes.extend_from_slice(&0u16.to_ne_bytes());
        }
        let blob = self.device.create_property_blob(bytes)?;
        let result = self.submit_legacy_atomic(
            &[super::atomic::Change {
                object: crtc_id,
                property: super::property::CRTC_GAMMA_LUT,
                value: blob as u64,
            }],
            None,
            None,
            false,
        );
        let destroy = self.device.destroy_property_blob(blob);
        result.and(destroy)
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
        drop(file);
        self.device.gem_handle_opened(size, false);
        Ok(DumbBuffer {
            handle,
            pitch,
            size,
            mmap_offset,
        })
    }

    pub fn close_handle(&self, handle: GemHandle) -> DrmResult<()> {
        let object = self
            .state
            .lock()
            .handles
            .remove(&handle)
            .ok_or(DrmError::NotFound)?;
        self.device
            .gem_handle_closed(object.size, object.render_blob_mem.is_some());
        Ok(())
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
        blob_mem: Option<u32>,
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
                blob_mem,
            )),
        );
        drop(state);
        self.device.gem_handle_opened(size, blob_mem.is_some());
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
        let init = self.state.lock().render_init;
        let context = adapter.create_context_with_init(b"thekernel-render", init)?;
        let mut state = self.state.lock();
        if let Some(existing) = state.render_context {
            drop(state);
            let _ = adapter.destroy_context(context);
            Ok(existing)
        } else {
            state.render_context = Some(context);
            drop(state);
            self.device.render_context_opened();
            Ok(context)
        }
    }
    pub(crate) fn set_render_context_init(
        &self,
        init: super::render::ContextInit,
    ) -> DrmResult<()> {
        let mut state = self.state.lock();
        if state.render_context.is_some() {
            return Err(DrmError::Busy);
        }
        state.render_init = init;
        Ok(())
    }
    pub(crate) fn render_ring_count(&self) -> u32 {
        self.state.lock().render_init.num_rings.max(1)
    }

    pub(crate) fn render_cancelled(&self) -> Arc<AtomicBool> {
        self.state.lock().render_cancelled.clone()
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
        let bytes = object.size;
        let blob = object.render_blob_mem.is_some();
        state.handles.insert(handle, object);
        drop(state);
        self.device.gem_handle_opened(bytes, blob);
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
    pub(crate) fn import_syncobj(
        &self,
        object: Arc<super::syncobj::Syncobj>,
    ) -> DrmResult<super::syncobj::SyncobjHandle> {
        let mut state = self.state.lock();
        let handle = state.next_syncobj;
        state.next_syncobj = handle.checked_add(1).ok_or(DrmError::Overflow)?;
        state.syncobjs.insert(handle, object);
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
        self.add_framebuffer_format_offset(
            handle,
            width,
            height,
            pitch,
            bpp,
            if bpp == 32 {
                0x3432_5258
            } else {
                return Err(DrmError::Unsupported);
            },
            0,
        )
    }

    /// Adds a linear framebuffer view beginning at a byte-aligned GEM offset.
    /// This is used by fbdev virtual pages; ordinary DRM ioctls use offset 0.
    pub(crate) fn add_framebuffer_offset(
        &self,
        handle: GemHandle,
        width: u32,
        height: u32,
        pitch: u32,
        bpp: u32,
        offset: u64,
    ) -> DrmResult<FramebufferId> {
        self.add_framebuffer_format_offset(handle, width, height, pitch, bpp, 0x3432_5258, offset)
    }

    pub(crate) fn add_framebuffer_format_offset(
        &self,
        handle: GemHandle,
        width: u32,
        height: u32,
        pitch: u32,
        bpp: u32,
        format: u32,
        offset: u64,
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
            || bpp != 32 || !matches!(format, 0x3432_5258 | 0x3432_5241)
            || !bpp.is_multiple_of(8)
            || pitch < width.checked_mul(bpp / 8).ok_or(DrmError::Overflow)?
            // Linear four-byte pixels may start part-way through a row; the
            // adapter carries the exact plane offset and source rectangle.
            || !offset.is_multiple_of(u64::from(bpp / 8))
            || (offset % u64::from(pitch))
                .checked_add(u64::from(width) * u64::from(bpp / 8))
                .is_none_or(|end| end > u64::from(pitch))
            || offset
                .checked_add(
                    (pitch as u64)
                .checked_mul(height as u64)
                .ok_or(DrmError::Overflow)?)
                .ok_or(DrmError::Overflow)? > object.size
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
                format,
                offset,
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

    /// Cursor ioctls name a GEM handle, whereas atomic FB_ID names a DRM
    /// framebuffer. Materialize one private 64x64 ARGB framebuffer per handle
    /// and cache its real object ID for all later cursor commits.
    pub(crate) fn cursor_framebuffer(&self, handle: GemHandle) -> DrmResult<FramebufferId> {
        if let Some(id) = self.state.lock().cursor_framebuffers.get(&handle).copied() {
            if self.framebuffer(id).is_ok() {
                return Ok(id);
            }
        }
        let id = self.add_framebuffer(handle, 64, 64, 256, 32)?;
        let mut state = self.state.lock();
        state.cursor_framebuffers.insert(handle, id);
        Ok(id)
    }

    pub(crate) fn set_legacy_property(
        &self,
        object: u32,
        object_type: u32,
        property: u32,
        value: u64,
    ) -> DrmResult<()> {
        self.require_master()?;
        let resources = self.resources();
        if object_type != super::uapi::DRM_MODE_OBJECT_CONNECTOR
            || object != resources.connector.id
            || !matches!(
                property,
                super::property::CONNECTOR_CRTC_ID | super::property::CONNECTOR_DPMS
            )
        {
            return Err(DrmError::Invalid);
        }
        if property == super::property::CONNECTOR_CRTC_ID
            && (value > u32::MAX as u64 || (value as u32 != 0 && value as u32 != resources.crtc.id))
        {
            return Err(DrmError::Invalid);
        }
        self.submit_legacy_atomic(
            &[super::atomic::Change {
                object,
                property,
                value,
            }],
            None,
            None,
            false,
        )
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
        if self.seat_owned_primary && self.seat_revoked.load(Ordering::Acquire) {
            return Err(LinuxError::ENODEV.into());
        }
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
        if self.seat_owned_primary && self.seat_revoked.load(Ordering::Acquire) {
            return IoEvents::HANGUP | IoEvents::ERROR;
        }
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
        if self.seat_owned_primary && self.seat_revoked.load(Ordering::Acquire) {
            return Err(LinuxError::ENODEV.into());
        }
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
        self.require_live_seat_lease()?;
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
        device.primary_session_leases.remove(&self.id);
        if device.master == Some(self.id) {
            device.master = None;
        }
        remove_owned_framebuffers(&mut device, self.id);
        drop(device);
        let released_handles = {
            let state = self.state.lock();
            state
                .handles
                .values()
                .map(|object| (object.size, object.render_blob_mem.is_some()))
                .collect::<alloc::vec::Vec<_>>()
        };
        for (bytes, blob) in released_handles {
            self.device.gem_handle_closed(bytes, blob);
        }
        if self.render_node
            && let Some(adapter) = self.device.render.clone()
        {
            let (context, resources) = {
                let mut state = self.state.lock();
                state.render_cancelled.store(true, Ordering::Release);
                let context = state.render_context.take();
                let resources = state
                    .handles
                    .values()
                    .filter_map(|o| o.render_resource)
                    .collect::<alloc::vec::Vec<_>>();
                (context, resources)
            };
            if let Some(context) = context {
                self.device.render_context_closed();
                adapter.cancel_context(context);
                // cancel_context resets the render transport and ends every
                // in-flight job before returning.  A reset invalidates host
                // context state, so issuing detach/destroy afterwards would
                // race a stale host context rather than improve cleanup.
                let _ = resources;
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
        fn present(&self, _: Scanout) -> DrmResult<Arc<super::super::fence::Fence>> {
            Ok(super::super::fence::Fence::new(true))
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
        let _context = crate::test_support::scheduler_test_context();
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
        let resources = file.resources();
        let source = 8 << 16;
        file.submit_legacy_atomic(
            &[
                super::super::atomic::Change {
                    object: resources.connector.id,
                    property: super::super::property::CONNECTOR_CRTC_ID,
                    value: resources.crtc.id as u64,
                },
                super::super::atomic::Change {
                    object: resources.crtc.id,
                    property: super::super::property::CRTC_ACTIVE,
                    value: 1,
                },
                super::super::atomic::Change {
                    object: resources.crtc.id,
                    property: super::super::property::CRTC_MODE_ID,
                    value: 0,
                },
                super::super::atomic::Change {
                    object: resources.primary_plane_id,
                    property: super::super::property::PLANE_FB_ID,
                    value: fb as u64,
                },
                super::super::atomic::Change {
                    object: resources.primary_plane_id,
                    property: super::super::property::PLANE_CRTC_ID,
                    value: resources.crtc.id as u64,
                },
                super::super::atomic::Change {
                    object: resources.primary_plane_id,
                    property: super::super::property::PLANE_SRC_W,
                    value: source,
                },
                super::super::atomic::Change {
                    object: resources.primary_plane_id,
                    property: super::super::property::PLANE_SRC_H,
                    value: source,
                },
                super::super::atomic::Change {
                    object: resources.primary_plane_id,
                    property: super::super::property::PLANE_CRTC_W,
                    value: 8,
                },
                super::super::atomic::Change {
                    object: resources.primary_plane_id,
                    property: super::super::property::PLANE_CRTC_H,
                    value: 8,
                },
            ],
            Some(mode),
            None,
            false,
        )
        .unwrap();
        file.submit_legacy_atomic(
            &[super::super::atomic::Change {
                object: resources.primary_plane_id,
                property: super::super::property::PLANE_FB_ID,
                value: fb as u64,
            }],
            None,
            Some(7),
            false,
        )
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
        assert!(object.fence().is_err());

        let original = super::fence::Fence::new(false);
        object.import_fence(original.clone());
        assert!(!original.is_signaled());

        original.signal();
        assert!(original.is_signaled());
        object.reset();
        assert!(object.fence().is_err());
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
