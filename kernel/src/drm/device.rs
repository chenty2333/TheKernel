use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    vec::Vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

use axerrno::AxError;
use axtask::WaitQueue;
use spin::Mutex;

use super::{
    gem::{DumbRequest, GemBacking, MmapOffset},
    kms::{CrtcInfo, Framebuffer, FramebufferId, KmsResources, Mode},
};

pub type DrmResult<T> = Result<T, DrmError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrmError {
    Invalid,
    NotFound,
    PermissionDenied,
    Busy,
    NoMemory,
    QueueFull,
    Overflow,
    Unsupported,
}

impl From<DrmError> for AxError {
    fn from(error: DrmError) -> Self {
        match error {
            DrmError::Invalid | DrmError::Overflow => AxError::InvalidInput,
            DrmError::NotFound => AxError::NotFound,
            DrmError::PermissionDenied => AxError::PermissionDenied,
            DrmError::Busy => AxError::ResourceBusy,
            DrmError::NoMemory => AxError::NoMemory,
            DrmError::QueueFull => AxError::WouldBlock,
            DrmError::Unsupported => AxError::OperationNotSupported,
        }
    }
}

impl fmt::Display for DrmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// A driver adapter owns transport/hardware state. Calls happen with no core
/// lock held, so an implementation may complete a virtio command synchronously.
pub trait DisplayAdapter: Send + Sync {
    fn create_dumb(
        &self,
        request: DumbRequest,
        pitch: u32,
        size: u64,
    ) -> DrmResult<Arc<dyn GemBacking>>;
    fn present(&self, scanout: Scanout) -> DrmResult<()>;
    fn preferred_mode(&self) -> Mode {
        Mode {
            width: 1024,
            height: 768,
            refresh_millihz: 60_000,
        }
    }
}

static PRIMARY_DEVICE: Mutex<Option<Arc<DrmDevice>>> = Mutex::new(None);

/// Publishes the single primary device after its transport is ready.
pub fn register_primary_device(device: Arc<DrmDevice>) -> DrmResult<()> {
    let mut primary = PRIMARY_DEVICE.lock();
    if primary.is_some() {
        return Err(DrmError::Busy);
    }
    *primary = Some(device);
    Ok(())
}

/// Returns the primary device for devfs construction, if a GPU registered.
pub fn primary_device() -> Option<Arc<DrmDevice>> {
    PRIMARY_DEVICE.lock().as_ref().map(Arc::clone)
}

#[derive(Clone)]
pub struct Scanout {
    pub backing: Arc<dyn GemBacking>,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub mode: Mode,
}

pub struct DrmDevice {
    pub(crate) adapter: Arc<dyn DisplayAdapter>,
    pub(crate) render: Option<Arc<dyn super::render::RenderAdapter>>,
    pub(crate) state: Mutex<DeviceState>,
    vblank_waiters: WaitQueue,
    worker_started: AtomicBool,
    worker_epoch: AtomicU64,
}

pub(crate) struct DeviceState {
    pub(crate) next_open: u64,
    pub(crate) open_ids: BTreeSet<u64>,
    pub(crate) next_mmap_offset: MmapOffset,
    pub(crate) master: Option<u64>,
    pub(crate) resources: KmsResources,
    pub(crate) framebuffers: BTreeMap<FramebufferId, Framebuffer>,
    pub(crate) next_framebuffer: FramebufferId,
    pub(crate) vblank: u64,
    /// One software gamma LUT for the sole virtual CRTC, stored as RGB
    /// triplets in the legacy DRM 16-bit component representation.
    pub(crate) gamma_lut: Vec<u16>,
    pub(crate) atomic: super::atomic::State,
    pub(crate) atomic_tail: super::atomic::State,
    pub(crate) atomic_generation: u64,
    /// Once the generation counter is exhausted, no new proposal may be
    /// accepted: reusing a generation would permit an ABA stale enqueue.
    pub(crate) atomic_generation_poisoned: bool,
    pub(crate) pending_fb_pins: BTreeMap<FramebufferId, usize>,
    pending_commits: VecDeque<AtomicCommit>,
    pending_vblanks: VecDeque<VblankEvent>,
}
impl DeviceState {
    fn rebuild_atomic_tail(&mut self) {
        self.atomic_tail = self
            .pending_commits
            .back()
            .map_or(self.atomic, |job| job.next);
    }
}

