// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! DRM device/KMS registration for a VirtIO GPU supplied by the display bus.
//!
//! The bus driver owns VirtIO feature negotiation and DMA.  This module takes
//! an already initialized [`DisplayDriverOps`] implementation, retains it with
//! a Rust lifetime, and registers the Linux DRM core device around it.

use alloc::sync::Arc;
use core::{
    ffi::{c_char, c_void},
    mem::{offset_of, size_of, MaybeUninit},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};
use axdriver_display::{DevError, DevResult, DisplayDriverOps, GpuQueue};
use tk_drm_upstream::{
    drm_atomic_helper_upstream::{drm_atomic_helper_check, drm_atomic_helper_commit, drm_atomic_helper_commit_tail},
    drm_atomic_state_helper_upstream::{
        drm_atomic_helper_connector_destroy_state, drm_atomic_helper_connector_duplicate_state,
        drm_atomic_helper_connector_reset,
    },
    drm_connector_types::{
        DrmConnector, DrmConnectorFuncs, CONNECTOR_STATUS_CONNECTED,
        CONNECTOR_STATUS_DISCONNECTED,
    },
    drm_connector_upstream::{drm_connector_cleanup, drm_connector_init},
    drm_device_types::DrmDevice,
    drm_drv_types::DrmDriver,
    drm_drv_upstream::{drm_dev_alloc, drm_dev_put, drm_dev_register, drm_dev_unregister},
    drm_gem_framebuffer_helper_upstream::drm_gem_fb_create,
    drm_mode_config_types::DrmModeConfigFuncs,
    drm_modes_types::{DrmDisplayMode, DrmModeStatus, MODE_OK, MODE_VIRTUAL_X, MODE_VIRTUAL_Y},
    drm_modeset_helper_vtables_types::{DrmConnectorHelperFuncs, DrmModeConfigHelperFuncs},
    drm_simple_kms_helper_types_upstream::{DrmSimpleDisplayPipe, DrmSimpleDisplayPipeFuncs},
    drm_simple_kms_helper_upstream::drm_simple_display_pipe_init,
    drm_vblank_upstream::drm_vblank_init,
    drm_mode_config_upstream::drmm_mode_config_init,
    uapi_drm_consts::{DRM_FORMAT_XRGB8888, DRM_MODE_CONNECTOR_VIRTUAL, DRM_MODE_TYPE_PREFERRED},
};
use tk_linux_kapi::{
    config::{GFP_KERNEL, PAGE_SIZE},
    memory::{kfree, kmalloc},
    mutex::{mutex_init, mutex_lock, mutex_unlock},
    types::Mutex,
};

use crate::{ioctl::VIRTGPU_IOCTLS, transport::GpuEngine};

const DRIVER_GEM: u32 = 1 << 0;
const DRIVER_MODESET: u32 = 1 << 1;
const DRIVER_RENDER: u32 = 1 << 3;
const DRIVER_ATOMIC: u32 = 1 << 4;
const MIN_MODE: u32 = 32;
const MAX_MODE: u32 = 8192;

/// The single-scanout initial V0 DRM device.  The display bus remains the
/// owner of VirtIO queues and DMA; dropping this object unregisters/unplugs
/// the DRM device and releases the borrow of that bus-owned transport.
pub struct VirtioGpuDevice {
    drm: *mut DrmDevice,
    private: Arc<DriverPrivate>,
}

/// State addressed by DRM callbacks through `drm->dev_private`.
///
/// `engine` contains a real borrowed trait object, not a lifetime-erased
/// transport pointer. The `dev_private` pointer owns an additional Arc strong
/// reference released by the DRM driver's final `release` callback. On
/// `VirtioGpuDevice::drop`, the engine is reset and removed under `engine_lock`
/// before the caller's display borrow is released; late callbacks then see an
/// unplugged device with no transport.
pub(crate) struct DriverPrivate {
    engine_lock: Mutex,
    state: core::cell::UnsafeCell<Option<DriverState>>,
    connected: AtomicBool,
    width: u32,
    height: u32,
    pipe: core::cell::UnsafeCell<DrmSimpleDisplayPipe>,
    connector: core::cell::UnsafeCell<DrmConnector>,
}

