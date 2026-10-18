// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! DRM device/KMS registration for a VirtIO GPU supplied by the display bus.
//!
//! The bus driver owns VirtIO feature negotiation and DMA.  This module takes
//! an already initialized [`DisplayDriverOps`] implementation, retains it with
//! a Rust lifetime, and registers the Linux DRM core device around it.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ffi::{c_char, c_void},
    mem::{MaybeUninit, offset_of, size_of},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use axdriver_display::{
    BlobMapping, DevError, DevResult, DisplayDriverOps, DisplayPciIdentity, GpuBatch,
    GpuCompletionData, GpuQueue, HostVisibleAperture, HostVisibleApertureLease,
};
use tk_drm_upstream::{
    drm_atomic_helper_upstream::{
        drm_atomic_helper_check, drm_atomic_helper_commit, drm_atomic_helper_commit_error,
        drm_atomic_helper_commit_tail,
    },
    drm_atomic_state_helper_upstream::{
        drm_atomic_helper_connector_destroy_state, drm_atomic_helper_connector_duplicate_state,
        drm_atomic_helper_connector_reset,
    },
    drm_atomic_types::DrmAtomicCommit,
    drm_atomic_upstream::{
        drm_atomic_add_affected_planes, drm_atomic_get_new_crtc_state,
        drm_atomic_get_new_plane_state, drm_atomic_get_old_plane_state,
    },
    drm_connector_types::{
        CONNECTOR_STATUS_CONNECTED, CONNECTOR_STATUS_DISCONNECTED, DrmConnector, DrmConnectorFuncs,
    },
    drm_connector_upstream::{drm_connector_cleanup, drm_connector_init},
    drm_crtc_types::DrmCrtc,
    drm_device_types::DrmDevice,
    drm_drv_types::DrmDriver,
    drm_drv_upstream::{drm_dev_alloc, drm_dev_put, drm_dev_register, drm_dev_unregister},
    drm_gem_framebuffer_helper_upstream::drm_gem_fb_create,
    drm_gem_types::DrmGemObject,
    drm_gem_upstream::{drm_gem_object_get, drm_gem_object_put},
    drm_mode_config_types::DrmModeConfigFuncs,
    drm_mode_config_upstream::drmm_mode_config_init,
    drm_modes_types::{DrmDisplayMode, DrmModeStatus, MODE_OK, MODE_VIRTUAL_X, MODE_VIRTUAL_Y},
    drm_modeset_helper_vtables_types::{
        DrmConnectorHelperFuncs, DrmCrtcHelperFuncs, DrmModeConfigHelperFuncs,
    },
    drm_plane_types::DrmPlaneState,
    drm_simple_kms_helper_types_upstream::{DrmSimpleDisplayPipe, DrmSimpleDisplayPipeFuncs},
    drm_simple_kms_helper_upstream::drm_simple_display_pipe_init,
    drm_vblank_helper_upstream::{
        drm_crtc_vblank_helper_disable_vblank_timer, drm_crtc_vblank_helper_enable_vblank_timer,
    },
    drm_vblank_upstream::drm_vblank_init,
    uapi_drm_consts::{DRM_FORMAT_XRGB8888, DRM_MODE_CONNECTOR_VIRTUAL, DRM_MODE_TYPE_PREFERRED},
};
use tk_linux_kapi::{
    config::PAGE_SIZE,
    mutex::{mutex_init, mutex_lock, mutex_unlock},
    types::Mutex,
};

use crate::{
    gem::{VirtioGpuObject, create_classic_resource},
    ioctl::VIRTGPU_IOCTLS,
    transport::GpuEngine,
};

const DRIVER_GEM: u32 = 1 << 0;
const DRIVER_MODESET: u32 = 1 << 1;
const DRIVER_RENDER: u32 = 1 << 3;
const DRIVER_ATOMIC: u32 = 1 << 4;
const MIN_MODE: u32 = 32;
const MAX_MODE: u32 = 8192;

/// The single-scanout initial V0 DRM device. Dropping this object unregisters
/// and unplugs DRM, then quiesces the owned display transport before releasing
/// it.
pub struct VirtioGpuDevice {
    drm: *mut DrmDevice,
    private: Arc<DriverPrivate>,
}

/// Failed DRM installation. The original display transport is recoverable
/// only when no uncertain host work remains and the unregistered DRM device
/// has been fully torn down. A probe failure returns no transport so callers
/// must reprobe rather than revive a queue with unknown DMA quiescence.
pub struct VirtioGpuInstallError {
    pub error: DevError,
    display: Option<Box<dyn DisplayDriverOps>>,
}

impl VirtioGpuInstallError {
    fn recoverable(error: DevError, display: Box<dyn DisplayDriverOps>) -> Self {
        Self {
            error,
            display: Some(display),
        }
    }

    fn unrecoverable(error: DevError) -> Self {
        Self {
            error,
            display: None,
        }
    }

    /// Return the transport to a fallback driver when it remains usable.
    /// `None` means queue/device work was uncertain and the bus must be reprobed.
    pub fn into_display(self) -> Option<Box<dyn DisplayDriverOps>> {
        self.display
    }
}

impl core::fmt::Debug for VirtioGpuInstallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtioGpuInstallError")
            .field("error", &self.error)
            .field("display_recoverable", &self.display.is_some())
            .finish()
    }
}

/// State addressed by DRM callbacks through `drm->dev_private`.
///
/// `DriverState` owns the display transport behind a stable Arc allocation,
/// not a lifetime-erased transport pointer. The `dev_private` pointer owns an
/// additional Arc strong reference released by the DRM driver's final
/// `release` callback. On `VirtioGpuDevice::drop`, the engine is reset and
/// removed under `engine_lock`; late callbacks then see an unplugged device
/// with no transport.
pub(crate) struct DriverPrivate {
    engine_lock: Mutex,
    state: core::cell::UnsafeCell<Option<DriverState>>,
    connected: AtomicBool,
    /// The commit whose hardware work completed before a blocking state swap.
    /// Its commit tail consumes this token instead of submitting the same frame
    /// a second time. The helper serializes this driver's KMS commits through
    /// `hw_done`, so at most one token can be live.
    staged_commit: AtomicPtr<DrmAtomicCommit>,
    width: u32,
    height: u32,
    pci_identity: Option<DisplayPciIdentity>,
    pub(crate) aperture: HostVisibleAperture,
    pipe: core::cell::UnsafeCell<DrmSimpleDisplayPipe>,
    connector: core::cell::UnsafeCell<DrmConnector>,
    crtc_helper: core::cell::UnsafeCell<DrmCrtcHelperFuncs>,
}

pub(crate) struct DriverState {
    pub(crate) engine: GpuEngine,
    pub(crate) display: Box<dyn DisplayDriverOps>,
    removed: bool,
    pub(crate) pending_refs: Vec<(u64, Vec<usize>)>,
    /// GEM object whose resource is currently owned by host scanout. It is
    /// independent of the KMS current-state ref because async commit errors
    /// retire old plane states without proving the host stopped scanning them.
    scanout_refs: ScanoutRefs,
}

#[derive(Default)]
struct ScanoutRefs {
    current_object: Option<usize>,
    /// Objects from failed presentation commands may have reached host scanout
    /// before a later stage failed. Keep them until the queue reset in quiesce.
    uncertain_objects: Vec<usize>,
}

impl ScanoutRefs {
    fn replace_current(&mut self, object: Option<usize>) -> Option<usize> {
        match object {
            Some(object) => self.current_object.replace(object),
            None => self.current_object.take(),
        }
    }

    fn take_after_reset(&mut self, quiesced: bool) -> (Option<usize>, Vec<usize>) {
        if !quiesced {
            // DriverState will drop its raw bookkeeping when unplug teardown
            // finishes, but deliberately do not balance these GEM refs: their
            // refcounts keep possibly DMA-visible pages alive after reset
            // failure, matching the lower transport's quarantined-owner path.
            return (None, alloc::vec::Vec::new());
        }
        (
            self.current_object.take(),
            core::mem::take(&mut self.uncertain_objects),
        )
    }
}