impl DrmDevice {
    pub fn new(
        adapter: Arc<dyn DisplayAdapter>,
        connector_id: u32,
        encoder_id: u32,
        crtc_id: u32,
        primary_plane_id: u32,
    ) -> Arc<Self> {
        Self::with_render(
            adapter,
            None,
            connector_id,
            encoder_id,
            crtc_id,
            primary_plane_id,
        )
    }

    pub fn with_render(
        adapter: Arc<dyn DisplayAdapter>,
        render: Option<Arc<dyn super::render::RenderAdapter>>,
        connector_id: u32,
        encoder_id: u32,
        crtc_id: u32,
        primary_plane_id: u32,
    ) -> Arc<Self> {
        let preferred_mode = adapter.preferred_mode();
        Arc::new(Self {
            adapter,
            render,
            state: Mutex::new(DeviceState {
                next_open: 1,
                open_ids: BTreeSet::new(),
                next_mmap_offset: 4096,
                master: None,
                resources: KmsResources {
                    connector: super::kms::ConnectorInfo {
                        id: connector_id,
                        connected: true,
                    },
                    encoder_id,
                    crtc: CrtcInfo {
                        id: crtc_id,
                        mode: None,
                        framebuffer: None,
                    },
                    primary_plane_id,
                    preferred_mode,
                },
                framebuffers: BTreeMap::new(),
                next_framebuffer: 1,
                vblank: 0,
                gamma_lut: (0..256)
                    .flat_map(|index| {
                        let value = (index * 257) as u16;
                        [value, value, value]
                    })
                    .collect(),
                atomic: super::atomic::initial(&KmsResources {
                    connector: super::kms::ConnectorInfo {
                        id: connector_id,
                        connected: true,
                    },
                    encoder_id,
                    crtc: CrtcInfo {
                        id: crtc_id,
                        mode: None,
                        framebuffer: None,
                    },
                    primary_plane_id,
                    preferred_mode,
                }),
                atomic_tail: super::atomic::initial(&KmsResources {
                    connector: super::kms::ConnectorInfo {
                        id: connector_id,
                        connected: true,
                    },
                    encoder_id,
                    crtc: CrtcInfo {
                        id: crtc_id,
                        mode: None,
                        framebuffer: None,
                    },
                    primary_plane_id,
                    preferred_mode,
                }),
                atomic_generation: 0,
                atomic_generation_poisoned: false,
                pending_fb_pins: BTreeMap::new(),
                pending_commits: VecDeque::new(),
                pending_vblanks: VecDeque::new(),
            }),
            vblank_waiters: WaitQueue::new(),
            worker_started: AtomicBool::new(false),
            worker_epoch: AtomicU64::new(0),
        })
    }

    /// Factory for a primary-node OFD. Devfs supplies its own open policy.
    pub fn open_primary(self: &Arc<Self>) -> super::DrmFile {
        let id = {
            let mut state = self.state.lock();
            let id = state.next_open;
            state.next_open = state.next_open.wrapping_add(1).max(1);
            state.open_ids.insert(id);
            id
        };
        super::DrmFile::new(Arc::clone(self), id, false)
    }

    /// Factory for an unprivileged render-node OFD.  It exists only when the
    /// transport negotiated the legacy VIRGL capability.
    pub fn open_render(self: &Arc<Self>) -> DrmResult<super::DrmFile> {
        if self.render.is_none() {
            return Err(DrmError::Unsupported);
        }
        let id = {
            let mut state = self.state.lock();
            let id = state.next_open;
            state.next_open = state.next_open.wrapping_add(1).max(1);
            state.open_ids.insert(id);
            id
        };
        Ok(super::DrmFile::new(Arc::clone(self), id, true))
    }

    pub fn has_render(&self) -> bool {
        self.render.is_some()
    }

    pub fn preferred_mode(&self) -> Mode {
        self.state.lock().resources.preferred_mode
    }

