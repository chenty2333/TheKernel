//! DRM transport adapter for the sole VirtIO GPU selected by `axdisplay`.

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};

use axdriver_display::{
    BlobMem as DriverBlobMem, BlobResource as DriverBlobResource, ContextInit as DriverContextInit,
    DevError, DisplayDriverOps, DrmCursorUpdate as DriverCursorUpdate, GpuBatch as DriverGpuBatch,
    GpuCompletion as DriverGpuCompletion, GpuCompletionData as DriverGpuCompletionData,
    GpuFeatures as DriverGpuFeatures, GpuQueue as DriverGpuQueue,
    GpuSubmission as DriverGpuSubmission, RenderResource3D, RenderTransfer3D,
};
use axhal::paging::PageSize;
use axsync::Mutex;
use hashbrown::HashMap;
use memory_addr::PhysAddr;

use super::{
    AdapterMetrics, DisplayAdapter, DrmDevice, DrmError, DrmResult, DumbRequest, GemBacking,
    RenderAdapter, Scanout,
    device::{CursorUpdate, DamageRect, DisplayConfig},
    fence::Fence,
    render::{BlobMem, BlobResource, ContextInit, RenderJob, RenderResource, RenderTransfer},
};
use crate::mm::{ExternalPageLease, SharedPages, checked_align_up};

trait GpuTransport: Send {
    fn modern_features(&mut self) -> DriverGpuFeatures {
        DriverGpuFeatures::empty()
    }
    fn host_visible_len(&mut self) -> Option<u64> {
        None
    }
    fn preferred_mode(&self) -> (u32, u32) {
        (1024, 768)
    }
    /// The sole asynchronous command boundary. The batch owns all payloads;
    /// lower layers retain request/response DMA until a matching terminal
    /// completion is drained from this exact queue.
    fn submit(
        &mut self,
        queue: DriverGpuQueue,
        batch: DriverGpuBatch,
        fence_id: u64,
    ) -> Result<DriverGpuSubmission, DevError>;
    fn drain_completions(
        &mut self,
        queue: DriverGpuQueue,
        out: &mut [DriverGpuCompletion],
    ) -> Result<usize, DevError>;
    fn reset(&mut self, queue: DriverGpuQueue, out: &mut [DriverGpuCompletion]) -> usize;
    fn display_config_changed(&mut self) -> Result<Option<DisplayConfig>, DevError> {
        Ok(None)
    }
}

struct DisplayTransport(Box<dyn DisplayDriverOps>);

impl GpuTransport for DisplayTransport {
    fn modern_features(&mut self) -> DriverGpuFeatures {
        self.0
            .render_transport()
            .map_or(DriverGpuFeatures::empty(), |transport| {
                transport.modern_features()
            })
    }
    fn host_visible_len(&mut self) -> Option<u64> {
        self.0
            .render_transport()
            .and_then(|transport| transport.host_visible_len())
    }
    fn preferred_mode(&self) -> (u32, u32) {
        let info = self.0.info();
        (info.width, info.height)
    }
    fn submit(
        &mut self,
        queue: DriverGpuQueue,
        batch: DriverGpuBatch,
        fence_id: u64,
    ) -> Result<DriverGpuSubmission, DevError> {
        self.0.drm_submit(queue, batch, fence_id)
    }
    fn drain_completions(
        &mut self,
        queue: DriverGpuQueue,
        out: &mut [DriverGpuCompletion],
    ) -> Result<usize, DevError> {
        self.0.drm_drain_completions(queue, out)
    }
    fn reset(&mut self, queue: DriverGpuQueue, out: &mut [DriverGpuCompletion]) -> usize {
        self.0.drm_reset(queue, out)
    }
    fn display_config_changed(&mut self) -> Result<Option<DisplayConfig>, DevError> {
        self.0.drm_display_config_changed().map(|change| {
            change.map(|change| DisplayConfig {
                connected: change.connected,
                mode: change.connected.then_some(super::Mode {
                    width: change.width,
                    height: change.height,
                    refresh_millihz: 60_000,
                }),
            })
        })
    }
}

/// The DRM-side owner of the display transport. Its backing map translates a
/// type-erased GEM object back to the VirtIO resource which owns its DMA range.
struct VirtioGpuAdapter<T: GpuTransport> {
    state: Arc<AdapterState<T>>,
}

struct AdapterState<T: GpuTransport> {
    transport: Mutex<T>,
    resources: Mutex<HashMap<usize, u32>>,
    retired_2d_resources: Mutex<Vec<Retired2dResource>>,
    retired_render_resources: Mutex<Vec<RetiredRenderResource>>,
    /// One non-droppable overflow owner per retirement class.  A full normal
    /// retry queue applies backpressure instead of abandoning the sole DMA
    /// lifetime token.
    overflow_2d_retired: Mutex<Option<Retired2dResource>>,
    overflow_render_retired: Mutex<Option<RetiredRenderResource>>,
    retirement_worker_started: AtomicBool,
    /// Weak registry of every host-visible external mapping. Reset/remove
    /// revokes them before transport/BAR teardown, while their strong lease
    /// remains pinned by extant VMAs and deferred resource retirement.
    external_leases: Mutex<Vec<ExternalLeaseRegistration>>,
    /// Monotonic, page-aligned host-visible aperture reservations.  The
    /// lower transport rejects any reservation outside its negotiated SHM.
    blob_aperture: Mutex<BlobAperture>,
    render_dead: AtomicBool,
    /// Completion records are keyed by host fence ID, not used-ring order:
    /// an unrelated completion may be observed while another waiter owns the
    /// transport lock and must remain terminal for its original waiter.
    render_completions: Mutex<HashMap<u64, Result<(), DevError>>>,
    render_pending: Mutex<HashMap<u64, ()>>,
    /// EXECBUFFER ownership stays here from ioctl admission until the exact
    /// used-ring completion is consumed and context attachments are removed.
    render_jobs: Mutex<Vec<QueuedRenderJob>>,
    lifecycle_jobs: Mutex<Vec<LifecycleJob>>,
    lifecycle_pending: Mutex<HashMap<u64, ()>>,
    lifecycle_completions: Mutex<HashMap<u64, Result<(), DevError>>>,
    /// Synchronous control callers own a token before releasing transport so
    /// a worker can never drain a completion into the wrong domain.
    sync_pending: Mutex<HashMap<u64, ()>>,
    sync_completions: Mutex<HashMap<u64, DriverGpuCompletion>>,
    resource_ready_fences: Mutex<HashMap<u32, Arc<Fence>>>,
    render_worker_started: AtomicBool,
    present_jobs: Mutex<Vec<PresentJob>>,
    present_completions: Mutex<HashMap<u64, Result<(), DevError>>>,
    present_pending: Mutex<HashMap<u64, ()>>,
    present_worker_started: AtomicBool,
    /// Per-shared-backing 2D scanout shadows for legacy virgl resources.
    /// The key is the backing Arc allocation, hence PRIME aliases reuse the
    /// same resource rather than creating divergent scanout copies.
    render_shadows: Mutex<HashMap<usize, RenderShadow>>,
    /// Fence token -> KMS/GEM lifetime pin. The lower queue owns DMA until
    /// completion; this map owns both the matching DRM fence and, for an
    /// UPDATE_CURSOR, the exact GEM backing consumed by the host command.
    cursor_jobs: Mutex<HashMap<u64, CursorJob>>,
    /// Serializes cursorq capacity reservation through fence-token
    /// publication. The completion worker observes `cursor_admitting` and
    /// never drains a token in that interval.
    cursor_admission: Mutex<()>,
    cursor_worker_started: AtomicBool,
    cursor_admitting: AtomicBool,
    final_2d_leaks: AtomicUsize,
    final_render_leaks: AtomicUsize,
}

enum QueuedRenderJob {
    Waiting(RenderJob),
    Submitted { job: RenderJob, submission: u64 },
}

/// Resource creation is a two-command ownership transaction.  The guest ID
/// is reserved before CREATE reaches the device, but its ready fence remains
/// unsignaled until CREATE and (where needed) ATTACH_BACKING have both
/// completed successfully.
enum LifecycleJob {
    Creating {
        resource: u32,
        entries: Vec<(u64, u32)>,
        pages: Arc<SharedPages>,
        ready: Arc<Fence>,
        blob: bool,
        submission: u64,
    },
    Attaching {
        resource: u32,
        pages: Arc<SharedPages>,
        ready: Arc<Fence>,
        blob: bool,
        submission: u64,
    },
}

enum PresentJob {
    Waiting {
        scanout: Scanout,
        completion: Arc<Fence>,
    },
    Submitted {
        scanout: Scanout,
        completion: Arc<Fence>,
        submission: u64,
    },
}

struct CursorJob {
    completion: Arc<Fence>,
    /// MOVE_CURSOR has no backing. UPDATE_CURSOR must retain this reference
    /// until the matching cursorq completion has become terminal.
    _backing: Option<Arc<dyn GemBacking>>,
}