// All engine access goes through the sleepable Linux mutex. KMS objects are
// initialized before the DRM device is registered and then remain pinned in
// the Arc allocation until the final DRM device release.
// SAFETY: Access to mutable DriverState is synchronized via the Linux mutex `engine_lock`.
// KMS objects and the per-device CRTC helper table are initialized prior to DRM device
// registration and remain pinned at stable addresses inside the Arc allocation for the
// lifetime of the DRM device.
unsafe impl Send for DriverPrivate {}
// SAFETY: DriverState uses `engine_lock`, `connected` uses atomic operations, `aperture` provides
// internal synchronization, and KMS pipe/connector/helper objects are initialized before publication
// and protected by DRM modeset serialization afterwards.
unsafe impl Sync for DriverPrivate {}

struct SyncDriver(DrmDriver);
// SAFETY: DrmDriver contains only function pointers, primitive constants, and pointers
// to immutable static byte slices and ioctl descriptor tables. No fields are mutated
// concurrently after initialization.
unsafe impl Sync for SyncDriver {}

static DRIVER_NAME: &[u8] = b"virtio_gpu\0";
static DRIVER_DESC: &[u8] = b"VirtIO GPU\0";

static DRM_DRIVER: SyncDriver = SyncDriver(DrmDriver {
    load: None,
    open: Some(crate::ioctl::virtgpu_open),
    postclose: Some(crate::ioctl::virtgpu_postclose),
    unload: None,
    release: Some(virtgpu_release),
    master_set: None,
    master_drop: None,
    debugfs_init: None,
    gem_create_object: Some(crate::gem::virtgpu_gem_create_object),
    prime_handle_to_fd: None,
    prime_fd_to_handle: None,
    gem_prime_import: Some(crate::gem::virtgpu_gem_prime_import),
    gem_prime_import_sg_table: Some(crate::gem::virtgpu_gem_prime_import_sg_table),
    dumb_create: Some(crate::gem::virtgpu_dumb_create),
    dumb_map_offset: Some(tk_drm_upstream::drm_gem_upstream::drm_gem_dumb_map_offset),
    // DRM_FBDEV_SHMEM_DRIVER_OPS (virtgpu_drv.c).
    fbdev_probe: Some(
        tk_drm_upstream::drm_fbdev_shmem_upstream::drm_fbdev_shmem_driver_fbdev_probe,
    ),
    show_fdinfo: None,
    major: 0,
    minor: 1,
    patchlevel: 0,
    name: DRIVER_NAME.as_ptr() as *mut c_char,
    desc: DRIVER_DESC.as_ptr() as *mut c_char,
    driver_features: DRIVER_GEM
        | DRIVER_MODESET
        | DRIVER_RENDER
        | DRIVER_ATOMIC
        | tk_drm_upstream::drm_ioctl_upstream::DRIVER_SYNCOBJ
        | tk_drm_upstream::drm_ioctl_upstream::DRIVER_SYNCOBJ_TIMELINE,
    ioctls: VIRTGPU_IOCTLS.as_ptr() as *mut _,
    num_ioctls: VIRTGPU_IOCTLS.len() as i32,
    fops: ptr::null_mut(),
});

static MODE_FUNCS: DrmModeConfigFuncs = DrmModeConfigFuncs {
    fb_create: Some(virtgpu_fb_create),
    get_format_info: None,
    mode_valid: Some(virtgpu_mode_valid),
    atomic_check: Some(drm_atomic_helper_check),
    atomic_commit: Some(drm_atomic_helper_commit),
    atomic_state_alloc: None,
    atomic_state_clear: None,
    atomic_state_free: None,
};

static MODE_HELPER_FUNCS: DrmModeConfigHelperFuncs = DrmModeConfigHelperFuncs {
    atomic_commit_tail: Some(virtgpu_atomic_commit_tail),
    atomic_commit_setup: None,
    atomic_commit_pre_swap: Some(virtgpu_atomic_commit_pre_swap),
};

static CONNECTOR_FUNCS: DrmConnectorFuncs = DrmConnectorFuncs {
    dpms: None,
    reset: Some(drm_atomic_helper_connector_reset),
    detect: Some(virtgpu_connector_detect),
    force: None,
    // virtgpu_display.c: .fill_modes = drm_helper_probe_single_connector_modes
    fill_modes: Some(
        tk_drm_upstream::drm_probe_helper_upstream::drm_helper_probe_single_connector_modes,
    ),
    set_property: None,
    late_register: None,
    early_unregister: None,
    destroy: Some(virtgpu_connector_destroy),
    atomic_duplicate_state: Some(drm_atomic_helper_connector_duplicate_state),
    atomic_destroy_state: Some(drm_atomic_helper_connector_destroy_state),
    atomic_set_property: None,
    atomic_get_property: None,
    atomic_print_state: None,
    oob_hotplug_event: None,
    debugfs_init: None,
};

static CONNECTOR_HELPER_FUNCS: DrmConnectorHelperFuncs = DrmConnectorHelperFuncs {
    get_modes: Some(virtgpu_connector_get_modes),
    detect_ctx: None,
    mode_valid: Some(virtgpu_connector_mode_valid),
    mode_valid_ctx: None,
    best_encoder: Some(virtgpu_best_encoder),
    atomic_best_encoder: None,
    atomic_check: None,
    atomic_commit: None,
    prepare_writeback_job: None,
    cleanup_writeback_job: None,
    enable_hpd: None,
    disable_hpd: None,
};

static PIPE_FUNCS: DrmSimpleDisplayPipeFuncs = DrmSimpleDisplayPipeFuncs {
    mode_valid: Some(virtgpu_pipe_mode_valid),
    enable: Some(virtgpu_pipe_enable),
    disable: Some(virtgpu_pipe_disable),
    check: None,
    update: Some(virtgpu_pipe_update),
    prepare_fb: None,
    cleanup_fb: None,
    begin_fb_access: None,
    end_fb_access: None,
    enable_vblank: Some(virtgpu_pipe_enable_vblank),
    disable_vblank: Some(virtgpu_pipe_disable_vblank),
    reset_crtc: None,
    duplicate_crtc_state: None,
    destroy_crtc_state: None,
    reset_plane: None,
    duplicate_plane_state: None,
    destroy_plane_state: None,
};

fn zeroed<T>() -> T {
    // SAFETY: All in-crate call sites use zero-valid C-layout DRM/kernel structs following Linux
    // kzalloc conventions (integers, pointers, fixed arrays whose all-zero bit pattern represents
    // valid initial state), rather than arbitrary `T`.
    unsafe { MaybeUninit::<T>::zeroed().assume_init() }
}