    pub(crate) fn commit_atomic(
        self: &Arc<Self>,
        mut job: AtomicCommit,
        generation: u64,
    ) -> DrmResult<()> {
        let completion = Arc::new(CommitCompletion::new());
        job.completion = Some(Arc::clone(&completion));
        self.enqueue_atomic(job, generation)?;
        completion.wait()
    }

    pub(crate) fn queue_atomic(
        self: &Arc<Self>,
        job: AtomicCommit,
        generation: u64,
    ) -> DrmResult<()> {
        self.enqueue_atomic(job, generation)
    }

    fn enqueue_atomic(self: &Arc<Self>, job: AtomicCommit, generation: u64) -> DrmResult<()> {
        self.ensure_vblank_worker()?;
        let mut state = self.state.lock();
        if state.atomic_generation_poisoned {
            job.discard_event();
            return Err(DrmError::Overflow);
        }
        if state.atomic_generation != generation {
            job.discard_event();
            return Err(DrmError::Busy);
        }
        if state.pending_commits.len() == 64 {
            job.discard_event();
            return Err(DrmError::QueueFull);
        }
        state.advance_atomic_generation()?;
        state.atomic_tail = job.next;
        if let Some(fb) = job.next.active.then_some(job.next.fb) {
            *state.pending_fb_pins.entry(fb).or_insert(0) += 1;
        }
        state.pending_commits.push_back(job);
        if let Err(error) = self.ensure_vblank_worker() {
            let job = state.pending_commits.pop_back().unwrap();
            if let Some(fb) = job.next.active.then_some(job.next.fb) {
                decrement_pin(&mut state.pending_fb_pins, fb);
            }
            state.atomic_tail = state
                .pending_commits
                .back()
                .map_or(state.atomic, |job| job.next);
            job.discard_event();
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn queue_vblank_event(self: &Arc<Self>, event: VblankEvent) -> DrmResult<()> {
        self.ensure_vblank_worker()?;
        let mut state = self.state.lock();
        if state.pending_vblanks.len() == 64 {
            event.queue.discard(event.token);
            return Err(DrmError::QueueFull);
        }
        state.pending_vblanks.push_back(event);
        state
            .pending_vblanks
            .make_contiguous()
            .sort_by_key(|event| event.target);
        if let Err(error) = self.ensure_vblank_worker() {
            let event = state.pending_vblanks.pop_back().unwrap();
            event.queue.discard(event.token);
            return Err(error);
        }
        Ok(())
    }

    fn ensure_vblank_worker(self: &Arc<Self>) -> DrmResult<()> {
        if self.worker_started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let device = Arc::clone(self);
        if axtask::try_spawn_with_name(move || vblank_worker(device), "drm-vblank".into()).is_err()
        {
            self.worker_started.store(false, Ordering::Release);
            return Err(DrmError::NoMemory);
        }
        Ok(())
    }

    fn complete_atomic(&self, job: AtomicCommit, sequence: u64) -> DrmResult<()> {
        if !job.cancellation.try_begin_delivery() {
            job.discard_event();
            self.discard_atomic_head();
            return Ok(());
        }
        if job.next.active && !self.state.lock().framebuffers.contains_key(&job.next.fb) {
            job.discard_event();
            job.cancellation.end_delivery();
            return Err(DrmError::NotFound);
        }
        let scanout = match job.scanout() {
            Ok(scanout) => scanout,
            Err(error) => {
                job.discard_event();
                job.cancellation.end_delivery();
                return Err(error);
            }
        };
        if let Err(error) = scanout.map_or(Ok(()), |scanout| self.adapter.present(scanout)) {
            job.discard_event();
            job.cancellation.end_delivery();
            return Err(error);
        }
        {
            let mut state = self.state.lock();
            state.atomic = job.next;
            state.rebuild_atomic_tail();
            state.resources.crtc.mode = job.next.mode;
            state.resources.crtc.framebuffer = job.next.active.then_some(job.next.fb);
        }
        if let Some(token) = job.event {
            job.cancellation
                .complete(token, sequence, axhal::time::monotonic_time_nanos() / 1_000);
        }
        job.cancellation.end_delivery();
        Ok(())
    }

    pub(crate) fn wait_for_vblank(self: &Arc<Self>) -> DrmResult<u64> {
        let epoch = self.worker_epoch.load(Ordering::Acquire);
        self.ensure_vblank_worker()?;
        let seen = self.state.lock().vblank;
        self.vblank_waiters.wait_until(|| {
            self.state.lock().vblank != seen || self.worker_epoch.load(Ordering::Acquire) != epoch
        });
        if self.worker_epoch.load(Ordering::Acquire) != epoch {
            return Err(DrmError::Busy);
        }
        Ok(self.state.lock().vblank)
    }
    pub(crate) fn vblank_sequence(&self) -> u64 {
        self.state.lock().vblank
    }
    pub(crate) fn wait_for_vblank_at_least(self: &Arc<Self>, target: u64) -> DrmResult<u64> {
        let epoch = self.worker_epoch.load(Ordering::Acquire);
        self.ensure_vblank_worker()?;
        self.vblank_waiters.wait_until(|| {
            self.state.lock().vblank >= target || self.worker_epoch.load(Ordering::Acquire) != epoch
        });
        if self.worker_epoch.load(Ordering::Acquire) != epoch {
            return Err(DrmError::Busy);
        }
        Ok(self.state.lock().vblank)
    }
}

impl DrmDevice {
    fn discard_atomic_head(&self) {
        let mut state = self.state.lock();
        state.rebuild_atomic_tail();
        state.poison_on_generation_overflow();
    }

    fn unpin_framebuffer(&self, fb: Option<FramebufferId>) {
        if let Some(fb) = fb {
            let mut state = self.state.lock();
            decrement_pin(&mut state.pending_fb_pins, fb);
        }
    }
    pub(crate) fn framebuffer_pinned(&self, id: FramebufferId) -> bool {
        self.state.lock().pending_fb_pins.contains_key(&id)
    }
}

fn vblank_worker(device: Arc<DrmDevice>) {
    loop {
        let refresh = device
            .state
            .lock()
            .resources
            .preferred_mode
            .refresh_millihz
            .max(1);
        let delay = Duration::from_nanos(1_000_000_000_000u64 / u64::from(refresh));
        if !matches!(
            axtask::future::block_on(axtask::future::sleep(delay)),
            Ok(Ok(()))
        ) {
            device.worker_failed();
            return;
        }
        let (sequence, job, events) = {
            let mut state = device.state.lock();
            state.vblank = state.vblank.wrapping_add(1);
            let sequence = state.vblank;
            let mut events = Vec::new();
            while state
                .pending_vblanks
                .front()
                .is_some_and(|event| event.target <= sequence)
            {
                events.push(state.pending_vblanks.pop_front().unwrap());
            }
            (sequence, state.pending_commits.pop_front(), events)
        };
        device.vblank_waiters.notify_all(true);
        let timestamp_us = axhal::time::monotonic_time_nanos() / 1_000;
        for event in events {
            event
                .queue
                .complete_vblank(event.token, sequence, timestamp_us);
        }
        if let Some(job) = job {
            let result = if job.cancellation.is_closed() {
                job.discard_event();
                device.discard_atomic_head();
                Ok(())
            } else {
                device.complete_atomic(job.clone(), sequence)
            };
            if let Some(completion) = job.completion {
                completion.complete(result);
            }
            device.unpin_framebuffer(job.next.active.then_some(job.next.fb));
            if result.is_err() {
                device.cancel_pending_commits();
            }
        }
    }
}

impl DrmDevice {
    fn cancel_pending_commits(&self) {
        let commits = {
            let mut state = self.state.lock();
            let commits = core::mem::take(&mut state.pending_commits);
            state.rebuild_atomic_tail();
            state.poison_on_generation_overflow();
            commits
        };
        for job in commits {
            job.discard_event();
            self.unpin_framebuffer(job.next.active.then_some(job.next.fb));
            if let Some(done) = job.completion {
                done.complete(Err(DrmError::Busy));
            }
        }
    }
    fn worker_failed(&self) {
        self.worker_epoch.fetch_add(1, Ordering::AcqRel);
        self.worker_started.store(false, Ordering::Release);
        let (commits, events) = {
            let mut state = self.state.lock();
            let drained = (
                core::mem::take(&mut state.pending_commits),
                core::mem::take(&mut state.pending_vblanks),
            );
            state.rebuild_atomic_tail();
            state.poison_on_generation_overflow();
            drained
        };
        for job in commits {
            job.discard_event();
            self.unpin_framebuffer(job.next.active.then_some(job.next.fb));
            if let Some(done) = job.completion {
                done.complete(Err(DrmError::Busy));
            }
        }
        for event in events {
            event.queue.discard(event.token);
        }
        self.vblank_waiters.notify_all(true);
    }
}

impl DrmDevice {
    /// Cancels work owned by a closing file before its framebuffer namespace is
    /// removed.  The device lock is the linearization point with worker pop.
    pub(crate) fn cancel_file_commits(&self, queue: &Arc<super::file::EventQueue>) {
        let (commits, events) = {
            let mut state = self.state.lock();
            let mut commits = Vec::new();
            let mut remaining = VecDeque::new();
            while let Some(job) = state.pending_commits.pop_front() {
                if Arc::ptr_eq(&job.cancellation, queue) {
                    if let Some(fb) = job.next.active.then_some(job.next.fb) {
                        decrement_pin(&mut state.pending_fb_pins, fb);
                    }
                    commits.push(job);
                } else {
                    remaining.push_back(job);
                }
            }
            state.pending_commits = remaining;
            let mut events = Vec::new();
            let mut remaining_events = VecDeque::new();
            while let Some(event) = state.pending_vblanks.pop_front() {
                if Arc::ptr_eq(&event.queue, queue) {
                    events.push(event);
                } else {
                    remaining_events.push_back(event);
                }
            }
            state.pending_vblanks = remaining_events;
            state.rebuild_atomic_tail();
            state.poison_on_generation_overflow();
            (commits, events)
        };
        for job in commits {
            job.discard_event();
            if let Some(done) = job.completion {
                done.complete(Err(DrmError::Busy));
            }
        }
        for event in events {
            event.queue.discard(event.token);
        }
    }
}

fn decrement_pin(pins: &mut BTreeMap<FramebufferId, usize>, fb: FramebufferId) {
    if let Some(count) = pins.get_mut(&fb) {
        *count -= 1;
        if *count == 0 {
            pins.remove(&fb);
        }
    }
}

pub(crate) struct CommitCompletion {
    result: Mutex<Option<DrmResult<()>>>,
    waiters: WaitQueue,
}
impl CommitCompletion {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            waiters: WaitQueue::new(),
        }
    }
    fn complete(&self, result: DrmResult<()>) {
        *self.result.lock() = Some(result);
        self.waiters.notify_all(true);
    }
    fn wait(&self) -> DrmResult<()> {
        self.waiters.wait_until(|| self.result.lock().is_some());
        self.result.lock().unwrap()
    }
}

