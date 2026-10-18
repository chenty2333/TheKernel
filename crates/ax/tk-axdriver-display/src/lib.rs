//! Common traits and types for graphics display device drivers.

#![no_std]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};
use core::any::Any;

#[doc(no_inline)]
pub use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
pub use axgpu::PixelLayout;
use spin::Mutex;

/// PCI identity retained from the owning function's configuration space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayPciIdentity {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor: u16,
    pub subsystem_device: u16,
    pub revision: u8,
}

/// What a display driver's scanout is, as every consumer of it has to see it.
///
/// Geometry alone is not a surface.  Two consumers of this type previously
/// fabricated the missing half — the console assumed four bytes per pixel and
/// the fbdev ABI reported a fixed 32-bit format — and both were wrong for a
/// surface whose firmware mode was 16 or 24 bits deep, which paints a garbled
/// screen rather than failing.  Pitch and [`PixelLayout`] are stated here for
/// the same reason a driver states its width: so that nothing downstream has to
/// guess.
#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Virtual address of the first byte of the driver's linear framebuffer, or
    /// zero when it publishes none.
    pub fb_base_vaddr: usize,
    /// Bytes of linear framebuffer published at [`Self::fb_base_vaddr`].
    ///
    /// Zero when the driver has no linear framebuffer to publish: a DRM-owned
    /// scanout hands its pixels to userspace as GEM objects instead, and
    /// reporting an address for it would be a fabrication a consumer could
    /// write into.
    pub fb_size: usize,
    /// Bytes between the starts of two consecutive scan lines of that
    /// framebuffer; zero when there is none.
    ///
    /// This is not necessarily the visible width times the pixel size:
    /// backends pad scan lines, and a consumer which recomputed the stride
    /// instead of reading it would shear the image.
    pub pitch: u32,
    /// How the driver's scanout stores one pixel.
    ///
    /// Valid whether or not a linear framebuffer is published: a driver whose
    /// pixels leave through GEM still has one scanout format, and a caller
    /// asking for a compatible buffer needs it.
    pub layout: PixelLayout,
}

impl DisplayInfo {
    /// A display whose pixels are not published through a linear framebuffer.
    ///
    /// The base, size and pitch of a framebuffer that does not exist are all
    /// zero, so no consumer can mistake this for a surface it may write into.
    /// `layout` is still the format of the driver's own scanout.
    pub const fn without_framebuffer(width: u32, height: u32, layout: PixelLayout) -> Self {
        Self {
            width,
            height,
            fb_base_vaddr: 0,
            fb_size: 0,
            pitch: 0,
            layout,
        }
    }
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

/// Shared first-fit allocator for the device's host-visible blob aperture.
/// Both DRM-facing adapters use this same implementation so reservations
/// cannot diverge in alignment, overlap, or boundary behavior.
#[derive(Clone)]
pub struct HostVisibleAperture {
    inner: Arc<Mutex<HostVisibleApertureState>>,
}

struct HostVisibleApertureState {
    length: Option<u64>,
    ranges: Vec<ApertureRange>,
}

#[derive(Clone, Copy)]
struct ApertureRange {
    resource: u32,
    offset: u64,
    length: u64,
}

/// Reservation returned by [`HostVisibleAperture::reserve`]. The aperture
/// range is not reusable until the owner has completed host-side UNMAP and
/// dropped its final lease.
pub struct HostVisibleApertureLease {
    inner: Arc<Mutex<HostVisibleApertureState>>,
    resource: u32,
    offset: u64,
    length: u64,
}

impl HostVisibleAperture {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HostVisibleApertureState {
                length: None,
                ranges: Vec::new(),
            })),
        }
    }

    /// Reserve a page-aligned, first-fit range within the negotiated host
    /// aperture. A resource may have at most one active map at a time.
    pub fn reserve(
        &self,
        resource: u32,
        size: u64,
        aperture_length: Option<u64>,
    ) -> axdriver_base::DevResult<HostVisibleApertureLease> {
        const PAGE: u64 = 4096;
        if resource == 0 || size == 0 {
            return Err(axdriver_base::DevError::InvalidParam);
        }
        let length = size
            .checked_add(PAGE - 1)
            .map(|value| value & !(PAGE - 1))
            .ok_or(axdriver_base::DevError::InvalidParam)?;
        let mut state = self.inner.lock();
        match (state.length, aperture_length) {
            (Some(old), Some(new)) if old != new => return Err(axdriver_base::DevError::BadState),
            (Some(_), _) => {}
            (None, Some(length)) if length != 0 => state.length = Some(length),
            (None, _) => return Err(axdriver_base::DevError::Unsupported),
        }
        let aperture_length = state.length.ok_or(axdriver_base::DevError::Unsupported)?;
        if state.ranges.iter().any(|range| range.resource == resource) {
            return Err(axdriver_base::DevError::ResourceBusy);
        }
        let mut offset = 0u64;
        let mut insert_at = state.ranges.len();
        for (index, range) in state.ranges.iter().enumerate() {
            if offset
                .checked_add(length)
                .is_some_and(|end| end <= range.offset)
            {
                insert_at = index;
                break;
            }
            offset = range
                .offset
                .checked_add(range.length)
                .ok_or(axdriver_base::DevError::BadState)?;
        }
        if offset
            .checked_add(length)
            .is_none_or(|end| end > aperture_length)
        {
            return Err(axdriver_base::DevError::ResourceBusy);
        }
        state
            .ranges
            .try_reserve(1)
            .map_err(|_| axdriver_base::DevError::NoMemory)?;
        state.ranges.insert(
            insert_at,
            ApertureRange {
                resource,
                offset,
                length,
            },
        );
        Ok(HostVisibleApertureLease {
            inner: self.inner.clone(),
            resource,
            offset,
            length,
        })
    }
}