impl VirtioGpuDevice {
    /// Register one DRM device around an owned display transport.
    ///
    /// DRM callbacks, GEM objects and VMA leases can outlive this handle. The
    /// driver therefore takes ownership of the display transport and keeps it
    /// behind the stable DRM-private `Arc` until the core and all external
    /// mapping leases release their final reference.
    pub fn install(mut display: Box<dyn DisplayDriverOps>) -> Result<Self, VirtioGpuInstallError> {
        if !display.supports_drm_transport() {
            return Err(VirtioGpuInstallError::recoverable(
                DevError::Unsupported,
                display,
            ));
        }
        let info = display.info();
        let pci_identity = display.pci_identity();
        if info.width < MIN_MODE
            || info.height < MIN_MODE
            || info.width > MAX_MODE
            || info.height > MAX_MODE
        {
            return Err(VirtioGpuInstallError::recoverable(
                DevError::InvalidParam,
                display,
            ));
        }

        let mut engine = match GpuEngine::new(&mut *display) {
            Ok(engine) => engine,
            Err(error) => return Err(VirtioGpuInstallError::recoverable(error, display)),
        };
        if let Err(error) = engine.discover_capsets(&mut *display) {
            // A capset query can fail while a request is still device-owned.
            // Reset both queues to prove quiescence, but do not hand this
            // transport back as a live fallback device: lower VirtIO reset is
            // terminal for this transport instance.
            let _ = engine.reset(&mut *display, GpuQueue::Control);
            let _ = engine.reset(&mut *display, GpuQueue::Cursor);
            return Err(VirtioGpuInstallError::unrecoverable(error));
        }
        let private = Arc::new(DriverPrivate {
            engine_lock: zeroed(),
            state: core::cell::UnsafeCell::new(Some(DriverState {
                engine,
                display,
                removed: false,
                pending_refs: Vec::new(),
                scanout_refs: ScanoutRefs::default(),
            })),
            connected: AtomicBool::new(true),
            staged_commit: AtomicPtr::new(ptr::null_mut()),
            width: info.width,
            height: info.height,
            pci_identity,
            aperture: HostVisibleAperture::new(),
            pipe: core::cell::UnsafeCell::new(zeroed()),
            connector: core::cell::UnsafeCell::new(zeroed()),
            crtc_helper: core::cell::UnsafeCell::new(zeroed()),
        });
        // SAFETY: private.engine_lock is a newly allocated, zeroed Linux mutex at a stable
        // address inside the Arc allocation, and no concurrent threads have access yet.
        unsafe { mutex_init(&private.engine_lock) };

        // SAFETY: DRM_DRIVER.0 is a valid static reference to DrmDriver, and a null parent device
        // is valid for virtual/platform DRM devices.
        let drm = unsafe { drm_dev_alloc(&DRM_DRIVER.0, ptr::null_mut()) };
        if drm.is_null() {
            return Err(match private.take_display_after_probe() {
                Some(display) => VirtioGpuInstallError::recoverable(DevError::NoMemory, display),
                None => VirtioGpuInstallError::unrecoverable(DevError::NoMemory),
            });
        }

        // `dev_private` owns this strong reference until the core's final
        // release callback, not just until this Rust wrapper is dropped.
        // SAFETY: drm was successfully allocated and is non-null. dev_private receives an
        // owned Arc strong reference to DriverPrivate, which will be dropped in virtgpu_release.
        unsafe {
            (*drm).dev_private =
                Arc::into_raw(private.clone()) as *mut DriverPrivate as *mut c_void;
        }

        // SAFETY: drm is a valid allocated DrmDevice, and private is a valid heap-allocated
        // DriverPrivate whose memory remains pinned and alive via the private Arc.
        let init = unsafe { init_drm_device(drm, Arc::as_ptr(&private) as *mut DriverPrivate) };
        if init != 0 {
            // SAFETY: drm is a valid DrmDevice allocation. Releasing its initial reference
            // initiates cleanup and calls virtgpu_release to reclaim dev_private.
            unsafe { drm_dev_put(drm) };
            let error = errno_to_dev_error(init);
            return Err(match private.take_display_after_probe() {
                Some(display) => VirtioGpuInstallError::recoverable(error, display),
                None => VirtioGpuInstallError::unrecoverable(error),
            });
        }
        // SAFETY: drm is a valid DrmDevice whose mode configuration and display pipe have
        // been fully initialized and is ready for registration.
        let ret = unsafe { drm_dev_register(drm, 0) };
        if ret != 0 {
            // Registration failed before a DRM node could publish this device.
            // Tear down the core before returning its transport to fallback.
            // SAFETY: drm is a valid DrmDevice allocation; drm_dev_put tears down the device
            // and releases dev_private via virtgpu_release.
            unsafe { drm_dev_put(drm) };
            let error = errno_to_dev_error(ret);
            return Err(match private.take_display_after_probe() {
                Some(display) => VirtioGpuInstallError::recoverable(error, display),
                None => VirtioGpuInstallError::unrecoverable(error),
            });
        }

        // virtgpu_probe(): drm_client_setup(vdev->priv, NULL) after drm_dev_register().
        // SAFETY: drm is registered and its driver's fbdev_probe is set above.
        unsafe {
            tk_drm_upstream::drm_client_setup_upstream::drm_client_setup(drm, core::ptr::null())
        };

        Ok(Self { drm, private })
    }

    /// Borrow the Linux DRM core device for node integration. The raw pointer
    /// is valid only while this `VirtioGpuDevice` owner remains alive.
    pub fn drm_device(&self) -> *mut DrmDevice {
        self.drm
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.private.width, self.private.height)
    }

    pub fn pci_identity(&self) -> Option<DisplayPciIdentity> {
        self.private.pci_identity
    }

    /// Map a negotiated host-visible resource and return an exact-4KiB
    /// physical page vector. The aperture lease remains held until the final
    /// mapping owner drops after terminal host UNMAP (or device reset).
    pub fn map_blob(&self, resource: u32, size: u64) -> DevResult<BlobMapping> {
        self.map_blob_inner(resource, size, None)
    }

    /// Map a host-visible blob directly from its live GEM object. The mapping
    /// lease also owns a GEM reference, so resource unref cannot race VMA use.
    pub unsafe fn map_blob_gem_object(&self, object: *mut DrmGemObject) -> DevResult<BlobMapping> {
        // SAFETY: Caller guarantees object is either null or points to a valid DrmGemObject.
        unsafe { self.map_blob_gem_object_if_mappable(object) }?.ok_or(DevError::InvalidParam)
    }

    /// Map a host-visible blob if this GEM object is an eligible MAP_BLOB
    /// resource. `Ok(None)` means this is an ordinary or guest-only GEM
    /// object; malformed objects and actual MAP_BLOB failures remain errors.
    pub unsafe fn map_blob_gem_object_if_mappable(
        &self,
        object: *mut DrmGemObject,
    ) -> DevResult<Option<BlobMapping>> {
        if object.is_null() {
            return Err(DevError::InvalidParam);
        }
        // SAFETY: object was verified non-null and caller guarantees it points to a valid DrmGemObject.
        if unsafe { (*object).dev } != self.drm {
            return Err(DevError::InvalidParam);
        }
        let gpu = object.cast::<crate::gem::VirtioGpuObject>();
        // SAFETY: object is verified non-null and belongs to self.drm, confirming it embeds
        // a valid VirtioGpuObject at the start of the GEM object allocation.
        let (resource, size, mappable) = unsafe {
            (
                (*gpu).resource_id,
                (*object).size as u64,
                (*gpu).is_blob
                    && (*gpu).blob_mem != crate::uapi::VIRTGPU_BLOB_MEM_GUEST
                    && (*gpu).blob_flags & crate::uapi::VIRTGPU_BLOB_FLAG_USE_MAPPABLE != 0,
            )
        };
        if !mappable {
            return Ok(None);
        }
        if resource == 0 {
            return Err(DevError::InvalidParam);
        }
        // SAFETY: object is a valid DrmGemObject; increment its reference count so it remains
        // alive for the duration of the aperture lease held in BlobMapping.
        unsafe {
            drm_gem_object_get(object);
        }
        self.map_blob_inner(
            resource,
            size,
            Some(GemObjectHold {
                object: object as usize,
            }),
        )
        .map(Some)
    }

    fn map_blob_inner(
        &self,
        resource: u32,
        size: u64,
        object_hold: Option<GemObjectHold>,
    ) -> DevResult<BlobMapping> {
        let host_len = self
            .private
            .with_engine(|engine, _| engine.host_visible_len())
            .flatten()
            .ok_or(DevError::Unsupported)?;
        let bytes = usize::try_from(size)
            .ok()
            .and_then(|size| size.checked_add(PAGE_SIZE - 1))
            .map(|size| size & !(PAGE_SIZE - 1))
            .filter(|size| *size != 0)
            .ok_or(DevError::InvalidParam)?;
        let count = bytes / PAGE_SIZE;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(count)
            .map_err(|_| DevError::NoMemory)?;
        let aperture = self
            .private
            .aperture
            .reserve(resource, size, Some(host_len))?;
        let offset = aperture.offset();
        let mut lease = BlobMapLease {
            private: self.private.clone(),
            resource,
            _aperture: aperture,
            object: object_hold,
            mapped: false,
        };
        let map_result = self
            .private
            .with_state(|state| {
                let fence = state.engine.submit(
                    &mut *state.display,
                    GpuQueue::Control,
                    GpuBatch::MapBlob { resource, offset },
                )?;
                // Once accepted, treat the host mapping as live even if its
                // completion is lost; the lease Drop path will issue UNMAP after
                // any wait-triggered queue reset.
                lease.mapped = true;
                let completed = match state.engine.wait(&mut *state.display, fence, 5_000_000_000) {
                    Ok(completed) => completed,
                    Err(error) => {
                        let _ = state.engine.reap(fence);
                        return Err(error);
                    }
                };
                let data = match state.engine.take_completion_data(fence) {
                    Ok(data) => data,
                    Err(error) => {
                        let _ = state.engine.reap(fence);
                        return Err(error);
                    }
                };
                let _ = state.engine.reap(fence);
                let GpuCompletionData::MapInfo(info) = data else {
                    return Err(DevError::BadState);
                };
                if completed.status != crate::transport::FenceStatus::Complete {
                    return Err(DevError::Io);
                }
                Ok(info)
            })
            .ok_or(DevError::Unsupported)?;
        let info = match map_result {
            Ok(info) => info,
            Err(error) => return Err(error),
        };
        if info.aperture_offset != offset || info.physical_base as usize & (PAGE_SIZE - 1) != 0 {
            return Err(DevError::BadState);
        }
        let base = usize::try_from(info.physical_base).map_err(|_| DevError::InvalidParam)?;
        for index in 0..count {
            let address = base
                .checked_add(index.checked_mul(PAGE_SIZE).ok_or(DevError::InvalidParam)?)
                .ok_or(DevError::InvalidParam)?;
            pages.push(address as u64);
        }
        let lease: Arc<dyn core::any::Any + Send + Sync> = Arc::new(lease);
        Ok(BlobMapping {
            pages,
            aperture_offset: info.aperture_offset,
            cache_policy: info.cache_policy,
            lease,
        })
    }

    fn unplug(&mut self) {
        if self.drm.is_null() {
            return;
        }
        // SAFETY: self.drm is non-null and points to a registered DrmDevice owned by this instance.
        unsafe { tk_drm_upstream::drm_drv_upstream::drm_dev_unplug(self.drm) };
        // SAFETY: self.drm is a valid DrmDevice pointer being unregistered from the system.
        unsafe { drm_dev_unregister(self.drm) };
        self.private.quiesce();
        // SAFETY: self.drm is valid; releases the reference owned by VirtioGpuDevice.
        unsafe { drm_dev_put(self.drm) };
        self.drm = ptr::null_mut();
    }
}