struct DriverState {
    engine: GpuEngine,
    display: Box<dyn DisplayDriverOps>,
    removed: bool,
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
    open: None,
    postclose: None,
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
    pub fn install(mut display: Box<dyn DisplayDriverOps>) -> DevResult<Self> {
        if !display.supports_drm_transport() {
            return Err(DevError::Unsupported);
        }
        let info = display.info();
        if info.width < MIN_MODE || info.height < MIN_MODE || info.width > MAX_MODE || info.height > MAX_MODE {
            return Err(DevError::InvalidParam);
        }

        let engine = GpuEngine::new(&mut *display)?;
        let private = Arc::new(DriverPrivate {
            engine_lock: zeroed(),
            state: core::cell::UnsafeCell::new(Some(DriverState {
                engine,
                display,
                removed: false,
            })),
            connected: AtomicBool::new(true),
            width: info.width,
            height: info.height,
            pipe: core::cell::UnsafeCell::new(zeroed()),
            connector: core::cell::UnsafeCell::new(zeroed()),
        });
        unsafe { mutex_init(&private.engine_lock) };

        let drm = unsafe { drm_dev_alloc(&DRM_DRIVER.0, ptr::null_mut()) };
        if drm.is_null() {
            return Err(DevError::NoMemory);
        }

        // `dev_private` owns this strong reference until the core's final
        // release callback, not just until this Rust wrapper is dropped.
        unsafe {
            (*drm).dev_private = Arc::into_raw(private.clone()) as *mut DriverPrivate as *mut c_void;
        }

        let init = unsafe { init_drm_device(drm, Arc::as_ptr(&private) as *mut DriverPrivate) };
        if init != 0 {
            private.quiesce();
            unsafe { drm_dev_put(drm) };
            return Err(errno_to_dev_error(init));
        }
        let ret = unsafe { drm_dev_register(drm, 0) };
        if ret != 0 {
            private.quiesce();
            unsafe { drm_dev_put(drm) };
            return Err(errno_to_dev_error(ret));
        }

        Ok(Self {
            drm,
            private,
        })
    }

    /// Borrow the Linux DRM core device for node integration. The raw pointer
    /// is valid only while this `VirtioGpuDevice` owner remains alive.
    pub fn drm_device(&self) -> *mut DrmDevice {
        self.drm
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.private.width, self.private.height)
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

impl DriverPrivate {
    fn with_engine<R>(
        &self,
        f: impl FnOnce(&mut GpuEngine, &mut dyn DisplayDriverOps) -> R,
    ) -> Option<R> {
        unsafe { mutex_lock(&self.engine_lock) };
        let result = unsafe { (&mut *self.state.get()).as_mut().and_then(|state| {
            if state.removed {
                None
            } else {
                Some(f(&mut state.engine, &mut *state.display))
            }
        }) };
        unsafe { mutex_unlock(&self.engine_lock) };
        result
    }

    fn quiesce(&self) {
        unsafe { mutex_lock(&self.engine_lock) };
        unsafe {
            if let Some(state) = (&mut *self.state.get()).as_mut() {
                state.engine.reset(&mut *state.display, GpuQueue::Control);
                state.engine.reset(&mut *state.display, GpuQueue::Cursor);
                state.removed = true;
            }
        };
        unsafe { mutex_unlock(&self.engine_lock) };
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

unsafe extern "C" fn virtgpu_release(drm: *mut DrmDevice) {
    if drm.is_null() {
        return;
    }
    unsafe {
        let raw = (*drm).dev_private.cast::<DriverPrivate<'_>>();
        (*drm).dev_private = ptr::null_mut();
        if !raw.is_null() {
            drop(Arc::from_raw(raw));
        }
    }
}

unsafe extern "C" fn virtgpu_connector_destroy(connector: *mut DrmConnector) {
    unsafe { drm_connector_cleanup(connector) };
}

unsafe extern "C" fn virtgpu_connector_detect(
    connector: *mut DrmConnector,
    _force: bool,
) -> u32 {
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

unsafe extern "C" fn virtgpu_best_encoder(connector: *mut DrmConnector) -> *mut tk_drm_upstream::drm_encoder_types::DrmEncoder {
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
    _plane_state: *mut tk_drm_upstream::drm_plane_types::DrmPlaneState,
) {
    // Scanout object upload/present is wired by the GEM object layer. The
    // simple KMS pipe and atomic object graph are nevertheless real DRM core
    // objects, not private stand-ins.
}

const _: () = {
    assert!(offset_of!(DrmSimpleDisplayPipe, crtc) == 0);
    assert!(size_of::<DrmSimpleDisplayPipe>() > size_of::<DrmDisplayMode>());
};
