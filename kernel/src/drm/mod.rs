//! DRM device bring-up. Every DRM device (VirtIO GPU, efidrm, i915) is a
//! driver on the upstream Linux DRM core (`tk-drm-upstream`), published by
//! [`upstream`] and served to userspace by `pseudofs::dev::upstream_dri`.

mod efidrm;
pub(crate) mod intel;
pub(crate) mod sync_file;
pub(crate) mod upstream;

use alloc::{boxed::Box, sync::Arc};

use axdriver_display::{BlobMapping, DevError};
use tk_drm_upstream::drm_gem_types::DrmGemObject;

/// The kernel's single decision about which surface drives the console and
/// `/dev/fb0`, and the registry a display driver enters it through.
pub(crate) mod screen;

/// Register the boot DRM devices. Returns whether one now drives the screen.
pub fn init_virtio_gpu() -> bool {
    // An Intel GPU is not a VirtIO device, but this is the DRM initialization
    // hook the kernel entry point already calls, so the Intel probe runs here:
    // before devfs is mounted, and therefore before any debug file could be
    // read, so that its report reaches the console on a machine that has no
    // other way to show one.
    //
    // The GT bring-up runs immediately after the probe, in the same place and
    // for the same reason. i915 itself registers later, once the rootfs
    // supplies its firmware; until then (and without native modeset) the
    // screen is driven by efidrm over the firmware (GOP) framebuffer.
    intel::probe_at_boot();
    intel::bring_up_gt_at_boot();
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
                        match upstream::install(
                            raw,
                            owner_any,
                            mode,
                            pci,
                            "virtio0",
                            Some(mapping_provider),
                        ) {
                            Ok(_) => return true,
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
    efidrm::init_firmware()
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