impl Drop for VirtioGpuDevice {
    fn drop(&mut self) {
        self.unplug();
    }
}

struct BlobMapLease {
    private: Arc<DriverPrivate>,
    resource: u32,
    _aperture: HostVisibleApertureLease,
    object: Option<GemObjectHold>,
    mapped: bool,
}

struct GemObjectHold {
    object: usize,
}

impl Drop for GemObjectHold {
    fn drop(&mut self) {
        // SAFETY: self.object holds a raw pointer to a DrmGemObject whose reference count
        // was incremented when this hold was created; drm_gem_object_put drops that reference.
        unsafe {
            drm_gem_object_put(self.object as *mut DrmGemObject);
        }
    }
}

impl Drop for BlobMapLease {
    fn drop(&mut self) {
        if !self.mapped {
            return;
        }
        let resource = self.resource;
        let _ = self.private.with_state(|state| {
            match state.engine.submit(
                &mut *state.display,
                GpuQueue::Control,
                GpuBatch::UnmapBlob { resource },
            ) {
                Ok(fence) => {
                    if state
                        .engine
                        .wait(&mut *state.display, fence, 5_000_000_000)
                        .is_err()
                    {
                        // A failed/timeout unmap is not permission to reuse
                        // the aperture. Reset makes it terminal before the
                        // RAII reservation below is dropped.
                        state.engine.reset(&mut *state.display, GpuQueue::Control);
                    }
                    let _ = state.engine.reap(fence);
                }
                Err(_) => {
                    state.engine.reset(&mut *state.display, GpuQueue::Control);
                }
            }
        });
        // Drop the GEM reference only after terminal host unmap/reset.
        drop(self.object.take());
    }
}

impl DriverPrivate {
    fn with_engine<R>(
        &self,
        f: impl FnOnce(&mut GpuEngine, &mut dyn DisplayDriverOps) -> R,
    ) -> Option<R> {
        self.with_state(|state| f(&mut state.engine, &mut *state.display))
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut DriverState) -> R) -> Option<R> {
        // SAFETY: self.engine_lock is a valid, initialized Linux mutex.
        unsafe { mutex_lock(&self.engine_lock) };
        let mut retired_refs = Vec::new();
        // SAFETY: Mutual exclusion is guaranteed by holding self.engine_lock, so accessing
        // the UnsafeCell contents via &mut is safe and race-free.
        let result = unsafe {
            (&mut *self.state.get()).as_mut().and_then(|state| {
                if state.removed {
                    None
                } else {
                    let result = f(state);
                    retired_refs = take_terminal_gem_refs(state);
                    Some(result)
                }
            })
        };
        // SAFETY: self.engine_lock is currently held by this thread.
        unsafe { mutex_unlock(&self.engine_lock) };
        // GEM free callbacks can submit teardown work and re-enter this
        // mutex. Drop terminal submission references only after unlocking.
        drop_gem_refs(retired_refs);
        result
    }

    /// Wait for an asynchronous submission to reach a terminal transport
    /// state, retire GEM ownership and remove its DRM fence record. The caller
    /// keeps an Arc to this private state for the duration of the wait.
    pub(crate) fn wait_submission_completion(&self, fence_id: u64) -> Option<Result<(), DevError>> {
        self.with_state(|state| {
            let result = state
                .engine
                .wait(&mut *state.display, fence_id, u64::MAX)
                .map(|_| ());
            let retired_refs = take_terminal_gem_refs(state);
            let _ = state.engine.reap(fence_id);
            (result, retired_refs)
        })
        .map(|(result, retired_refs)| {
            drop_gem_refs(retired_refs);
            result
        })
    }

    /// Take the original transport back after a successful probe but before
    /// the DRM device is published. Probe commands are terminal/reaped before
    /// this point, so no GPU queue reset is needed or desirable.
    fn take_display_after_probe(&self) -> Option<Box<dyn DisplayDriverOps>> {
        // SAFETY: self.engine_lock is a valid, initialized Linux mutex.
        unsafe { mutex_lock(&self.engine_lock) };
        // SAFETY: Holding self.engine_lock grants exclusive access to self.state.
        let state = unsafe { &mut *self.state.get() };
        let Some(current) = state.as_mut() else {
            // SAFETY: self.engine_lock is held and must be released before returning.
            unsafe { mutex_unlock(&self.engine_lock) };
            return None;
        };
        if !current.pending_refs.is_empty() {
            // SAFETY: self.engine_lock is held and must be released before returning.
            unsafe { mutex_unlock(&self.engine_lock) };
            return None;
        }
        current.removed = true;
        let owned = state.take();
        // SAFETY: self.engine_lock is held and must be released before returning.
        unsafe { mutex_unlock(&self.engine_lock) };
        let DriverState { display, .. } = owned?;
        Some(display)
    }

    fn quiesce(&self) {
        // SAFETY: self.engine_lock is a valid, initialized Linux mutex.
        unsafe { mutex_lock(&self.engine_lock) };
        // SAFETY: Holding self.engine_lock guarantees exclusive access to self.state.
        let (retired_refs, scanout_object, uncertain_scanout_refs) = unsafe {
            if let Some(state) = (&mut *self.state.get()).as_mut() {
                let control_reset = state.engine.reset(&mut *state.display, GpuQueue::Control);
                state.engine.reset(&mut *state.display, GpuQueue::Cursor);
                let retired_refs = take_terminal_gem_refs(state);
                state.removed = true;
                let (scanout_object, uncertain_scanout_refs) =
                    state.scanout_refs.take_after_reset(control_reset.quiesced);
                (retired_refs, scanout_object, uncertain_scanout_refs)
            } else {
                (Vec::new(), None, Vec::new())
            }
        };
        // SAFETY: self.engine_lock is held by the current thread.
        unsafe { mutex_unlock(&self.engine_lock) };
        drop_gem_refs(retired_refs);
        if let Some(object) = scanout_object {
            // SAFETY: `scanout_object` owns an independent GEM reference that
            // is released only after transport reset made host access terminal.
            unsafe { drm_gem_object_put(object as *mut DrmGemObject) };
        }
        drop_gem_refs(uncertain_scanout_refs);
    }
}