#[derive(Clone)]
pub(crate) struct AtomicCommit {
    pub(crate) next: super::atomic::State,
    pub(crate) fb: Option<Framebuffer>,
    pub(crate) cancellation: Arc<super::file::EventQueue>,
    pub(crate) event: Option<u64>,
    pub(crate) completion: Option<Arc<CommitCompletion>>,
}

pub(crate) struct VblankEvent {
    pub(crate) target: u64,
    pub(crate) token: u64,
    pub(crate) queue: Arc<super::file::EventQueue>,
}

impl AtomicCommit {
    fn scanout(&self) -> DrmResult<Option<Scanout>> {
        self.fb.as_ref().map_or(Ok(None), |fb| {
            let mode = self.next.mode.ok_or(DrmError::Invalid)?;
            Ok(Some(Scanout {
                backing: Arc::clone(&fb.object.backing),
                width: fb.width,
                height: fb.height,
                pitch: fb.pitch,
                bpp: fb.bpp,
                mode,
            }))
        })
    }
    fn discard_event(&self) {
        if let Some(token) = self.event {
            self.cancellation.discard(token);
        }
    }
}

pub(crate) fn dumb_layout(request: DumbRequest) -> DrmResult<(u32, u64)> {
    if request.width == 0 || request.height == 0 || request.bpp == 0 || request.bpp % 8 != 0 {
        return Err(DrmError::Invalid);
    }
    let bytes = request.bpp / 8;
    let unaligned = request.width.checked_mul(bytes).ok_or(DrmError::Overflow)?;
    let pitch = unaligned.checked_add(63).ok_or(DrmError::Overflow)? & !63;
    let size = (pitch as u64)
        .checked_mul(request.height as u64)
        .ok_or(DrmError::Overflow)?;
    Ok((pitch, size))
}