struct RenderShadow {
    source: Weak<dyn GemBacking>,
    resource: u32,
    pages: Arc<SharedPages>,
    width: u32,
    height: u32,
    pitch: u32,
    context: u32,
    attached_source: Option<u32>,
    retire_stage: ShadowRetireStage,
    cleanup_pending: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ShadowRetireStage {
    Active,
    DetachSource,
    DetachBacking,
    DestroyResource,
    DestroyContext,
}

struct ShadowTransferError {
    error: DrmError,
    attached_source: Option<u32>,
}

struct Retired2dResource {
    resource: u32,
    pages: Arc<SharedPages>,
}

/// A host render resource whose backing cannot yet be freed.  The token stays
/// owned by the adapter until both host operations complete.
struct RetiredRenderResource {
    resource: u32,
    pages: Arc<SharedPages>,
    backing_attached: bool,
    stage: RenderRetireStage,
}

struct ExternalLeaseRegistration {
    lease: Weak<ExternalPageLease>,
    pages: Weak<SharedPages>,
}
struct BlobAperture {
    len: u64,
    used: BTreeMap<u64, (u32, u64)>,
    by_resource: BTreeMap<u32, u64>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RenderRetireStage {
    UnmapBlob,
    DetachBacking,
    Unref,
}

const MAX_RETIRED_RESOURCES: usize = 128;

impl<T: GpuTransport> VirtioGpuAdapter<T> {
    fn new(transport: T) -> Self {
        Self {
            state: Arc::new(AdapterState {
                transport: Mutex::new(transport),
                resources: Mutex::new(HashMap::new()),
                retired_2d_resources: Mutex::new(Vec::new()),
                retired_render_resources: Mutex::new(Vec::new()),
                overflow_2d_retired: Mutex::new(None),
                overflow_render_retired: Mutex::new(None),
                retirement_worker_started: AtomicBool::new(false),
                external_leases: Mutex::new(Vec::new()),
                blob_aperture: Mutex::new(BlobAperture {
                    len: 0,
                    used: BTreeMap::new(),
                    by_resource: BTreeMap::new(),
                }),
                render_dead: AtomicBool::new(false),
                render_completions: Mutex::new(HashMap::new()),
                render_pending: Mutex::new(HashMap::new()),
                render_jobs: Mutex::new(Vec::new()),
                lifecycle_jobs: Mutex::new(Vec::new()),
                lifecycle_pending: Mutex::new(HashMap::new()),
                lifecycle_completions: Mutex::new(HashMap::new()),
                sync_pending: Mutex::new(HashMap::new()),
                sync_completions: Mutex::new(HashMap::new()),
                resource_ready_fences: Mutex::new(HashMap::new()),
                render_worker_started: AtomicBool::new(false),
                present_jobs: Mutex::new(Vec::new()),
                present_completions: Mutex::new(HashMap::new()),
                present_pending: Mutex::new(HashMap::new()),
                present_worker_started: AtomicBool::new(false),
                render_shadows: Mutex::new(HashMap::new()),
                cursor_jobs: Mutex::new(HashMap::new()),
                cursor_admission: Mutex::new(()),
                cursor_worker_started: AtomicBool::new(false),
                cursor_admitting: AtomicBool::new(false),
                final_2d_leaks: AtomicUsize::new(0),
                final_render_leaks: AtomicUsize::new(0),
            }),
        }
    }
}

struct VirtioRenderAdapter<T: GpuTransport> {
    state: Arc<AdapterState<T>>,
}
impl<T: GpuTransport + 'static> RenderAdapter for VirtioRenderAdapter<T> {
    fn modern_features(&self) -> super::render::ModernFeatures {
        if self.state.render_dead.load(Ordering::Acquire) {
            return super::render::ModernFeatures::default();
        }
        let features = self.state.transport.lock().modern_features();
        super::render::ModernFeatures {
            resource_uuid: features.contains(DriverGpuFeatures::RESOURCE_UUID),
            resource_blob: features.contains(DriverGpuFeatures::RESOURCE_BLOB),
            context_init: features.contains(DriverGpuFeatures::CONTEXT_INIT),
            host_visible: features.contains(DriverGpuFeatures::HOST_VISIBLE),
        }
    }
    fn capset_info(&self, i: u32) -> DrmResult<(u32, u32, u32)> {
        self.state.retry_retired_render_resources();
        match self
            .state
            .submit_control_and_wait(DriverGpuBatch::CapsetInfo { index: i })?
            .1
            .data
        {
            DriverGpuCompletionData::CapsetInfo {
                id,
                max_version,
                max_size,
            } => Ok((id, max_version, max_size)),
            _ => Err(DrmError::Invalid),
        }
    }
    fn capset(&self, id: u32, v: u32, d: &mut [u8]) -> DrmResult<usize> {
        self.state.retry_retired_render_resources();
        match self
            .state
            .submit_control_and_wait(DriverGpuBatch::Capset {
                id,
                version: v,
                bytes: d.len(),
            })?
            .1
            .data
        {
            DriverGpuCompletionData::Capset(data) if data.len() == d.len() => {
                d.copy_from_slice(&data);
                Ok(d.len())
            }
            _ => Err(DrmError::Invalid),
        }
    }
    fn create_context(&self, n: &[u8]) -> DrmResult<u32> {
        self.state.retry_retired_render_resources();
        self.create_context_with_init(n, ContextInit::default())
    }
    fn create_context_with_init(&self, n: &[u8], init: ContextInit) -> DrmResult<u32> {
        self.state.retry_retired_render_resources();
        let (submission, _) =
            self.state
                .submit_control_and_wait(DriverGpuBatch::CreateContext {
                    name: n.to_vec(),
                    init: DriverContextInit {
                        capset_id: init.capset_id,
                        num_rings: init.num_rings,
                        poll_rings_mask: init.poll_rings_mask,
                        debug_name: init.debug_name,
                        debug_name_len: init.debug_name_len,
                    },
                })?;
        let context = submission.context_id.ok_or(DrmError::Invalid)?;
        Ok(context)
    }
    fn destroy_context(&self, c: u32) -> DrmResult<()> {
        self.state.retry_retired_render_resources();
        self.state
            .submit_control_and_wait(DriverGpuBatch::DestroyContext { context: c })?;
        Ok(())
    }
    fn cancel_context(&self, context: u32) {
        self.state.cancel_render_context(context);
    }
    fn create_resource(
        &self,
        r: RenderResource,
        e: &[(u64, u32)],
        pages: Arc<SharedPages>,
    ) -> DrmResult<u32> {
        if self.state.render_dead.load(Ordering::Acquire) {
            return Err(DrmError::DeviceLost);
        }
        self.state.ensure_retirement_worker()?;
        self.state.ensure_render_worker()?;
        self.state.retry_retired_render_resources();
        let entries = e.to_vec();
        self.state.reserve_lifecycle_admission()?;
        let mut transport = self.state.transport.lock();
        let submission = transport
            .submit(
                DriverGpuQueue::Control,
                DriverGpuBatch::CreateResource3d {
                    resource: RenderResource3D {
                        target: r.target,
                        format: r.format,
                        bind: r.bind,
                        width: r.width,
                        height: r.height,
                        depth: r.depth,
                        array_size: r.array_size,
                        last_level: r.last_level,
                        nr_samples: r.nr_samples,
                        flags: r.flags,
                    },
                },
                0,
            )
            .map_err(map_dev_error)?;
        let Some(id) = submission.resource_id else {
            drop(transport);
            self.state.reset_render_transport();
            return Err(DrmError::Invalid);
        };
        if let Err(error) = self.state.enqueue_resource_creation(
            id,
            entries,
            pages.clone(),
            false,
            submission.fence_id,
        ) {
            drop(transport);
            self.state.reset_render_transport();
            self.state.retire_render_resource(id, pages, false);
            return Err(error);
        }
        drop(transport);
        Ok(id)
    }
    fn resource_ready(&self, resource: u32) -> DrmResult<Arc<Fence>> {
        self.state.resource_ready(resource)
    }
    fn create_blob(
        &self,
        blob: BlobResource,
        entries: &[(u64, u32)],
        pages: Arc<SharedPages>,
    ) -> DrmResult<u32> {
        if self.state.render_dead.load(Ordering::Acquire) {
            return Err(DrmError::DeviceLost);
        }
        self.state.ensure_retirement_worker()?;
        self.state.retry_retired_render_resources();
        self.state.ensure_render_worker()?;
        let create_entries = entries.to_vec();
        let lifecycle_entries = entries.to_vec();
        self.state.reserve_lifecycle_admission()?;
        let mut transport = self.state.transport.lock();
        let submission = transport
            .submit(
                DriverGpuQueue::Control,
                DriverGpuBatch::CreateBlob {
                    resource: DriverBlobResource {
                        mem: match blob.mem {
                            BlobMem::Guest => DriverBlobMem::Guest,
                            BlobMem::Host3d => DriverBlobMem::Host3d,
                            BlobMem::Host3dGuest => DriverBlobMem::Host3dGuest,
                        },
                        flags: blob.flags,
                        size: blob.size,
                        blob_id: blob.blob_id,
                    },
                    entries: create_entries,
                },
                0,
            )
            .map_err(map_dev_error)?;
        let Some(id) = submission.resource_id else {
            drop(transport);
            self.state.reset_render_transport();
            return Err(DrmError::Invalid);
        };
        // Blob SG is part of CREATE_BLOB itself; it is never followed by the
        // legacy ATTACH_BACKING command.
        if let Err(error) = self.state.enqueue_resource_creation(
            id,
            lifecycle_entries,
            pages.clone(),
            true,
            submission.fence_id,
        ) {
            drop(transport);
            self.state.reset_render_transport();
            self.state.retire_render_resource(id, pages, false);
            return Err(error);
        }
        drop(transport);
        Ok(id)
    }
    fn map_blob(&self, resource: u32, size: u64) -> DrmResult<Arc<SharedPages>> {
        if self.state.render_dead.load(Ordering::Acquire) {
            return Err(DrmError::DeviceLost);
        }
        if let Ok(ready) = self.state.resource_ready(resource) {
            if !ready.is_signaled() {
                return Err(DrmError::QueueFull);
            }
            if ready.is_failed() {
                return Err(DrmError::DeviceLost);
            }
        }
        {
            let mut leases = self.state.external_leases.lock();
            leases
                .retain(|entry| entry.lease.strong_count() != 0 && entry.pages.strong_count() != 0);
            if leases.len() == MAX_RETIRED_RESOURCES || leases.try_reserve(1).is_err() {
                return Err(DrmError::QueueFull);
            }
        }
        self.state.ensure_render_worker()?;
        let offset = self.state.reserve_blob_aperture(resource, size)?;
        let submitted = self
            .state
            .submit_control_and_wait(DriverGpuBatch::MapBlob { resource, offset });
        let map = match submitted {
            Err(error) => {
                self.state.release_blob_aperture(resource);
                return Err(error);
            }
            Ok((_, completion)) => match completion.data {
                DriverGpuCompletionData::MapInfo(map) => map,
                _ => {
                    self.state.release_blob_aperture(resource);
                    return Err(DrmError::Invalid);
                }
            },
        };
        // Reset may have raced the protocol round trip while this mapping had
        // no lease registration yet. Never publish an external SharedPages
        // object after observing terminal removal; its resource is retained
        // by the caller's normal deferred retirement path.
        if self.state.render_dead.load(Ordering::Acquire) {
            return Err(DrmError::DeviceLost);
        }
        let mut pages: Vec<PhysAddr> = Vec::new();
        let bytes = checked_align_up(usize::try_from(size).map_err(|_| DrmError::Overflow)?, 4096)
            .ok_or(DrmError::Overflow)?;
        let count = bytes / 4096;
        pages
            .try_reserve_exact(count)
            .map_err(|_| DrmError::NoMemory)?;
        let base = usize::try_from(map.physical_base).map_err(|_| DrmError::Overflow)?;
        for index in 0..count {
            let address = base
                .checked_add(index.checked_mul(4096).ok_or(DrmError::Overflow)?)
                .ok_or(DrmError::Overflow)?;
            pages.push(PhysAddr::from(address));
        }
        let transport_owner: Arc<dyn core::any::Any + Send + Sync> = self.state.clone();
        let owner: Arc<dyn core::any::Any + Send + Sync> = Arc::new(resource);
        let lease = Arc::try_new(ExternalPageLease::new_with_transport(
            owner,
            transport_owner,
        ))
        .map_err(|_| DrmError::NoMemory)?;
        let weak_lease = Arc::downgrade(&lease);
        let pages = Arc::try_new(
            SharedPages::new_external_4k(
                pages,
                lease,
                axhal::paging::MappingFlags::DEVICE | axhal::paging::MappingFlags::UNCACHED,
            )
            .map_err(map_ax_error)?,
        )
        .map_err(|_| DrmError::NoMemory)?;
        let mut registrations = self.state.external_leases.lock();
        if self.state.render_dead.load(Ordering::Acquire) {
            return Err(DrmError::DeviceLost);
        }
        registrations.push(ExternalLeaseRegistration {
            lease: weak_lease,
            pages: Arc::downgrade(&pages),
        });
        drop(registrations);
        Ok(pages)
    }
    fn resource_uuid(&self, resource: u32) -> DrmResult<[u8; 16]> {
        if let Ok(ready) = self.state.resource_ready(resource) {
            if !ready.is_signaled() {
                return Err(DrmError::QueueFull);
            }
            if ready.is_failed() {
                return Err(DrmError::DeviceLost);
            }
        }
        match self
            .state
            .submit_control_and_wait(DriverGpuBatch::AssignUuid { resource })?
            .1
            .data
        {
            DriverGpuCompletionData::Uuid(uuid) => Ok(uuid),
            _ => Err(DrmError::Invalid),
        }
    }
    fn retire_resource(&self, r: u32, pages: Arc<SharedPages>, backing_attached: bool) {
        self.state
            .retire_render_resource(r, pages, backing_attached);
    }
    fn attach_resource(&self, c: u32, r: u32) -> DrmResult<()> {
        self.state.retry_retired_render_resources();
        self.state
            .submit_control_and_wait(DriverGpuBatch::AttachResource {
                context: c,
                resource: r,
            })?;
        Ok(())
    }
    fn detach_resource(&self, c: u32, r: u32) -> DrmResult<()> {
        self.state.retry_retired_render_resources();
        self.state
            .submit_control_and_wait(DriverGpuBatch::DetachResource {
                context: c,
                resource: r,
            })?;
        Ok(())
    }
    fn transfer(&self, c: u32, r: u32, t: RenderTransfer, h: bool) -> DrmResult<()> {
        self.state.retry_retired_render_resources();
        self.state
            .submit_control_and_wait(DriverGpuBatch::Transfer3d {
                context: c,
                resource: r,
                transfer: RenderTransfer3D {
                    x: t.x,
                    y: t.y,
                    z: t.z,
                    width: t.width,
                    height: t.height,
                    depth: t.depth,
                    offset: t.offset,
                    level: t.level,
                    stride: t.stride,
                    layer_stride: t.layer_stride,
                },
                to_host: h,
            })?;
        Ok(())
    }
    fn submit(&self, job: RenderJob) -> DrmResult<()> {
        if self.state.render_dead.load(Ordering::Acquire) {
            return Err(DrmError::DeviceLost);
        }
        self.state.enqueue_render_job(job)
    }
}

impl<T: GpuTransport> AdapterState<T> {
    fn reserve_blob_aperture(&self, resource: u32, size: u64) -> DrmResult<u64> {
        let bytes = u64::try_from(
            checked_align_up(usize::try_from(size).map_err(|_| DrmError::Overflow)?, 4096)
                .ok_or(DrmError::Overflow)?,
        )
        .map_err(|_| DrmError::Overflow)?;
        let mut aperture = self.blob_aperture.lock();
        if aperture.len == 0 {
            aperture.len = self
                .transport
                .lock()
                .host_visible_len()
                .ok_or(DrmError::Unsupported)?;
        }
        let mut offset = 0u64;
        for (&start, &(_, len)) in aperture.used.iter() {
            if offset.checked_add(bytes).is_some_and(|end| end <= start) {
                break;
            }
            offset = start.checked_add(len).ok_or(DrmError::Overflow)?;
        }
        if offset
            .checked_add(bytes)
            .is_none_or(|end| end > aperture.len)
        {
            return Err(DrmError::QueueFull);
        }
        aperture.used.insert(offset, (resource, bytes));
        aperture.by_resource.insert(resource, offset);
        Ok(offset)
    }
    fn release_blob_aperture(&self, resource: u32) {
        let mut aperture = self.blob_aperture.lock();
        if let Some(offset) = aperture.by_resource.remove(&resource) {
            aperture.used.remove(&offset);
        }
    }
    /// The sole ioctl-boundary synchronous control path.  It publishes an
    /// owned batch, then consumes only its exact typed terminal completion;
    /// all other controlq tokens keep their existing owner maps.  No
    /// operation-specific lower transport entry point is permitted above
    /// this boundary.
    fn submit_control_and_wait(
        &self,
        batch: DriverGpuBatch,
    ) -> DrmResult<(DriverGpuSubmission, DriverGpuCompletion)> {
        if self.render_dead.load(Ordering::Acquire) {
            return Err(DrmError::DeviceLost);
        }
        // Reserve before publishing: the control completion must have an
        // owner while the transport lock is still held.
        let mut pending = self.sync_pending.lock();
        pending.try_reserve(1).map_err(|_| DrmError::NoMemory)?;
        let mut transport = self.transport.lock();
        let submission = transport
            .submit(DriverGpuQueue::Control, batch, 0)
            .map_err(map_dev_error)?;
        let token = submission.fence_id;
        if pending.insert(token, ()).is_some() {
            drop(transport);
            drop(pending);
            self.reset_render_transport();
            return Err(DrmError::Invalid);
        }
        drop(transport);
        drop(pending);
        loop {
            if let Some(record) = self.sync_completions.lock().remove(&token) {
                self.sync_pending.lock().remove(&token);
                if let Err(error) = record.result {
                    return Err(map_dev_error(error));
                }
                return Ok((submission, record));
            }
            self.drain_render_completions()?;
            core::hint::spin_loop();
        }
    }
    fn ensure_retirement_worker(self: &Arc<Self>) -> DrmResult<()>
    where
        T: 'static,
    {
        #[cfg(test)]
        {
            self.retirement_worker_started
                .store(true, Ordering::Release);
            return Ok(());
        }
        #[cfg(not(test))]
        if !self.retirement_worker_started.swap(true, Ordering::AcqRel) {
            let state = self.clone();
            if axtask::try_spawn_with_name(
                move || retirement_worker(state),
                "drm-gpu-retire".into(),
            )
            .is_err()
            {
                self.retirement_worker_started
                    .store(false, Ordering::Release);
                return Err(DrmError::NoMemory);
            }
        }
        Ok(())
    }