fn take_terminal_gem_refs(state: &mut DriverState) -> Vec<usize> {
    let ref_count = state
        .pending_refs
        .iter()
        .filter(|(fence_id, _)| {
            state
                .engine
                .fence(*fence_id)
                .is_none_or(|snapshot| snapshot.status != crate::transport::FenceStatus::Pending)
        })
        .try_fold(0usize, |total, (_, refs)| total.checked_add(refs.len()));
    let Some(ref_count) = ref_count else {
        return Vec::new();
    };
    let mut retired = Vec::new();
    if retired.try_reserve_exact(ref_count).is_err() {
        return retired;
    }
    let mut index = 0;
    while index < state.pending_refs.len() {
        let fence_id = state.pending_refs[index].0;
        let snapshot = state.engine.fence(fence_id);
        let done = snapshot
            .is_none_or(|snapshot| snapshot.status != crate::transport::FenceStatus::Pending);
        if done {
            let (_, refs) = state.pending_refs.swap_remove(index);
            for &object in &refs {
                // SAFETY: object was registered from a live VirtioGpuObject whose GEM reference
                // count was held for the duration of this submission, ensuring valid memory access.
                unsafe {
                    let gem = object as *mut crate::gem::VirtioGpuObject;
                    if (*gem).last_fence_id == fence_id {
                        (*gem).last_fence_status = match snapshot.map(|snapshot| snapshot.status) {
                            Some(crate::transport::FenceStatus::Complete) => {
                                crate::gem::FENCE_COMPLETE
                            }
                            _ => crate::gem::FENCE_FAILED,
                        };
                    }
                }
            }
            retired.extend(refs);
            let _ = state.engine.reap(fence_id);
        } else {
            index += 1;
        }
    }
    retired
}

fn drop_gem_refs(refs: Vec<usize>) {
    for object in refs {
        // SAFETY: Each object pointer represents a strong GEM reference acquired during
        // submission tracking. Releasing it via drm_gem_object_put balances the reference count.
        unsafe {
            tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(
                object as *mut tk_drm_upstream::drm_gem_types::DrmGemObject,
            );
        }
    }
}

unsafe fn init_drm_device(drm: *mut DrmDevice, private: *mut DriverPrivate) -> i32 {
    // SAFETY: Caller guarantees drm is a valid, allocated DrmDevice and private points
    // to a valid DriverPrivate whose connector and pipe cells are ready for initialization.
    unsafe {
        let ret = drmm_mode_config_init(drm);
        if ret != 0 {
            return ret;
        }
        (*drm).mode_config.min_width = MIN_MODE;
        (*drm).mode_config.min_height = MIN_MODE;
        (*drm).mode_config.max_width = MAX_MODE;
        (*drm).mode_config.max_height = MAX_MODE;
        (*drm).mode_config.preferred_depth = 24;
        (*drm).mode_config.fb_modifiers_not_supported = true;
        (*drm).mode_config.funcs = ptr::addr_of!(MODE_FUNCS) as *mut _;
        (*drm).mode_config.helper_private = ptr::addr_of!(MODE_HELPER_FUNCS) as *mut _;

        let ret = drm_vblank_init(drm, 1);
        if ret != 0 {
            return ret;
        }

        let connector = (*private).connector.get();
        let ret = drm_connector_init(
            drm,
            connector,
            ptr::addr_of!(CONNECTOR_FUNCS),
            DRM_MODE_CONNECTOR_VIRTUAL as i32,
        );
        if ret != 0 {
            return ret;
        }
        (*connector).helper_private = ptr::addr_of!(CONNECTOR_HELPER_FUNCS) as *mut _;

        let formats = [DRM_FORMAT_XRGB8888];
        let ret = drm_simple_display_pipe_init(
            drm,
            (*private).pipe.get(),
            ptr::addr_of!(PIPE_FUNCS),
            formats.as_ptr(),
            formats.len() as u32,
            ptr::null(),
            connector,
        );
        if ret != 0 {
            drm_connector_cleanup(connector);
            return ret;
        }
        // The upstream simple-KMS CRTC check requires a primary framebuffer
        // whenever the CRTC is enabled. VirtGPU supports an active output with
        // its primary plane detached, so clone the helper table into this
        // device's stable private storage and replace only that check.
        let pipe = (*private).pipe.get();
        let helpers = (*pipe).crtc.helper_private.cast::<DrmCrtcHelperFuncs>();
        if helpers.is_null() {
            return NEG_EINVAL;
        }
        let driver_helpers = (*private).crtc_helper.get();
        ptr::copy_nonoverlapping(helpers, driver_helpers, 1);
        (*driver_helpers).atomic_check = Some(virtgpu_crtc_atomic_check);
        (*pipe).crtc.helper_private = driver_helpers;
        // virtio_gpu_modeset_init(): drm_mode_config_reset() creates the
        // initial CRTC, plane and connector states after the outputs exist.
        tk_drm_upstream::drm_mode_config_upstream::drm_mode_config_reset(drm);
        0
    }
}

const NEG_ENOMEM: i32 = -tk_linux_kapi::config::ENOMEM;
const NEG_EINVAL: i32 = -tk_linux_kapi::config::EINVAL;
const NEG_ENODEV: i32 = -tk_linux_kapi::config::ENODEV;

fn errno_to_dev_error(errno: i32) -> DevError {
    match errno {
        NEG_ENOMEM => DevError::NoMemory,
        NEG_EINVAL => DevError::InvalidParam,
        NEG_ENODEV => DevError::Unsupported,
        _ => DevError::Io,
    }
}

unsafe fn private_from_device<'a>(drm: *mut DrmDevice) -> Option<&'a DriverPrivate> {
    if drm.is_null() {
        return None;
    }
    // SAFETY: drm is non-null and caller guarantees it points to a valid DrmDevice whose
    // dev_private was initialized to point to an Arc-managed DriverPrivate during device creation.
    unsafe { (*drm).dev_private.cast::<DriverPrivate>().as_ref() }
}

pub(crate) unsafe fn with_device_engine<R>(
    drm: *mut DrmDevice,
    f: impl FnOnce(&mut GpuEngine, &mut dyn DisplayDriverOps) -> R,
) -> Option<R> {
    // SAFETY: Caller guarantees drm is either null or points to a valid DrmDevice.
    let private = unsafe { private_from_device(drm) }?;
    private.with_engine(f)
}

pub(crate) unsafe fn with_device_state<R>(
    drm: *mut DrmDevice,
    f: impl FnOnce(&mut DriverState) -> R,
) -> Option<R> {
    // SAFETY: Caller guarantees drm is either null or points to a valid DrmDevice.
    let private = unsafe { private_from_device(drm) }?;
    private.with_state(f)
}

pub(crate) unsafe fn private_arc_from_device(drm: *mut DrmDevice) -> Option<Arc<DriverPrivate>> {
    // SAFETY: Caller guarantees drm is either null or points to a valid DrmDevice.
    let private = unsafe { private_from_device(drm) }?;
    let raw = private as *const DriverPrivate;
    // SAFETY: raw points to a valid DriverPrivate originating from an Arc allocation.
    unsafe { Arc::increment_strong_count(raw) };
    // SAFETY: The strong count was just incremented, so reconstructing an Arc from raw is safe.
    Some(unsafe { Arc::from_raw(raw) })
}

unsafe extern "C" fn virtgpu_release(drm: *mut DrmDevice) {
    if drm.is_null() {
        return;
    }
    // SAFETY: drm is a valid DrmDevice undergoing release. dev_private holds the raw
    // Arc<DriverPrivate> reference transferred during install, which must now be dropped.
    unsafe {
        let raw = (*drm).dev_private.cast::<DriverPrivate>();
        (*drm).dev_private = ptr::null_mut();
        if !raw.is_null() {
            drop(Arc::from_raw(raw));
        }
    }
}

unsafe extern "C" fn virtgpu_connector_destroy(connector: *mut DrmConnector) {
    // SAFETY: connector is a valid DrmConnector provided by the DRM core for cleanup.
    unsafe { drm_connector_cleanup(connector) };
}

