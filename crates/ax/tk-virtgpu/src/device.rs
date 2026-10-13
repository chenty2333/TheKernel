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
    sync::atomic::{AtomicBool, Ordering},
};

use axdriver_display::{
    BlobMapping, DevError, DevResult, DisplayDriverOps, DisplayPciIdentity, GpuBatch,
    GpuCompletionData, GpuQueue, HostVisibleAperture, HostVisibleApertureLease,
};
use tk_drm_upstream::{
    drm_atomic_helper_upstream::{
        drm_atomic_helper_check, drm_atomic_helper_commit, drm_atomic_helper_commit_tail,
    },
    drm_atomic_state_helper_upstream::{
        drm_atomic_helper_connector_destroy_state, drm_atomic_helper_connector_duplicate_state,
        drm_atomic_helper_connector_reset,
    },
    drm_connector_types::{
        CONNECTOR_STATUS_CONNECTED, CONNECTOR_STATUS_DISCONNECTED, DrmConnector, DrmConnectorFuncs,
    },
    drm_connector_upstream::{drm_connector_cleanup, drm_connector_init},
    drm_device_types::DrmDevice,
    drm_drv_types::DrmDriver,
    drm_drv_upstream::{drm_dev_alloc, drm_dev_put, drm_dev_register, drm_dev_unregister},
    drm_gem_framebuffer_helper_upstream::drm_gem_fb_create,
    drm_gem_types::DrmGemObject,
    drm_gem_upstream::{drm_gem_object_get, drm_gem_object_put},
    drm_mode_config_types::DrmModeConfigFuncs,
    drm_mode_config_upstream::drmm_mode_config_init,
    drm_modes_types::{DrmDisplayMode, DrmModeStatus, MODE_OK, MODE_VIRTUAL_X, MODE_VIRTUAL_Y},
    drm_modeset_helper_vtables_types::{DrmConnectorHelperFuncs, DrmModeConfigHelperFuncs},
    drm_plane_types::DrmPlaneState,
    drm_simple_kms_helper_types_upstream::{DrmSimpleDisplayPipe, DrmSimpleDisplayPipeFuncs},
    drm_simple_kms_helper_upstream::drm_simple_display_pipe_init,
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
    width: u32,
    height: u32,
    pci_identity: Option<DisplayPciIdentity>,
    pub(crate) aperture: HostVisibleAperture,
    pipe: core::cell::UnsafeCell<DrmSimpleDisplayPipe>,
    connector: core::cell::UnsafeCell<DrmConnector>,
}

pub(crate) struct DriverState {
    pub(crate) engine: GpuEngine,
    pub(crate) display: Box<dyn DisplayDriverOps>,
    removed: bool,
    pub(crate) pending_refs: Vec<(u64, Vec<usize>)>,
}

// All engine access goes through the sleepable Linux mutex. KMS objects are
// initialized before the DRM device is registered and then remain pinned in
// the Arc allocation until the final DRM device release.
unsafe impl Send for DriverPrivate {}
unsafe impl Sync for DriverPrivate {}

struct SyncDriver(DrmDriver);
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
    fbdev_probe: None,
    show_fdinfo: None,
    major: 0,
    minor: 1,
    patchlevel: 0,
    name: DRIVER_NAME.as_ptr() as *mut c_char,
    desc: DRIVER_DESC.as_ptr() as *mut c_char,
    driver_features: DRIVER_GEM | DRIVER_MODESET | DRIVER_RENDER | DRIVER_ATOMIC,
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
    atomic_commit_tail: Some(drm_atomic_helper_commit_tail),
    atomic_commit_setup: None,
};

static CONNECTOR_FUNCS: DrmConnectorFuncs = DrmConnectorFuncs {
    dpms: None,
    reset: Some(drm_atomic_helper_connector_reset),
    detect: Some(virtgpu_connector_detect),
    force: None,
    fill_modes: None,
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
    enable_vblank: None,
    disable_vblank: None,
    reset_crtc: None,
    duplicate_crtc_state: None,
    destroy_crtc_state: None,
    reset_plane: None,
    duplicate_plane_state: None,
    destroy_plane_state: None,
};

