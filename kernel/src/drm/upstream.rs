//! Shared ownership for the imported Linux DRM primary-device instance.
//!
//! The transport owner installs the upstream core once during boot. Both
//! `/dev/dri` and `/dev/fb0` obtain this same handle, so the raw C device
//! pointer never outlives its driver/transport owner.

use alloc::{boxed::Box, sync::Arc};
use core::{any::Any, ptr::NonNull};

use axdriver_display::DisplayPciIdentity;
use axerrno::{AxError, AxResult};
use axsync::Mutex;
use lazy_static::lazy_static;
use tk_drm_upstream::{
    drm_device_types::DrmDevice,
    drm_fb_helper_types::{DrmFbHelper, DrmFbHelperFuncs},
};

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
    let mut current = PRIMARY.lock();
    let Some(device) = current.as_ref() else {
        return Err(AxError::NoSuchDevice);
    };
    if device.raw() != expected {
        return Err(AxError::InvalidInput);
    }
    unsafe { tk_drm_upstream::drm_drv_upstream::drm_dev_unplug(expected) };
    if let Some(helper) = device.fb_helper.as_ref() {
        let helper = (&**helper as *const DrmFbHelper).cast_mut();
        if unsafe { !(*helper).info.is_null() } {
            unsafe {
                tk_drm_upstream::drm_fb_helper_upstream::drm_fb_helper_unregister_info(helper)
            };
        }
    }
    current.take();
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
