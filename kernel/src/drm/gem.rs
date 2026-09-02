use alloc::sync::Arc;

use crate::mm::SharedPages;

/// Driver-owned storage for a GEM object.  The DRM core never maps or copies it.
pub trait GemBacking: Send + Sync {
    /// The fixed pages retained by a VMA after its originating GEM handle closes.
    fn shared_pages(&self) -> super::DrmResult<Arc<SharedPages>>;
    /// Host VirtIO resource identity, when this backing is directly owned by
    /// a render/blob or scanout resource.  PRIME aliases retain the same
    /// backing Arc, so this identity follows imports instead of being tied to
    /// a per-file GEM handle.
    fn host_resource(&self) -> Option<HostResource> {
        None
    }
}

/// VirtIO resource IDs have disjoint 2D, 3D and blob semantics.  Numeric
/// equality is never authority to submit a 3D resource to SET_SCANOUT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostResourceKind {
    Scanout2d,
    Render3d,
    Blob,
}

/// Typed host ownership carried by the backing itself.  A GEM handle is only
/// a per-file name, so this is deliberately attached to the shared backing
/// and survives PRIME import/export unchanged.
#[derive(Clone, Copy)]
pub enum HostResource {
    Scanout2d {
        resource: u32,
    },
    Render3d {
        resource: u32,
        meta: super::render::RenderResource,
    },
    Blob {
        resource: u32,
        mem: super::render::BlobMem,
        flags: u32,
        size: u64,
        mapped: bool,
    },
}

impl HostResource {
    pub const fn id(self) -> u32 {
        match self {
            Self::Scanout2d { resource }
            | Self::Render3d { resource, .. }
            | Self::Blob { resource, .. } => resource,
        }
    }
    pub const fn kind(self) -> HostResourceKind {
        match self {
            Self::Scanout2d { .. } => HostResourceKind::Scanout2d,
            Self::Render3d { .. } => HostResourceKind::Render3d,
            Self::Blob { .. } => HostResourceKind::Blob,
        }
    }
}

/// Per-OFD object name, never valid in another [`crate::drm::DrmFile`].
pub type GemHandle = u32;
/// Per-device mmap token.  The devfs adapter turns this into a byte offset.
pub type MmapOffset = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumbRequest {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
}

#[derive(Clone)]
pub struct DumbBuffer {
    pub handle: GemHandle,
    pub pitch: u32,
    pub size: u64,
    pub mmap_offset: MmapOffset,
}

pub(crate) struct GemObject {
    pub(crate) backing: Arc<dyn GemBacking>,
    pub(crate) size: u64,
    pub(crate) mmap_offset: MmapOffset,
    pub(crate) reservation: super::fence::Reservation,
    /// Nonzero only for a legacy virgl resource owned by this GEM object.
    pub(crate) render_resource: Option<u32>,
    pub(crate) render_meta: Option<super::render::RenderResource>,
    pub(crate) render_blob_mem: Option<u32>,
}

impl GemObject {
    pub(crate) fn new(backing: Arc<dyn GemBacking>, size: u64, mmap_offset: MmapOffset) -> Self {
        Self {
            backing,
            size,
            mmap_offset,
            reservation: super::fence::Reservation::new(),
            render_resource: None,
            render_meta: None,
            render_blob_mem: None,
        }
    }

    pub(crate) fn render(
        backing: Arc<dyn GemBacking>,
        size: u64,
        mmap_offset: MmapOffset,
        resource: u32,
        meta: super::render::RenderResource,
        blob_mem: Option<u32>,
    ) -> Self {
        Self {
            backing,
            size,
            mmap_offset,
            reservation: super::fence::Reservation::new(),
            render_resource: Some(resource),
            render_meta: Some(meta),
            render_blob_mem: blob_mem,
        }
    }
}