fn zeroed<T>() -> T {
    // DRM object initializers follow Linux kzalloc conventions. These C-layout
    // DRM objects contain only integers, pointers, fixed arrays and C-layout
    // subobjects whose all-zero representation is valid.
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
            })),
            connected: AtomicBool::new(true),
            width: info.width,
            height: info.height,
            pci_identity,
            aperture: HostVisibleAperture::new(),
            pipe: core::cell::UnsafeCell::new(zeroed()),
            connector: core::cell::UnsafeCell::new(zeroed()),
        });
        unsafe { mutex_init(&private.engine_lock) };

        let drm = unsafe { drm_dev_alloc(&DRM_DRIVER.0, ptr::null_mut()) };
        if drm.is_null() {
            return Err(match private.take_display_after_probe() {
                Some(display) => VirtioGpuInstallError::recoverable(DevError::NoMemory, display),
                None => VirtioGpuInstallError::unrecoverable(DevError::NoMemory),
            });
        }

        // `dev_private` owns this strong reference until the core's final
        // release callback, not just until this Rust wrapper is dropped.
        unsafe {
            (*drm).dev_private =
                Arc::into_raw(private.clone()) as *mut DriverPrivate as *mut c_void;
        }

        let init = unsafe { init_drm_device(drm, Arc::as_ptr(&private) as *mut DriverPrivate) };
        if init != 0 {
            unsafe { drm_dev_put(drm) };
            let error = errno_to_dev_error(init);
            return Err(match private.take_display_after_probe() {
                Some(display) => VirtioGpuInstallError::recoverable(error, display),
                None => VirtioGpuInstallError::unrecoverable(error),
            });
        }
        let ret = unsafe { drm_dev_register(drm, 0) };
        if ret != 0 {
            // Registration failed before a DRM node could publish this device.
            // Tear down the core before returning its transport to fallback.
            unsafe { drm_dev_put(drm) };
            let error = errno_to_dev_error(ret);
            return Err(match private.take_display_after_probe() {
                Some(display) => VirtioGpuInstallError::recoverable(error, display),
                None => VirtioGpuInstallError::unrecoverable(error),
            });
        }

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
        if object.is_null() || unsafe { (*object).dev != self.drm } {
            return Err(DevError::InvalidParam);
        }
        let gpu = object.cast::<crate::gem::VirtioGpuObject>();
        let (resource, size, mappable) = unsafe {
            (
                (*gpu).resource_id,
                (*object).size as u64,
                (*gpu).is_blob
                    && (*gpu).blob_mem != crate::uapi::VIRTGPU_BLOB_MEM_GUEST
                    && (*gpu).blob_flags & crate::uapi::VIRTGPU_BLOB_FLAG_USE_MAPPABLE != 0,
            )
        };
        if resource == 0 || !mappable {
            return Err(DevError::InvalidParam);
        }
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
        unsafe { tk_drm_upstream::drm_drv_upstream::drm_dev_unplug(self.drm) };
        unsafe { drm_dev_unregister(self.drm) };
        self.private.quiesce();
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
        unsafe { mutex_lock(&self.engine_lock) };
        let mut retired_refs = Vec::new();
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
        unsafe { mutex_unlock(&self.engine_lock) };
        // GEM free callbacks can submit teardown work and re-enter this
        // mutex. Drop terminal submission references only after unlocking.
        drop_gem_refs(retired_refs);
        result
    }

    /// Take the original transport back after a successful probe but before
    /// the DRM device is published. Probe commands are terminal/reaped before
    /// this point, so no GPU queue reset is needed or desirable.
    fn take_display_after_probe(&self) -> Option<Box<dyn DisplayDriverOps>> {
        unsafe { mutex_lock(&self.engine_lock) };
        let state = unsafe { &mut *self.state.get() };
        let Some(current) = state.as_mut() else {
            unsafe { mutex_unlock(&self.engine_lock) };
            return None;
        };
        if !current.pending_refs.is_empty() {
            unsafe { mutex_unlock(&self.engine_lock) };
            return None;
        }
        current.removed = true;
        let owned = state.take();
        unsafe { mutex_unlock(&self.engine_lock) };
        let DriverState { display, .. } = owned?;
        Some(display)
    }

    fn quiesce(&self) {
        unsafe { mutex_lock(&self.engine_lock) };
        let retired_refs = unsafe {
            if let Some(state) = (&mut *self.state.get()).as_mut() {
                state.engine.reset(&mut *state.display, GpuQueue::Control);
                state.engine.reset(&mut *state.display, GpuQueue::Cursor);
                let retired_refs = take_terminal_gem_refs(state);
                state.removed = true;
                retired_refs
            } else {
                Vec::new()
            }
        };
        unsafe { mutex_unlock(&self.engine_lock) };
        drop_gem_refs(retired_refs);
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
        unsafe {
            tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(
                object as *mut tk_drm_upstream::drm_gem_types::DrmGemObject,
            );
        }
    }
}