impl Default for HostVisibleAperture {
    fn default() -> Self {
        Self::new()
    }
}

impl HostVisibleApertureLease {
    pub fn resource(&self) -> u32 {
        self.resource
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn length(&self) -> u64 {
        self.length
    }
}

impl Drop for HostVisibleApertureLease {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        if let Some(index) = state.ranges.iter().position(|range| {
            range.resource == self.resource
                && range.offset == self.offset
                && range.length == self.length
        }) {
            state.ranges.remove(index);
        }
    }
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

/// Result of resetting one GPU queue. `completions` is only the number of
/// terminal records copied to the caller; `quiesced` is the independent proof
/// that the device can no longer access any DMA backing owned by the queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuResetResult {
    pub completions: usize,
    pub quiesced: bool,
}

/// An owned GPU command batch. Borrowed userspace or GEM memory is copied
/// into this object before it reaches the device driver, so the lower layer
/// can retain request and response DMA buffers until terminal completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GpuBatch {
    /// Upload CPU-written pixels without changing the scanout.
    Transfer2d {
        resource: u32,
        width: u32,
        height: u32,
    },
    /// Upload one bounded rectangle of a classic 2D resource. This preserves
    /// the legacy DRM transfer ioctl's offset and box semantics without
    /// making callers construct VirtIO wire headers.
    Transfer2dRect {
        resource: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        offset: u32,
    },
    Create2d {
        width: u32,
        height: u32,
        format: u32,
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
    /// Detach resource 0 from scanout 0 and leave the output blank.  This is
    /// distinct from destroying a resource: KMS can disable its primary plane
    /// while keeping the virtual CRTC active.
    DisableScanout,
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
        ring_idx: Option<u32>,
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
    /// The guest-selected host-visible aperture byte offset returned by
    /// MAP_BLOB. It is the owner of the reservation, not a physical address.
    pub aperture_offset: u64,
    /// VirtIO GPU MAP_CACHE_* attributes from the terminal host response.
    pub cache_policy: u32,
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
    /// reset is never successful execution. The returned `quiesced` flag is
    /// the separate proof needed before releasing DMA-visible backing; a
    /// terminal fence/error alone is not that proof.
    fn reset(&mut self, queue: GpuQueue, out: &mut [GpuCompletion]) -> GpuResetResult;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuFeatures(u32);
impl GpuFeatures {
    pub const RESOURCE_UUID: Self = Self(1 << 0);
    pub const RESOURCE_BLOB: Self = Self(1 << 1);
    pub const CONTEXT_INIT: Self = Self(1 << 2);
    /// The driver has a hostmem aperture that it can map into the guest.
    pub const HOST_VISIBLE: Self = Self(1 << 3);
    /// The device negotiated the legacy VIRGL 3D feature.  A resource-blob
    /// capable Venus-only device need not provide this command stream ABI.
    pub const VIRGL_3D: Self = Self(1 << 4);
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
    fn pci_identity(&self) -> Option<DisplayPciIdentity> {
        None
    }
    fn set_pci_identity(&mut self, _identity: DisplayPciIdentity) {}

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
    /// Reset one queue. Drivers without an explicit transport reset contract
    /// must leave `quiesced` false so callers retain DMA-visible references.
    fn drm_reset(&mut self, _queue: GpuQueue, _out: &mut [GpuCompletion]) -> GpuResetResult {
        GpuResetResult {
            completions: 0,
            quiesced: false,
        }
    }
    /// Acknowledge and consume one display-config notification.  The returned
    /// mode is sampled after acknowledgement, never reconstructed from stale
    /// framebuffer state.
    fn drm_display_config_changed(&mut self) -> DevResult<Option<DrmDisplayConfig>> {
        Ok(None)
    }
}

#[cfg(test)]
mod host_visible_aperture_tests {
    use super::*;

    #[test]
    fn reservations_align_first_fit_and_release_on_last_lease_drop() {
        let aperture = HostVisibleAperture::new();
        let first = aperture.reserve(1, 1, Some(3 * 4096)).unwrap();
        assert_eq!((first.offset(), first.length()), (0, 4096));
        let second = aperture.reserve(2, 4097, Some(3 * 4096)).unwrap();
        assert_eq!((second.offset(), second.length()), (4096, 8192));
        assert!(aperture.reserve(2, 4096, Some(3 * 4096)).is_err());
        drop(first);
        let reused = aperture.reserve(3, 4096, Some(3 * 4096)).unwrap();
        assert_eq!(reused.offset(), 0);
    }
}
