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

/// Tunable policy boundary for bounded damage coalescing. Keep it explicit so
/// platform measurements can adjust it without changing clip semantics.
pub const DAMAGE_FULL_THRESHOLD_NUMERATOR: u64 = 1;
pub const DAMAGE_FULL_THRESHOLD_DENOMINATOR: u64 = 2;

use super::{
    fence::Fence,
    gem::{DumbRequest, GemBacking, MmapOffset},
    kms::{CrtcInfo, Framebuffer, FramebufferId, KmsResources, Mode},
};

pub type DrmResult<T> = Result<T, DrmError>;

/// Read-only accounting exported through the graphics debug endpoint.  Values
/// are deliberately object/queue counts, never transport completion records:
/// sampling them must not consume or otherwise change GPU progress.
#[derive(Clone, Copy, Default)]
pub struct AdapterMetrics {
    pub resources: u64,
    pub retired_2d: u64,
    pub retired_render: u64,
    pub render_jobs: u64,
    pub render_pending: u64,
    pub present_jobs: u64,
    pub cursor_jobs: u64,
    pub final_2d_leaks: u64,
    pub final_render_leaks: u64,
}

#[derive(Clone, Copy, Default)]
pub struct DrmMetrics {
    pub open_ofds: u64,
    pub gem_handles: u64,
    pub gem_handle_bytes: u64,
    pub resource_blobs: u64,
    pub resource_blob_bytes: u64,
    pub framebuffers: u64,
    pub property_blobs: u64,
    pub property_blob_bytes: u64,
    pub render_contexts: u64,
    pub pending_atomic_commits: u64,
    pub pending_vblank_events: u64,
    pub atomic_commits: u64,
    pub vblanks: u64,
    pub adapter: AdapterMetrics,
}