unsafe fn init_drm_device(drm: *mut DrmDevice, private: *mut DriverPrivate) -> i32 {
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
    unsafe { (*drm).dev_private.cast::<DriverPrivate>().as_ref() }
}

pub(crate) unsafe fn with_device_engine<R>(
    drm: *mut DrmDevice,
    f: impl FnOnce(&mut GpuEngine, &mut dyn DisplayDriverOps) -> R,
) -> Option<R> {
    let private = unsafe { private_from_device(drm) }?;
    private.with_engine(f)
}

pub(crate) unsafe fn with_device_state<R>(
    drm: *mut DrmDevice,
    f: impl FnOnce(&mut DriverState) -> R,
) -> Option<R> {
    let private = unsafe { private_from_device(drm) }?;
    private.with_state(f)
}

unsafe extern "C" fn virtgpu_release(drm: *mut DrmDevice) {
    if drm.is_null() {
        return;
    }
    unsafe {
        let raw = (*drm).dev_private.cast::<DriverPrivate>();
        (*drm).dev_private = ptr::null_mut();
        if !raw.is_null() {
            drop(Arc::from_raw(raw));
        }
    }
}

unsafe extern "C" fn virtgpu_connector_destroy(connector: *mut DrmConnector) {
    unsafe { drm_connector_cleanup(connector) };
}

unsafe extern "C" fn virtgpu_connector_detect(connector: *mut DrmConnector, _force: bool) -> u32 {
    if connector.is_null() {
        return CONNECTOR_STATUS_DISCONNECTED;
    }
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
    let Some(private) = (unsafe { private_from_device((*connector).dev) }) else {
        return 0;
    };
    if !private.connected.load(Ordering::Acquire) {
        return 0;
    }
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
    unsafe { virtgpu_pipe_mode_valid(ptr::null_mut(), mode) }
}

unsafe extern "C" fn virtgpu_best_encoder(
    connector: *mut DrmConnector,
) -> *mut tk_drm_upstream::drm_encoder_types::DrmEncoder {
    if connector.is_null() {
        return ptr::null_mut();
    }
    let Some(private) = (unsafe { private_from_device((*connector).dev) }) else {
        return ptr::null_mut();
    };
    unsafe { ptr::addr_of_mut!((*private.pipe.get()).encoder) }
}

unsafe extern "C" fn virtgpu_mode_valid(
    _drm: *mut DrmDevice,
    mode: *mut DrmDisplayMode,
) -> DrmModeStatus {
    unsafe { virtgpu_pipe_mode_valid(ptr::null_mut(), mode) }
}

unsafe extern "C" fn virtgpu_fb_create(
    dev: *mut DrmDevice,
    file: *mut tk_drm_upstream::drm_file_types::DrmFile,
    info: *mut tk_drm_upstream::drm_fourcc_types::DrmFormatInfo,
    cmd: *mut tk_drm_upstream::uapi_drm_mode_types::DrmModeFbCmd2,
) -> *mut tk_drm_upstream::drm_framebuffer_types::DrmFramebuffer {
    unsafe { drm_gem_fb_create(dev, file, info.cast_const(), cmd.cast_const()) }
}

unsafe extern "C" fn virtgpu_pipe_mode_valid(
    _pipe: *mut DrmSimpleDisplayPipe,
    mode: *const DrmDisplayMode,
) -> DrmModeStatus {
    if mode.is_null() {
        return MODE_VIRTUAL_X;
    }
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
    if pipe.is_null() || unsafe { (*pipe).crtc.dev.is_null() } {
        return;
    }
    let Some(private) = (unsafe { private_from_device((*pipe).crtc.dev) }) else {
        return;
    };
    private.connected.store(true, Ordering::Release);
}