    /// Gather independent, short-held queue locks. This is observability only:
    /// in particular, it never calls a transport drain or retry path.
    fn metrics(&self) -> AdapterMetrics {
        AdapterMetrics {
            resources: self.resources.lock().len() as u64,
            retired_2d: self.retired_2d_resources.lock().len() as u64,
            retired_render: self.retired_render_resources.lock().len() as u64,
            render_jobs: self.render_jobs.lock().len() as u64,
            render_pending: self.render_pending.lock().len() as u64,
            present_jobs: self.present_jobs.lock().len() as u64,
            cursor_jobs: self.cursor_jobs.lock().len() as u64,
            final_2d_leaks: self.final_2d_leaks.load(Ordering::Acquire) as u64,
            final_render_leaks: self.final_render_leaks.load(Ordering::Acquire) as u64,
        }
    }

    fn enqueue_render_job(self: &Arc<Self>, job: RenderJob) -> DrmResult<()>
    where
        T: 'static,
    {
        {
            let mut jobs = self.render_jobs.lock();
            if jobs.len() == 8 {
                return Err(DrmError::QueueFull);
            }
            jobs.try_reserve(1).map_err(|_| DrmError::NoMemory)?;
            jobs.push(QueuedRenderJob::Waiting(job));
        }
        self.ensure_render_worker()
    }

    fn enqueue_resource_creation(
        &self,
        resource: u32,
        entries: Vec<(u64, u32)>,
        pages: Arc<SharedPages>,
        blob: bool,
        submission: u64,
    ) -> DrmResult<()> {
        let ready = Fence::new(false);
        // The caller reserves all maps before it publishes CREATE while it
        // owns `transport`; this insertion therefore cannot strand a host
        // completion after an allocation failure.
        if self
            .lifecycle_pending
            .lock()
            .insert(submission, ())
            .is_some()
        {
            ready.signal_error();
            return Err(DrmError::Invalid);
        }
        if self
            .resource_ready_fences
            .lock()
            .insert(resource, ready.clone())
            .is_some()
        {
            ready.signal_error();
            return Err(DrmError::Invalid);
        }
        self.lifecycle_jobs.lock().push(LifecycleJob::Creating {
            resource,
            entries,
            pages,
            ready,
            blob,
            submission,
        });
        Ok(())
    }

    fn resource_ready(&self, resource: u32) -> DrmResult<Arc<Fence>> {
        self.resource_ready_fences
            .lock()
            .get(&resource)
            .cloned()
            .ok_or(DrmError::NotFound)
    }

    fn reserve_lifecycle_admission(&self) -> DrmResult<()> {
        self.lifecycle_pending
            .lock()
            .try_reserve(1)
            .map_err(|_| DrmError::NoMemory)?;
        self.lifecycle_jobs
            .lock()
            .try_reserve(1)
            .map_err(|_| DrmError::NoMemory)?;
        self.resource_ready_fences
            .lock()
            .try_reserve(1)
            .map_err(|_| DrmError::NoMemory)
    }

    fn service_lifecycle_jobs(&self) {
        let completed = {
            let mut jobs = self.lifecycle_jobs.lock();
            let index = jobs.iter().position(|job| {
                let submission = match job {
                    LifecycleJob::Creating { submission, .. }
                    | LifecycleJob::Attaching { submission, .. } => submission,
                };
                self.lifecycle_completions.lock().contains_key(submission)
            });
            index.and_then(|index| {
                let job = jobs.swap_remove(index);
                let submission = match &job {
                    LifecycleJob::Creating { submission, .. }
                    | LifecycleJob::Attaching { submission, .. } => *submission,
                };
                self.lifecycle_completions
                    .lock()
                    .remove(&submission)
                    .map(|result| (job, result))
            })
        };
        let Some((job, result)) = completed else {
            return;
        };
        let submission = match &job {
            LifecycleJob::Creating { submission, .. }
            | LifecycleJob::Attaching { submission, .. } => *submission,
        };
        self.lifecycle_pending.lock().remove(&submission);
        match (job, result) {
            (
                LifecycleJob::Creating {
                    resource,
                    entries,
                    pages,
                    ready,
                    blob,
                    ..
                },
                Ok(()),
            ) if blob || entries.is_empty() => {
                let _ = (resource, entries, pages);
                ready.signal();
            }
            (
                LifecycleJob::Creating {
                    resource,
                    entries,
                    pages,
                    ready,
                    blob,
                    ..
                },
                Ok(()),
            ) => {
                // Reserve before publication, but never hold an owner map
                // while taking transport: all control submission paths use
                // the transport -> owner-map lock order.
                if self.lifecycle_pending.lock().try_reserve(1).is_err()
                    || self.lifecycle_jobs.lock().try_reserve(1).is_err()
                {
                    ready.signal_error();
                    self.retire_render_resource(resource, pages, false);
                    return;
                }
                let mut transport = self.transport.lock();
                let submitted = transport.submit(
                    DriverGpuQueue::Control,
                    DriverGpuBatch::AttachBacking { resource, entries },
                    0,
                );
                match submitted {
                    Ok(submission) => {
                        let duplicate = self
                            .lifecycle_pending
                            .lock()
                            .insert(submission.fence_id, ())
                            .is_some();
                        if !duplicate {
                            self.lifecycle_jobs.lock().push(LifecycleJob::Attaching {
                                resource,
                                pages,
                                ready,
                                blob,
                                submission: submission.fence_id,
                            });
                        } else {
                            drop(transport);
                            ready.signal_error();
                            self.reset_render_transport();
                            self.retire_render_resource(resource, pages, false);
                        }
                    }
                    Err(_) => {
                        drop(transport);
                        ready.signal_error();
                        self.retire_render_resource(resource, pages, false);
                    }
                }
            }
            (
                LifecycleJob::Attaching {
                    pages,
                    ready,
                    blob,
                    ..
                },
                Ok(()),
            ) => {
                let _ = (pages, blob);
                ready.signal();
            }
            (
                LifecycleJob::Creating {
                    resource,
                    pages,
                    ready,
                    ..
                },
                Err(_),
            ) => {
                ready.signal_error();
                self.retire_render_resource(resource, pages, false);
            }
            (
                LifecycleJob::Attaching {
                    resource,
                    pages,
                    ready,
                    ..
                },
                Err(_),
            ) => {
                ready.signal_error();
                // The lower driver conservatively treats an error completion
                // as DMA ownership-uncertain. Reset revokes host execution
                // before dropping the caller pages; never attempt UNREF on
                // an ambiguous attachment.
                self.reset_render_transport();
                self.retire_render_resource(resource, pages, false);
            }
        }
    }

    fn ensure_render_worker(self: &Arc<Self>) -> DrmResult<()>
    where
        T: 'static,
    {
        #[cfg(test)]
        {
            self.render_worker_started.store(true, Ordering::Release);
            return Ok(());
        }
        #[cfg(not(test))]
        {
            if self.render_worker_started.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            let state = Arc::clone(self);
            if axtask::try_spawn_with_name(
                move || render_completion_worker(state),
                "drm-render".into(),
            )
            .is_err()
            {
                self.render_worker_started.store(false, Ordering::Release);
                self.cancel_render_jobs();
                return Err(DrmError::NoMemory);
            }
            Ok(())
        }
    }

    /// One bounded, nonblocking completion pass.  Host command execution is
    /// never performed while a job, GEM, or transport-state lock is held.
    fn service_render_jobs(&self) {
        self.retry_retired_render_resources();
        if self.render_dead.load(Ordering::Acquire) {
            self.cancel_render_jobs();
            return;
        }
        if self.drain_render_completions().is_err() {
            self.reset_render_transport();
            self.cancel_render_jobs();
            return;
        }
        self.complete_render_jobs();
        self.service_lifecycle_jobs();
        self.submit_ready_render_job();
    }

    fn complete_render_jobs(&self) {
        let completed = {
            let mut jobs = self.render_jobs.lock();
            let mut index = 0;
            let mut done = None;
            while index < jobs.len() {
                let submission = match &jobs[index] {
                    QueuedRenderJob::Submitted { submission, .. } => *submission,
                    QueuedRenderJob::Waiting(_) => {
                        index += 1;
                        continue;
                    }
                };
                if let Some(result) = self.render_completions.lock().remove(&submission) {
                    self.render_pending.lock().remove(&submission);
                    done = Some((jobs.swap_remove(index), result));
                    break;
                }
                index += 1;
            }
            done
        };
        let Some((QueuedRenderJob::Submitted { job, .. }, result)) = completed else {
            return;
        };
        let mut detach_error = None;
        for resource in job.resources.iter().rev() {
            if let Err(error) = self.submit_control_and_wait(DriverGpuBatch::DetachResource {
                context: job.context,
                resource: *resource,
            }) {
                detach_error.get_or_insert(error);
            }
        }
        if result.is_err() || detach_error.is_some() {
            job.completion.signal_error();
        } else {
            job.completion.signal();
        }
        if job.cancelled.load(Ordering::Acquire)
            && !self.render_jobs.lock().iter().any(|entry| {
                matches!(entry,
                QueuedRenderJob::Submitted { job: other, .. } if other.context == job.context)
            })
        {
            let _ = self.submit_control_and_wait(DriverGpuBatch::DestroyContext {
                context: job.context,
            });
        }
    }

