//! Shared ownership for the Linux DRM devices registered with the upstream core.
//!
//! Each driver owner installs its registered `drm_device` here once. `/dev/dri`
//! publishes one card (and render) node per device from its DRM minors, and
//! `/dev/fb0` uses the first device that has an fbdev client (devfs publishes
//! it only if that client registered an fb_info), so a raw C device pointer
//! never outlives its driver/transport owner.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{any::Any, ptr::NonNull};

use axdriver_display::{BlobMapping, DevError, DisplayPciIdentity};
use axerrno::{AxError, AxResult};
use axsync::Mutex;
use lazy_static::lazy_static;
use tk_drm_upstream::{drm_device_types::DrmDevice, drm_gem_types::DrmGemObject};

use crate::mm::{ExternalPageLease, SharedPages};

pub(crate) struct UpstreamDrmDevice {
    raw: NonNull<DrmDevice>,
    owner: Arc<dyn Any + Send + Sync>,
    mode: (u32, u32),
    pci: Option<DisplayPciIdentity>,
    label: &'static str,
    gem_mmap: Option<Arc<dyn UpstreamGemMmapProvider>>,
    unplugged: core::sync::atomic::AtomicBool,
    external_pages: Mutex<Vec<(Weak<ExternalPageLease>, Weak<SharedPages>)>>,
}

/// Transport-owned hook for GEM objects whose backing is a device aperture
/// rather than ordinary shmem RAM.
pub(crate) trait UpstreamGemMmapProvider: Send + Sync {
    /// Returns `Ok(None)` only for objects that are not host-visible blobs.
    ///
    /// # Safety
    /// `object` must be a live GEM object owned by this provider's DRM device.
    unsafe fn map_blob_gem_object(
        &self,
        object: *mut DrmGemObject,
    ) -> Result<Option<BlobMapping>, DevError>;
}

// SAFETY: the owner retains the immutable registered DRM core and its
// transport; mutations are performed by the upstream DRM synchronization.
unsafe impl Send for UpstreamDrmDevice {}
// SAFETY: Upstream DRM device state is synchronized internally via Linux mutexes,
// spinlocks, and atomic refcounts; concurrent references across threads are safe.
unsafe impl Sync for UpstreamDrmDevice {}

impl UpstreamDrmDevice {
    pub(crate) fn raw(&self) -> *mut DrmDevice {
        self.raw.as_ptr()
    }

    pub(crate) fn mode(&self) -> (u32, u32) {
        self.mode
    }

    pub(crate) fn pci_identity(&self) -> Option<DisplayPciIdentity> {
        self.pci
    }

