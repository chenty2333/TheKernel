//! Common traits and types for graphics display device drivers.

#![no_std]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};
use core::any::Any;

#[doc(no_inline)]
pub use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};

#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub fb_base_vaddr: usize,
    pub fb_size: usize,
}

/// A fully validated hardware-cursor update for the sole virtual scanout.
/// The resource is caller-owned and must remain alive until the driver
/// returns a terminal result from the cursor queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrmCursorUpdate {
    pub resource: u32,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub x: i32,
    pub y: i32,
}

/// One bounded resource-space region for a 2D transfer/flush.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrmDamage {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Atomically sampled state of the sole VirtIO scanout after EVENT_DISPLAY.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrmDisplayConfig {
    pub connected: bool,
    pub width: u32,
    pub height: u32,
}

pub struct FrameBuffer<'a> {
    _raw: &'a mut [u8],
}

/// Monotonic identity of a submitted GPU command.  It is not complete merely
/// because submission was accepted; callers must consume a terminal record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuSubmission {
    pub fence_id: u64,
    /// Guest-reserved identities become usable only after this submission's
    /// terminal completion succeeds.  Commands that do not create an object
    /// leave these fields `None`.
    pub resource_id: Option<u32>,
    pub context_id: Option<u32>,
}

/// The two independently drained VirtIO GPU queues exposed to DRM. Queue
/// selection is part of every submission: cursor traffic never shares
/// controlq ownership with rendering or scanout traffic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuQueue {
    Control,
    Cursor,
}

/// An owned GPU command batch. Borrowed userspace or GEM memory is copied
/// into this object before it reaches the device driver, so the lower layer
/// can retain request and response DMA buffers until terminal completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GpuBatch {
    Create2d {
        width: u32,
        height: u32,
        entries: Vec<(u64, u32)>,
    },
    Present {
        resource: u32,
        width: u32,
        height: u32,
        source_x: u32,
        source_y: u32,
        damage: Option<DrmDamage>,
    },
    DestroyResource {
        resource: u32,
    },
    CreateContext {
        name: Vec<u8>,
        init: ContextInit,
    },
    DestroyContext {
        context: u32,
    },
    CreateResource3d {
        resource: RenderResource3D,
    },
    CreateBlob {
        resource: BlobResource,
        entries: Vec<(u64, u32)>,
    },
    /// Direct blob scanout.  This is valid only for a live blob resource
    /// negotiated with RESOURCE_BLOB; the lower driver validates the plane
    /// layout before it reaches the wire.
    PresentBlob {
        resource: u32,
        source_x: u32,
        source_y: u32,
        width: u32,
        height: u32,
        framebuffer_width: u32,
        framebuffer_height: u32,
        format: u32,
        stride: u32,
        offset: u32,
        damage: Option<DrmDamage>,
    },
    CapsetInfo {
        index: u32,
    },
    Capset {
        id: u32,
        version: u32,
        bytes: usize,
    },
    MapBlob {
        resource: u32,
        offset: u64,
    },
    UnmapBlob {
        resource: u32,
    },
    AssignUuid {
        resource: u32,
    },
    AttachBacking {
        resource: u32,
        entries: Vec<(u64, u32)>,
    },
    DetachBacking {
        resource: u32,
    },
    UnrefResource {
        resource: u32,
    },
    AttachResource {
        context: u32,
        resource: u32,
    },
    DetachResource {
        context: u32,
        resource: u32,
    },
    DetachResourceEverywhere {
        resource: u32,
    },
    Transfer3d {
        context: u32,
        resource: u32,
        transfer: RenderTransfer3D,
        to_host: bool,
    },
    Submit3d {
        context: u32,
        ring_idx: u32,
        commands: Vec<u8>,
        resources: Vec<u32>,
    },
    UpdateCursor(DrmCursorUpdate),
    MoveCursor {
        x: i32,
        y: i32,
    },
}

/// Terminal result for one [`GpuSubmission`].
#[derive(Debug)]
pub struct GpuCompletion {
    pub fence_id: u64,
    pub result: DevResult,
    pub data: GpuCompletionData,
}

/// Bounded response data transferred only with a terminal completion.  The
/// DMA owner remains in the transport until this record is drained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GpuCompletionData {
    None,
    MapInfo(BlobMapInfo),
    Uuid([u8; 16]),
    CapsetInfo {
        id: u32,
        max_version: u32,
        max_size: u32,
    },
    Capset(Vec<u8>),
}

/// MAP_BLOB returns cache attributes, not an address.  `aperture_offset` is
/// selected by the guest and identifies the local SHM aperture reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobMapInfo {
    pub aperture_offset: u64,
    pub aperture_base: u64,
    pub physical_base: u64,
    pub cache_policy: u32,
}