    fn submit_ready_render_job(&self) {
        if self.render_dead.load(Ordering::Acquire) {
            self.cancel_render_jobs();
            return;
        }
        let job = {
            let mut jobs = self.render_jobs.lock();
            let index = jobs.iter().position(|entry| match entry {
                QueuedRenderJob::Waiting(job) => {
                    job.inputs.iter().all(|fence| fence.is_signaled())
                        && job.predecessors.iter().all(|fence| fence.is_signaled())
                }
                QueuedRenderJob::Submitted { .. } => false,
            });
            index.map(|index| jobs.swap_remove(index))
        };
        let Some(QueuedRenderJob::Waiting(job)) = job else {
            return;
        };
        if job.cancelled.load(Ordering::Acquire)
            || job.inputs.iter().any(|fence| fence.is_failed())
            || job.predecessors.iter().any(|fence| fence.is_failed())
        {
            job.completion.signal_error();
            return;
        }
        let mut attached = Vec::new();
        let mut error: Option<DrmError> = None;
        for resource in &job.resources {
            if job.cancelled.load(Ordering::Acquire) {
                error = Some(map_dev_error(DevError::Io));
                break;
            }
            if let Err(failure) = self.submit_control_and_wait(DriverGpuBatch::AttachResource {
                context: job.context,
                resource: *resource,
            }) {
                error = Some(failure);
                break;
            }
            attached.push(*resource);
        }
        let submission = if error.is_none() && !job.cancelled.load(Ordering::Acquire) {
            let admission_full = {
                let mut pending = self.render_pending.lock();
                pending.len() == 8 || pending.try_reserve(1).is_err()
            };
            if admission_full {
                None
            } else {
                let mut transport = self.transport.lock();
                match transport.submit(
                    DriverGpuQueue::Control,
                    DriverGpuBatch::Submit3d {
                        context: job.context,
                        ring_idx: job.ring_idx,
                        commands: job.commands.clone(),
                        resources: job.resources.clone(),
                    },
                    0,
                ) {
                    Ok(submission)
                        if self
                            .render_pending
                            .lock()
                            .insert(submission.fence_id, ())
                            .is_none() =>
                    {
                        Some(submission.fence_id)
                    }
                    Ok(_) => {
                        drop(transport);
                        self.reset_render_transport();
                        None
                    }
                    Err(_) => None,
                }
            }
        } else {
            None
        };
        if let Some(submission) = submission {
            if job.cancelled.load(Ordering::Acquire) {
                // The host owns this command now. Keep it tracked until its
                // exact terminal record; context-local cancellation is not a
                // reason to reset unrelated clients' control queue.
            }
            self.render_jobs
                .lock()
                .push(QueuedRenderJob::Submitted { job, submission });
            return;
        }
        for resource in attached.iter().rev() {
            let _ = self.submit_control_and_wait(DriverGpuBatch::DetachResource {
                context: job.context,
                resource: *resource,
            });
        }
        job.completion.signal_error();
    }

    fn cancel_render_jobs(&self) {
        let jobs = core::mem::take(&mut *self.render_jobs.lock());
        self.render_completions.lock().clear();
        self.render_pending.lock().clear();
        for job in jobs {
            let job = match job {
                QueuedRenderJob::Waiting(job) | QueuedRenderJob::Submitted { job, .. } => job,
            };
            job.completion.signal_error();
        }
    }

    fn cancel_render_context(&self, context: u32) {
        let (cancelled, has_submitted) = {
            let mut jobs = self.render_jobs.lock();
            let mut retained = Vec::new();
            let mut cancelled = Vec::new();
            let mut has_submitted = false;
            for entry in core::mem::take(&mut *jobs) {
                match entry {
                    QueuedRenderJob::Waiting(job) if job.context == context => {
                        cancelled.push(job);
                    }
                    QueuedRenderJob::Submitted { job, submission } if job.context == context => {
                        has_submitted = true;
                        retained.push(QueuedRenderJob::Submitted { job, submission });
                    }
                    entry => retained.push(entry),
                }
            }
            *jobs = retained;
            (cancelled, has_submitted)
        };
        for job in cancelled {
            job.completion.signal_error();
        }
        if !has_submitted {
            let _ = self.submit_control_and_wait(DriverGpuBatch::DestroyContext { context });
        }
    }

    fn enqueue_present_job(self: &Arc<Self>, scanout: Scanout) -> DrmResult<Arc<Fence>>
    where
        T: 'static,
    {
        if scanout.bpp != 32
            || scanout.pitch < scanout.width.checked_mul(4).ok_or(DrmError::Overflow)?
        {
            return Err(DrmError::Unsupported);
        }
        // Resolve typed backing ownership before publication.  In particular,
        // a Render3d ID must never reach the ordinary 2D SET_SCANOUT path.
        if let Some(resource) = scanout.backing.host_resource() {
            match resource {
                super::gem::HostResource::Scanout2d { .. } => {}
                super::gem::HostResource::Blob {
                    size, mapped: _, ..
                } => {
                    self.validate_blob_scanout(&scanout, size)?;
                    if !self
                        .transport
                        .lock()
                        .modern_features()
                        .contains(DriverGpuFeatures::RESOURCE_BLOB)
                    {
                        return Err(DrmError::Unsupported);
                    }
                }
                super::gem::HostResource::Render3d { meta, .. } => {
                    if meta.target != 2 || !(1..=4).contains(&meta.format) {
                        return Err(DrmError::Unsupported);
                    }
                }
            }
        } else {
            self.resource_for(&scanout.backing)?;
        }
        let completion = Fence::new(false);
        {
            let mut jobs = self.present_jobs.lock();
            if jobs.len() == 8 {
                return Err(DrmError::QueueFull);
            }
            jobs.try_reserve(1).map_err(|_| DrmError::NoMemory)?;
            jobs.push(PresentJob::Waiting {
                scanout,
                completion: completion.clone(),
            });
        }
        if let Err(error) = self.ensure_present_worker() {
            let job = self.present_jobs.lock().pop();
            if let Some(job) = job {
                match job {
                    PresentJob::Waiting { completion, .. }
                    | PresentJob::Submitted { completion, .. } => completion.signal_error(),
                }
            }
            return Err(error);
        }
        Ok(completion)
    }

    fn validate_blob_scanout(&self, scanout: &Scanout, size: u64) -> DrmResult<()> {
        if !matches!(scanout.format, 0x3432_5258 | 0x3432_5241)
            || scanout.framebuffer_width == 0
            || scanout.framebuffer_height == 0
            || scanout.width == 0
            || scanout.height == 0
            || scanout
                .source_x
                .checked_add(scanout.width)
                .is_none_or(|end| end > scanout.framebuffer_width)
            || scanout
                .source_y
                .checked_add(scanout.height)
                .is_none_or(|end| end > scanout.framebuffer_height)
            || scanout.pitch
                < scanout
                    .framebuffer_width
                    .checked_mul(4)
                    .ok_or(DrmError::Overflow)?
        {
            return Err(DrmError::Invalid);
        }
        let end = scanout
            .framebuffer_offset
            .checked_add(
                u64::from(scanout.pitch)
                    .checked_mul(u64::from(scanout.framebuffer_height))
                    .ok_or(DrmError::Overflow)?,
            )
            .ok_or(DrmError::Overflow)?;
        if end > size
            || end > scanout.backing_size
            || scanout.framebuffer_offset > u64::from(u32::MAX)
        {
            return Err(DrmError::Invalid);
        }
        Ok(())
    }

    fn visible_source(scanout: &Scanout) -> DrmResult<(u32, u32)> {
        let relative = scanout
            .offset
            .checked_sub(scanout.framebuffer_offset)
            .ok_or(DrmError::Invalid)?;
        let y = relative / u64::from(scanout.pitch);
        let x_bytes = relative % u64::from(scanout.pitch);
        if !x_bytes.is_multiple_of(4) {
            return Err(DrmError::Invalid);
        }
        let x = u32::try_from(x_bytes / 4).map_err(|_| DrmError::Overflow)?;
        let y = u32::try_from(y).map_err(|_| DrmError::Overflow)?;
        if x.checked_add(scanout.width)
            .is_none_or(|end| end > scanout.framebuffer_width)
            || y.checked_add(scanout.height)
                .is_none_or(|end| end > scanout.framebuffer_height)
        {
            return Err(DrmError::Invalid);
        }
        Ok((x, y))
    }

    /// Stage a linear legacy-virgl resource through a 2D resource sharing the
    /// exact GEM SG pages.  This is deliberately host-side only: no CPU copy
    /// and no accidental use of a 3D ID as a SET_SCANOUT resource.
    fn render_shadow_for(
        &self,
        scanout: &Scanout,
        resource: u32,
        meta: super::render::RenderResource,
    ) -> DrmResult<u32> {
        self.evict_render_shadows();
        if meta.target != 2
            || !(1..=4).contains(&meta.format)
            || meta.depth != 1
            || meta.array_size != 1
            || meta.last_level != 0
            || meta.nr_samples != 0
            || scanout.pitch
                < scanout
                    .framebuffer_width
                    .checked_mul(4)
                    .ok_or(DrmError::Overflow)?
            || scanout.framebuffer_width > meta.width
            || scanout.framebuffer_height > meta.height
        {
            return Err(DrmError::Unsupported);
        }
        let key = Arc::as_ptr(&scanout.backing) as *const () as usize;
        let mut shadows = self.render_shadows.lock();
        if let Some(shadow) = shadows.get(&key) {
            if shadow.width == scanout.framebuffer_width
                && shadow.height == scanout.framebuffer_height
                && shadow.pitch == scanout.pitch
            {
                let context = shadow.context;
                let shadow_resource = shadow.resource;
                let pages = shadow.pages.clone();
                drop(shadows);
                return match self.transfer_render_to_shadow(context, resource, scanout) {
                    Ok(()) => Ok(shadow_resource),
                    Err(failure) => {
                        if failure.attached_source.is_some() {
                            let mut shadows = self.render_shadows.lock();
                            if let Some(shadow) = shadows.get_mut(&key) {
                                shadow.attached_source = failure.attached_source;
                                shadow.retire_stage = ShadowRetireStage::DetachSource;
                                shadow.cleanup_pending = true;
                            } else {
                                drop(shadows);
                                self.retain_shadow_cleanup(
                                    key,
                                    Arc::downgrade(&scanout.backing),
                                    shadow_resource,
                                    pages,
                                    context,
                                    failure.attached_source,
                                );
                            }
                        }
                        Err(failure.error)
                    }
                };
            }
            return Err(DrmError::Busy);
        }
        if shadows.len() >= 8 {
            return Err(DrmError::QueueFull);
        }
        // Reserve the table slot before publishing any shadow DMA resource;
        // every later rollback can then retain its resource/pages/context for
        // retry without a fallible allocation.
        shadows.try_reserve(1).map_err(|_| DrmError::NoMemory)?;
        let pages = scanout.backing.shared_pages()?;
        let width = scanout.pitch / 4;
        let shadow_height = u32::try_from(
            scanout
                .backing_size
                .checked_add(u64::from(scanout.pitch) - 1)
                .ok_or(DrmError::Overflow)?
                / u64::from(scanout.pitch),
        )
        .map_err(|_| DrmError::Overflow)?;
        if shadow_height < scanout.framebuffer_height {
            return Err(DrmError::Invalid);
        }
        let mut entries: Vec<(u64, u32)> = Vec::new();
        entries
            .try_reserve_exact(pages.len())
            .map_err(|_| DrmError::NoMemory)?;
        for index in 0..pages.len() {
            let paddr = pages
                .paddr_at(index)
                .map_err(|_| DrmError::Invalid)?
                .as_usize() as u64;
            let mut merged = false;
            if let Some((base, length)) = entries.last_mut() {
                if base.checked_add(u64::from(*length)) == Some(paddr)
                    && *length <= u32::MAX - PageSize::Size4K as u32
                {
                    *length += PageSize::Size4K as u32;
                    merged = true;
                }
            }
            if !merged {
                entries.push((paddr, PageSize::Size4K as u32));
            }
        }
        let sg_len = entries
            .iter()
            .try_fold(0u64, |total, (_, length)| {
                total.checked_add(u64::from(*length))
            })
            .ok_or(DrmError::Overflow)?;
        if sg_len < scanout.backing_size {
            return Err(DrmError::Invalid);
        }
        // All fallible SG work completes before CREATE publishes a host
        // resource, so pre-publication failure simply releases `pages`.
        drop(shadows);
        let created = self
            .submit_control_and_wait(DriverGpuBatch::Create2d {
                width,
                height: shadow_height,
                entries: Vec::new(),
            })?
            .0;
        let Some(shadow_resource) = created.resource_id else {
            self.reset_render_transport();
            return Err(DrmError::Invalid);
        };
        if let Err(error) = self.submit_control_and_wait(DriverGpuBatch::AttachBacking {
            resource: shadow_resource,
            entries,
        }) {
            self.reset_render_transport();
            self.retire_render_resource(shadow_resource, pages, false);
            return Err(error);
        }
        let context = match self.submit_control_and_wait(DriverGpuBatch::CreateContext {
            name: b"thekernel-kms-blit".to_vec(),
            init: DriverContextInit::default(),
        }) {
            Ok((submission, _)) => match submission.context_id {
                Some(context) => context,
                None => {
                    self.retire_render_resource(shadow_resource, pages, true);
                    return Err(DrmError::Invalid);
                }
            },
            Err(error) => {
                self.retire_render_resource(shadow_resource, pages, true);
                return Err(error);
            }
        };
        if let Err(failure) = self.transfer_render_to_shadow(context, resource, scanout) {
            self.retain_shadow_cleanup(
                key,
                Arc::downgrade(&scanout.backing),
                shadow_resource,
                pages,
                context,
                failure.attached_source,
            );
            return Err(failure.error);
        }
        let mut shadows = self.render_shadows.lock();
        if shadows.len() >= 8 {
            drop(shadows);
            self.retain_shadow_cleanup(
                key,
                Arc::downgrade(&scanout.backing),
                shadow_resource,
                pages,
                context,
                None,
            );
            return Err(DrmError::QueueFull);
        }
        shadows.insert(
            key,
            RenderShadow {
                source: Arc::downgrade(&scanout.backing),
                resource: shadow_resource,
                pages,
                width: scanout.framebuffer_width,
                height: scanout.framebuffer_height,
                pitch: scanout.pitch,
                context,
                attached_source: None,
                retire_stage: ShadowRetireStage::Active,
                cleanup_pending: false,
            },
        );
        Ok(shadow_resource)
    }