    /// Short driver/transport label used for sysfs identities and platform
    /// parents (for example `virtio0`, `efi-framebuffer.0`, `i915`).
    pub(crate) fn label(&self) -> &'static str {
        self.label
    }

    /// `drm_minor.index` of the primary node.
    pub(crate) fn primary_index(&self) -> Option<u32> {
        // SAFETY: minors are assigned by drm_dev_register() before install and
        // stay immutable while the device is published.
        unsafe {
            let minor = (*self.raw()).primary;
            (!minor.is_null()).then(|| (*minor).index as u32)
        }
    }

    /// `drm_minor.index` of the render node, if the driver has one.
    pub(crate) fn render_index(&self) -> Option<u32> {
        // SAFETY: see `primary_index`.
        unsafe {
            let minor = (*self.raw()).render;
            (!minor.is_null()).then(|| (*minor).index as u32)
        }
    }

    /// Whether this device has an fbdev client. Every fbdev client is
    /// created by drm_client_setup() in its driver's probe, as Linux does.
    /// The client may still have failed to register an fb_info, which is
    /// what devfs checks before publishing `/dev/fb0`.
    pub(crate) fn has_fbdev(&self) -> bool {
        // SAFETY: clientlist is initialized by drm_dev_init(); clients are
        // only added during probe, before the device was published.
        unsafe {
            let list = core::ptr::addr_of!((*self.raw()).clientlist);
            !core::ptr::eq((*list).next, list)
        }
    }

    pub(crate) fn mapping_owner(&self) -> Arc<dyn Any + Send + Sync> {
        self.owner.clone()
    }

    pub(crate) fn is_unplugged(&self) -> bool {
        self.unplugged.load(core::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn map_blob_gem_object(
        &self,
        object: *mut DrmGemObject,
    ) -> Result<Option<BlobMapping>, DevError> {
        if self.unplugged.load(core::sync::atomic::Ordering::Acquire) {
            return Err(DevError::BadState);
        }
        let Some(provider) = &self.gem_mmap else {
            return Ok(None);
        };
        // SAFETY: callers resolve and pin this GEM object from this device's
        // live VMA-offset manager before invoking the provider.
        unsafe { provider.map_blob_gem_object(object) }
    }

    pub(crate) fn track_external_pages(
        &self,
        lease: &Arc<ExternalPageLease>,
        pages: &Arc<SharedPages>,
    ) -> AxResult {
        let mut mappings = self.external_pages.lock();
        if self.unplugged.load(core::sync::atomic::Ordering::Acquire) {
            return Err(AxError::NoSuchDevice);
        }
        mappings.retain(|(lease, pages)| lease.strong_count() != 0 && pages.strong_count() != 0);
        mappings.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        mappings.push((Arc::downgrade(lease), Arc::downgrade(pages)));
        Ok(())
    }

    fn revoke_external_pages(&self) {
        self.unplugged
            .store(true, core::sync::atomic::Ordering::Release);
        let mappings = core::mem::take(&mut *self.external_pages.lock());
        for (lease, pages) in mappings {
            if let (Some(lease), Some(pages)) = (lease.upgrade(), pages.upgrade()) {
                lease.mark_dead();
                crate::mm::revoke_external_shared_pages(&pages);
            }
        }
    }
}

lazy_static! {
    static ref DEVICES: Mutex<Vec<Arc<UpstreamDrmDevice>>> = Mutex::new(Vec::new());
}

/// Publish an upstream DRM device only after its core/minor registration has
/// succeeded. The caller's owner must remain valid through every open file
/// and VMA that uses the raw Linux core structures. `mode` is the scanout
/// size reported to sysfs, `(0, 0)` for a render-only device.
pub(crate) fn install(
    raw: *mut DrmDevice,
    owner: Arc<dyn Any + Send + Sync>,
    mode: (u32, u32),
    pci: Option<DisplayPciIdentity>,
    label: &'static str,
    gem_mmap: Option<Arc<dyn UpstreamGemMmapProvider>>,
) -> AxResult<Arc<UpstreamDrmDevice>> {
    let raw = NonNull::new(raw).ok_or(AxError::InvalidInput)?;
    // SAFETY: `raw` is a validated NonNull pointer to an initialized `DrmDevice`.
    // Inspecting `registered` and `primary` verifies that device registration completed.
    if unsafe { !raw.as_ref().registered || raw.as_ref().primary.is_null() } {
        return Err(AxError::InvalidInput);
    }
    let mut devices = DEVICES.lock();
    if devices.iter().any(|device| device.raw() == raw.as_ptr()) {
        return Err(AxError::AlreadyExists);
    }
    devices.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    let device = Arc::try_new(UpstreamDrmDevice {
        raw,
        owner,
        mode,
        pci,
        label,
        gem_mmap,
        unplugged: core::sync::atomic::AtomicBool::new(false),
        external_pages: Mutex::new(Vec::new()),
    })
    .map_err(|_| AxError::NoMemory)?;
    devices.push(device.clone());
    devices.sort_by_key(|device| device.primary_index());
    Ok(device)
}

/// Every published device, ordered by primary minor index.
pub(crate) fn devices() -> Vec<Arc<UpstreamDrmDevice>> {
    DEVICES.lock().clone()
}

/// The device behind `/dev/dri/card0`.
pub(crate) fn primary() -> Option<Arc<UpstreamDrmDevice>> {
    DEVICES.lock().first().cloned()
}

/// The device whose fbdev emulation backs `/dev/fb0` and the console.
pub(crate) fn fbdev_device() -> Option<Arc<UpstreamDrmDevice>> {
    DEVICES.lock().iter().find(|device| device.has_fbdev()).cloned()
}

/// Remove publication after transport unplug. Existing file/VMA owners still
/// pin the device until their final upstream close callbacks finish.
pub(crate) fn unplug(expected: *mut DrmDevice) -> AxResult<()> {
    let device = {
        let mut devices = DEVICES.lock();
        let index = devices
            .iter()
            .position(|device| device.raw() == expected)
            .ok_or(AxError::NoSuchDevice)?;
        devices.remove(index)
    };
    device
        .unplugged
        .store(true, core::sync::atomic::Ordering::Release);
    // SAFETY: `expected` was verified to match `device.raw()`, pointing to the registered
    // `DrmDevice`. `drm_dev_unplug` unregisters minor nodes and marks the device unplugged
    // while keeping existing open file references intact.
    unsafe { tk_drm_upstream::drm_drv_upstream::drm_dev_unplug(expected) };
    device.revoke_external_pages();
    // SAFETY: `expected` is the registered device being unplugged; this is the
    // Linux drm_dev_unregister() client teardown, which unregisters each fbdev
    // client and its fb_info.
    unsafe { tk_drm_upstream::drm_client_event_upstream::drm_client_dev_unregister(expected) };
    Ok(())
}

