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
pub(crate) mod intel;
mod ioctl;
mod kms;
pub mod modes;
mod property;
mod render;
mod syncobj;
pub(crate) mod uapi;
mod virtio;

/// The kernel's single decision about which surface drives the console and
/// `/dev/fb0`, and the registry a display driver enters it through.
pub(crate) mod screen;

pub use device::{
    AdapterMetrics, DisplayAdapter, DrmDevice, DrmError, DrmMetrics, DrmResult, Scanout,
    primary_device, register_primary_device,
};
pub(crate) use fbdev::drm_scanout;
pub use fbdev::DrmFbdev;
pub(crate) use fence::metrics as fence_metrics;
pub use file::{DrmEvent, DrmFile, OpenId};
pub use gem::{DumbBuffer, DumbRequest, GemBacking, GemHandle, MmapOffset};
pub use kms::{ConnectorInfo, CrtcInfo, FramebufferId, KmsResources, Mode, PageFlip};
pub use render::RenderAdapter;

pub fn init_virtio_gpu() -> DrmResult<bool> {
    // An Intel GPU is not a VirtIO device, but this is the DRM initialization
    // hook the kernel entry point already calls, so the Intel probe runs here:
    // before devfs is mounted, and therefore before any debug file could be
    // read, so that its report reaches the console on a machine that has no
    // other way to show one.
    //
    // The bring-up order's later steps run immediately after, in the same place
    // and for the same reason.  They are separate calls because they are
    // separate claims: the probe can succeed on a device whose power never
    // comes up, and saying which of the two happened is the difference between
    // a log a person can act on and one that only says "no display".
    intel::probe_at_boot();
    intel::bring_up_at_boot();
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