    fn retain_shadow_cleanup(
        &self,
        key: usize,
        source: Weak<dyn GemBacking>,
        resource: u32,
        pages: Arc<SharedPages>,
        context: u32,
        attached_source: Option<u32>,
    ) {
        // Acquiring the table lock after reset has set `render_dead` proves
        // reset quiesced the lower control queue and cleared shadow DMA
        // ownership. Do not resurrect cleanup state after that boundary.
        let mut shadows = self.render_shadows.lock();
        if self.render_dead.load(Ordering::Acquire) {
            return;
        }
        shadows.insert(
            key,
            RenderShadow {
                source,
                resource,
                pages,
                width: 0,
                height: 0,
                pitch: 0,
                context,
                attached_source,
                retire_stage: if attached_source.is_some() {
                    ShadowRetireStage::DetachSource
                } else {
                    ShadowRetireStage::DetachBacking
                },
                cleanup_pending: true,
            },
        );
    }

    fn evict_render_shadows(&self) {
        let mut stale = None;
        {
            let mut shadows = self.render_shadows.lock();
            if let Some((&key, _)) = shadows
                .iter()
                .find(|(_, shadow)| shadow.cleanup_pending || shadow.source.strong_count() == 0)
            {
                stale = shadows.remove(&key).map(|shadow| (key, shadow));
            }
        }
        let Some((key, mut shadow)) = stale else {
            return;
        };
        // Keep page/context ownership in the table until every terminal
        // cleanup command succeeds.  Failed retirement is retried by the
        // periodic worker instead of dropping DMA backing.
        let complete = match shadow.retire_stage {
            ShadowRetireStage::DetachSource => match shadow.attached_source {
                Some(resource) => {
                    match self.submit_control_and_wait(DriverGpuBatch::DetachResource {
                        context: shadow.context,
                        resource,
                    }) {
                        Ok(_) => {
                            shadow.attached_source = None;
                            shadow.retire_stage = ShadowRetireStage::DetachBacking;
                            false
                        }
                        Err(_) => false,
                    }
                }
                None => {
                    shadow.retire_stage = ShadowRetireStage::DetachBacking;
                    false
                }
            },
            ShadowRetireStage::Active | ShadowRetireStage::DetachBacking => match self
                .submit_control_and_wait(DriverGpuBatch::DetachBacking {
                    resource: shadow.resource,
                }) {
                Ok(_) => {
                    shadow.retire_stage = ShadowRetireStage::DestroyResource;
                    false
                }
                Err(_) => false,
            },
            ShadowRetireStage::DestroyResource => {
                match self.submit_control_and_wait(DriverGpuBatch::DestroyResource {
                    resource: shadow.resource,
                }) {
                    Ok(_) => {
                        shadow.retire_stage = ShadowRetireStage::DestroyContext;
                        false
                    }
                    Err(_) => false,
                }
            }
            ShadowRetireStage::DestroyContext => self
                .submit_control_and_wait(DriverGpuBatch::DestroyContext {
                    context: shadow.context,
                })
                .is_ok(),
        };
        if !complete && !self.render_dead.load(Ordering::Acquire) {
            let mut shadows = self.render_shadows.lock();
            // Reset sets `render_dead` before it clears the table. Recheck
            // under the table lock so a cleanup worker cannot resurrect a
            // shadow whose host DMA rights were already revoked.
            if !self.render_dead.load(Ordering::Acquire) {
                shadows.insert(key, shadow);
            }
        }
    }

    fn transfer_render_to_shadow(
        &self,
        context: u32,
        resource: u32,
        scanout: &Scanout,
    ) -> Result<(), ShadowTransferError> {
        let (source_x, source_y) =
            Self::visible_source(scanout).map_err(|error| ShadowTransferError {
                error,
                attached_source: None,
            })?;
        let damage = scanout.damage.unwrap_or(DamageRect {
            x: source_x,
            y: source_y,
            width: scanout.width,
            height: scanout.height,
        });
        if damage.width == 0
            || damage.height == 0
            || damage
                .x
                .checked_add(damage.width)
                .is_none_or(|end| end > scanout.framebuffer_width)
            || damage
                .y
                .checked_add(damage.height)
                .is_none_or(|end| end > scanout.framebuffer_height)
        {
            return Err(ShadowTransferError {
                error: DrmError::Invalid,
                attached_source: None,
            });
        }
        let transfer_offset = scanout
            .framebuffer_offset
            .checked_add(
                u64::from(damage.y)
                    .checked_mul(u64::from(scanout.pitch))
                    .ok_or(ShadowTransferError {
                        error: DrmError::Overflow,
                        attached_source: None,
                    })?,
            )
            .and_then(|value| value.checked_add(u64::from(damage.x).checked_mul(4)?))
            .ok_or(ShadowTransferError {
                error: DrmError::Overflow,
                attached_source: None,
            })?;
        self.submit_control_and_wait(DriverGpuBatch::AttachResource { context, resource })
            .map_err(|error| ShadowTransferError {
                error,
                attached_source: None,
            })?;
        let transfer = self.submit_control_and_wait(DriverGpuBatch::Transfer3d {
            context,
            resource,
            transfer: RenderTransfer3D {
                x: damage.x,
                y: damage.y,
                z: 0,
                width: damage.width,
                height: damage.height,
                depth: 1,
                offset: transfer_offset,
                level: 0,
                stride: scanout.pitch,
                layer_stride: 0,
            },
            to_host: false,
        });
        let detach =
            self.submit_control_and_wait(DriverGpuBatch::DetachResource { context, resource });
        match detach {
            Err(error) => Err(ShadowTransferError {
                error,
                attached_source: Some(resource),
            }),
            Ok(_) => transfer.map(|_| ()).map_err(|error| ShadowTransferError {
                error,
                attached_source: None,
            }),
        }
    }

    fn ensure_present_worker(self: &Arc<Self>) -> DrmResult<()>
    where
        T: 'static,
    {
        #[cfg(test)]
        {
            self.present_worker_started.store(true, Ordering::Release);
            return Ok(());
        }
        #[cfg(not(test))]
        {
            if self.present_worker_started.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            let state = Arc::clone(self);
            if axtask::try_spawn_with_name(
                move || present_completion_worker(state),
                "drm-present".into(),
            )
            .is_err()
            {
                self.present_worker_started.store(false, Ordering::Release);
                return Err(DrmError::NoMemory);
            }
            Ok(())
        }
    }

    fn enqueue_cursor(
        self: &Arc<Self>,
        cursor: DriverCursorUpdate,
        backing: Arc<dyn GemBacking>,
    ) -> DrmResult<Arc<Fence>>
    where
        T: 'static,
    {
        let _admission = self.cursor_admission.lock();
        let fence = Fence::new(false);
        let mut jobs = self.cursor_jobs.lock();
        if jobs.len() == 8 {
            fence.signal_error();
            return Err(DrmError::QueueFull);
        }
        if jobs.try_reserve(1).is_err() {
            fence.signal_error();
            return Err(DrmError::NoMemory);
        }
        drop(jobs);
        if let Err(error) = self.ensure_cursor_worker() {
            fence.signal_error();
            return Err(error);
        }
        self.cursor_admitting.store(true, Ordering::Release);
        let submission = match self.transport.lock().submit(
            DriverGpuQueue::Cursor,
            DriverGpuBatch::UpdateCursor(cursor),
            0,
        ) {
            Ok(submission) => submission,
            Err(error) => {
                self.cursor_admitting.store(false, Ordering::Release);
                fence.signal_error();
                return Err(map_dev_error(error));
            }
        };
        let mut jobs = self.cursor_jobs.lock();
        if jobs
            .insert(
                submission.fence_id,
                CursorJob {
                    completion: fence.clone(),
                    _backing: Some(backing),
                },
            )
            .is_some()
        {
            self.cursor_admitting.store(false, Ordering::Release);
            fence.signal_error();
            return Err(DrmError::Invalid);
        }
        self.cursor_admitting.store(false, Ordering::Release);
        drop(jobs);
        Ok(fence)
    }
    fn enqueue_cursor_move(self: &Arc<Self>, x: i32, y: i32) -> DrmResult<Arc<Fence>>
    where
        T: 'static,
    {
        let _admission = self.cursor_admission.lock();
        let fence = Fence::new(false);
        let mut jobs = self.cursor_jobs.lock();
        if jobs.len() == 8 {
            fence.signal_error();
            return Err(DrmError::QueueFull);
        }
        if jobs.try_reserve(1).is_err() {
            fence.signal_error();
            return Err(DrmError::NoMemory);
        }
        drop(jobs);
        if let Err(error) = self.ensure_cursor_worker() {
            fence.signal_error();
            return Err(error);
        }
        self.cursor_admitting.store(true, Ordering::Release);
        let submission = match self.transport.lock().submit(
            DriverGpuQueue::Cursor,
            DriverGpuBatch::MoveCursor { x, y },
            0,
        ) {
            Ok(submission) => submission,
            Err(error) => {
                self.cursor_admitting.store(false, Ordering::Release);
                fence.signal_error();
                return Err(map_dev_error(error));
            }
        };
        let mut jobs = self.cursor_jobs.lock();
        if jobs
            .insert(
                submission.fence_id,
                CursorJob {
                    completion: fence.clone(),
                    _backing: None,
                },
            )
            .is_some()
        {
            self.cursor_admitting.store(false, Ordering::Release);
            fence.signal_error();
            return Err(DrmError::Invalid);
        }
        self.cursor_admitting.store(false, Ordering::Release);
        Ok(fence)
    }
    fn ensure_cursor_worker(self: &Arc<Self>) -> DrmResult<()>
    where
        T: 'static,
    {
        #[cfg(test)]
        {
            self.cursor_worker_started.store(true, Ordering::Release);
            return Ok(());
        }
        #[cfg(not(test))]
        {
            if self.cursor_worker_started.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            let state = Arc::clone(self);
            if axtask::try_spawn_with_name(
                move || cursor_completion_worker(state),
                "drm-cursor".into(),
            )
            .is_err()
            {
                self.cursor_worker_started.store(false, Ordering::Release);
                return Err(DrmError::NoMemory);
            }
            Ok(())
        }
    }
    fn service_cursor_jobs(&self) {
        if self.cursor_admitting.load(Ordering::Acquire) {
            return;
        }
        let mut records: [DriverGpuCompletion; 8] = core::array::from_fn(|_| DriverGpuCompletion {
            fence_id: 0,
            result: Ok(()),
            data: DriverGpuCompletionData::None,
        });
        let count = match self
            .transport
            .lock()
            .drain_completions(DriverGpuQueue::Cursor, &mut records)
        {
            Ok(count) => count,
            Err(_) => self
                .transport
                .lock()
                .reset(DriverGpuQueue::Cursor, &mut records),
        };
        let mut jobs = self.cursor_jobs.lock();
        for record in records.into_iter().take(count) {
            if let Some(job) = jobs.remove(&record.fence_id) {
                if record.result.is_ok() {
                    job.completion.signal()
                } else {
                    job.completion.signal_error()
                }
            } else {
                // A completion not owned by this map proves token ownership
                // diverged. Every pending KMS cursor fence becomes terminal.
                for (_, job) in jobs.drain() {
                    job.completion.signal_error();
                }
                break;
            }
        }
    }