pub(crate) fn remove_owned_framebuffers(state: &mut DeviceState, owner: u64) {
    let count = state.framebuffers.len();
    state.framebuffers.retain(|_, fb| fb.owner != owner);
    if state.framebuffers.len() != count {
        state.poison_on_generation_overflow();
    }
    if state.atomic.active && !state.framebuffers.contains_key(&state.atomic.fb) {
        state.atomic.active = false;
        state.atomic.mode = None;
        state.atomic.mode_blob = 0;
        state.atomic.fb = 0;
        state.atomic.src_w = 0;
        state.atomic.src_h = 0;
        state.atomic.crtc_w = 0;
        state.atomic.crtc_h = 0;
    }
    state.rebuild_atomic_tail();
    state.resources.crtc.framebuffer = state.atomic.active.then_some(state.atomic.fb);
    state.resources.crtc.mode = state.atomic.mode;
}

impl DeviceState {
    pub(crate) fn advance_atomic_generation(&mut self) -> DrmResult<()> {
        if self.atomic_generation_poisoned {
            return Err(DrmError::Overflow);
        }
        self.atomic_generation = self.atomic_generation.checked_add(1).ok_or_else(|| {
            self.atomic_generation_poisoned = true;
            DrmError::Overflow
        })?;
        Ok(())
    }