unsafe extern "C" fn virtgpu_pipe_disable(_pipe: *mut DrmSimpleDisplayPipe) {}

unsafe extern "C" fn virtgpu_pipe_update(
    _pipe: *mut DrmSimpleDisplayPipe,
    plane_state: *mut DrmPlaneState,
) {
    if plane_state.is_null() {
        return;
    }
    let fb = unsafe { (*plane_state).fb };
    if fb.is_null() {
        return;
    }
    let object = unsafe { (*fb).obj[0] };
    if object.is_null() {
        return;
    }
    let gpu = object.cast::<VirtioGpuObject>();
    let resource = unsafe { (*gpu).resource_id };
    if resource == 0 {
        let ret = create_classic_resource(gpu, unsafe { (*fb).width }, unsafe { (*fb).height });
        if ret != 0 {
            return;
        }
    }
    let resource = unsafe { (*gpu).resource_id };
    if resource == 0 {
        return;
    }
    let dev = unsafe { (*fb).dev };
    let (fb_width, fb_height) = unsafe { ((*fb).width, (*fb).height) };
    let (width, height) = unsafe { ((*plane_state).crtc_w, (*plane_state).crtc_h) };
    if fb_width == 0
        || fb_height == 0
        || fb_width > 8192
        || fb_height > 8192
        || width == 0
        || height == 0
        || width > 8192
        || height > 8192
    {
        return;
    }
    let x = unsafe { (*plane_state).src_x >> 16 };
    let y = unsafe { (*plane_state).src_y >> 16 };
    if x.checked_add(width).is_none_or(|right| right > fb_width)
        || y.checked_add(height)
            .is_none_or(|bottom| bottom > fb_height)
    {
        return;
    }
    let is_blob = unsafe { (*gpu).is_blob };
    if !is_blob && unsafe { (*gpu).resource_kind == 3 } {
        // The initial virtual scanout supports classic 2D resources and
        // directly scanned-out blobs; arbitrary virgl resources are not a
        // scanout source.
        return;
    }
    let format = unsafe { (*fb).format };
    if is_blob && format.is_null() {
        return;
    }
    let Some(result) = (unsafe {
        with_device_state(dev, |state| {
            if is_blob {
                let batch = GpuBatch::PresentBlob {
                    resource,
                    source_x: x,
                    source_y: y,
                    width,
                    height,
                    framebuffer_width: fb_width,
                    framebuffer_height: fb_height,
                    format: (*format).format,
                    stride: (*fb).pitches[0],
                    offset: (*fb).offsets[0],
                    damage: None,
                };
                let present = state
                    .engine
                    .submit(&mut *state.display, GpuQueue::Control, batch)
                    .map_err(|_| ())?;
                if state
                    .engine
                    .wait(&mut *state.display, present, 5_000_000_000)
                    .is_err()
                {
                    return Err(());
                }
                let _ = state.engine.reap(present);
                return Ok(());
            }
            let transfer = state
                .engine
                .submit(
                    &mut *state.display,
                    GpuQueue::Control,
                    GpuBatch::Transfer2d {
                        resource,
                        width: fb_width,
                        height: fb_height,
                    },
                )
                .map_err(|_| ())?;
            if state
                .engine
                .wait(&mut *state.display, transfer, 5_000_000_000)
                .is_err()
            {
                return Err(());
            }
            let _ = state.engine.reap(transfer);
            let present = state
                .engine
                .submit(
                    &mut *state.display,
                    GpuQueue::Control,
                    GpuBatch::Present {
                        resource,
                        width,
                        height,
                        source_x: x,
                        source_y: y,
                        damage: None,
                    },
                )
                .map_err(|_| ())?;
            if state
                .engine
                .wait(&mut *state.display, present, 5_000_000_000)
                .is_err()
            {
                return Err(());
            }
            let _ = state.engine.reap(present);
            Ok(())
        })
    }) else {
        return;
    };
    let _ = result;
}

const _: () = {
    assert!(offset_of!(DrmSimpleDisplayPipe, crtc) == 0);
    assert!(size_of::<DrmSimpleDisplayPipe>() > size_of::<DrmDisplayMode>());
};