    fn service_present_job(&self) {
        if self.drain_render_completions().is_err() {
            self.reset_render_transport();
            return;
        }
        let completed = {
            let mut jobs = self.present_jobs.lock();
            let index = jobs.iter().position(|job| {
                matches!(job,
                    PresentJob::Submitted { submission, .. }
                        if self.present_completions.lock().contains_key(submission)
                )
            });
            index.and_then(|index| match jobs.swap_remove(index) {
                PresentJob::Submitted {
                    completion,
                    submission,
                    ..
                } => self
                    .present_completions
                    .lock()
                    .remove(&submission)
                    .map(|result| (completion, submission, result)),
                PresentJob::Waiting { .. } => None,
            })
        };
        if let Some((completion, submission, result)) = completed {
            self.present_pending.lock().remove(&submission);
            if result.is_ok() {
                completion.signal();
            } else {
                completion.signal_error();
            }
        }
        let next = {
            let mut jobs = self.present_jobs.lock();
            // A shadow transfer writes the same shared pages used by every
            // scanout resource.  Do not start another transfer/present until
            // the preceding scanout completion made that ownership terminal.
            (!jobs
                .iter()
                .any(|job| matches!(job, PresentJob::Submitted { .. })))
            .then(|| {
                jobs.iter()
                    .position(|job| matches!(job, PresentJob::Waiting { .. }))
            })
            .flatten()
            .map(|index| jobs.swap_remove(index))
        };
        let Some(PresentJob::Waiting {
            scanout,
            completion,
        }) = next
        else {
            return;
        };
        let batch = match scanout.backing.host_resource() {
            Some(super::gem::HostResource::Blob { resource, size, .. }) => {
                self.validate_blob_scanout(&scanout, size).and_then(|_| {
                    let (source_x, source_y) = Self::visible_source(&scanout)?;
                    let format = if scanout.format == 0x3432_5258 { 2 } else { 1 };
                    Ok(DriverGpuBatch::PresentBlob {
                        resource,
                        source_x,
                        source_y,
                        width: scanout.width,
                        height: scanout.height,
                        framebuffer_width: scanout.framebuffer_width,
                        framebuffer_height: scanout.framebuffer_height,
                        format,
                        stride: scanout.pitch,
                        offset: scanout.framebuffer_offset as u32,
                        damage: scanout.damage.map(|damage| axdriver_display::DrmDamage {
                            x: damage.x,
                            y: damage.y,
                            width: damage.width,
                            height: damage.height,
                        }),
                    })
                })
            }
            Some(super::gem::HostResource::Render3d { resource, meta }) => self
                .render_shadow_for(&scanout, resource, meta)
                .and_then(|resource| {
                    Ok(DriverGpuBatch::Present {
                        resource,
                        width: scanout.width,
                        height: scanout.height,
                        source_x: scanout.source_x,
                        source_y: scanout.source_y,
                        damage: scanout.damage.map(|damage| axdriver_display::DrmDamage {
                            x: damage.x,
                            y: damage.y,
                            width: damage.width,
                            height: damage.height,
                        }),
                    })
                }),
            _ => self.resource_for(&scanout.backing).and_then(|resource| {
                Ok(DriverGpuBatch::Present {
                    resource,
                    width: scanout.width,
                    height: scanout.height,
                    source_x: scanout.source_x,
                    source_y: scanout.source_y,
                    damage: scanout.damage.map(|damage| axdriver_display::DrmDamage {
                        x: damage.x,
                        y: damage.y,
                        width: damage.width,
                        height: damage.height,
                    }),
                })
            }),
        };
        match batch {
            Ok(batch) => {
                let admission_full = {
                    let mut pending = self.present_pending.lock();
                    pending.len() == 8 || pending.try_reserve(1).is_err()
                };
                if admission_full {
                    self.present_jobs.lock().push(PresentJob::Waiting {
                        scanout,
                        completion,
                    });
                    return;
                }
                let mut transport = self.transport.lock();
                let submitted = transport
                    .submit(DriverGpuQueue::Control, batch, 0)
                    .map_err(map_dev_error);
                let submission = match submitted {
                    Ok(submission)
                        if self
                            .present_pending
                            .lock()
                            .insert(submission.fence_id, ())
                            .is_none() =>
                    {
                        submission.fence_id
                    }
                    Ok(_) => {
                        drop(transport);
                        completion.signal_error();
                        self.reset_render_transport();
                        return;
                    }
                    Err(DrmError::QueueFull) => {
                        drop(transport);
                        self.present_jobs.lock().push(PresentJob::Waiting {
                            scanout,
                            completion,
                        });
                        return;
                    }
                    Err(_) => {
                        completion.signal_error();
                        return;
                    }
                };
                drop(transport);
                self.present_jobs.lock().push(PresentJob::Submitted {
                    scanout,
                    completion,
                    submission,
                });
            }
            Err(DrmError::QueueFull) => {
                self.present_jobs.lock().push(PresentJob::Waiting {
                    scanout,
                    completion,
                });
            }
            Err(_) => completion.signal_error(),
        }
    }

    fn reset_render_transport(&self) {
        // This is terminal, not a recoverable queue hiccup.  Revoke every
        // external mapping before reset removes host execution rights; VMA
        // owners stay pinned, but no new fault/mmap/submit may publish the
        // stale PCI aperture after this point.
        // Serialize every possible shadow-retain transition with reset. The
        // lock stays held through lower reset and fence termination, so a
        // concurrent rollback can only drop its pages after host DMA is
        // known quiescent.
        let mut shadows = self.render_shadows.lock();
        self.render_dead.store(true, Ordering::Release);
        {
            let mut aperture = self.blob_aperture.lock();
            aperture.used.clear();
            aperture.by_resource.clear();
        }
        // Never hold the device registry lock while taking MM locks.  Walk
        // one weak registration at a time, which keeps reset bounded without
        // an allocation failure path that could leave a BAR PTE live.
        let mut index = 0;
        loop {
            let mapping = {
                let mut leases = self.external_leases.lock();
                while index < leases.len()
                    && (leases[index].lease.strong_count() == 0
                        || leases[index].pages.strong_count() == 0)
                {
                    leases.swap_remove(index);
                }
                if index == leases.len() {
                    None
                } else {
                    let entry = &leases[index];
                    index += 1;
                    entry.lease.upgrade().zip(entry.pages.upgrade())
                }
            };
            let Some((lease, pages)) = mapping else { break };
            lease.mark_dead();
            crate::mm::revoke_external_shared_pages(&pages);
        }
        let mut records: [DriverGpuCompletion; 8] = core::array::from_fn(|_| DriverGpuCompletion {
            fence_id: 0,
            result: Ok(()),
            data: DriverGpuCompletionData::None,
        });
        // The lower driver releases a bounded batch per call.  A reset is a
        // finite terminal drain, so keep consuming until it explicitly
        // reports empty rather than assuming a particular ring depth.
        loop {
            let count = self
                .transport
                .lock()
                .reset(DriverGpuQueue::Control, &mut records);
            if count == 0 {
                break;
            }
            // `reset` made each returned command terminal in the lower ring;
            // the owner maps below convert every remaining external fence to
            // an error before this function returns.
        }
        let presents = core::mem::take(&mut *self.present_jobs.lock());
        for present in presents {
            match present {
                PresentJob::Waiting { completion, .. }
                | PresentJob::Submitted { completion, .. } => completion.signal_error(),
            }
        }
        self.present_pending.lock().clear();
        self.present_completions.lock().clear();
        for (_, fence) in self.resource_ready_fences.lock().drain() {
            fence.signal_error();
        }
        for job in core::mem::take(&mut *self.lifecycle_jobs.lock()) {
            match job {
                LifecycleJob::Creating { ready, .. } | LifecycleJob::Attaching { ready, .. } => {
                    ready.signal_error()
                }
            }
        }
        self.lifecycle_pending.lock().clear();
        self.lifecycle_completions.lock().clear();
        for job in core::mem::take(&mut *self.render_jobs.lock()) {
            match job {
                QueuedRenderJob::Waiting(job) | QueuedRenderJob::Submitted { job, .. } => {
                    job.completion.signal_error()
                }
            }
        }
        self.render_pending.lock().clear();
        self.render_completions.lock().clear();
        let sync_tokens: Vec<u64> = self.sync_pending.lock().keys().copied().collect();
        let mut sync_completed = self.sync_completions.lock();
        for token in sync_tokens {
            sync_completed.insert(
                token,
                DriverGpuCompletion {
                    fence_id: token,
                    result: Err(DevError::Io),
                    data: DriverGpuCompletionData::None,
                },
            );
        }
        // All externally visible fences are terminal above; only now may the
        // shadow page owners be released after the host reset revoked DMA.
        shadows.clear();
    }

    /// Retain every terminal completion before returning control to a
    /// particular waiter. The control queue is small (eight entries) and the
    /// completion map is explicitly bounded, so this never turns a hostile
    /// used ring into unbounded kernel allocation.
    fn drain_render_completions(&self) -> DrmResult<()> {
        let mut completed = self.render_completions.lock();
        completed.try_reserve(8).map_err(|_| DrmError::NoMemory)?;
        drop(completed);
        let mut present_completed = self.present_completions.lock();
        present_completed
            .try_reserve(8)
            .map_err(|_| DrmError::NoMemory)?;
        drop(present_completed);
        let mut lifecycle_completed = self.lifecycle_completions.lock();
        lifecycle_completed
            .try_reserve(8)
            .map_err(|_| DrmError::NoMemory)?;
        drop(lifecycle_completed);
        let mut sync_completed = self.sync_completions.lock();
        sync_completed
            .try_reserve(8)
            .map_err(|_| DrmError::NoMemory)?;
        drop(sync_completed);

        let mut records: [DriverGpuCompletion; 8] = core::array::from_fn(|_| DriverGpuCompletion {
            fence_id: 0,
            result: Ok(()),
            data: DriverGpuCompletionData::None,
        });
        let drained = {
            let mut transport = self.transport.lock();
            transport.drain_completions(DriverGpuQueue::Control, &mut records)
        };
        let count = match drained {
            Ok(count) => count,
            Err(_) => {
                // A malformed used entry or pop failure makes ownership of
                // the queue unknowable. Reset makes every remaining command
                // terminal before returning the original error.
                let mut transport = self.transport.lock();
                transport.reset(DriverGpuQueue::Control, &mut records)
            }
        };
        for record in records.into_iter().take(count) {
            let fence_id = record.fence_id;
            let render_pending = self.render_pending.lock().contains_key(&fence_id);
            let present_pending = self.present_pending.lock().contains_key(&fence_id);
            let lifecycle_pending = self.lifecycle_pending.lock().contains_key(&fence_id);
            let sync_pending = self.sync_pending.lock().contains_key(&fence_id);
            if (render_pending as u8
                + present_pending as u8
                + lifecycle_pending as u8
                + sync_pending as u8)
                != 1
            {
                return Err(DrmError::Invalid);
            }
            if sync_pending {
                if self
                    .sync_completions
                    .lock()
                    .insert(fence_id, record)
                    .is_some()
                {
                    return Err(DrmError::Invalid);
                }
                continue;
            }
            let result = record.result;
            if render_pending {
                if self
                    .render_completions
                    .lock()
                    .insert(fence_id, result)
                    .is_some()
                {
                    return Err(DrmError::Invalid);
                }
            } else if present_pending {
                if self
                    .present_completions
                    .lock()
                    .insert(fence_id, result)
                    .is_some()
                {
                    return Err(DrmError::Invalid);
                }
            } else if lifecycle_pending {
                if self
                    .lifecycle_completions
                    .lock()
                    .insert(fence_id, result)
                    .is_some()
                {
                    return Err(DrmError::Invalid);
                }
            } else {
                return Err(DrmError::Invalid);
            }
        }
        Ok(())
    }

