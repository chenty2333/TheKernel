//! Unprivileged legacy-virgl render-node ABI.
//!
//! This deliberately implements only the old VIRGL path.  Blob, Venus,
//! context-init and UUID ioctls are not accepted because the transport cannot
//! truthfully provide their lifetime or memory-domain guarantees.

use alloc::{sync::Arc, vec::Vec};
use core::{
    mem::{MaybeUninit, size_of},
    slice,
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axhal::paging::PageSize;

use super::{
    DrmError, DrmFile, DrmResult, GemBacking, fence::Fence, gem::GemObject, ioctl::UserCopy,
    syncobj, uapi,
};
use crate::mm::{SharedPages, checked_align_up};

const PARAM_3D_FEATURES: u64 = 1;
const PARAM_CAPSET_QUERY_FIX: u64 = 2;
const PARAM_SUPPORTED_CAPSET_IDS: u64 = 7;
const PARAM_RESOURCE_BLOB: u64 = 3;
const PARAM_HOST_VISIBLE: u64 = 4;
const PARAM_CONTEXT_INIT: u64 = 6;
const CAPSET_VIRGL: u32 = 1;
const CAPSET_VENUS: u32 = 4;
/// One virtio control-buffer payload.  This is the negotiated driver limit,
/// not an arbitrary userspace-visible 4 KiB restriction.
const MAX_COMMAND: usize = 1024 * 1024;
const MAX_HANDLES: usize = 128;
// The only formats for which this legacy path can prove a linear four-byte
// layout. These are the first four virgl/Gallium BGRA/ARGB 8:8:8:8 formats.
const LINEAR_4BPP_FORMATS: core::ops::RangeInclusive<u32> = 1..=4;
const PIPE_TEXTURE_2D: u32 = 2;

#[derive(Clone, Copy)]
pub struct RenderResource {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}
#[derive(Clone, Copy)]
pub struct RenderTransfer {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub offset: u64,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModernFeatures {
    pub resource_uuid: bool,
    pub resource_blob: bool,
    pub context_init: bool,
    pub host_visible: bool,
}

/// One queued EXECBUFFER.  The adapter owns this record until the host has
/// reported a terminal completion and every context attachment has been
/// removed.  Keeping GEM objects here, rather than merely their resource
/// numbers, prevents a close or handle deletion from retiring host backing
/// while the command is in flight.
pub struct RenderJob {
    pub context: u32,
    pub ring_idx: u32,
    pub commands: Vec<u8>,
    pub resources: Vec<u32>,
    // Deliberately not exposed to adapters: it is solely an ownership pin.
    objects: Vec<Arc<GemObject>>,
    pub inputs: Vec<Arc<Fence>>,
    /// These syncobjs all retain the exact completion fence.  A timeline
    /// point is published unsignaled before queue admission, then becomes
    /// terminal only when that host completion signals the shared fence.
    pub outputs: Vec<(Arc<syncobj::Syncobj>, u64, bool)>,
    pub predecessors: Vec<Arc<Fence>>,
    pub completion: Arc<Fence>,
    pub cancelled: Arc<core::sync::atomic::AtomicBool>,
}

struct FenceFailureGuard(Arc<Fence>);
impl FenceFailureGuard {
    fn disarm(self) {
        core::mem::forget(self);
    }
}
impl Drop for FenceFailureGuard {
    fn drop(&mut self) {
        self.0.signal_error();
    }
}

/// The narrow safe boundary implemented by the virtio adapter. Retirement is
/// responsible for detaching before unref and retaining pages until that
/// sequence succeeds.
pub trait RenderAdapter: Send + Sync {
    fn modern_features(&self) -> ModernFeatures {
        ModernFeatures::default()
    }
    fn capset_info(&self, index: u32) -> DrmResult<(u32, u32, u32)>;
    fn capset(&self, id: u32, version: u32, data: &mut [u8]) -> DrmResult<usize>;
    fn create_context(&self, name: &[u8]) -> DrmResult<u32>;
    fn create_context_with_init(&self, name: &[u8], init: ContextInit) -> DrmResult<u32> {
        let _ = init;
        self.create_context(name)
    }
    fn destroy_context(&self, context: u32) -> DrmResult<()>;
    /// Make all outstanding jobs for `context` terminal before its host
    /// context may be destroyed.  This is a cancellation boundary, not a
    /// best-effort notification.
    fn cancel_context(&self, context: u32);
    fn create_resource(
        &self,
        resource: RenderResource,
        entries: &[(u64, u32)],
        pages: Arc<SharedPages>,
    ) -> DrmResult<u32>;
    /// The object ID returned by create is guest-reserved.  Consumers must
    /// retain this fence as a reservation dependency until host CREATE and
    /// guest-backing attach have reached a terminal completion.
    fn resource_ready(&self, _: u32) -> DrmResult<Arc<Fence>> {
        Err(DrmError::NotFound)
    }
    fn create_blob(
        &self,
        blob: BlobResource,
        entries: &[(u64, u32)],
        pages: Arc<SharedPages>,
    ) -> DrmResult<u32> {
        let _ = (blob, entries, pages);
        Err(DrmError::Unsupported)
    }
    /// Returns an externally owned, cache-typed host-visible page vector.
    /// The returned SharedPages retains MAP_BLOB until its final VMA/GEM drop.
    fn map_blob(&self, _: u32, _: u64) -> DrmResult<Arc<SharedPages>> {
        Err(DrmError::Unsupported)
    }
    fn resource_uuid(&self, _: u32) -> DrmResult<[u8; 16]> {
        Err(DrmError::Unsupported)
    }
    fn retire_resource(&self, resource: u32, pages: Arc<SharedPages>, backing_attached: bool);
    fn attach_resource(&self, context: u32, resource: u32) -> DrmResult<()>;
    fn detach_resource(&self, context: u32, resource: u32) -> DrmResult<()>;
    fn transfer(
        &self,
        context: u32,
        resource: u32,
        transfer: RenderTransfer,
        to_host: bool,
    ) -> DrmResult<()>;
    /// Queue an EXECBUFFER without waiting.  The adapter must retain `job`
    /// through the exact terminal host completion, then detach its resources
    /// and signal or error-signal `job.completion`.
    fn submit(&self, job: RenderJob) -> DrmResult<()>;
}

struct RenderBacking {
    pages: Arc<SharedPages>,
    resource: u32,
    adapter: Arc<dyn RenderAdapter>,
    kind: RenderBackingKind,
    meta: RenderResource,
}
#[derive(Clone, Copy)]
enum RenderBackingKind {
    Render3d,
    Blob {
        mem: BlobMem,
        flags: u32,
        size: u64,
        mapped: bool,
    },
}
impl GemBacking for RenderBacking {
    fn shared_pages(&self) -> DrmResult<Arc<SharedPages>> {
        Ok(self.pages.clone())
    }
    fn host_resource(&self) -> Option<super::gem::HostResource> {
        Some(match self.kind {
            RenderBackingKind::Render3d => super::gem::HostResource::Render3d {
                resource: self.resource,
                meta: self.meta,
            },
            RenderBackingKind::Blob {
                mem,
                flags,
                size,
                mapped,
            } => super::gem::HostResource::Blob {
                resource: self.resource,
                mem,
                flags,
                size,
                mapped,
            },
        })
    }
}
impl Drop for RenderBacking {
    fn drop(&mut self) {
        self.adapter.retire_resource(
            self.resource,
            self.pages.clone(),
            matches!(self.kind, RenderBackingKind::Render3d),
        );
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Map {
    offset: u64,
    handle: u32,
    pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetParam {
    param: u64,
    value: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Create {
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
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    stride: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Info {
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    blob_mem: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Box3 {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Transfer {
    bo_handle: u32,
    box_: Box3,
    level: u32,
    offset: u32,
    stride: u32,
    layer_stride: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Wait {
    handle: u32,
    flags: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Caps {
    cap_set_id: u32,
    cap_set_ver: u32,
    addr: u64,
    size: u32,
    pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Exec {
    flags: u32,
    size: u32,
    command: u64,
    bo_handles: u64,
    num_bo_handles: u32,
    fence_fd: i32,
    ring_idx: u32,
    syncobj_stride: u32,
    num_in_syncobjs: u32,
    num_out_syncobjs: u32,
    in_syncobjs: u64,
    out_syncobjs: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ExecSyncobj {
    handle: u32,
    flags: u32,
    point: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CreateBlob {
    blob_mem: u32,
    blob_flags: u32,
    bo_handle: u32,
    res_handle: u32,
    size: u64,
    pad: u32,
    cmd_size: u32,
    cmd: u64,
    blob_id: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ContextSetParam {
    param: u64,
    value: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ContextInitIoctl {
    num_params: u32,
    pad: u32,
    ctx_set_params: u64,
}

fn read<T: Copy>(copy: &impl super::ioctl::UserCopy, addr: usize) -> AxResult<T> {
    let mut v = MaybeUninit::<T>::zeroed();
    let b = unsafe {
        slice::from_raw_parts_mut(v.as_mut_ptr().cast::<MaybeUninit<u8>>(), size_of::<T>())
    };
    copy.read(addr, b)?;
    Ok(unsafe { v.assume_init() })
}
fn write<T>(copy: &impl super::ioctl::UserCopy, addr: usize, value: &T) -> AxResult<()> {
    copy.write(addr, unsafe {
        slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>())
    })
}
fn bytes(copy: &impl super::ioctl::UserCopy, addr: u64, n: usize, max: usize) -> AxResult<Vec<u8>> {
    if n > max {
        return Err(AxError::InvalidInput);
    }
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| AxError::NoMemory)?;
    v.resize(n, 0);
    copy.read(
        usize::try_from(addr).map_err(|_| AxError::BadAddress)?,
        unsafe { slice::from_raw_parts_mut(v.as_mut_ptr().cast(), n) },
    )?;
    Ok(v)
}
fn handles(copy: &impl super::ioctl::UserCopy, addr: u64, n: usize) -> AxResult<Vec<u32>> {
    if n > MAX_HANDLES {
        return Err(AxError::InvalidInput);
    };
    let mut out: Vec<u32> = Vec::new();
    out.try_reserve_exact(n).map_err(|_| AxError::NoMemory)?;
    let base = usize::try_from(addr).map_err(|_| AxError::BadAddress)?;
    for i in 0..n {
        out.push(read(
            copy,
            base.checked_add(i.checked_mul(4).ok_or(AxError::InvalidInput)?)
                .ok_or(AxError::BadAddress)?,
        )?)
    }
    Ok(out)
}
fn exec_syncobjs(
    file: &DrmFile,
    copy: &impl super::ioctl::UserCopy,
    addr: u64,
    count: u32,
    stride: u32,
    input: bool,
) -> AxResult<Vec<(Arc<syncobj::Syncobj>, u64, bool)>> {
    const SYNCOBJ_RESET: u32 = 1;
    if count as usize > MAX_HANDLES || (count != 0 && (stride as usize) < size_of::<ExecSyncobj>())
    {
        return Err(AxError::InvalidInput);
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let base = usize::try_from(addr).map_err(|_| AxError::BadAddress)?;
    let mut out: Vec<(Arc<syncobj::Syncobj>, u64, bool)> = Vec::new();
    out.try_reserve_exact(count as usize)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..count as usize {
        let offset = index
            .checked_mul(stride as usize)
            .ok_or(AxError::BadAddress)?;
        let item: ExecSyncobj = read(copy, base.checked_add(offset).ok_or(AxError::BadAddress)?)?;
        if item.flags & !SYNCOBJ_RESET != 0 || (input && item.flags != 0) {
            return Err(AxError::OperationNotSupported);
        }
        let object = file.syncobj(item.handle).map_err(drm)?;
        let reset = item.flags & SYNCOBJ_RESET != 0;
        if input {
            object.fence_at(item.point)?;
        } else if item.point != 0 && !reset {
            match object.fence_at(item.point) {
                Err(AxError::NotFound) => {}
                Ok(_) => return Err(AxError::InvalidInput),
                Err(error) => return Err(error),
            }
        }
        // One output update per syncobj makes RESET + point publication a
        // single object-local transaction, rather than exposing an ordering
        // dependent sequence of partial updates.
        if !input
            && out
                .iter()
                .any(|(existing, ..)| Arc::ptr_eq(existing, &object))
        {
            return Err(AxError::InvalidInput);
        }
        out.push((object, item.point, reset));
    }
    Ok(out)
}
fn drm(e: DrmError) -> AxError {
    e.into()
}

pub(super) fn dispatch(
    file: &DrmFile,
    context: &crate::file::IoctlContext,
    cmd: u32,
    arg: usize,
) -> AxResult<usize> {
    let copy = context;
    match cmd as u64 {
        uapi::DRM_IOCTL_VIRTGPU_GETPARAM => {
            let mut r: GetParam = read(copy, arg)?;
            let modern = file.render_adapter().map_err(drm)?.modern_features();
            r.value = match r.param {
                PARAM_3D_FEATURES | PARAM_CAPSET_QUERY_FIX => 1,
                PARAM_SUPPORTED_CAPSET_IDS => supported_capsets(file, modern)?,
                PARAM_RESOURCE_BLOB => u64::from(modern.resource_blob),
                PARAM_HOST_VISIBLE => u64::from(modern.host_visible),
                PARAM_CONTEXT_INIT => u64::from(modern.context_init),
                _ => return Err(AxError::InvalidInput),
            };
            write(copy, arg, &r)?;
        }
        uapi::DRM_IOCTL_VIRTGPU_GET_CAPS => {
            let r: Caps = read(copy, arg)?;
            let modern = file.render_adapter().map_err(drm)?.modern_features();
            if r.pad != 0 || r.cap_set_id >= 64 {
                return Err(AxError::InvalidInput);
            }
            let a = file.render_adapter().map_err(drm)?;
            let Some((_, max)) = capset_index(&a, r.cap_set_id, modern).map_err(drm)? else {
                return Err(AxError::InvalidInput);
            };
            if r.size > max {
                return Err(AxError::InvalidInput);
            }
            let mut data = bytes(copy, r.addr, r.size as usize, MAX_COMMAND)?;
            let actual = a
                .capset(r.cap_set_id, r.cap_set_ver, &mut data)
                .map_err(drm)?;
            if actual != data.len() {
                return Err(AxError::InvalidInput);
            }
            copy.write(
                usize::try_from(r.addr).map_err(|_| AxError::BadAddress)?,
                &data,
            )?;
        }
        uapi::DRM_IOCTL_VIRTGPU_RESOURCE_CREATE => create(file, copy, arg)?,
        uapi::DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB => create_blob(file, copy, arg)?,
        uapi::DRM_IOCTL_VIRTGPU_CONTEXT_INIT => context_init(file, copy, arg)?,
        uapi::DRM_IOCTL_VIRTGPU_RESOURCE_INFO => info(file, copy, arg)?,
        uapi::DRM_IOCTL_VIRTGPU_MAP => map(file, copy, arg)?,
        uapi::DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST => transfer(file, copy, arg, true)?,
        uapi::DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST => transfer(file, copy, arg, false)?,
        uapi::DRM_IOCTL_VIRTGPU_EXECBUFFER => exec(file, context, arg)?,
        uapi::DRM_IOCTL_VIRTGPU_WAIT => {
            let r: Wait = read(copy, arg)?;
            if r.flags & !1 != 0 {
                return Err(AxError::InvalidInput);
            }
            let (_, object) = file.render_resource(r.handle).map_err(drm)?;
            if let Some(predecessor) = object.reservation.predecessor() {
                let timeout = (r.flags & 1 != 0).then_some(Duration::ZERO);
                predecessor.wait(timeout)?;
            }
        }
        _ => return Err(AxError::NotATty),
    };
    Ok(0)
}
fn capset_index(
    adapter: &Arc<dyn RenderAdapter>,
    id: u32,
    modern: ModernFeatures,
) -> DrmResult<Option<(u32, u32)>> {
    for index in 0..8 {
        let (candidate, _, max_size) = match adapter.capset_info(index) {
            Ok(info) => info,
            Err(_) => continue,
        };
        if candidate == id {
            if candidate == CAPSET_VENUS && !(modern.resource_blob && modern.context_init) {
                return Ok(None);
            }
            return Ok(Some((index, max_size)));
        }
    }
    Ok(None)
}
fn supported_capsets(file: &DrmFile, modern: ModernFeatures) -> AxResult<u64> {
    let adapter = file.render_adapter().map_err(drm)?;
    let mut ids = 0u64;
    for index in 0..8 {
        let Ok((id, ..)) = adapter.capset_info(index) else {
            continue;
        };
        if id >= 64 || (id == CAPSET_VENUS && !(modern.resource_blob && modern.context_init)) {
            continue;
        }
        ids |= 1u64 << id;
    }
    Ok(ids)
}
fn create(file: &DrmFile, copy: &impl super::ioctl::UserCopy, arg: usize) -> AxResult<()> {
    let mut r: Create = read(copy, arg)?;
    if r.bo_handle != 0 || r.res_handle != 0 || r.size == 0 {
        return Err(AxError::InvalidInput);
    };
    validate_linear_resource(&r)?;
    let size = r.size as u64;
    let alloc = checked_align_up(
        usize::try_from(size).map_err(|_| AxError::InvalidInput)?,
        PageSize::Size4K as usize,
    )
    .ok_or(AxError::InvalidInput)?;
    let pages = Arc::try_new(
        SharedPages::new_fixed(alloc, PageSize::Size4K).map_err(|_| AxError::NoMemory)?,
    )
    .map_err(|_| AxError::NoMemory)?;
    let mut entries = Vec::new();
    for i in 0..pages.len() {
        entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        entries.push((
            pages
                .paddr_at(i)
                .map_err(|_| AxError::InvalidInput)?
                .as_usize() as u64,
            4096,
        ));
    }
    let a = file.render_adapter().map_err(drm)?;
    let meta = RenderResource {
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
    };
    let resource = a
        .create_resource(meta, &entries, pages.clone())
        .map_err(drm)?;
    let backing: Arc<dyn GemBacking> = Arc::new(RenderBacking {
        pages,
        resource,
        adapter: a.clone(),
        kind: RenderBackingKind::Render3d,
        meta,
    });
    let handle = file
        .create_render_gem(backing, size, resource, meta, None)
        .map_err(drm)?;
    // CREATE_3D and ATTACH_BACKING are asynchronous. Publish their shared
    // ready fence into the initial reservation so every transfer/EXECBUFFER
    // observes the guest-reserved resource only after it is host-live.
    let (_, object) = file.render_resource(handle).map_err(drm)?;
    object
        .reservation
        .publish(a.resource_ready(resource).map_err(drm)?);
    r.bo_handle = handle;
    r.res_handle = resource;
    write(copy, arg, &r)
}

fn validate_linear_resource(r: &Create) -> AxResult<()> {
    // Transfers below use a four-byte-per-pixel bound. Refuse every format or
    // layout for which that calculation would not prove the host DMA range.
    if !LINEAR_4BPP_FORMATS.contains(&r.format)
        || r.target != PIPE_TEXTURE_2D
        || r.nr_samples != 0
        || r.last_level != 0
        || r.width == 0
        || r.height == 0
        || r.depth != 1
        || r.array_size != 1
    {
        return Err(AxError::InvalidInput);
    }
    let bytes = u64::from(r.width)
        .checked_mul(u64::from(r.height))
        .and_then(|value| value.checked_mul(4))
        .ok_or(AxError::InvalidInput)?;
    if bytes > u64::from(r.size) {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}
fn create_blob(file: &DrmFile, copy: &impl super::ioctl::UserCopy, arg: usize) -> AxResult<()> {
    let mut r: CreateBlob = read(copy, arg)?;
    let modern = file.render_adapter().map_err(drm)?.modern_features();
    if !modern.resource_blob || r.bo_handle != 0 || r.res_handle != 0 || r.size == 0 || r.pad != 0 {
        return Err(AxError::OperationNotSupported);
    }
    // Mappable HOST3D/HOST3D_GUEST is backed by the validated PCI hostmem
    // aperture returned from MAP_BLOB.  Non-mappable blobs retain the normal
    // guest SG backing and transfer fallback.
    let mem = match r.blob_mem {
        1 => BlobMem::Guest,
        3 => BlobMem::Host3dGuest,
        2 => BlobMem::Host3d,
        _ => return Err(AxError::InvalidInput),
    };
    const BLOB_FLAG_MAPPABLE: u32 = 1;
    if r.blob_flags & BLOB_FLAG_MAPPABLE != 0 && !modern.host_visible {
        return Err(AxError::OperationNotSupported);
    }
    if r.blob_flags & BLOB_FLAG_MAPPABLE != 0 && mem == BlobMem::Guest {
        return Err(AxError::OperationNotSupported);
    }
    if r.cmd_size != 0 || r.cmd != 0 {
        return Err(AxError::OperationNotSupported);
    }
    let alloc = checked_align_up(
        usize::try_from(r.size).map_err(|_| AxError::InvalidInput)?,
        PageSize::Size4K as usize,
    )
    .ok_or(AxError::InvalidInput)?;
    let guest_pages = Arc::try_new(
        SharedPages::new_fixed(alloc, PageSize::Size4K).map_err(|_| AxError::NoMemory)?,
    )
    .map_err(|_| AxError::NoMemory)?;
    let mappable = r.blob_flags & BLOB_FLAG_MAPPABLE != 0;
    let mut entries = Vec::new();
    if mem != BlobMem::Host3d {
        entries
            .try_reserve_exact(guest_pages.len())
            .map_err(|_| AxError::NoMemory)?;
        for i in 0..guest_pages.len() {
            entries.push((
                guest_pages
                    .paddr_at(i)
                    .map_err(|_| AxError::InvalidInput)?
                    .as_usize() as u64,
                4096,
            ));
        }
    }
    let adapter = file.render_adapter().map_err(drm)?;
    let resource = adapter
        .create_blob(
            BlobResource {
                mem,
                flags: r.blob_flags,
                size: r.size,
                blob_id: r.blob_id,
            },
            &entries,
            guest_pages.clone(),
        )
        .map_err(drm)?;
    if mappable {
        let ready = adapter.resource_ready(resource).map_err(drm)?;
        ready.wait(None).map_err(|_| AxError::Io)?;
        if ready.is_failed() {
            adapter.retire_resource(resource, guest_pages, false);
            return Err(AxError::Io);
        }
    }
    let (pages, mapped) = if mappable {
        match adapter.map_blob(resource, r.size) {
            Ok(pages) => (pages, true),
            Err(error) => {
                adapter.retire_resource(resource, guest_pages, false);
                return Err(drm(error));
            }
        }
    } else {
        (guest_pages, false)
    };
    let meta = RenderResource {
        target: PIPE_TEXTURE_2D,
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
    let backing: Arc<dyn GemBacking> = Arc::new(RenderBacking {
        pages,
        resource,
        adapter,
        kind: RenderBackingKind::Blob {
            mem,
            flags: r.blob_flags,
            size: r.size,
            mapped,
        },
        meta,
    });
    let handle = file
        .create_render_gem(backing, r.size, resource, meta, Some(r.blob_mem))
        .map_err(drm)?;
    let adapter = file.render_adapter().map_err(drm)?;
    let (_, object) = file.render_resource(handle).map_err(drm)?;
    object
        .reservation
        .publish(adapter.resource_ready(resource).map_err(drm)?);
    r.bo_handle = handle;
    r.res_handle = resource;
    write(copy, arg, &r)
}
fn context_init(file: &DrmFile, copy: &impl super::ioctl::UserCopy, arg: usize) -> AxResult<()> {
    let r: ContextInitIoctl = read(copy, arg)?;
    let modern = file.render_adapter().map_err(drm)?.modern_features();
    if !modern.context_init || r.pad != 0 || r.num_params > 4 {
        return Err(AxError::OperationNotSupported);
    }
    if r.num_params == 0 {
        return file
            .set_render_context_init(ContextInit::default())
            .map_err(drm);
    }
    let base = usize::try_from(r.ctx_set_params).map_err(|_| AxError::BadAddress)?;
    let mut init = ContextInit::default();
    for index in 0..r.num_params as usize {
        let offset = index
            .checked_mul(size_of::<ContextSetParam>())
            .ok_or(AxError::BadAddress)?;
        let param: ContextSetParam =
            read(copy, base.checked_add(offset).ok_or(AxError::BadAddress)?)?;
        match param.param {
            1 => init.capset_id = u32::try_from(param.value).map_err(|_| AxError::InvalidInput)?,
            2 => init.num_rings = u32::try_from(param.value).map_err(|_| AxError::InvalidInput)?,
            3 => init.poll_rings_mask = param.value,
            4 => {
                let address = usize::try_from(param.value).map_err(|_| AxError::BadAddress)?;
                let mut found = false;
                for index in 0..64usize {
                    let byte: u8 =
                        read(copy, address.checked_add(index).ok_or(AxError::BadAddress)?)?;
                    if byte == 0 {
                        init.debug_name_len = index as u8;
                        found = true;
                        break;
                    }
                    init.debug_name[index] = byte;
                }
                if !found {
                    return Err(AxError::InvalidInput);
                }
            }
            _ => return Err(AxError::InvalidInput),
        }
    }
    if init.num_rings > 64
        || (init.num_rings == 0 && init.poll_rings_mask != 0)
        || (init.num_rings != 0 && init.poll_rings_mask >> init.num_rings != 0)
    {
        return Err(AxError::InvalidInput);
    }
    file.set_render_context_init(init).map_err(drm)
}
fn info(file: &DrmFile, copy: &impl super::ioctl::UserCopy, arg: usize) -> AxResult<()> {
    let mut r: Info = read(copy, arg)?;
    let (res, o) = file.render_resource(r.bo_handle).map_err(drm)?;
    r.res_handle = res;
    r.size = u32::try_from(o.size).map_err(|_| AxError::InvalidInput)?;
    r.blob_mem = o.render_blob_mem.unwrap_or(0);
    write(copy, arg, &r)
}
fn map(file: &DrmFile, copy: &impl super::ioctl::UserCopy, arg: usize) -> AxResult<()> {
    let mut r: Map = read(copy, arg)?;
    r.offset = file.map_dumb(r.handle).map_err(drm)?;
    write(copy, arg, &r)
}
fn transfer(
    file: &DrmFile,
    copy: &impl super::ioctl::UserCopy,
    arg: usize,
    to_host: bool,
) -> AxResult<()> {
    let r: Transfer = read(copy, arg)?;
    if r.box_.w == 0 || r.box_.h == 0 || r.box_.d == 0 {
        return Err(AxError::InvalidInput);
    };
    let (res, obj) = file.render_resource(r.bo_handle).map_err(drm)?;
    let meta = obj.render_meta.ok_or(AxError::InvalidInput)?;
    if r.level != 0
        || r.box_
            .x
            .checked_add(r.box_.w)
            .is_none_or(|v| v > meta.width)
        || r.box_
            .y
            .checked_add(r.box_.h)
            .is_none_or(|v| v > meta.height)
        || r.box_
            .z
            .checked_add(r.box_.d)
            .is_none_or(|v| v > meta.depth)
    {
        return Err(AxError::InvalidInput);
    }
    let row = if r.stride == 0 {
        u64::from(r.box_.w).checked_mul(4)
    } else {
        Some(u64::from(r.stride))
    }
    .ok_or(AxError::InvalidInput)?;
    let layer = if r.layer_stride == 0 {
        row.checked_mul(u64::from(r.box_.h))
    } else {
        Some(u64::from(r.layer_stride))
    }
    .ok_or(AxError::InvalidInput)?;
    let end = u64::from(r.offset)
        .checked_add(
            layer
                .checked_mul(u64::from(r.box_.d - 1))
                .ok_or(AxError::InvalidInput)?,
        )
        .and_then(|v| v.checked_add(row.checked_mul(u64::from(r.box_.h - 1))?))
        .and_then(|v| v.checked_add(u64::from(r.box_.w).checked_mul(4)?))
        .ok_or(AxError::InvalidInput)?;
    if end > obj.size {
        return Err(AxError::InvalidInput);
    }
    let a = file.render_adapter().map_err(drm)?;
    let c = file.render_context().map_err(drm)?;
    let completion = Fence::new(false);
    let guard = FenceFailureGuard(completion.clone());
    let mut reservations = [&obj.reservation];
    let predecessors =
        match super::fence::Reservation::replace_many(&mut reservations, completion.clone()) {
            Ok(predecessors) => predecessors,
            Err(error) => {
                completion.signal();
                return Err(error);
            }
        };
    let execution = (|| -> AxResult<()> {
        for predecessor in predecessors {
            predecessor.wait(None)?;
        }
        a.attach_resource(c, res).map_err(drm)?;
        let result = a.transfer(
            c,
            res,
            RenderTransfer {
                x: r.box_.x,
                y: r.box_.y,
                z: r.box_.z,
                width: r.box_.w,
                height: r.box_.h,
                depth: r.box_.d,
                offset: r.offset as u64,
                level: r.level,
                stride: r.stride,
                layer_stride: r.layer_stride,
            },
            to_host,
        );
        let detach = a.detach_resource(c, res);
        result.and(detach).map_err(drm)
    })();
    match execution {
        Ok(()) => {
            guard.disarm();
            completion.signal();
            Ok(())
        }
        Err(error) => {
            // The guard makes every post-publication failure terminal.
            Err(error)
        }
    }
}
fn exec(file: &DrmFile, context: &crate::file::IoctlContext, arg: usize) -> AxResult<()> {
    let copy = context;
    let mut r: Exec = read(copy, arg)?;
    const FENCE_FD_IN: u32 = 1;
    const FENCE_FD_OUT: u32 = 2;
    const RING_IDX: u32 = 4;
    if r.flags & !(FENCE_FD_IN | FENCE_FD_OUT | RING_IDX) != 0
        || (r.flags & RING_IDX == 0 && r.ring_idx != 0)
        || (r.flags & RING_IDX != 0 && r.ring_idx >= file.render_ring_count())
        || (r.num_in_syncobjs != 0 || r.num_out_syncobjs != 0)
            && (r.syncobj_stride as usize) < size_of::<ExecSyncobj>()
        || (r.num_in_syncobjs == 0 && r.in_syncobjs != 0)
        || (r.num_out_syncobjs == 0 && r.out_syncobjs != 0)
        || (r.flags & FENCE_FD_IN == 0 && r.fence_fd != -1)
    {
        return Err(AxError::OperationNotSupported);
    };
    if r.num_in_syncobjs as usize > MAX_HANDLES || r.num_out_syncobjs as usize > MAX_HANDLES {
        return Err(AxError::InvalidInput);
    }
    let commands = bytes(copy, r.command, r.size as usize, MAX_COMMAND)?;
    let hs = handles(copy, r.bo_handles, r.num_bo_handles as usize)?;
    // A GEM object can have more than one handle after PRIME import.  Reserve
    // and attach it once, keyed by object identity rather than by the
    // userspace-provided handle or host resource number.
    let mut objects = Vec::new();
    for h in hs {
        let (resource, object) = file.render_resource(h).map_err(drm)?;
        if !objects
            .iter()
            .any(|(_, existing)| Arc::ptr_eq(existing, &object))
        {
            objects.push((resource, object));
        }
    }
    let resources: Vec<u32> = objects.iter().map(|(resource, _)| *resource).collect();
    let mut reservations = Vec::new();
    reservations
        .try_reserve_exact(objects.len())
        .map_err(|_| AxError::NoMemory)?;
    for (_, object) in &objects {
        reservations.push(&object.reservation);
    }

    // Publish before waiting.  `replace_many` snapshots all predecessors and
    // replaces the full BO set under a single ordered lock acquisition, so a
    // later overlapping submission depends on this completion fence instead
    // of racing an individual object replacement.
    // Do every operation which can fail without changing reservation state
    // first.  Once published, FenceFailureGuard makes every exit terminal.
    let mut inputs = Vec::new();
    inputs
        .try_reserve_exact(r.num_in_syncobjs as usize + usize::from(r.flags & FENCE_FD_IN != 0))
        .map_err(|_| AxError::NoMemory)?;
    if r.flags & FENCE_FD_IN != 0 {
        inputs.push(syncobj::import(context, r.fence_fd)?);
    }
    let input_syncobjs = exec_syncobjs(
        file,
        copy,
        r.in_syncobjs,
        r.num_in_syncobjs,
        r.syncobj_stride,
        true,
    )?;
    for (object, point, _) in input_syncobjs {
        inputs.push(object.fence_at(point)?);
    }
    let outputs = exec_syncobjs(
        file,
        copy,
        r.out_syncobjs,
        r.num_out_syncobjs,
        r.syncobj_stride,
        false,
    )?;
    let adapter = file.render_adapter().map_err(drm)?;
    let render_context = file.render_context().map_err(drm)?;
    // `render_context` freezes CONTEXT_INIT for this file. Recheck after that
    // transition so a concurrent first context creation cannot admit a ring
    // selected against stale pre-init state.
    if r.ring_idx >= file.render_ring_count() {
        return Err(AxError::InvalidInput);
    }
    let completion = Fence::new(false);
    let guard = FenceFailureGuard(completion.clone());
    let exported_fd = if r.flags & FENCE_FD_OUT != 0 {
        let fd = syncobj::export(completion.clone(), context, false)?;
        r.fence_fd = fd;
        if let Err(error) = write(copy, arg, &r) {
            let _ = crate::file::close_file_like(fd);
            return Err(error);
        }
        Some(fd)
    } else {
        None
    };
    let predecessors =
        match super::fence::Reservation::replace_many(&mut reservations, completion.clone()) {
            Ok(predecessors) => predecessors,
            Err(error) => {
                if let Some(fd) = exported_fd {
                    let _ = crate::file::close_file_like(fd);
                }
                return Err(error);
            }
        };

    // Publish each output before queue admission.  The completion stays
    // unsignaled until the VirtIO completion worker observes the host's
    // terminal record, so a fast worker can never complete a job before its
    // output syncobj names the same fence.  From this point on the failure
    // guard makes admission failure visible through that same error fence.
    for (object, point, reset) in &outputs {
        object
            .apply_exec_output(*reset, *point, completion.clone())
            .map_err(|error| {
                if let Some(fd) = exported_fd {
                    let _ = crate::file::close_file_like(fd);
                }
                error
            })?;
    }

    let job = RenderJob {
        context: render_context,
        ring_idx: r.ring_idx,
        commands,
        resources,
        objects: objects.into_iter().map(|(_, object)| object).collect(),
        inputs,
        outputs,
        predecessors,
        completion: completion.clone(),
        cancelled: file.render_cancelled(),
    };

    // This only admits a state-owned job.  In particular it never waits for
    // an input or reservation fence in ioctl context, and FENCE_FD_OUT is
    // exported while still unsignaled.
    if let Err(error) = adapter.submit(job).map_err(drm) {
        if let Some(fd) = exported_fd {
            let _ = crate::file::close_file_like(fd);
        }
        return Err(error);
    }
    // Admission owns the already-published completion fence now.
    let _ = exported_fd;
    guard.disarm();
    Ok(())
}

const _: [(); 16] = [(); size_of::<Map>()];
const _: [(); 64] = [(); size_of::<Exec>()];
const _: [(); 16] = [(); size_of::<ExecSyncobj>()];
const _: [(); 16] = [(); size_of::<GetParam>()];
const _: [(); 56] = [(); size_of::<Create>()];
const _: [(); 16] = [(); size_of::<Info>()];
const _: [(); 44] = [(); size_of::<Transfer>()];
const _: [(); 8] = [(); size_of::<Wait>()];
const _: [(); 24] = [(); size_of::<Caps>()];
const _: [(); 48] = [(); size_of::<CreateBlob>()];
const _: [(); 16] = [(); size_of::<ContextSetParam>()];
const _: [(); 16] = [(); size_of::<ContextInitIoctl>()];

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_create() -> Create {
        Create {
            target: PIPE_TEXTURE_2D,
            format: 1,
            width: 16,
            height: 8,
            depth: 1,
            array_size: 1,
            size: 16 * 8 * 4,
            ..Default::default()
        }
    }

    #[test]
    fn resource_create_rejects_layouts_the_transfer_bounds_cannot_prove() {
        let valid = linear_create();
        assert!(validate_linear_resource(&valid).is_ok());

        let mut invalid = valid;
        invalid.format = 5;
        assert!(validate_linear_resource(&invalid).is_err());
        invalid = valid;
        invalid.nr_samples = 1;
        assert!(validate_linear_resource(&invalid).is_err());
        invalid = valid;
        invalid.size -= 1;
        assert!(validate_linear_resource(&invalid).is_err());
        invalid = valid;
        invalid.depth = 2;
        assert!(validate_linear_resource(&invalid).is_err());
        invalid = valid;
        invalid.array_size = 2;
        assert!(validate_linear_resource(&invalid).is_err());
    }
}
