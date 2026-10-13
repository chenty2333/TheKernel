//! Shared ownership for the imported Linux DRM primary-device instance.
//!
//! The transport owner installs the upstream core once during boot. Both
//! `/dev/dri` and `/dev/fb0` obtain this same handle, so the raw C device
//! pointer never outlives its driver/transport owner.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{any::Any, ptr::NonNull};

use axdriver_display::{BlobMapping, DevError, DisplayPciIdentity};
use axerrno::{AxError, AxResult};
use axsync::Mutex;
use lazy_static::lazy_static;
use tk_drm_upstream::{
    drm_device_types::DrmDevice,
    drm_fb_helper_types::{DrmFbHelper, DrmFbHelperFuncs},
    drm_gem_types::DrmGemObject,
};

use crate::mm::{ExternalPageLease, SharedPages};

static FB_HELPER_FUNCS: DrmFbHelperFuncs = DrmFbHelperFuncs {
    fb_dirty: None,
    fb_restore: None,
    fb_set_suspend: None,
};

pub(crate) struct UpstreamDrmDevice {
    raw: NonNull<DrmDevice>,
    owner: Arc<dyn Any + Send + Sync>,
    mode: (u32, u32),
    pci: Option<DisplayPciIdentity>,
    fb_helper: Option<Box<DrmFbHelper>>,
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
    static ref PRIMARY: Mutex<Option<Arc<UpstreamDrmDevice>>> = Mutex::new(None);
}

/// Publish the upstream primary device only after its core/minor registration
/// has succeeded. The caller's owner must remain valid through every open file
/// and VMA that uses the raw Linux core structures.
pub(crate) fn install_primary(
    raw: *mut DrmDevice,
    owner: Arc<dyn Any + Send + Sync>,
    mode: (u32, u32),
    pci: Option<DisplayPciIdentity>,
    gem_mmap: Option<Arc<dyn UpstreamGemMmapProvider>>,
) -> AxResult<Arc<UpstreamDrmDevice>> {
    let raw = NonNull::new(raw).ok_or(AxError::InvalidInput)?;
    if mode.0 == 0
        || mode.1 == 0
        || unsafe { !raw.as_ref().registered || raw.as_ref().primary.is_null() }
    {
        return Err(AxError::InvalidInput);
    }
    let mut current = PRIMARY.lock();
    if current.is_some() {
        return Err(AxError::AlreadyExists);
    }
    let fb_helper = initialize_fbdev(raw.as_ptr());
    let device = Arc::try_new(UpstreamDrmDevice {
        raw,
        owner,
        mode,
        pci,
        fb_helper,
        gem_mmap,
        unplugged: core::sync::atomic::AtomicBool::new(false),
        external_pages: Mutex::new(Vec::new()),
    })
    .map_err(|_| AxError::NoMemory)?;
    *current = Some(device.clone());
    Ok(device)
}

pub(crate) fn primary() -> Option<Arc<UpstreamDrmDevice>> {
    PRIMARY.lock().clone()
}

/// Remove publication after transport unplug. Existing file/VMA owners still
/// pin the device until their final upstream close callbacks finish.
pub(crate) fn unplug_primary(expected: *mut DrmDevice) -> AxResult<()> {
    let device = {
        let mut current = PRIMARY.lock();
        let Some(device) = current.as_ref() else {
            return Err(AxError::NoSuchDevice);
        };
        if device.raw() != expected {
            return Err(AxError::InvalidInput);
        }
        current.take().ok_or(AxError::NoSuchDevice)?
    };
    device
        .unplugged
        .store(true, core::sync::atomic::Ordering::Release);
    unsafe { tk_drm_upstream::drm_drv_upstream::drm_dev_unplug(expected) };
    device.revoke_external_pages();
    if let Some(helper) = device.fb_helper.as_ref() {
        let helper = (&**helper as *const DrmFbHelper).cast_mut();
        if unsafe { !(*helper).info.is_null() } {
            unsafe {
                tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_unregister_info(helper)
            };
        }
    }
    Ok(())
}

/// Initialize the upstream generic shmem fbdev client. Lack of a usable
/// modeset or dumb-buffer path leaves DRM itself available but does not invent
/// a framebuffer node; devfs publishes `/dev/fb0` only after real fb_info
/// registration succeeds.
fn initialize_fbdev(dev: *mut DrmDevice) -> Option<Box<DrmFbHelper>> {
    let mut helper = match Box::try_new(unsafe { core::mem::zeroed::<DrmFbHelper>() }) {
        Ok(helper) => helper,
        Err(_) => {
            warn!("drm: no memory for upstream fbdev helper");
            return None;
        }
    };
    let helper_ptr = (&mut *helper) as *mut DrmFbHelper;
    let driver = unsafe { (*dev).driver };
    if driver.is_null() {
        return None;
    }
    unsafe {
        if (*driver).fbdev_probe.is_none() {
            (*driver).fbdev_probe =
                Some(tk_drm_upstream::drm_fbdev_shmem_upstream::drm_fbdev_shmem_driver_fbdev_probe);
        }
        tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_prepare(
            dev,
            helper_ptr,
            32,
            (&FB_HELPER_FUNCS as *const DrmFbHelperFuncs).cast_mut(),
        );
        if tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_init(dev, helper_ptr) != 0 {
            tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_unprepare(helper_ptr);
            return None;
        }
        let ret = tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_initial_config(helper_ptr);
        if ret != 0 {
            if !(*helper_ptr).info.is_null() {
                tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_unregister_info(helper_ptr);
            }
            tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_fini(helper_ptr);
            tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_unprepare(helper_ptr);
            warn!("drm: upstream fbdev setup failed with errno {ret}");
            return None;
        }
    }
    Some(helper)
}