    fn resource_for(&self, backing: &Arc<dyn GemBacking>) -> DrmResult<u32> {
        if let Some(resource) = backing.host_resource() {
            return if resource.kind() == super::gem::HostResourceKind::Scanout2d {
                Ok(resource.id())
            } else {
                Err(DrmError::Unsupported)
            };
        }
        let key = Arc::as_ptr(backing) as *const () as usize;
        self.resources
            .lock()
            .get(&key)
            .copied()
            .ok_or(DrmError::NotFound)
    }

    fn retire_resource(&self, resource: u32, backing_key: Option<usize>, pages: Arc<SharedPages>) {
        self.retry_retired_2d_resources();
        if let Some(backing_key) = backing_key {
            let removed = self.resources.lock().remove(&backing_key);
            debug_assert!(removed.is_none_or(|candidate| candidate == resource));
        }
        let token = Retired2dResource { resource, pages };
        if self.try_retire_2d_resource(&token) {
            return;
        }
        let mut retired = self.retired_2d_resources.lock();
        if retired.len() < MAX_RETIRED_RESOURCES && retired.try_reserve(1).is_ok() {
            retired.push(token);
        } else {
            drop(retired);
            self.backpressure_2d_retirement(token);
        }
    }

    fn backpressure_2d_retirement(&self, token: Retired2dResource) {
        loop {
            self.retry_retired_2d_resources();
            let mut overflow = self.overflow_2d_retired.lock();
            if overflow.is_none() {
                *overflow = Some(token);
                return;
            }
            drop(overflow);
            // The dedicated worker makes progress independently; yielding
            // here keeps the final owner alive without allocating an
            // unbounded overflow list or leaking DMA pages.
            let _ = axtask::future::block_on(axtask::future::sleep(Duration::from_millis(1)));
            core::hint::spin_loop();
        }
    }

