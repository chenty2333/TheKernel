//! Shared ownership for the Linux DRM devices registered with the upstream core.
//!
//! Each driver owner installs its registered `drm_device` here once. `/dev/dri`
//! publishes one card (and render) node per device from its DRM minors, and
//! `/dev/fb0` uses the first device whose fbdev client registered an fb_info,
//! so a raw C device pointer never outlives its driver/transport owner.

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
    label: &'static str,
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

    /// Whether an fbdev client of this device registered an fb_info: the
    /// generic helper installed here, or the driver's own drm_client_setup().
    pub(crate) fn has_fbdev(&self) -> bool {
        // SAFETY: clientlist is initialized by drm_dev_init(); clients are
        // only added during probe, before the device was published.
        let has_client = unsafe {
            let list = core::ptr::addr_of!((*self.raw()).clientlist);
            !core::ptr::eq((*list).next, list)
        };
        has_client
            || self
                .fb_helper
                .as_ref()
                // SAFETY: the helper is pinned in its Box for the device lifetime.
                .is_some_and(|helper| unsafe { !(**helper).info.is_null() })
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
    // A driver that ran drm_client_setup() itself (efidrm) already owns its
    // fbdev client; only the others get the generic shmem client here.
    // SAFETY: clientlist is initialized by drm_dev_init() and only appended
    // to by drm_client_register() during this single-threaded probe.
    let has_client = unsafe {
        let list = core::ptr::addr_of!((*raw.as_ptr()).clientlist);
        !core::ptr::eq((*list).next, list)
    };
    let fb_helper = if has_client {
        None
    } else {
        initialize_fbdev(raw.as_ptr())
    };
    let device = Arc::try_new(UpstreamDrmDevice {
        raw,
        owner,
        mode,
        pci,
        label,
        fb_helper,
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
    if let Some(helper) = device.fb_helper.as_ref() {
        let helper = (&**helper as *const DrmFbHelper).cast_mut();
        // SAFETY: `helper` points to the pinned `DrmFbHelper` instance initialized in `initialize_fbdev`.
        // Reading `info` checks whether an associated `fb_info` was registered.
        if unsafe { !(*helper).info.is_null() } {
            // SAFETY: `helper` is valid and `(*helper).info` is non-null. `drm_fb_helper_unregister_info`
            // tears down the registered framebuffer device before dropping the helper.
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
    // SAFETY: An all-zero byte pattern is valid initialization for `DrmFbHelper` before
    // `drm_fb_helper_prepare` sets up its fields and function table.
    let mut helper = match Box::try_new(unsafe { core::mem::zeroed::<DrmFbHelper>() }) {
        Ok(helper) => helper,
        Err(_) => {
            warn!("drm: no memory for upstream fbdev helper");
            return None;
        }
    };
    let helper_ptr = (&mut *helper) as *mut DrmFbHelper;
    // SAFETY: `dev` is a valid, live `DrmDevice` pointer passed from `install`.
    // Dereferencing `dev` to read the immutable `driver` pointer is safe and race-free.
    let driver = unsafe { (*dev).driver };
    if driver.is_null() {
        return None;
    }
    // SAFETY: `dev` and `driver` are valid pointers to the live DRM device and its driver descriptor.
    // `helper_ptr` points to the exclusively owned, allocated `DrmFbHelper`. We invoke upstream
    // fb helper lifecycle APIs (`prepare`, `init`, `initial_config`, and error unwind paths) sequentially
    // while initializing the fbdev emulation.
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