#[derive(Default)]
pub(crate) struct DrmTelemetry {
    gem_handles: AtomicU64,
    gem_handle_bytes: AtomicU64,
    resource_blobs: AtomicU64,
    resource_blob_bytes: AtomicU64,
    render_contexts: AtomicU64,
    atomic_commits: AtomicU64,
    vblanks: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrmError {
    Invalid,
    NotFound,
    PermissionDenied,
    Busy,
    NoMemory,
    QueueFull,
    /// The sole GPU has reset or been removed. Existing external VMA leases
    /// are revoked and new submission/mmap admission must fail terminally.
    DeviceLost,
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
            DrmError::DeviceLost => AxError::Io,
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
/// lock held. Presentation returns a fence which becomes terminal only after
/// the host has consumed the scanout command.
pub trait DisplayAdapter: Send + Sync {
    fn create_dumb(
        &self,
        request: DumbRequest,
        pitch: u32,
        size: u64,
    ) -> DrmResult<Arc<dyn GemBacking>>;
    fn present(&self, scanout: Scanout) -> DrmResult<Arc<Fence>>;
    /// A lock-bounded snapshot. Implementations must not drain queues or
    /// advance fences while reporting these values.
    fn metrics(&self) -> AdapterMetrics {
        AdapterMetrics::default()
    }
    /// Cursor submission is separate from scanout presentation.  The caller
    /// keeps `backing` alive until this method has consumed the exact cursor
    /// queue completion.
    fn update_cursor(&self, cursor: CursorUpdate) -> DrmResult<Arc<Fence>> {
        let _ = cursor;
        Err(DrmError::Unsupported)
    }
    fn move_cursor(&self, x: i32, y: i32) -> DrmResult<Arc<Fence>> {
        let _ = (x, y);
        Err(DrmError::Unsupported)
    }
    fn display_config_changed(&self) -> DrmResult<Option<DisplayConfig>> {
        Ok(None)
    }
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
    /// DRM_FORMAT_XRGB8888 or DRM_FORMAT_ARGB8888.  The VirtIO adapter maps
    /// this explicitly for SET_SCANOUT_BLOB.
    pub format: u32,
    /// Complete framebuffer dimensions, distinct from the visible CRTC
    /// rectangle below.  Blob scanout needs both for its plane layout.
    pub framebuffer_width: u32,
    pub framebuffer_height: u32,
    /// Byte size of the backing at framebuffer creation time.
    pub backing_size: u64,
    /// Base byte offset of the framebuffer plane (before SRC_X/Y).  This is
    /// separately required by SET_SCANOUT_BLOB's plane offset field.
    pub framebuffer_offset: u64,
    /// First visible byte in the GEM backing. The adapter must submit this as
    /// a real transport region, never merely retain it as metadata.
    pub offset: u64,
    pub source_x: u32,
    pub source_y: u32,
    pub mode: Mode,
    /// `None` requests a full visible-region transfer. A bounded rectangle is
    /// expressed in resource coordinates, not CRTC coordinates.
    pub damage: Option<DamageRect>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayConfig {
    pub connected: bool,
    pub mode: Option<Mode>,
}

#[derive(Clone)]
pub struct CursorUpdate {
    pub backing: Arc<dyn GemBacking>,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub x: i32,
    pub y: i32,
}

pub struct DrmDevice {
    pub(crate) adapter: Arc<dyn DisplayAdapter>,
    pub(crate) render: Option<Arc<dyn super::render::RenderAdapter>>,
    pub(crate) state: Mutex<DeviceState>,
    vblank_waiters: WaitQueue,
    worker_started: AtomicBool,
    worker_epoch: AtomicU64,
    telemetry: DrmTelemetry,
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
    /// DRM property blobs are device objects, not per-file scratch records.
    /// `destroyed` only drops the creator's reference; queued and installed
    /// atomic state keeps the payload alive until it is no longer referenced.
    pub(crate) next_property_blob: u32,
    pub(crate) property_blobs: BTreeMap<u32, PropertyBlob>,
    pub(crate) atomic: super::atomic::State,
    pub(crate) atomic_owner: Option<u64>,
    pub(crate) atomic_tail: super::atomic::State,
    pub(crate) atomic_generation: u64,
    /// Once the generation counter is exhausted, no new proposal may be
    /// accepted: reusing a generation would permit an ABA stale enqueue.
    pub(crate) atomic_generation_poisoned: bool,
    pub(crate) pending_fb_pins: BTreeMap<FramebufferId, usize>,
    /// Revocation bits for session-owned primary OFDs.  fbdev's private OFD
    /// is deliberately not registered here: it is the rollback console.
    pub(crate) primary_session_leases: BTreeMap<u64, super::file::SeatLease>,
    /// Seat release closes this gate before cancelling queued commits.  A
    /// compositor cannot race a VT/logind release by enqueuing another KMS
    /// update between cancellation and master revocation.
    pub(crate) kms_suspended: bool,
    pending_commits: VecDeque<AtomicCommit>,
    pending_vblanks: VecDeque<VblankEvent>,
}

pub(crate) struct PropertyBlob {
    pub(crate) bytes: Vec<u8>,
    pub(crate) references: usize,
    pub(crate) destroyed: bool,
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
        let edid = default_edid(preferred_mode);
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
                        edid_blob: 1,
                    },
                    encoder_id,
                    crtc: CrtcInfo {
                        id: crtc_id,
                        mode: None,
                        framebuffer: None,
                    },
                    primary_plane_id,
                    cursor_plane_id: primary_plane_id.checked_add(1).unwrap_or(primary_plane_id),
                    preferred_mode,
                    modes: alloc::vec![preferred_mode],
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
                next_property_blob: 2,
                property_blobs: BTreeMap::from([(
                    1,
                    PropertyBlob {
                        bytes: edid,
                        // The connector owns its immutable EDID for the
                        // lifetime of the device.
                        references: 1,
                        destroyed: true,
                    },
                )]),
                atomic: super::atomic::initial(&KmsResources {
                    connector: super::kms::ConnectorInfo {
                        id: connector_id,
                        connected: true,
                        edid_blob: 1,
                    },
                    encoder_id,
                    crtc: CrtcInfo {
                        id: crtc_id,
                        mode: None,
                        framebuffer: None,
                    },
                    primary_plane_id,
                    cursor_plane_id: primary_plane_id.checked_add(1).unwrap_or(primary_plane_id),
                    preferred_mode,
                    modes: alloc::vec![preferred_mode],
                }),
                atomic_owner: None,
                atomic_tail: super::atomic::initial(&KmsResources {
                    connector: super::kms::ConnectorInfo {
                        id: connector_id,
                        connected: true,
                        edid_blob: 1,
                    },
                    encoder_id,
                    crtc: CrtcInfo {
                        id: crtc_id,
                        mode: None,
                        framebuffer: None,
                    },
                    primary_plane_id,
                    cursor_plane_id: primary_plane_id.checked_add(1).unwrap_or(primary_plane_id),
                    preferred_mode,
                    modes: alloc::vec![preferred_mode],
                }),
                atomic_generation: 0,
                atomic_generation_poisoned: false,
                pending_fb_pins: BTreeMap::new(),
                primary_session_leases: BTreeMap::new(),
                kms_suspended: false,
                pending_commits: VecDeque::new(),
                pending_vblanks: VecDeque::new(),
            }),
            vblank_waiters: WaitQueue::new(),
            worker_started: AtomicBool::new(false),
            worker_epoch: AtomicU64::new(0),
            telemetry: DrmTelemetry::default(),
        })
    }

    /// Factory for a primary-node OFD. Devfs supplies its own open policy.
    pub fn open_primary(self: &Arc<Self>) -> super::DrmFile {
        self.open_primary_with_seat_lease(true)
    }

    /// fbdev owns the sole rollback scanout and must survive session revoke.
    pub(crate) fn open_fbdev_primary(self: &Arc<Self>) -> super::DrmFile {
        self.open_primary_with_seat_lease(false)
    }

    fn open_primary_with_seat_lease(self: &Arc<Self>, session_owned: bool) -> super::DrmFile {
        let id = {
            let mut state = self.state.lock();
            let id = state.next_open;
            state.next_open = state.next_open.wrapping_add(1).max(1);
            state.open_ids.insert(id);
            id
        };
        let file = super::DrmFile::new(Arc::clone(self), id, false, session_owned);
        if session_owned {
            self.state
                .lock()
                .primary_session_leases
                .insert(id, file.seat_lease());
        }
        file
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
        Ok(super::DrmFile::new(Arc::clone(self), id, true, false))
    }

    pub fn has_render(&self) -> bool {
        self.render.is_some()
    }

    pub fn preferred_mode(&self) -> Mode {
        self.state.lock().resources.preferred_mode
    }

    pub fn metrics(&self) -> DrmMetrics {
        let (
            open_ofds,
            framebuffers,
            property_blobs,
            property_blob_bytes,
            pending_atomic_commits,
            pending_vblank_events,
        ) = {
            let state = self.state.lock();
            let property_blob_bytes = state.property_blobs.values().fold(0u64, |total, blob| {
                total.saturating_add(blob.bytes.len() as u64)
            });
            (
                state.open_ids.len() as u64,
                state.framebuffers.len() as u64,
                state.property_blobs.len() as u64,
                property_blob_bytes,
                state.pending_commits.len() as u64,
                state.pending_vblanks.len() as u64,
            )
        };
        DrmMetrics {
            open_ofds,
            gem_handles: self.telemetry.gem_handles.load(Ordering::Acquire),
            gem_handle_bytes: self.telemetry.gem_handle_bytes.load(Ordering::Acquire),
            resource_blobs: self.telemetry.resource_blobs.load(Ordering::Acquire),
            resource_blob_bytes: self.telemetry.resource_blob_bytes.load(Ordering::Acquire),
            framebuffers,
            property_blobs,
            property_blob_bytes,
            render_contexts: self.telemetry.render_contexts.load(Ordering::Acquire),
            pending_atomic_commits,
            pending_vblank_events,
            atomic_commits: self.telemetry.atomic_commits.load(Ordering::Acquire),
            vblanks: self.telemetry.vblanks.load(Ordering::Acquire),
            adapter: self.adapter.metrics(),
        }
    }

    pub(crate) fn gem_handle_opened(&self, bytes: u64, blob: bool) {
        self.telemetry.gem_handles.fetch_add(1, Ordering::Relaxed);
        self.telemetry
            .gem_handle_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        if blob {
            self.telemetry
                .resource_blobs
                .fetch_add(1, Ordering::Relaxed);
            self.telemetry
                .resource_blob_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub(crate) fn gem_handle_closed(&self, bytes: u64, blob: bool) {
        self.telemetry.gem_handles.fetch_sub(1, Ordering::Relaxed);
        self.telemetry
            .gem_handle_bytes
            .fetch_sub(bytes, Ordering::Relaxed);
        if blob {
            self.telemetry
                .resource_blobs
                .fetch_sub(1, Ordering::Relaxed);
            self.telemetry
                .resource_blob_bytes
                .fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    pub(crate) fn render_context_opened(&self) {
        self.telemetry
            .render_contexts
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn render_context_closed(&self) {
        self.telemetry
            .render_contexts
            .fetch_sub(1, Ordering::Relaxed);
    }

    /// Convert the checked FB_DAMAGE_CLIPS blob to one resource-space union.
    /// A full transfer is selected once the coalesced box covers at least half
    /// the visible frame; this bounds bookkeeping while avoiding pathological
    /// wide unions. The blob is copied under the device lock before enqueue.
    pub(crate) fn damage_for_atomic(
        &self,
        atomic: super::atomic::State,
        fb: Option<&Framebuffer>,
    ) -> DrmResult<Option<DamageRect>> {
        let Some(fb) = fb else { return Ok(None) };
        if atomic.damage_clips_blob == 0 {
            return Ok(None);
        }
        let bytes = self
            .state
            .lock()
            .property_blobs
            .get(&atomic.damage_clips_blob)
            .filter(|blob| !blob.destroyed)
            .map(|blob| blob.bytes.clone())
            .ok_or(DrmError::NotFound)?;
        let visible_x = atomic.src_x >> 16;
        let visible_y = atomic.src_y >> 16;
        let visible_w = atomic.crtc_w;
        let visible_h = atomic.crtc_h;
        let visible_x2 = visible_x.checked_add(visible_w).ok_or(DrmError::Overflow)?;
        let visible_y2 = visible_y.checked_add(visible_h).ok_or(DrmError::Overflow)?;
        let mut union: Option<(u32, u32, u32, u32)> = None;
        for clip in bytes.chunks_exact(8) {
            let x1 = u32::try_from(i16::from_ne_bytes([clip[0], clip[1]]))
                .map_err(|_| DrmError::Invalid)?;
            let y1 = u32::try_from(i16::from_ne_bytes([clip[2], clip[3]]))
                .map_err(|_| DrmError::Invalid)?;
            let x2 = u32::try_from(i16::from_ne_bytes([clip[4], clip[5]]))
                .map_err(|_| DrmError::Invalid)?;
            let y2 = u32::try_from(i16::from_ne_bytes([clip[6], clip[7]]))
                .map_err(|_| DrmError::Invalid)?;
            let x1 = x1.max(visible_x);
            let y1 = y1.max(visible_y);
            let x2 = x2.min(visible_x2);
            let y2 = y2.min(visible_y2);
            if x1 >= x2 || y1 >= y2 {
                continue;
            }
            union = Some(match union {
                Some((left, top, right, bottom)) => {
                    (left.min(x1), top.min(y1), right.max(x2), bottom.max(y2))
                }
                None => (x1, y1, x2, y2),
            });
        }
        let Some((x1, y1, x2, y2)) = union else {
            // VirtIO has no no-op present command. Conservatively keep the
            // full visible update semantics for an empty clip list.
            return Ok(None);
        };
        let width = x2 - x1;
        let height = y2 - y1;
        let union_area = u64::from(width) * u64::from(height);
        let visible_area = u64::from(visible_w) * u64::from(visible_h);
        if union_area.saturating_mul(DAMAGE_FULL_THRESHOLD_DENOMINATOR)
            >= visible_area.saturating_mul(DAMAGE_FULL_THRESHOLD_NUMERATOR)
        {
            return Ok(None);
        }
        let base_y =
            u32::try_from(fb.offset / u64::from(fb.pitch)).map_err(|_| DrmError::Overflow)?;
        Ok(Some(DamageRect {
            x: x1,
            y: base_y.checked_add(y1).ok_or(DrmError::Overflow)?,
            width,
            height,
        }))
    }

    /// Update the single connector's hotplug state.  Transport code owns the
    /// policy for notifying userspace; this method only changes the KMS object
    /// state after that transport has established the new physical state.
    pub fn set_connector_connected(&self, connected: bool) -> DrmResult<()> {
        let mut state = self.state.lock();
        if connected && state.resources.modes.is_empty() {
            return Err(DrmError::Invalid);
        }
        state.resources.connector.connected = connected;
        replace_connector_edid(&mut state)?;
        Ok(())
    }

    /// Replace the connector's complete advertised mode list.  An empty list
    /// is valid for a disconnected connector; a connected connector always
    /// needs at least one concrete mode.
    pub fn set_connector_modes(&self, modes: Vec<Mode>) -> DrmResult<()> {
        if modes
            .iter()
            .any(|mode| mode.width == 0 || mode.height == 0 || mode.refresh_millihz == 0)
        {
            return Err(DrmError::Invalid);
        }
        let mut state = self.state.lock();
        if modes.is_empty() && state.resources.connector.connected {
            return Err(DrmError::Invalid);
        }
        if let Some(preferred) = modes.first().copied() {
            state.resources.preferred_mode = preferred;
        }
        state.resources.modes = modes;
        replace_connector_edid(&mut state)?;
        Ok(())
    }

    pub(crate) fn create_property_blob(&self, bytes: Vec<u8>) -> DrmResult<u32> {
        let mut state = self.state.lock();
        let id = state.next_property_blob;
        state.next_property_blob = id.checked_add(1).ok_or(DrmError::Overflow)?;
        state.property_blobs.insert(
            id,
            PropertyBlob {
                bytes,
                references: 0,
                destroyed: false,
            },
        );
        Ok(id)
    }

    pub(crate) fn property_blob(&self, id: u32) -> Option<Vec<u8>> {
        self.state
            .lock()
            .property_blobs
            .get(&id)
            .map(|blob| blob.bytes.clone())
    }

    pub(crate) fn live_property_blob(&self, id: u32) -> Option<Vec<u8>> {
        self.state
            .lock()
            .property_blobs
            .get(&id)
            .filter(|blob| !blob.destroyed)
            .map(|blob| blob.bytes.clone())
    }

    pub(crate) fn destroy_property_blob(&self, id: u32) -> DrmResult<()> {
        let mut state = self.state.lock();
        let blob = state
            .property_blobs
            .get_mut(&id)
            .ok_or(DrmError::NotFound)?;
        if blob.destroyed {
            return Err(DrmError::NotFound);
        }
        blob.destroyed = true;
        if blob.references == 0 {
            state.property_blobs.remove(&id);
        }
        Ok(())
    }

    pub(crate) fn queue_atomic_with_completion(
        self: &Arc<Self>,
        mut job: AtomicCommit,
        generation: u64,
    ) -> DrmResult<Arc<CommitCompletion>> {
        let completion = Arc::new(CommitCompletion::new());
        job.completion = Some(Arc::clone(&completion));
        self.enqueue_atomic(job, generation)?;
        Ok(completion)
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
        if state.kms_suspended {
            job.discard_event();
            return Err(DrmError::Busy);
        }
        if state.atomic_generation_poisoned {
            job.discard_event();
            return Err(DrmError::Overflow);
        }
        if state.atomic_generation != generation {
            job.discard_event();
            return Err(DrmError::Busy);
        }
        // A master can be dropped after atomic validation but before this
        // commit is enqueued.  The device lock serializes that transition with
        // this final authorization check, so an old open never publishes a
        // modeset after it has lost mastership.
        if state.master != Some(job.owner) || !state.open_ids.contains(&job.owner) {
            job.discard_event();
            return Err(DrmError::PermissionDenied);
        }
        if state.pending_commits.len() == 64 {
            job.discard_event();
            return Err(DrmError::QueueFull);
        }
        hold_state_blobs(&mut state, job.next)?;
        state.advance_atomic_generation()?;
        state.atomic_tail = job.next;
        if let Some(fb) = job.next.active.then_some(job.next.fb) {
            *state.pending_fb_pins.entry(fb).or_insert(0) += 1;
        }
        state.pending_commits.push_back(job);
        self.telemetry
            .atomic_commits
            .fetch_add(1, Ordering::Relaxed);
        if let Err(error) = self.ensure_vblank_worker() {
            let job = state.pending_commits.pop_back().unwrap();
            self.telemetry
                .atomic_commits
                .fetch_sub(1, Ordering::Relaxed);
            if let Some(fb) = job.next.active.then_some(job.next.fb) {
                decrement_pin(&mut state.pending_fb_pins, fb);
            }
            release_state_blobs(&mut state, job.next);
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
        #[cfg(test)]
        {
            self.worker_started.store(true, Ordering::Release);
            Ok(())
        }

        #[cfg(not(test))]
        {
            if self.worker_started.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            let device = Arc::clone(self);
            if axtask::try_spawn_with_name(move || vblank_worker(device), "drm-vblank".into())
                .is_err()
            {
                self.worker_started.store(false, Ordering::Release);
                return Err(DrmError::NoMemory);
            }
            Ok(())
        }
    }

    /// Returns whether this vblank made the job terminal.  A successful host
    /// present is intentionally observed on the first following vblank, not
    /// in the transport completion context.
    fn complete_atomic(&self, job: &mut AtomicCommit, sequence: u64) -> DrmResult<bool> {
        if !job.cancellation.try_begin_delivery() {
            job.discard_event();
            job.signal_scanout_error();
            return Ok(true);
        }
        if job
            .input_fences
            .iter()
            .chain(&job.reservation_predecessors)
            .any(|fence| fence.is_failed())
        {
            job.discard_event();
            job.cancellation.end_delivery();
            return Err(DrmError::Busy);
        }
        if !job
            .input_fences
            .iter()
            .chain(&job.reservation_predecessors)
            .all(|fence| fence.is_signaled())
        {
            job.cancellation.end_delivery();
            return Ok(false);
        }
        if job.next.active && !self.state.lock().framebuffers.contains_key(&job.next.fb) {
            job.discard_event();
            job.cancellation.end_delivery();
            return if job.cancellation.is_closed() {
                Ok(true)
            } else {
                Err(DrmError::NotFound)
            };
        }
        let scanout = match job.scanout() {
            Ok(scanout) => scanout,
            Err(error) => {
                job.discard_event();
                job.cancellation.end_delivery();
                return Err(error);
            }
        };
        if !job.cursor_submitted {
            if let Some(cursor) = job.cursor.clone() {
                let fence = match self.adapter.update_cursor(cursor) {
                    Ok(fence) => fence,
                    Err(error) => {
                        job.discard_event();
                        job.cancellation.end_delivery();
                        return Err(error);
                    }
                };
                job.cursor_fence = Some(fence);
                job.cursor_target = sequence.checked_add(1).ok_or(DrmError::Overflow)?;
            }
            job.cursor_submitted = true;
            if job.cursor_fence.is_some() {
                job.cancellation.end_delivery();
                return Ok(false);
            }
        }
        if let Some(fence) = &job.cursor_fence {
            if !fence.is_signaled() {
                job.cancellation.end_delivery();
                return Ok(false);
            }
            if fence.is_failed() {
                job.discard_event();
                job.cancellation.end_delivery();
                return Err(DrmError::Busy);
            }
            if sequence < job.cursor_target {
                job.cancellation.end_delivery();
                return Ok(false);
            }
        }
        if let Some(scanout) = scanout
            && job.present.is_none()
        {
            let present = match self.adapter.present(scanout) {
                Ok(present) => present,
                Err(error) => {
                    job.discard_event();
                    job.cancellation.end_delivery();
                    return Err(error);
                }
            };
            let target = match sequence.checked_add(1) {
                Some(target) => target,
                None => {
                    job.discard_event();
                    job.cancellation.end_delivery();
                    return Err(DrmError::Overflow);
                }
            };
            job.present = Some(present);
            job.present_target = target;
            job.cancellation.end_delivery();
            return Ok(false);
        }
        if let Some(present) = &job.present {
            if !present.is_signaled() {
                job.cancellation.end_delivery();
                return Ok(false);
            }
            if present.is_failed() {
                job.discard_event();
                job.cancellation.end_delivery();
                return Err(DrmError::Busy);
            }
            if sequence < job.present_target {
                job.cancellation.end_delivery();
                return Ok(false);
            }
        }
        if !self.publish_atomic(&job) {
            // A file can close while the adapter presents an already-validated
            // scanout.  Do not revive its framebuffer after Drop removes it.
            job.discard_event();
            job.signal_scanout_error();
            job.cancellation.end_delivery();
            return Ok(true);
        }
        if let Some(fence) = &job.scanout_fence {
            fence.signal();
        }
        if let Some(token) = job.event {
            job.cancellation
                .complete(token, sequence, axhal::time::monotonic_time_nanos() / 1_000);
        }
        job.cancellation.end_delivery();
        Ok(true)
    }

    /// Publishes a completed atomic job only while its file and framebuffer
    /// still exist.  This is deliberately checked under the device lock: file
    /// teardown removes framebuffers under the same lock.
    fn publish_atomic(&self, job: &AtomicCommit) -> bool {
        let mut state = self.state.lock();
        if job.cancellation.is_closed()
            || state.master != Some(job.owner)
            || !state.open_ids.contains(&job.owner)
            || (job.next.active && !state.framebuffers.contains_key(&job.next.fb))
        {
            state.rebuild_atomic_tail();
            return false;
        }
        let previous = state.atomic;
        state.atomic = job.next;
        state.atomic_owner = Some(job.owner);
        hold_state_blobs(&mut state, job.next).expect("validated atomic blob lost before publish");
        release_state_blobs(&mut state, previous);
        if previous.gamma_lut_blob != job.next.gamma_lut_blob {
            apply_gamma_lut(&mut state, job.next.gamma_lut_blob)
                .expect("validated gamma LUT blob changed before publish");
        }
        state.rebuild_atomic_tail();
        state.resources.crtc.mode = job.next.mode;
        state.resources.crtc.framebuffer = job.next.active.then_some(job.next.fb);
        true
    }

    pub(crate) fn wait_for_vblank(self: &Arc<Self>) -> DrmResult<u64> {
        let epoch = self.worker_epoch.load(Ordering::Acquire);
        self.ensure_vblank_worker()?;
        let seen = self.state.lock().vblank;
        self.vblank_waiters
            .wait_until(|| {
                self.state.lock().vblank != seen
                    || self.worker_epoch.load(Ordering::Acquire) != epoch
            })
            .map_err(|_| DrmError::Busy)?;
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
        self.vblank_waiters
            .wait_until(|| {
                self.state.lock().vblank >= target
                    || self.worker_epoch.load(Ordering::Acquire) != epoch
            })
            .map_err(|_| DrmError::Busy)?;
        if self.worker_epoch.load(Ordering::Acquire) != epoch {
            return Err(DrmError::Busy);
        }
        Ok(self.state.lock().vblank)
    }
}

impl DrmDevice {
    /// Seat release gate.  The gate is set before draining, so all queued
    /// atomic completion waiters receive an error and no new primary-node KMS
    /// work can enter until the matching acquire.
    pub(crate) fn suspend_kms_for_seat(&self) {
        let leases = {
            let mut state = self.state.lock();
            state.kms_suspended = true;
            if state
                .master
                .is_some_and(|master| state.primary_session_leases.contains_key(&master))
            {
                state.master = None;
            }
            state
                .primary_session_leases
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        for lease in leases {
            lease.revoke();
        }
        self.cancel_pending_commits();
    }

    pub(crate) fn resume_kms_for_seat(&self) {
        self.state.lock().kms_suspended = false;
    }

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
    fn release_commit_blobs(&self, atomic: super::atomic::State) {
        release_state_blobs(&mut self.state.lock(), atomic);
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
        if device.advance_vblank().is_err() {
            device.cancel_pending_commits();
        }
    }
}

impl DrmDevice {
    /// Advances one vblank and completes the first queued atomic commit.
    ///
    /// The worker calls this after each hardware-timer wake.  Keeping the
    /// transition separate also lets host unit tests exercise the real commit
    /// completion path without pretending the dummy platform has a clock.
    pub(crate) fn advance_vblank(&self) -> DrmResult<()> {
        self.refresh_display_config()?;
        let (sequence, job, events) = {
            let mut state = self.state.lock();
            state.vblank = state.vblank.wrapping_add(1);
            self.telemetry.vblanks.fetch_add(1, Ordering::Relaxed);
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
        self.vblank_waiters.notify_all(true);
        let timestamp_us = axhal::time::monotonic_time_nanos() / 1_000;
        for event in events {
            event
                .queue
                .complete_vblank(event.token, sequence, timestamp_us);
        }
        if let Some(mut job) = job {
            let result = if job.cancellation.is_closed() {
                job.discard_event();
                job.signal_scanout_error();
                Ok(true)
            } else {
                self.complete_atomic(&mut job, sequence)
            };
            match result {
                Ok(false) => self.state.lock().pending_commits.push_front(job),
                Ok(true) => {
                    if let Some(completion) = job.completion {
                        completion.complete(Ok(()));
                    }
                    self.unpin_framebuffer(job.next.active.then_some(job.next.fb));
                    self.release_commit_blobs(job.next);
                }
                Err(error) => {
                    job.signal_scanout_error();
                    if let Some(completion) = job.completion {
                        completion.complete(Err(error));
                    }
                    self.unpin_framebuffer(job.next.active.then_some(job.next.fb));
                    self.release_commit_blobs(job.next);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Consume a post-IRQ VirtIO display sample as one KMS-state transition.
    /// Mode/EDID replacement and invalidation of queued stale commits share
    /// the device lock, so no old mode can become visible after hotplug.
    fn refresh_display_config(&self) -> DrmResult<()> {
        let Some(config) = self.adapter.display_config_changed()? else {
            return Ok(());
        };
        let stale = {
            let mut state = self.state.lock();
            let modes = config.mode.into_iter().collect::<Vec<_>>();
            if config.connected && modes.is_empty() {
                return Err(DrmError::Invalid);
            }
            let unchanged = state.resources.connector.connected == config.connected
                && state.resources.modes == modes;
            if unchanged {
                return Ok(());
            }
            state.resources.connector.connected = config.connected;
            if let Some(mode) = modes.first().copied() {
                state.resources.preferred_mode = mode;
            }
            state.resources.modes = modes;
            replace_connector_edid(&mut state)?;
            state.advance_atomic_generation()?;
            state.atomic_tail = state.atomic;
            core::mem::take(&mut state.pending_commits)
        };
        for job in stale {
            job.discard_event();
            job.signal_scanout_error();
            if let Some(done) = job.completion {
                done.complete(Err(DrmError::Busy));
            }
            self.unpin_framebuffer(job.next.active.then_some(job.next.fb));
            self.release_commit_blobs(job.next);
        }
        let _ = crate::file::netlink::emit_init_net_kobject_uevent(
            "change",
            "/devices/virtual/drm/card0",
            "drm",
            &[("HOTPLUG", "1")],
        );
        Ok(())
    }
}

impl DrmDevice {
    fn cancel_pending_commits(&self) {
        let commits = {
            let mut state = self.state.lock();
            let commits = core::mem::take(&mut state.pending_commits);
            for job in &commits {
                release_state_blobs(&mut state, job.next);
            }
            state.rebuild_atomic_tail();
            state.poison_on_generation_overflow();
            commits
        };
        for job in commits {
            job.discard_event();
            job.signal_scanout_error();
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
            for job in &drained.0 {
                release_state_blobs(&mut state, job.next);
            }
            state.rebuild_atomic_tail();
            state.poison_on_generation_overflow();
            drained
        };
        for job in commits {
            job.discard_event();
            job.signal_scanout_error();
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
                    release_state_blobs(&mut state, job.next);
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
            job.signal_scanout_error();
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

fn hold_state_blobs(state: &mut DeviceState, atomic: super::atomic::State) -> DrmResult<()> {
    let mut held = [0u32; 3];
    let mut held_count = 0;
    for id in super::atomic::referenced_blobs(&atomic) {
        if id == 0 || held[..held_count].contains(&id) {
            continue;
        }
        let Some(blob) = state.property_blobs.get_mut(&id) else {
            for id in &held[..held_count] {
                release_blob(state, *id);
            }
            return Err(DrmError::NotFound);
        };
        let Some(references) = blob.references.checked_add(1) else {
            for id in &held[..held_count] {
                release_blob(state, *id);
            }
            return Err(DrmError::Overflow);
        };
        blob.references = references;
        held[held_count] = id;
        held_count += 1;
    }
    Ok(())
}

fn release_state_blobs(state: &mut DeviceState, atomic: super::atomic::State) {
    let mut released = [0u32; 3];
    let mut released_count = 0;
    for id in super::atomic::referenced_blobs(&atomic) {
        if id != 0 && !released[..released_count].contains(&id) {
            release_blob(state, id);
            released[released_count] = id;
            released_count += 1;
        }
    }
}

fn release_blob(state: &mut DeviceState, id: u32) {
    let remove = if let Some(blob) = state.property_blobs.get_mut(&id) {
        debug_assert!(blob.references != 0, "property blob reference underflow");
        blob.references = blob.references.saturating_sub(1);
        blob.references == 0 && blob.destroyed
    } else {
        false
    };
    if remove {
        state.property_blobs.remove(&id);
    }
}

fn default_edid(mode: Mode) -> Vec<u8> {
    // A minimal EDID 1.4 base block whose detailed timing reflects the
    // adapter's preferred mode.  The virtual connector has no physical size,
    // so the remaining descriptive fields intentionally remain zero.
    let mut edid = alloc::vec![0u8; 128];
    edid[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    edid[8..10].copy_from_slice(&0x544bu16.to_be_bytes()); // "TK"
    edid[10..12].copy_from_slice(&1u16.to_le_bytes());
    edid[16] = 1;
    edid[17] = 1;
    edid[18] = 1;
    edid[19] = 4;
    edid[20] = 0x80;
    // Detailed-timing descriptor: only advertise values representable by the
    // EDID 12-bit active-size fields.  Larger virtual modes still remain in
    // the connector mode list but do not falsify this base EDID descriptor.
    if mode.width <= 4095 && mode.height <= 4095 {
        let pixel_clock_10khz = (u64::from(mode.width)
            .saturating_mul(u64::from(mode.height))
            .saturating_mul(u64::from(mode.refresh_millihz))
            / 10_000_000)
            .min(u16::MAX as u64) as u16;
        edid[54..56].copy_from_slice(&pixel_clock_10khz.to_le_bytes());
        edid[56] = mode.width as u8;
        edid[58] = ((mode.width >> 8) as u8) << 4;
        edid[59] = mode.height as u8;
        edid[61] = ((mode.height >> 8) as u8) << 4;
    }
    edid[126] = 0;
    edid[127] = (0u8).wrapping_sub(
        edid[..127]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
    );
    edid
}

fn replace_connector_edid(state: &mut DeviceState) -> DrmResult<()> {
    let id = state.next_property_blob;
    state.next_property_blob = id.checked_add(1).ok_or(DrmError::Overflow)?;
    state.property_blobs.insert(
        id,
        PropertyBlob {
            bytes: default_edid(state.resources.preferred_mode),
            // The connector owns this immutable blob until it is replaced.
            references: 1,
            destroyed: true,
        },
    );
    let previous = core::mem::replace(&mut state.resources.connector.edid_blob, id);
    release_blob(state, previous);
    Ok(())
}

fn apply_gamma_lut(state: &mut DeviceState, blob_id: u32) -> DrmResult<()> {
    if blob_id == 0 {
        for (index, triplet) in state.gamma_lut.chunks_exact_mut(3).enumerate() {
            let value = (index * 257) as u16;
            triplet.copy_from_slice(&[value, value, value]);
        }
        return Ok(());
    }
    let blob = state
        .property_blobs
        .get(&blob_id)
        .ok_or(DrmError::NotFound)?;
    if blob.bytes.len() != state.gamma_lut.len() / 3 * 8 {
        return Err(DrmError::Invalid);
    }
    for (destination, source) in state
        .gamma_lut
        .chunks_exact_mut(3)
        .zip(blob.bytes.chunks_exact(8))
    {
        destination[0] = u16::from_ne_bytes([source[0], source[1]]);
        destination[1] = u16::from_ne_bytes([source[2], source[3]]);
        destination[2] = u16::from_ne_bytes([source[4], source[5]]);
    }
    Ok(())
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
    pub(crate) fn is_complete(&self) -> bool {
        self.result.lock().is_some()
    }
    pub(crate) fn wait(&self) -> DrmResult<()> {
        // The host test platform intentionally has no advancing hardware
        // clock. Tests explicitly advance the device before entering here;
        // production completion always comes from the vblank worker.
        self.waiters
            .wait_until(|| self.result.lock().is_some())
            .map_err(|_| DrmError::Busy)?;
        self.result.lock().unwrap()
    }
}

#[derive(Clone)]
pub(crate) struct AtomicCommit {
    pub(crate) owner: u64,
    pub(crate) next: super::atomic::State,
    pub(crate) fb: Option<Framebuffer>,
    pub(crate) cancellation: Arc<super::file::EventQueue>,
    pub(crate) event: Option<u64>,
    pub(crate) completion: Option<Arc<CommitCompletion>>,
    pub(crate) present: Option<Arc<Fence>>,
    pub(crate) present_target: u64,
    pub(crate) damage: Option<DamageRect>,
    pub(crate) cursor: Option<CursorUpdate>,
    /// Real DRM framebuffer reference retained through cursor host terminal
    /// completion; unlike a GEM handle this survives handle close/reuse.
    pub(crate) cursor_fb: Option<Framebuffer>,
    pub(crate) cursor_submitted: bool,
    pub(crate) cursor_fence: Option<Arc<Fence>>,
    pub(crate) cursor_target: u64,
    /// Imported plane fence(s) and the framebuffer reservation snapshot are
    /// both waited before touching either VirtIO queue.
    pub(crate) input_fences: Vec<Arc<Fence>>,
    pub(crate) reservation_predecessors: Vec<Arc<Fence>>,
    /// The CRTC OUT_FENCE sync_file, if requested. It becomes successful only
    /// after host completion and atomic state publication at target vblank.
    pub(crate) scanout_fence: Option<Arc<Fence>>,
}

pub(crate) struct VblankEvent {
    pub(crate) target: u64,
    pub(crate) token: u64,
    pub(crate) queue: Arc<super::file::EventQueue>,
}

impl AtomicCommit {
    fn signal_scanout_error(&self) {
        if let Some(fence) = &self.scanout_fence {
            fence.signal_error();
        }
    }

    fn scanout(&self) -> DrmResult<Option<Scanout>> {
        if !self.next.active || self.next.dpms != super::atomic::DPMS_ON {
            return Ok(None);
        }
        self.fb.as_ref().map_or(Ok(None), |fb| {
            let mode = self.next.mode.ok_or(DrmError::Invalid)?;
            Ok(Some(Scanout {
                backing: Arc::clone(&fb.object.backing),
                width: self.next.crtc_w,
                height: self.next.crtc_h,
                pitch: fb.pitch,
                bpp: fb.bpp,
                format: fb.format,
                framebuffer_width: fb.width,
                framebuffer_height: fb.height,
                backing_size: fb.object.size,
                framebuffer_offset: fb.offset,
                offset: fb
                    .offset
                    .checked_add(u64::from(self.next.src_y >> 16) * u64::from(fb.pitch))
                    .and_then(|value| {
                        value.checked_add(u64::from(self.next.src_x >> 16) * u64::from(fb.bpp / 8))
                    })
                    .ok_or(DrmError::Overflow)?,
                source_x: u32::try_from(fb.offset % u64::from(fb.pitch) / u64::from(fb.bpp / 8))
                    .map_err(|_| DrmError::Overflow)?
                    .checked_add(self.next.src_x >> 16)
                    .ok_or(DrmError::Overflow)?,
                source_y: u32::try_from(
                    fb.offset / u64::from(fb.pitch) + u64::from(self.next.src_y >> 16),
                )
                .map_err(|_| DrmError::Overflow)?,
                mode,
                damage: self.damage,
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
    if request.width == 0
        || request.height == 0
        || request.bpp == 0
        || !request.bpp.is_multiple_of(8)
    {
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
    if state.atomic_owner == Some(owner)
        || (state.atomic.active && !state.framebuffers.contains_key(&state.atomic.fb))
    {
        let installed = state.atomic;
        state.atomic = super::atomic::initial(&state.resources);
        state.atomic_owner = None;
        release_state_blobs(state, installed);
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
    use crate::drm::gem::GemObject;

    struct Adapter;
    impl DisplayAdapter for Adapter {
        fn create_dumb(&self, _: DumbRequest, _: u32, _: u64) -> DrmResult<Arc<dyn GemBacking>> {
            Err(DrmError::Unsupported)
        }
        fn present(&self, _: Scanout) -> DrmResult<Arc<Fence>> {
            Ok(Fence::new(true))
        }
    }

    struct Backing;
    impl GemBacking for Backing {
        fn shared_pages(&self) -> DrmResult<Arc<crate::mm::SharedPages>> {
            Err(DrmError::Unsupported)
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
                owner: 1,
                next,
                fb: None,
                cancellation: Arc::clone(&first),
                event: Some(token),
                completion: None,
                present: None,
                present_target: 0,
                damage: None,
                cursor: None,
                cursor_fb: None,
                cursor_submitted: false,
                cursor_fence: None,
                cursor_target: 0,
                input_fences: Vec::new(),
                reservation_predecessors: Vec::new(),
                scanout_fence: None,
            });
            let atomic = state.atomic;
            state.pending_commits.push_back(AtomicCommit {
                owner: 2,
                next: atomic,
                fb: None,
                cancellation: Arc::clone(&second),
                event: None,
                completion: None,
                present: None,
                present_target: 0,
                damage: None,
                cursor: None,
                cursor_fb: None,
                cursor_submitted: false,
                cursor_fence: None,
                cursor_target: 0,
                input_fences: Vec::new(),
                reservation_predecessors: Vec::new(),
                scanout_fence: None,
            });
        }
        device.cancel_file_commits(&first);
        let state = device.state.lock();
        assert_eq!(state.pending_commits.len(), 1);
        assert!(!state.pending_fb_pins.contains_key(&7));
        assert_eq!(state.atomic_tail.fb, state.atomic.fb);
    }

    #[test]
    fn closed_in_flight_job_does_not_republish_removed_framebuffer() {
        let device = DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4);
        let queue = super::super::file::EventQueue::new();
        let mut next = super::super::atomic::initial(&device.state.lock().resources);
        next.active = true;
        next.fb = 7;
        {
            let mut state = device.state.lock();
            state.framebuffers.insert(
                next.fb,
                Framebuffer {
                    owner: 1,
                    handle: 1,
                    object: Arc::new(GemObject::new(Arc::new(Backing), 1, 0)),
                    width: 1,
                    height: 1,
                    pitch: 64,
                    bpp: 32,
                    format: 0x3432_5258,
                    offset: 0,
                },
            );
        }
        let job = AtomicCommit {
            owner: 1,
            next,
            fb: None,
            cancellation: Arc::clone(&queue),
            event: None,
            completion: None,
            present: None,
            present_target: 0,
            damage: None,
            cursor: None,
            cursor_fb: None,
            cursor_submitted: false,
            cursor_fence: None,
            cursor_target: 0,
            input_fences: Vec::new(),
            reservation_predecessors: Vec::new(),
            scanout_fence: None,
        };

        assert!(queue.try_begin_delivery());
        queue.begin_close();
        remove_owned_framebuffers(&mut device.state.lock(), 1);
        assert!(!device.publish_atomic(&job));
        queue.end_delivery();

        let state = device.state.lock();
        assert!(!state.atomic.active);
        assert_eq!(state.resources.crtc.framebuffer, None);
        assert_eq!(state.resources.crtc.mode, None);
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
