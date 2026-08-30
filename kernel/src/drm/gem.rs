use alloc::sync::Arc;

use crate::mm::SharedPages;

/// Driver-owned storage for a GEM object.  The DRM core never maps or copies it.
pub trait GemBacking: Send + Sync {
    /// The fixed pages retained by a VMA after its originating GEM handle closes.
    fn shared_pages(&self) -> super::DrmResult<Arc<SharedPages>>;
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
        }
    }

    pub(crate) fn render(
        backing: Arc<dyn GemBacking>,
        size: u64,
        mmap_offset: MmapOffset,
        resource: u32,
        meta: super::render::RenderResource,
    ) -> Self {
        Self {
            backing,
            size,
            mmap_offset,
            reservation: super::fence::Reservation::new(),
            render_resource: Some(resource),
            render_meta: Some(meta),
        }
    }
}