unsafe extern "C" fn virtgpu_connector_detect(connector: *mut DrmConnector, _force: bool) -> u32 {
    if connector.is_null() {
        return CONNECTOR_STATUS_DISCONNECTED;
    }
    // SAFETY: connector is non-null and DRM core guarantees its dev field points to a valid DrmDevice.
    let Some(private) = (unsafe { private_from_device((*connector).dev) }) else {
        return CONNECTOR_STATUS_DISCONNECTED;
    };
    if private.connected.load(Ordering::Acquire) {
        CONNECTOR_STATUS_CONNECTED
    } else {
        CONNECTOR_STATUS_DISCONNECTED
    }
}

unsafe extern "C" fn virtgpu_connector_get_modes(connector: *mut DrmConnector) -> i32 {
    if connector.is_null() {
        return 0;
    }
    // SAFETY: connector is non-null and DRM core guarantees its dev field points to a valid DrmDevice.
    let Some(private) = (unsafe { private_from_device((*connector).dev) }) else {
        return 0;
    };
    if !private.connected.load(Ordering::Acquire) {
        return 0;
    }
    // SAFETY: (*connector).dev is a valid DrmDevice, and dimensions are within validated display bounds.
    let mode = unsafe {
        tk_drm_upstream::drm_modes_upstream::drm_cvt_mode(
            (*connector).dev,
            private.width as i32,
            private.height as i32,
            60,
            false,
            false,
            false,
        )
    };
    if mode.is_null() {
        return 0;
    }
    // SAFETY: mode is non-null and freshly created by drm_cvt_mode; drm_mode_probed_add adds it
    // to connector's probed_modes list and takes ownership of it.
    unsafe {
        (*mode).r#type |= DRM_MODE_TYPE_PREFERRED as u8;
        tk_drm_upstream::drm_modes_upstream::drm_mode_probed_add(connector, mode);
    }
    1
}

unsafe extern "C" fn virtgpu_connector_mode_valid(
    _connector: *mut DrmConnector,
    mode: *mut DrmDisplayMode,
) -> DrmModeStatus {
    // SAFETY: virtgpu_pipe_mode_valid safely handles null pipe and validates mode bounds.
    unsafe { virtgpu_pipe_mode_valid(ptr::null_mut(), mode) }
}

/// simple-KMS normally requires a primary framebuffer whenever its CRTC is
/// enabled. VirtGPU can keep the CRTC active while unbinding the primary plane;
/// the generic atomic checker still validates changed plane geometry and adds
/// the affected planes to the transaction.
unsafe extern "C" fn virtgpu_crtc_atomic_check(
    crtc: *mut DrmCrtc,
    state: *mut DrmAtomicCommit,
) -> i32 {
    if crtc.is_null() || state.is_null() {
        return NEG_EINVAL;
    }
    // SAFETY: The DRM atomic checker passes a live CRTC and transaction, and
    // this callback runs under the transaction's modeset locks.
    unsafe { drm_atomic_add_affected_planes(state, crtc) }
}

unsafe extern "C" fn virtgpu_best_encoder(
    connector: *mut DrmConnector,
) -> *mut tk_drm_upstream::drm_encoder_types::DrmEncoder {
    if connector.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: connector is non-null and (*connector).dev points to a valid DrmDevice.
    let Some(private) = (unsafe { private_from_device((*connector).dev) }) else {
        return ptr::null_mut();
    };
    // SAFETY: private.pipe is a valid, pinned DrmSimpleDisplayPipe whose embedded encoder
    // remains valid for the lifetime of the DRM device.
    unsafe { ptr::addr_of_mut!((*private.pipe.get()).encoder) }
}

unsafe extern "C" fn virtgpu_mode_valid(
    _drm: *mut DrmDevice,
    mode: *mut DrmDisplayMode,
) -> DrmModeStatus {
    // SAFETY: virtgpu_pipe_mode_valid safely handles a null pipe and validates mode.
    unsafe { virtgpu_pipe_mode_valid(ptr::null_mut(), mode) }
}

unsafe extern "C" fn virtgpu_fb_create(
    dev: *mut DrmDevice,
    file: *mut tk_drm_upstream::drm_file_types::DrmFile,
    info: *mut tk_drm_upstream::drm_fourcc_types::DrmFormatInfo,
    cmd: *mut tk_drm_upstream::uapi_drm_mode_types::DrmModeFbCmd2,
) -> *mut tk_drm_upstream::drm_framebuffer_types::DrmFramebuffer {
    // SAFETY: Arguments are forwarded from the DRM core callback; dev, file, info, and cmd
    // are valid pointers supplied by the DRM subsystem.
    unsafe { drm_gem_fb_create(dev, file, info.cast_const(), cmd.cast_const()) }
}

unsafe extern "C" fn virtgpu_pipe_mode_valid(
    _pipe: *mut DrmSimpleDisplayPipe,
    mode: *const DrmDisplayMode,
) -> DrmModeStatus {
    if mode.is_null() {
        return MODE_VIRTUAL_X;
    }
    // SAFETY: mode is verified non-null and points to a valid DrmDisplayMode provided by the caller.
    unsafe {
        let width = (*mode).hdisplay as u32;
        let height = (*mode).vdisplay as u32;
        if width < MIN_MODE || width > MAX_MODE {
            MODE_VIRTUAL_X
        } else if height < MIN_MODE || height > MAX_MODE {
            MODE_VIRTUAL_Y
        } else {
            MODE_OK
        }
    }
}

unsafe extern "C" fn virtgpu_pipe_enable(
    pipe: *mut DrmSimpleDisplayPipe,
    _crtc_state: *mut tk_drm_upstream::drm_crtc_types::DrmCrtcState,
    _plane_state: *mut tk_drm_upstream::drm_plane_types::DrmPlaneState,
) {
    if pipe.is_null() {
        return;
    }
    // SAFETY: pipe is non-null and provided by the DRM simple display pipe helper.
    let dev = unsafe { (*pipe).crtc.dev };
    if dev.is_null() {
        return;
    }
    // SAFETY: dev is non-null and points to the owning DrmDevice.
    let Some(private) = (unsafe { private_from_device(dev) }) else {
        return;
    };
    private.connected.store(true, Ordering::Release);
}

unsafe extern "C" fn virtgpu_pipe_disable(_pipe: *mut DrmSimpleDisplayPipe) {}

unsafe extern "C" fn virtgpu_pipe_enable_vblank(pipe: *mut DrmSimpleDisplayPipe) -> i32 {
    // SAFETY: pipe is provided by DRM simple display pipe callbacks, and (*pipe).crtc is initialized.
    unsafe { drm_crtc_vblank_helper_enable_vblank_timer(core::ptr::addr_of_mut!((*pipe).crtc)) }
}

unsafe extern "C" fn virtgpu_pipe_disable_vblank(pipe: *mut DrmSimpleDisplayPipe) {
    // SAFETY: pipe is provided by DRM simple display pipe callbacks, and (*pipe).crtc is initialized.
    unsafe {
        drm_crtc_vblank_helper_disable_vblank_timer(core::ptr::addr_of_mut!((*pipe).crtc));
    }
}

#[derive(Clone, Copy)]
struct DisplayCommitError {
    errno: i32,
    device_lost: bool,
}

struct DisplayCommitOutcome {
    result: Result<(), DisplayCommitError>,
    retired_object: Option<usize>,
}

fn display_error_to_errno(error: DevError) -> i32 {
    match error {
        DevError::Again => -tk_linux_kapi::config::EAGAIN,
        DevError::BadState | DevError::Io => -tk_linux_kapi::config::EIO,
        DevError::InvalidParam => NEG_EINVAL,
        DevError::NoMemory => NEG_ENOMEM,
        DevError::ResourceBusy | DevError::AlreadyExists => -tk_linux_kapi::config::EBUSY,
        DevError::Unsupported => -tk_linux_kapi::config::EOPNOTSUPP,
    }
}

