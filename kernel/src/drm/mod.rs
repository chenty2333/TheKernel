//! Device-independent DRM state for the primary node.
//!
//! This layer deliberately has no VFS, devfs, ioctl decoding, or transport
//! dependency.  A character-device adapter creates a [`DrmFile`] for each OFD
//! and translates UAPI requests into these typed operations.

mod atomic;
mod device;
mod dmabuf;
mod fence;
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
    DisplayAdapter, DrmDevice, DrmError, DrmResult, Scanout, primary_device,
    register_primary_device,
};
pub use file::{DrmEvent, DrmFile, OpenId};
pub use gem::{DumbBuffer, DumbRequest, GemBacking, GemHandle, MmapOffset};
pub use kms::{ConnectorInfo, CrtcInfo, FramebufferId, KmsResources, Mode, PageFlip};
pub use render::RenderAdapter;

pub fn init_virtio_gpu() -> DrmResult<bool> {
    virtio::init()
}