/// Validated, exact-4KiB physical pages returned by MAP_BLOB.  `pages` is a
/// physical-page vector, never a host virtual pointer or a protocol token.
/// The DRM layer maps it as DEVICE|UNCACHED and retains `lease` in each VMA;
/// the owner keeps the mapped resource/aperture live until terminal unmap.
pub struct BlobMapping {
    pub pages: Vec<u64>,
    pub lease: Arc<dyn Any + Send + Sync>,
}

/// Wire-level, type-erased virgl transport made available to the kernel DRM
/// layer. Implementations retain all request/response DMA owners until a
/// completion is drained or reset reports a terminal error.
pub trait GpuTransport {
    /// Negotiated modern virtio-gpu facilities.  Callers must gate every
    /// modern UAPI advertisement on these bits; a protocol implementation is
    /// not itself evidence that the host provided its required memory domain.
    fn modern_features(&self) -> GpuFeatures {
        GpuFeatures::empty()
    }
    fn host_visible_len(&self) -> Option<u64> {
        None
    }
    /// Submit one owned batch without waiting for host execution. The
    /// returned fence is terminal only after `drain_completions(queue, ..)`
    /// returns its matching completion. Implementations retain every
    /// request/response DMA owner until then.
    fn submit(
        &mut self,
        queue: GpuQueue,
        batch: GpuBatch,
        fence_id: u64,
    ) -> DevResult<GpuSubmission>;
    /// Drain a bounded number of terminal records from exactly one queue.
    fn drain_completions(&mut self, queue: GpuQueue, out: &mut [GpuCompletion])
        -> DevResult<usize>;
    /// Reset one queue. Every returned completion is terminal with an error;
    /// reset is never successful execution.
    fn reset(&mut self, queue: GpuQueue, out: &mut [GpuCompletion]) -> usize;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuFeatures(u32);
impl GpuFeatures {
    pub const RESOURCE_UUID: Self = Self(1 << 0);
    pub const RESOURCE_BLOB: Self = Self(1 << 1);
    pub const CONTEXT_INIT: Self = Self(1 << 2);
    /// The driver has a hostmem aperture that it can map into the guest.
    pub const HOST_VISIBLE: Self = Self(1 << 3);
    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn contains(self, feature: Self) -> bool {
        self.0 & feature.0 == feature.0
    }
    pub const fn union(self, feature: Self) -> Self {
        Self(self.0 | feature.0)
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderCapsetInfo {
    pub id: u32,
    pub max_version: u32,
    pub max_size: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderResource3D {
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderTransfer3D {
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

impl<'a> FrameBuffer<'a> {
    pub unsafe fn from_raw_parts_mut(ptr: *mut u8, len: usize) -> Self {
        Self {
            _raw: core::slice::from_raw_parts_mut(ptr, len),
        }
    }

    pub fn from_slice(slice: &'a mut [u8]) -> Self {
        Self { _raw: slice }
    }
}

/// Operations required by display drivers.
///
/// The DRM methods transfer sole scanout ownership from the legacy framebuffer
/// user to a driver that supports caller-owned pinned backing. Drivers that do
/// not implement this transport remain usable through the framebuffer API.
pub trait DisplayDriverOps: BaseDriverOps {
    fn info(&self) -> DisplayInfo;
    fn fb(&self) -> FrameBuffer<'_>;
    fn need_flush(&self) -> bool;
    fn flush(&mut self) -> DevResult;

    fn supports_drm_transport(&self) -> bool {
        false
    }

    /// A non-virgl display returns `None`; its 2D fallback remains usable.
    fn render_transport(&mut self) -> Option<&mut dyn GpuTransport> {
        None
    }

    /// Unified asynchronous DRM submission path.  The driver owns every DMA
    /// request and response until the corresponding completion is drained.
    fn drm_submit(
        &mut self,
        _queue: GpuQueue,
        _batch: GpuBatch,
        _fence_id: u64,
    ) -> DevResult<GpuSubmission> {
        Err(DevError::Unsupported)
    }
    fn drm_drain_completions(
        &mut self,
        _queue: GpuQueue,
        _out: &mut [GpuCompletion],
    ) -> DevResult<usize> {
        Ok(0)
    }
    fn drm_reset(&mut self, _queue: GpuQueue, _out: &mut [GpuCompletion]) -> usize {
        0
    }
    /// Acknowledge and consume one display-config notification.  The returned
    /// mode is sampled after acknowledgement, never reconstructed from stale
    /// framebuffer state.
    fn drm_display_config_changed(&mut self) -> DevResult<Option<DrmDisplayConfig>> {
        Ok(None)
    }
}
