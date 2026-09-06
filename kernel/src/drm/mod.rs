//! Device-independent DRM state for the primary node.
//!
//! This layer deliberately has no VFS, devfs, ioctl decoding, or transport
//! dependency.  A character-device adapter creates a [`DrmFile`] for each OFD
//! and translates UAPI requests into these typed operations.

mod atomic;
mod device;
mod dmabuf;
mod fbdev;
pub(crate) mod fence;
mod file;
mod gem;
mod ioctl;
mod kms;
mod property;
mod render;
mod syncobj;
pub(crate) mod uapi;
mod virtio;

pub use device::{
    AdapterMetrics, DisplayAdapter, DrmDevice, DrmError, DrmMetrics, DrmResult, Scanout,
    primary_device, register_primary_device,
};
pub use fbdev::DrmFbdev;
pub(crate) use fence::metrics as fence_metrics;
pub use file::{DrmEvent, DrmFile, OpenId};
pub use gem::{DumbBuffer, DumbRequest, GemBacking, GemHandle, MmapOffset};
pub use kms::{ConnectorInfo, CrtcInfo, FramebufferId, KmsResources, Mode, PageFlip};
pub use render::RenderAdapter;

pub fn init_virtio_gpu() -> DrmResult<bool> {
    virtio::init()
}

/// Seat/session hooks used by the VT and logind control path.  They operate
/// on the single supported primary GPU and intentionally do not affect the
/// render node's render-group authorization.
pub(crate) fn suspend_primary_kms_for_seat() {
    if let Some(device) = primary_device() {
        device.suspend_kms_for_seat();
    }
}

pub(crate) fn resume_primary_kms_for_seat() {
    if let Some(device) = primary_device() {
        device.resume_kms_for_seat();
    }
}