fn submit_and_wait(state: &mut DriverState, batch: GpuBatch) -> Result<(), DisplayCommitError> {
    let fence = state
        .engine
        .submit(&mut *state.display, GpuQueue::Control, batch)
        .map_err(|error| DisplayCommitError {
            errno: display_error_to_errno(error),
            device_lost: false,
        })?;
    let result = state
        .engine
        .wait(&mut *state.display, fence, 5_000_000_000)
        .map(|_| ())
        .map_err(|error| DisplayCommitError {
            errno: display_error_to_errno(error),
            // A wait failure is terminalized by GpuEngine (including queue
            // reset on timeout); the host scanout may no longer match DRM.
            device_lost: true,
        });
    let _ = state.engine.reap(fence);
    result
}

fn submit_framebuffer(
    state: &mut DriverState,
    fb: *mut tk_drm_upstream::drm_framebuffer_types::DrmFramebuffer,
    plane_state: *mut DrmPlaneState,
) -> Result<(), DisplayCommitError> {
    if fb.is_null() || plane_state.is_null() {
        return Err(DisplayCommitError {
            errno: NEG_EINVAL,
            device_lost: false,
        });
    }
    // SAFETY: `fb` and `plane_state` come from the live atomic transaction,
    // whose references remain held until commit cleanup.
    let (object, fb_width, fb_height, width, height, x, y) = unsafe {
        (
            (*fb).obj[0],
            (*fb).width,
            (*fb).height,
            (*plane_state).crtc_w,
            (*plane_state).crtc_h,
            (*plane_state).src_x >> 16,
            (*plane_state).src_y >> 16,
        )
    };
    if object.is_null() {
        return Err(DisplayCommitError {
            errno: NEG_EINVAL,
            device_lost: false,
        });
    }
    if fb_width == 0
        || fb_height == 0
        || fb_width > MAX_MODE
        || fb_height > MAX_MODE
        || width == 0
        || height == 0
        || width > MAX_MODE
        || height > MAX_MODE
        || x.checked_add(width).is_none_or(|right| right > fb_width)
        || y.checked_add(height)
            .is_none_or(|bottom| bottom > fb_height)
    {
        return Err(DisplayCommitError {
            errno: NEG_EINVAL,
            device_lost: false,
        });
    }

    let gpu = object.cast::<VirtioGpuObject>();
    // SAFETY: `object` is the primary-plane GEM object held by the framebuffer
    // and belongs to this VirtGPU driver.
    let resource = unsafe { (*gpu).resource_id };
    if resource == 0 {
        return Err(DisplayCommitError {
            errno: -tk_linux_kapi::config::EIO,
            device_lost: false,
        });
    }
    // SAFETY: `gpu` is a live driver-owned GEM object retained by `fb`.
    let (is_blob, resource_kind) = unsafe { ((*gpu).is_blob, (*gpu).resource_kind) };
    if !is_blob && resource_kind == 3 {
        // This V0 scanout supports classic 2D resources and directly scanned
        // out blobs, not arbitrary virgl resources.
        return Err(DisplayCommitError {
            errno: -tk_linux_kapi::config::EOPNOTSUPP,
            device_lost: false,
        });
    }

    if is_blob {
        // SAFETY: `fb` is live and the blob framebuffer was validated by the
        // atomic plane checks; blob scanout requires its format metadata.
        let format = unsafe { (*fb).format };
        if format.is_null() {
            return Err(DisplayCommitError {
                errno: NEG_EINVAL,
                device_lost: false,
            });
        }
        // SAFETY: `format` is non-null, and `fb` remains held through this
        // commit, so its format, stride and offset are valid to read.
        let (format, stride, offset) =
            unsafe { ((*format).format, (*fb).pitches[0], (*fb).offsets[0]) };
        submit_and_wait(
            state,
            GpuBatch::PresentBlob {
                resource,
                source_x: x,
                source_y: y,
                width,
                height,
                framebuffer_width: fb_width,
                framebuffer_height: fb_height,
                format,
                stride,
                offset,
                damage: None,
            },
        )
    } else {
        submit_and_wait(
            state,
            GpuBatch::Transfer2d {
                resource,
                width: fb_width,
                height: fb_height,
            },
        )?;
        submit_and_wait(
            state,
            GpuBatch::Present {
                resource,
                width,
                height,
                source_x: x,
                source_y: y,
                damage: None,
            },
        )
    }
}

/// Submit the host operation for one live atomic transaction. Blocking
/// commits call this before the state swap; NONBLOCK calls it in the helper's
/// worker after input fences and commit dependencies have completed.
unsafe fn submit_atomic_display_state(
    atomic: *mut DrmAtomicCommit,
) -> Result<(), DisplayCommitError> {
    if atomic.is_null() {
        return Err(DisplayCommitError {
            errno: NEG_EINVAL,
            device_lost: false,
        });
    }
    // SAFETY: This helper is called by the atomic hooks with a live commit.
    let dev = unsafe { (*atomic).dev };
    if dev.is_null() {
        return Err(DisplayCommitError {
            errno: NEG_EINVAL,
            device_lost: false,
        });
    }
    // SAFETY: `dev` is retained by the atomic transaction for the duration of
    // its pre-swap callback or commit worker.
    let Some(private) = (unsafe { private_from_device(dev) }) else {
        return Err(DisplayCommitError {
            errno: NEG_ENODEV,
            device_lost: true,
        });
    };
    if !private.connected.load(Ordering::Acquire) {
        return Err(DisplayCommitError {
            errno: NEG_ENODEV,
            device_lost: true,
        });
    }

    let pipe = private.pipe.get();
    // SAFETY: `pipe` is pinned in `private`; atomic state lookup tolerates
    // missing replacements and falls back to the currently committed state.
    let (affects_pipe, active, framebuffer, plane_state) = unsafe {
        let crtc = ptr::addr_of_mut!((*pipe).crtc);
        let crtc_state = drm_atomic_get_new_crtc_state(atomic, crtc);
        let old_plane_state =
            drm_atomic_get_old_plane_state(atomic, ptr::addr_of_mut!((*pipe).plane));
        let new_plane_state =
            drm_atomic_get_new_plane_state(atomic, ptr::addr_of_mut!((*pipe).plane));
        let affects_pipe = !crtc_state.is_null()
            || old_plane_state
                .as_ref()
                .is_some_and(|plane| plane.crtc == crtc)
            || new_plane_state
                .as_ref()
                .is_some_and(|plane| plane.crtc == crtc);
        if !affects_pipe {
            (false, false, ptr::null_mut(), ptr::null_mut())
        } else {
            let crtc_state = crtc_state.as_ref().or_else(|| (*crtc).state.as_ref());
            let plane_state = new_plane_state
                .as_ref()
                .or_else(|| (*pipe).plane.state.as_ref());
            match (crtc_state, plane_state) {
                (Some(crtc_state), Some(plane_state)) => (
                    true,
                    crtc_state.enable
                        && crtc_state.active
                        && plane_state.crtc == crtc
                        && !plane_state.fb.is_null(),
                    plane_state.fb,
                    plane_state as *const DrmPlaneState as *mut DrmPlaneState,
                ),
                _ => {
                    return Err(DisplayCommitError {
                        errno: NEG_ENODEV,
                        device_lost: false,
                    });
                }
            }
        }
    };
    if !affects_pipe {
        return Ok(());
    }

    // Classic resource creation pins/maps GEM backing and takes the same
    // engine mutex as `with_device_state`, so it must happen before entering
    // the serialized present/scanout batch below.
    if active {
        // SAFETY: `active` is derived from this transaction's live plane state
        // and a non-null framebuffer, which remains referenced until callback
        // completion.
        let (object, fb_width, fb_height) = unsafe {
            (
                (*framebuffer).obj[0],
                (*framebuffer).width,
                (*framebuffer).height,
            )
        };
        if object.is_null() {
            return Err(DisplayCommitError {
                errno: NEG_EINVAL,
                device_lost: false,
            });
        }
        let gpu = object.cast::<VirtioGpuObject>();
        // SAFETY: `object` is a live driver-owned primary-plane GEM object.
        let resource_id = unsafe { (*gpu).resource_id };
        if resource_id == 0 {
            let ret = create_classic_resource(gpu, fb_width, fb_height);
            if ret != 0 {
                return Err(DisplayCommitError {
                    errno: ret,
                    device_lost: false,
                });
            }
        }
    }

    // SAFETY: `dev` is live; `with_device_state` serializes access to the
    // transport and owns no state pointer beyond this synchronous operation.
    let outcome = unsafe {
        with_device_state(dev, |state| {
            if active {
                // The atomic state's framebuffer reference is live until this
                // callback returns. Take an independent reference before the
                // host can adopt the resource as scanout.
                // SAFETY: `active` was derived from the live committed plane
                // state and its non-null framebuffer pointer.
                let object = unsafe { (*framebuffer).obj[0] };
                if object.is_null() {
                    return DisplayCommitOutcome {
                        result: Err(DisplayCommitError {
                            errno: NEG_EINVAL,
                            device_lost: false,
                        }),
                        retired_object: None,
                    };
                }
                if state.scanout_refs.uncertain_objects.try_reserve(1).is_err() {
                    return DisplayCommitOutcome {
                        result: Err(DisplayCommitError {
                            errno: NEG_ENOMEM,
                            device_lost: false,
                        }),
                        retired_object: None,
                    };
                }
                // SAFETY: `object` is a live GEM object owned by the atomic
                // transaction; its reference is transferred to scanout or to
                // the uncertain list below.
                unsafe { drm_gem_object_get(object) };
                match submit_framebuffer(state, framebuffer, plane_state) {
                    Ok(()) => DisplayCommitOutcome {
                        result: Ok(()),
                        retired_object: state.scanout_refs.replace_current(Some(object as usize)),
                    },
                    Err(error) if error.device_lost => {
                        state.scanout_refs.uncertain_objects.push(object as usize);
                        DisplayCommitOutcome {
                            result: Err(error),
                            retired_object: None,
                        }
                    }
                    Err(error) => DisplayCommitOutcome {
                        result: Err(error),
                        retired_object: Some(object as usize),
                    },
                }
            } else {
                match submit_and_wait(state, GpuBatch::DisableScanout) {
                    Ok(()) => DisplayCommitOutcome {
                        result: Ok(()),
                        retired_object: state.scanout_refs.replace_current(None),
                    },
                    Err(error) => DisplayCommitOutcome {
                        result: Err(error),
                        retired_object: None,
                    },
                }
            }
        })
    };
    let outcome = outcome.unwrap_or(DisplayCommitOutcome {
        result: Err(DisplayCommitError {
            errno: NEG_ENODEV,
            device_lost: true,
        }),
        retired_object: None,
    });
    if let Some(object) = outcome.retired_object {
        // SAFETY: The state lock has been released. This pointer represents a
        // scanout reference retired only after a successful replacement or
        // disable completion, or the temporary ref of a rejected submission.
        unsafe { drm_gem_object_put(object as *mut DrmGemObject) };
    }
    if let Err(error) = outcome.result {
        if error.device_lost {
            private.connected.store(false, Ordering::Release);
            // SAFETY: `dev` is a live DRM device retained by this atomic
            // commit; unplug records that its hardware/software state can no
            // longer be trusted after a terminal GPU wait failure.
            unsafe { tk_drm_upstream::drm_drv_upstream::drm_dev_unplug(dev) };
        }
        return Err(error);
    }
    Ok(())
}