    fn retry_retired_2d_resources(&self) {
        let overflow = self.overflow_2d_retired.lock().take();
        if let Some(token) = overflow {
            if !self.try_retire_2d_resource(&token) {
                *self.overflow_2d_retired.lock() = Some(token);
            }
        }
        let mut retired = self.retired_2d_resources.lock();
        let mut index = 0;
        while index < retired.len() {
            if self.try_retire_2d_resource(&retired[index]) {
                retired.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn try_retire_2d_resource(&self, token: &Retired2dResource) -> bool {
        self.submit_control_and_wait(DriverGpuBatch::DetachBacking {
            resource: token.resource,
        })
        .and_then(|_| {
            self.submit_control_and_wait(DriverGpuBatch::DestroyResource {
                resource: token.resource,
            })
        })
        .is_ok()
    }

    fn retire_render_resource(
        &self,
        resource: u32,
        pages: Arc<SharedPages>,
        backing_attached: bool,
    ) {
        self.resource_ready_fences.lock().remove(&resource);
        self.retry_retired_render_resources();
        let external_pages = pages.is_external();
        let mut token = RetiredRenderResource {
            resource,
            pages,
            backing_attached,
            stage: if external_pages {
                RenderRetireStage::UnmapBlob
            } else if backing_attached {
                RenderRetireStage::DetachBacking
            } else {
                RenderRetireStage::Unref
            },
        };
        if self.try_retire_render_resource(&mut token) {
            return;
        }
        let mut retired = self.retired_render_resources.lock();
        if retired.len() < MAX_RETIRED_RESOURCES && retired.try_reserve(1).is_ok() {
            retired.push(token);
        } else {
            drop(retired);
            self.backpressure_render_retirement(token);
        }
    }

    fn backpressure_render_retirement(&self, token: RetiredRenderResource) {
        let token = token;
        loop {
            self.retry_retired_render_resources();
            let mut overflow = self.overflow_render_retired.lock();
            if overflow.is_none() {
                *overflow = Some(token);
                return;
            }
            drop(overflow);
            let _ = axtask::future::block_on(axtask::future::sleep(Duration::from_millis(1)));
            core::hint::spin_loop();
        }
    }

    /// Makes one best-effort pass. This is called before subsequent render
    /// operations, which avoids needing an unbounded background queue.
    fn retry_retired_render_resources(&self) {
        let overflow = self.overflow_render_retired.lock().take();
        if let Some(mut token) = overflow {
            if !self.try_retire_render_resource(&mut token) {
                *self.overflow_render_retired.lock() = Some(token);
            }
        }
        let mut retired = self.retired_render_resources.lock();
        let mut index = 0;
        while index < retired.len() {
            if self.try_retire_render_resource(&mut retired[index]) {
                retired.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn try_retire_render_resource(&self, token: &mut RetiredRenderResource) -> bool {
        if let Ok(ready) = self.resource_ready(token.resource) {
            if !ready.is_signaled() {
                return false;
            }
        }
        // A HOST_VISIBLE resource's PCI aperture remains host-accessible from
        // every inherited/split VMA.  The retirement token owns one page Arc;
        // any additional Arc is a live GEM/VMA/fork fragment, so UNMAP/UNREF
        // must wait rather than tearing the BAR mapping out from under it.
        if token.pages.is_external() && Arc::strong_count(&token.pages) != 1 {
            return false;
        }
        if self.render_dead.load(Ordering::Acquire) {
            // reset/remove already revoked host execution and invalidated the
            // aperture lease.  No post-reset control request is trustworthy;
            // dropping the final token releases its retained transport owner
            // only after the last VMA fragment has gone away.
            return true;
        }
        // Resource/context attachment ownership is retired by the same
        // tokenized control path as all other render operations.
        if token.stage == RenderRetireStage::UnmapBlob {
            if self
                .submit_control_and_wait(DriverGpuBatch::UnmapBlob {
                    resource: token.resource,
                })
                .is_err()
            {
                return false;
            }
            self.release_blob_aperture(token.resource);
            token.stage = RenderRetireStage::Unref;
        }
        if token.stage == RenderRetireStage::DetachBacking {
            if self
                .submit_control_and_wait(DriverGpuBatch::DetachBacking {
                    resource: token.resource,
                })
                .is_err()
            {
                return false;
            }
            token.backing_attached = false;
            token.stage = RenderRetireStage::Unref;
        }
        self.submit_control_and_wait(DriverGpuBatch::UnrefResource {
            resource: token.resource,
        })
        .is_ok()
    }
}

fn render_completion_worker<T: GpuTransport + 'static>(state: Arc<AdapterState<T>>) {
    loop {
        state.service_render_jobs();
        // The lower VirtIO implementation exposes a bounded nonblocking drain
        // rather than a completion waitqueue.  Yield between passes so an
        // empty or dependency-blocked queue never monopolizes a CPU.
        let _ = axtask::future::block_on(axtask::future::sleep(Duration::from_millis(1)));
    }
}

fn present_completion_worker<T: GpuTransport + 'static>(state: Arc<AdapterState<T>>) {
    loop {
        state.service_present_job();
        let _ = axtask::future::block_on(axtask::future::sleep(Duration::from_millis(1)));
    }
}

fn cursor_completion_worker<T: GpuTransport + 'static>(state: Arc<AdapterState<T>>) {
    loop {
        state.service_cursor_jobs();
        let _ = axtask::future::block_on(axtask::future::sleep(Duration::from_millis(1)));
    }
}

fn retirement_worker<T: GpuTransport + 'static>(state: Arc<AdapterState<T>>) {
    loop {
        state.retry_retired_2d_resources();
        state.retry_retired_render_resources();
        state.evict_render_shadows();
        let _ = axtask::future::block_on(axtask::future::sleep(Duration::from_millis(1)));
    }
}

impl<T: GpuTransport> Drop for AdapterState<T> {
    fn drop(&mut self) {
        if let Some(token) = self.overflow_2d_retired.get_mut().take() {
            let token = token;
            while !self.try_retire_2d_resource(&token) {
                core::hint::spin_loop();
            }
        }
        if let Some(token) = self.overflow_render_retired.get_mut().take() {
            let mut token = token;
            while !self.try_retire_render_resource(&mut token) {
                core::hint::spin_loop();
            }
        }
        let mut retired_2d = core::mem::take(self.retired_2d_resources.get_mut());
        for token in retired_2d.drain(..) {
            if !self.try_retire_2d_resource(&token) {
                // `Drop` is the last owner: keep driving the terminal
                // detach/unref protocol rather than converting a full queue
                // into a permanent DMA-page leak.
                let token = token;
                while !self.try_retire_2d_resource(&token) {
                    core::hint::spin_loop();
                }
            }
        }
        let mut retired = core::mem::take(self.retired_render_resources.get_mut());
        for token in retired.drain(..) {
            let mut token = token;
            if !self.try_retire_render_resource(&mut token) {
                let mut token = token;
                while !self.try_retire_render_resource(&mut token) {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

struct VirtioGemBacking<T: GpuTransport> {
    pages: Arc<SharedPages>,
    resource: u32,
    // A backing may be exported and outlive every DRM file/device handle. It
    // must therefore keep the transport state alive until destroy_resource
    // has detached its host DMA backing and unrefed the resource.
    adapter: Arc<AdapterState<T>>,
}

impl<T: GpuTransport> GemBacking for VirtioGemBacking<T> {
    fn shared_pages(&self) -> DrmResult<Arc<SharedPages>> {
        Ok(self.pages.clone())
    }
    fn host_resource(&self) -> Option<super::gem::HostResource> {
        Some(super::gem::HostResource::Scanout2d {
            resource: self.resource,
        })
    }
}

impl<T: GpuTransport> Drop for VirtioGemBacking<T> {
    fn drop(&mut self) {
        let backing_key = self as *const Self as usize;
        self.adapter
            .retire_resource(self.resource, Some(backing_key), self.pages.clone());
    }
}

impl<T: GpuTransport + 'static> DisplayAdapter for VirtioGpuAdapter<T> {
    fn metrics(&self) -> AdapterMetrics {
        self.state.metrics()
    }

    fn preferred_mode(&self) -> super::Mode {
        let (width, height) = self.state.transport.lock().preferred_mode();
        super::Mode {
            width,
            height,
            refresh_millihz: 60_000,
        }
    }
    fn create_dumb(
        &self,
        request: DumbRequest,
        pitch: u32,
        size: u64,
    ) -> DrmResult<Arc<dyn GemBacking>> {
        self.state.ensure_retirement_worker()?;
        self.state.retry_retired_2d_resources();
        if request.bpp != 32
            || pitch < request.width.checked_mul(4).ok_or(DrmError::Overflow)?
            || !pitch.is_multiple_of(4)
        {
            return Err(DrmError::Unsupported);
        }
        let size = usize::try_from(size).map_err(|_| DrmError::Overflow)?;
        let bytes = checked_align_up(size, PageSize::Size4K as usize).ok_or(DrmError::Overflow)?;
        let pages =
            Arc::try_new(SharedPages::new_fixed(bytes, PageSize::Size4K).map_err(map_ax_error)?)
                .map_err(|_| DrmError::NoMemory)?;
        let mut entries: Vec<(u64, u32)> = Vec::new();
        entries
            .try_reserve_exact(pages.len())
            .map_err(|_| DrmError::NoMemory)?;
        for index in 0..pages.len() {
            let paddr = pages.paddr_at(index).map_err(map_ax_error)?.as_usize() as u64;
            let merged = if let Some((base, length)) = entries.last_mut() {
                if base.checked_add(*length as u64) == Some(paddr) {
                    *length = length
                        .checked_add(PageSize::Size4K as u32)
                        .ok_or(DrmError::Overflow)?;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !merged {
                entries.push((paddr, PageSize::Size4K as u32));
            }
        }
        let resource_width = pitch / 4;
        let resource = self
            .state
            .submit_control_and_wait(DriverGpuBatch::Create2d {
                width: resource_width,
                height: request.height,
                entries: Vec::new(),
            })?
            .0
            .resource_id
            .ok_or(DrmError::Invalid)?;
        self.state
            .submit_control_and_wait(DriverGpuBatch::AttachBacking { resource, entries })?;
        let backing = match Arc::try_new(VirtioGemBacking {
            pages: pages.clone(),
            resource,
            adapter: self.state.clone(),
        }) {
            Ok(backing) => backing,
            Err(_) => {
                // The resource is already attached to `pages`.  Destroy it
                // before dropping the final caller-side reference; on detach
                // failure `retire_resource` retains (or deliberately leaks)
                // the backing rather than letting the device DMA freed pages.
                self.state.retire_resource(resource, None, pages);
                return Err(DrmError::NoMemory);
            }
        };
        // `DisplayAdapter` is always reached through its owning Arc. Install
        // the map before publishing the type-erased backing to DRM.
        let key = Arc::as_ptr(&backing) as usize;
        let mut resources = self.state.resources.lock();
        if resources.try_reserve(1).is_err() {
            drop(resources);
            return Err(DrmError::NoMemory);
        }
        let replaced = resources.insert(key, resource);
        debug_assert!(replaced.is_none());
        drop(resources);
        Ok(backing)
    }

    fn present(&self, scanout: Scanout) -> DrmResult<Arc<Fence>> {
        self.state.retry_retired_2d_resources();
        self.state.enqueue_present_job(scanout)
    }

    fn update_cursor(&self, cursor: CursorUpdate) -> DrmResult<Arc<Fence>> {
        if cursor.width != 64 || cursor.height != 64 || cursor.hot_x >= 64 || cursor.hot_y >= 64 {
            return Err(DrmError::Invalid);
        }
        let backing = Arc::clone(&cursor.backing);
        let resource = self.state.resource_for(&backing)?;
        self.state.enqueue_cursor(
            DriverCursorUpdate {
                resource,
                width: cursor.width,
                height: cursor.height,
                hot_x: cursor.hot_x,
                hot_y: cursor.hot_y,
                x: cursor.x,
                y: cursor.y,
            },
            backing,
        )
    }

    fn move_cursor(&self, x: i32, y: i32) -> DrmResult<Arc<Fence>> {
        self.state.enqueue_cursor_move(x, y)
    }
    fn display_config_changed(&self) -> DrmResult<Option<DisplayConfig>> {
        self.state
            .transport
            .lock()
            .display_config_changed()
            .map_err(map_dev_error)
    }
}

fn map_dev_error(error: DevError) -> DrmError {
    match error {
        DevError::InvalidParam => DrmError::Invalid,
        DevError::NoMemory => DrmError::NoMemory,
        DevError::ResourceBusy | DevError::AlreadyExists => DrmError::Busy,
        DevError::Again => DrmError::QueueFull,
        DevError::Unsupported => DrmError::Unsupported,
        DevError::BadState | DevError::Io => DrmError::DeviceLost,
    }
}

fn map_ax_error(error: axerrno::AxError) -> DrmError {
    match error {
        axerrno::AxError::NoMemory => DrmError::NoMemory,
        axerrno::AxError::InvalidInput => DrmError::Invalid,
        _ => DrmError::Unsupported,
    }
}

/// Claims a VirtIO GPU from axdisplay and publishes the single DRM device.
/// No compatible GPU simply leaves the legacy display path untouched.
pub fn init() -> DrmResult<bool> {
    let Some(display) = axdisplay::take_drm_display() else {
        return Ok(false);
    };
    let adapter = VirtioGpuAdapter::new(DisplayTransport(Box::new(display)));
    let candidate: Arc<dyn RenderAdapter> = Arc::new(VirtioRenderAdapter {
        state: adapter.state.clone(),
    });
    // Do not create a render node for a plain 2D virtio-gpu.  Querying the
    // first capset also proves the VIRGL transport completed a round trip.
    let render = candidate
        .capset_info(0)
        .ok()
        .and_then(|(id, ..)| (id == 1).then_some(candidate));
    let adapter: Arc<dyn DisplayAdapter> = Arc::new(adapter);
    super::register_primary_device(DrmDevice::with_render(adapter, render, 1, 2, 3, 4))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        created: Vec<(u32, u32, u64, u32)>,
        presented: Vec<(u32, u32, u32)>,
        destroy_fails: bool,
        destroys: u32,
        destroy_calls: Arc<AtomicUsize>,
        transport_drops: Arc<AtomicUsize>,
        render_next: u32,
        render_attach_fails: u32,
        render_detach_fails: u32,
        render_unref_fails: u32,
        render_creates: u32,
        render_detaches: u32,
        render_unrefs: u32,
    }
    impl GpuTransport for FakeTransport {
        fn submit(
            &mut self,
            _: DriverGpuQueue,
            batch: DriverGpuBatch,
            _: u64,
        ) -> Result<DriverGpuSubmission, DevError> {
            if let DriverGpuBatch::Present {
                resource,
                width,
                height,
                ..
            } = batch
            {
                self.presented.push((resource, width, height));
                Ok(DriverGpuSubmission {
                    fence_id: 1,
                    resource_id: None,
                    context_id: None,
                })
            } else {
                Err(DevError::Unsupported)
            }
        }
        fn drain_completions(
            &mut self,
            _: DriverGpuQueue,
            _: &mut [DriverGpuCompletion],
        ) -> Result<usize, DevError> {
            Ok(0)
        }
        fn reset(&mut self, _: DriverGpuQueue, _: &mut [DriverGpuCompletion]) -> usize {
            0
        }
    }

    impl Drop for FakeTransport {
        fn drop(&mut self) {
            self.transport_drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn fake_transport_gets_page_aligned_backing_and_scanout() {
        let _context = crate::test_support::scheduler_test_context();
        let adapter = VirtioGpuAdapter::new(FakeTransport::default());
        let backing = adapter
            .create_dumb(
                DumbRequest {
                    width: 17,
                    height: 2,
                    bpp: 32,
                },
                128,
                256,
            )
            .unwrap();
        adapter
            .present(Scanout {
                backing,
                width: 17,
                height: 2,
                pitch: 128,
                bpp: 32,
                format: 0x3432_5258,
                framebuffer_width: 17,
                framebuffer_height: 2,
                backing_size: 256,
                framebuffer_offset: 0,
                offset: 0,
                source_x: 0,
                source_y: 0,
                mode: super::super::Mode {
                    width: 17,
                    height: 2,
                    refresh_millihz: 60_000,
                },
                damage: None,
            })
            .unwrap();
        assert!(adapter.state.resources.lock().is_empty());
        let transport = adapter.state.transport.lock();
        assert_eq!(transport.created[0].0, 32);
        assert_eq!(transport.created[0].3, 4096);
        assert_eq!(transport.presented, [(7, 17, 2)]);
    }

    #[test]
    fn failed_detach_retains_dma_backing() {
        let _context = crate::test_support::scheduler_test_context();
        let mut transport = FakeTransport::default();
        transport.destroy_fails = true;
        let adapter = VirtioGpuAdapter::new(transport);
        let backing = adapter
            .create_dumb(
                DumbRequest {
                    width: 16,
                    height: 1,
                    bpp: 32,
                },
                64,
                64,
            )
            .unwrap();
        drop(backing);
        assert_eq!(adapter.state.transport.lock().destroys, 1);
        assert_eq!(adapter.state.retired_2d_resources.lock().len(), 1);
    }

    #[test]
    fn exported_2d_backing_keeps_transport_alive_until_detach_then_unref() {
        let _context = crate::test_support::scheduler_test_context();
        let destroys = Arc::new(AtomicUsize::new(0));
        let transport_drops = Arc::new(AtomicUsize::new(0));
        let mut transport = FakeTransport::default();
        transport.destroy_calls = destroys.clone();
        transport.transport_drops = transport_drops.clone();
        let adapter = VirtioGpuAdapter::new(transport);
        let state = Arc::downgrade(&adapter.state);
        let backing = adapter
            .create_dumb(
                DumbRequest {
                    width: 16,
                    height: 1,
                    bpp: 32,
                },
                64,
                64,
            )
            .unwrap();
        drop(adapter);
        assert!(state.upgrade().is_some());
        assert_eq!(destroys.load(Ordering::Relaxed), 0);
        assert_eq!(transport_drops.load(Ordering::Relaxed), 0);
        drop(backing);
        assert!(state.upgrade().is_none());
        assert_eq!(destroys.load(Ordering::Relaxed), 1);
        assert_eq!(transport_drops.load(Ordering::Relaxed), 1);
    }

    fn render_pages() -> Arc<SharedPages> {
        Arc::new(SharedPages::new_fixed(4096, PageSize::Size4K).unwrap())
    }

    #[test]
    fn render_detach_failure_retries_before_freeing_pages() {
        let _context = crate::test_support::scheduler_test_context();
        let mut transport = FakeTransport::default();
        transport.render_detach_fails = 1;
        let adapter = VirtioGpuAdapter::new(transport);
        adapter
            .state
            .retire_render_resource(41, render_pages(), true);
        assert_eq!(adapter.state.retired_render_resources.lock().len(), 1);
        adapter.state.retry_retired_render_resources();
        assert!(adapter.state.retired_render_resources.lock().is_empty());
        let transport = adapter.state.transport.lock();
        assert_eq!(transport.render_detaches, 2);
        assert_eq!(transport.render_unrefs, 1);
    }

    #[test]
    fn render_unref_failure_retries_without_a_second_detach() {
        let _context = crate::test_support::scheduler_test_context();
        let mut transport = FakeTransport::default();
        transport.render_unref_fails = 1;
        let adapter = VirtioGpuAdapter::new(transport);
        adapter
            .state
            .retire_render_resource(42, render_pages(), true);
        assert_eq!(adapter.state.retired_render_resources.lock().len(), 1);
        adapter.state.retry_retired_render_resources();
        assert!(adapter.state.retired_render_resources.lock().is_empty());
        let transport = adapter.state.transport.lock();
        assert_eq!(transport.render_detaches, 1);
        assert_eq!(transport.render_unrefs, 2);
    }

    #[test]
    fn failed_render_attach_unrefs_and_does_not_exhaust_retirement_capacity() {
        let _context = crate::test_support::scheduler_test_context();
        let mut transport = FakeTransport::default();
        transport.render_attach_fails = 1;
        let adapter = VirtioGpuAdapter::new(transport);
        let render: Arc<dyn RenderAdapter> = Arc::new(VirtioRenderAdapter {
            state: adapter.state.clone(),
        });
        let resource = RenderResource {
            target: 2,
            format: 1,
            bind: 0,
            width: 1,
            height: 1,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: 0,
        };
        assert!(
            render
                .create_resource(resource, &[(0, 4096)], render_pages())
                .is_err()
        );
        for _ in 0..129 {
            let pages = render_pages();
            let id = render
                .create_resource(resource, &[(0, 4096)], pages.clone())
                .unwrap();
            render.retire_resource(id, pages, true);
        }
        let transport = adapter.state.transport.lock();
        assert_eq!(transport.render_creates, 130);
        assert_eq!(transport.render_detaches, 129);
        assert_eq!(transport.render_unrefs, 130);
        assert!(adapter.state.retired_render_resources.lock().is_empty());
        assert_eq!(adapter.state.final_render_leaks.load(Ordering::Relaxed), 0);
    }
}
