//! Device-independent DRM state for the primary node.
//!
//! This layer deliberately has no VFS, devfs, ioctl decoding, or transport
//! dependency.  A character-device adapter creates a [`DrmFile`] for each OFD
//! and translates UAPI requests into these typed operations.

mod atomic;
mod atomic_uapi_full;
mod color_mgmt_full;
mod connector_uapi_full;
mod crtc_uapi_full;
mod device;
mod dmabuf;
mod fbdev;
pub(crate) mod fence;
mod framebuffer_uapi_full;
mod file;
mod gem;
pub(crate) mod intel;
mod ioctl;
mod kms;
mod mode_config_full;
pub(crate) mod linear;
pub mod modes;
mod property;
mod plane_uapi_full;
mod property_uapi_full;
mod render;
mod syncobj;
pub(crate) mod uapi;
pub(crate) mod upstream;

use alloc::{boxed::Box, sync::Arc};

use axdriver_display::{BlobMapping, DevError};
use tk_drm_upstream::drm_gem_types::DrmGemObject;

/// The kernel's single decision about which surface drives the console and
/// `/dev/fb0`, and the registry a display driver enters it through.
pub(crate) mod screen;

pub use device::{
    AdapterMetrics, DisplayAdapter, DrmDevice, DrmError, DrmMetrics, DrmResult, Scanout,
    primary_device, register_primary_device,
};
pub use fbdev::DrmFbdev;
pub(crate) use fbdev::drm_scanout;
pub(crate) use fence::metrics as fence_metrics;
pub use file::{DrmEvent, DrmFile, OpenId};
pub use gem::{DumbBuffer, DumbRequest, GemBacking, GemHandle, MmapOffset};
pub(crate) use gem::GemMemoryCharge;
pub use kms::{ConnectorInfo, CrtcInfo, FramebufferId, KmsResources, Mode, PageFlip};
pub use render::RenderAdapter;

pub fn init_virtio_gpu() -> DrmResult<bool> {
    // An Intel GPU is not a VirtIO device, but this is the DRM initialization
    // hook the kernel entry point already calls, so the Intel probe runs here:
    // before devfs is mounted, and therefore before any debug file could be
    // read, so that its report reaches the console on a machine that has no
    // other way to show one.
    //
    // The GT bring-up runs immediately after the probe, in the same place and
    // for the same reason. There is no KMS takeover on Intel: the screen stays
    // on the firmware (GOP) framebuffer registered below.
    intel::probe_at_boot();
    intel::bring_up_gt_at_boot();
    if primary_device().is_some() {
        return Ok(true);
    }
    if let Some(display) = axdisplay::take_drm_display() {
        match tk_virtgpu::VirtioGpuDevice::install(Box::new(display)) {
            Ok(driver) => {
                let raw = driver.drm_device();
                let mode = driver.dimensions();
                let pci = driver.pci_identity();
                match Arc::try_new(VirtioGpuOwner { _driver: driver }) {
                    Ok(owner) => {
                        let mapping_provider: Arc<dyn upstream::UpstreamGemMmapProvider> =
                            owner.clone();
                        let owner_any: Arc<dyn core::any::Any + Send + Sync> = owner;
                        match upstream::install_primary(
                            raw,
                            owner_any,
                            mode,
                            pci,
                            Some(mapping_provider),
                        ) {
                            Ok(_) => return Ok(true),
                            Err(error) => axlog::warn!(
                                "drm: upstream VirtIO DRM publication failed: {error}; firmware \
                                 console retained"
                            ),
                        }
                    }
                    Err(owner) => {
                        drop(owner);
                        axlog::warn!(
                            "drm: no memory to publish upstream VirtIO DRM; firmware console \
                             retained"
                        );
                    }
                }
            }
            Err(error) => {
                axlog::warn!(
                    "drm: upstream VirtIO DRM probe failed ({:?}); using firmware console",
                    error.error
                );
            }
        }
    }
    match linear::init_firmware() {
        Ok(ready) => Ok(ready),
        Err(error) => {
            axlog::warn!(
                "drm: firmware KMS registration failed: {error}; direct firmware console retained"
            );
            Ok(false)
        }
    }
}

/// Own the transport-facing driver while the VFS and shared mappings keep
/// using its upstream DRM device. The driver serializes its mutable protocol
/// state internally; this wrapper is shared only as a lifetime anchor plus
/// the validated blob-mmap operation.
struct VirtioGpuOwner {
    _driver: tk_virtgpu::VirtioGpuDevice,
}

// SAFETY: mutable transport work in VirtioGpuDevice is serialized by its
// private engine lock; the raw DRM pointer is immutable after registration.
unsafe impl Send for VirtioGpuOwner {}
// SAFETY: see the Send implementation; blob mapping uses the same engine lock.
unsafe impl Sync for VirtioGpuOwner {}

impl upstream::UpstreamGemMmapProvider for VirtioGpuOwner {
    unsafe fn map_blob_gem_object(
        &self,
        object: *mut DrmGemObject,
    ) -> Result<Option<BlobMapping>, DevError> {
        // SAFETY: upstream_gem_mmap resolves and pins the object from the same
        // DRM device before asking this provider to map it.
        unsafe { self._driver.map_blob_gem_object_if_mappable(object) }
    }
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