    fn poison_on_generation_overflow(&mut self) {
        let _ = self.advance_atomic_generation();
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::*;

    struct Adapter;
    impl DisplayAdapter for Adapter {
        fn create_dumb(&self, _: DumbRequest, _: u32, _: u64) -> DrmResult<Arc<dyn GemBacking>> {
            Err(DrmError::Unsupported)
        }
        fn present(&self, _: Scanout) -> DrmResult<()> {
            Ok(())
        }
    }

    #[test]
    fn closing_file_drains_only_its_jobs_and_releases_its_pin() {
        let device = DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4);
        let first = super::super::file::EventQueue::new();
        let second = super::super::file::EventQueue::new();
        let token = first.reserve(1).unwrap();
        let mut next = super::super::atomic::initial(&device.state.lock().resources);
        next.active = true;
        next.fb = 7;
        {
            let mut state = device.state.lock();
            state.atomic_tail = next;
            state.pending_fb_pins.insert(7, 1);
            state.pending_commits.push_back(AtomicCommit {
                next,
                fb: None,
                cancellation: Arc::clone(&first),
                event: Some(token),
                completion: None,
            });
            let atomic = state.atomic;
            state.pending_commits.push_back(AtomicCommit {
                next: atomic,
                fb: None,
                cancellation: Arc::clone(&second),
                event: None,
                completion: None,
            });
        }
        device.cancel_file_commits(&first);
        let state = device.state.lock();
        assert_eq!(state.pending_commits.len(), 1);
        assert!(!state.pending_fb_pins.contains_key(&7));
        assert_eq!(state.atomic_tail.fb, state.atomic.fb);
    }

    #[test]
    fn generation_overflow_poison_prevents_aba_reuse() {
        let device = DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4);
        let mut state = device.state.lock();
        state.atomic_generation = u64::MAX;
        assert_eq!(state.advance_atomic_generation(), Err(DrmError::Overflow));
        assert!(state.atomic_generation_poisoned);
        assert_eq!(state.advance_atomic_generation(), Err(DrmError::Overflow));
    }
}