unsafe extern "C" fn virtgpu_atomic_commit_pre_swap(atomic: *mut DrmAtomicCommit) -> i32 {
    if atomic.is_null() {
        return NEG_EINVAL;
    }
    // SAFETY: The commit state is live during the helper's pre-swap callback.
    let dev = unsafe { (*atomic).dev };
    // SAFETY: `dev` is retained by `atomic` until the callback returns.
    let Some(private) = (unsafe { private_from_device(dev) }) else {
        return NEG_ENODEV;
    };
    if private
        .staged_commit
        .compare_exchange(ptr::null_mut(), atomic, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return -tk_linux_kapi::config::EBUSY;
    }
    // SAFETY: The helper invokes this only for a blocking commit after plane
    // preparation and input-fence waits, immediately before swapping state.
    match unsafe { submit_atomic_display_state(atomic) } {
        Ok(()) => 0,
        Err(error) => {
            let _ = private.staged_commit.compare_exchange(
                atomic,
                ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            error.errno
        }
    }
}

unsafe extern "C" fn virtgpu_atomic_commit_tail(atomic: *mut DrmAtomicCommit) {
    if atomic.is_null() {
        return;
    }
    // SAFETY: The core's commit worker retains the state and its device for
    // the duration of this callback.
    let dev = unsafe { (*atomic).dev };
    // SAFETY: `dev` is a live device retained by `atomic`.
    let Some(private) = (unsafe { private_from_device(dev) }) else {
        // SAFETY: This is a committed operation whose device disappeared after
        // publication; the generic helper cancels events and completes fences.
        unsafe { drm_atomic_helper_commit_error(atomic, NEG_ENODEV) };
        return;
    };

    let blocking_stage = private
        .staged_commit
        .compare_exchange(atomic, ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    if !blocking_stage {
        // NONBLOCK state is already visible. Its input fences and preceding
        // commit dependencies have been waited by the shared helper worker.
        // An error is asynchronous: cancel its event/fence and unplug rather
        // than reporting a false successful flip to userspace.
        // SAFETY: The commit worker still owns the active atomic state.
        if let Err(error) = unsafe { submit_atomic_display_state(atomic) } {
            private.connected.store(false, Ordering::Release);
            // SAFETY: `atomic` is committed and the operation failed after the
            // ioctl returned; the shared helper records terminal async error.
            unsafe { drm_atomic_helper_commit_error(atomic, error.errno) };
            return;
        }
    }

    // The blocking path already completed its fallible host operation before
    // swap; the NONBLOCK path completed it above. Simple-KMS callbacks now
    // handle only the shared state/event handoff, not another GPU submission.
    // SAFETY: The commit is published and the device operation succeeded.
    unsafe { drm_atomic_helper_commit_tail(atomic) };
}

unsafe extern "C" fn virtgpu_pipe_update(
    _pipe: *mut DrmSimpleDisplayPipe,
    _plane_state: *mut DrmPlaneState,
) {
    // The driver submits from its fallible pre-swap hook for blocking commits,
    // and from atomic_commit_tail for NONBLOCK. Doing I/O here would lose an
    // error return path and, on NONBLOCK failure, falsely send the flip event.
}

const _: () = {
    assert!(offset_of!(DrmSimpleDisplayPipe, crtc) == 0);
    assert!(size_of::<DrmSimpleDisplayPipe>() > size_of::<DrmDisplayMode>());
};

#[cfg(test)]
mod scanout_lifetime_tests {
    use super::ScanoutRefs;

    #[test]
    fn completed_present_replacement_and_disable_retire_the_previous_ref() {
        let mut refs = ScanoutRefs::default();
        assert_eq!(refs.replace_current(Some(11)), None);
        assert_eq!(refs.replace_current(Some(22)), Some(11));
        assert_eq!(refs.replace_current(None), Some(22));
        assert_eq!(refs.current_object, None);
    }

    #[test]
    fn failed_async_scanout_keeps_old_and_possible_new_until_reset() {
        let mut refs = ScanoutRefs::default();
        assert_eq!(refs.replace_current(Some(11)), None);
        refs.uncertain_objects.push(22);

        assert_eq!(refs.current_object, Some(11));
        assert_eq!(refs.uncertain_objects, [22]);
        let (current, uncertain) = refs.take_after_reset(true);
        assert_eq!(current, Some(11));
        assert_eq!(uncertain, [22]);
        assert_eq!(refs.current_object, None);
        assert!(refs.uncertain_objects.is_empty());
    }

    #[test]
    fn failed_reset_does_not_release_scanout_refs() {
        let mut refs = ScanoutRefs::default();
        assert_eq!(refs.replace_current(Some(11)), None);
        refs.uncertain_objects.push(22);

        assert_eq!(refs.take_after_reset(false), (None, alloc::vec::Vec::new()));
        assert_eq!(refs.current_object, Some(11));
        assert_eq!(refs.uncertain_objects, [22]);

        let (current, uncertain) = refs.take_after_reset(true);
        assert_eq!(current, Some(11));
        assert_eq!(uncertain.as_slice(), &[22]);
        assert_eq!(refs.current_object, None);
        assert!(refs.uncertain_objects.is_empty());
    }
}
