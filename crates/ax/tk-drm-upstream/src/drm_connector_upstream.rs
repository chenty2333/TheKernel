// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_connector.c` (batch B1a).
//!
//! Connector lifecycle (`__drm_mode_object_add` based init, dynamic and
//! managed variants, deferred free through `connector_free_work`), registration
//! state machine, the connector type/status/force/subpixel name tables, the
//! connector list iterator, and the standard, DVI-I, DP, TV, scaling, VRR,
//! colorspace, HDR and privacy-screen property sets, plus the connector and
//! tile-group ioctls.
//!
//! Deviations (see also the notes at each affected function):
//! - Variadic format helpers are not expanded; plain names are duplicated.
//! - The debugfs and sysfs hooks are omitted (CONFIG_DEBUG_FS / sysfs off).
//! - The `video_get_options()` cmdline hook is a no-op until the fbdev
//!   command-line parser (`video/cmdline`) is ported.
//! - `i2c_adapter` arguments are passed as opaque `*mut c_void`.

#![allow(non_snake_case, non_upper_case_globals, unsafe_op_in_unsafe_fn)]

use crate::drm_mode_object_upstream::{mutex_lock, mutex_unlock};

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use core::mem::{offset_of, size_of, MaybeUninit};
use core::sync::atomic::{AtomicBool, Ordering};

use tk_linux_kapi::drm_err;
use tk_linux_kapi::{
    assert::warn_on,
    config::{EFAULT, EINVAL, ENOMEM, GFP_KERNEL, ENOENT, EOPNOTSUPP},
    idr::{Ida, ida_alloc_range, ida_destroy, ida_free, ida_init},
    list::{INIT_LIST_HEAD, list_add_tail, list_del, list_del_init, list_empty},
    mm_native::copy_to_user_c,
    locks::{spin_lock_irq, spin_lock_irqsave, spin_unlock_irq, spin_unlock_irqrestore},
    memory::{kfree, kzalloc},
    mutex::mutex_init,
    types::{ListHead, LlistNode, Mutex, WorkStruct},
};

use crate::{
    drm_connector_types::*,
    drm_device_types::DrmDevice,
    drm_encoder_types::DrmEncoder,
    drm_encoder_upstream::kasprintf_name_id,
    drm_file_types::DrmFile,
    drm_mode_object_types::DrmModeObject,
    drm_mode_object_upstream::{
        __drm_mode_object_add, drm_core_check_feature, drm_drv_uses_atomic_modeset,
        drm_mode_object_put, drm_mode_object_unregister, drm_object_attach_property,
        drm_object_property_set_value, drm_mode_object_find, obj_to_connector, DRIVER_ATOMIC,
        DRIVER_MODESET,
    },
    drm_property_types::{DrmProperty, DrmPropEnumList},
    drm_property_upstream::{
        drm_property_create, drm_property_create_bool, drm_property_create_enum,
        drm_property_create_range, drm_property_destroy, drm_property_add_enum,
    },
    drm_modes_types::DrmDisplayMode,
    drm_modeset_lock_types::{DrmModesetAcquireCtx, DrmModesetLock},
    drm_mode_object_upstream::{
        drm_mode_object_get, drm_mode_object_get_properties, drm_mode_object_register, idr_alloc,
        idr_remove, put_user,
    },
    drm_encoder_upstream::drm_encoder_mask,
    drm_property_upstream::drm_property_replace_global_blob,
    uapi_drm_consts::*,
    uapi_drm_mode_types::*,
};

/// `list_entry(link, struct drm_encoder, head)`.
unsafe fn encoder_from_head(link: *mut ListHead) -> *mut DrmEncoder {
    link.cast::<u8>().sub(offset_of!(DrmEncoder, head)).cast::<DrmEncoder>()
}

/// `DRM_CONNECTOR_HDMI_VENDOR_LEN` / `_PRODUCT_LEN` (drm_connector.h).
const DRM_CONNECTOR_HDMI_VENDOR_LEN: usize = 8;
const DRM_CONNECTOR_HDMI_PRODUCT_LEN: usize = 16;
/// `DRM_PROP_NAME_LEN` (drm_property.h).
const DRM_PROP_NAME_LEN: usize = 32;

// Cross-batch callees: mode parsing and EDID/mode helpers (drm_modes.c,
// drm_edid.c), managed release (drm_managed.c), leases and DRM core, and the
// cmdline option lookup (video/cmdline.c).
unsafe extern "C" {
    fn __drmm_add_action_or_reset(
        dev: *mut DrmDevice,
        action: unsafe extern "C" fn(*mut DrmDevice, *mut c_void),
        data: *mut c_void,
        name: *const c_char,
    ) -> i32;
    fn video_get_options(name: *const c_char) -> *const c_char;
    fn drm_mode_parse_command_line_for_connector(
        option: *const c_char,
        connector: *mut DrmConnector,
        mode: *mut DrmCmdlineMode,
    ) -> bool;
    fn drm_modeset_lock(lock: *mut DrmModesetLock, ctx: *mut DrmModesetAcquireCtx) -> i32;
    fn drm_modeset_unlock(lock: *mut DrmModesetLock);
    fn drm_is_current_master(file_priv: *mut DrmFile) -> bool;
    fn drm_mode_destroy(dev: *mut DrmDevice, mode: *mut DrmDisplayMode);
    fn drm_mode_match(
        mode1: *const DrmDisplayMode,
        mode2: *const DrmDisplayMode,
        match_flags: u32,
    ) -> bool;
    fn drm_mode_convert_to_umode(out: *mut DrmModeModeinfo, mode: *const DrmDisplayMode);
    fn drm_privacy_screen_get_state(
        priv_: *mut c_void,
        sw_state: *mut DrmPrivacyScreenStatus,
        hw_state: *mut DrmPrivacyScreenStatus,
    );
    fn drm_privacy_screen_set_sw_state(priv_: *mut c_void, sw_state: DrmPrivacyScreenStatus) -> i32;
    fn drm_get_panel_orientation_quirk(width: i32, height: i32) -> DrmPanelOrientation;
}

/// Logs at `DRM_INFO` level through the kapi DRM logger.
macro_rules! drm_info {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let _ = $device;
        let __args: &[&dyn tk_linux_kapi::print::CFormatArg] =
            &[$(&($argument) as &dyn tk_linux_kapi::print::CFormatArg),*];
        let __message = tk_linux_kapi::print::format_message($format, __args);
        tk_linux_kapi::print::drm_log_at(
            tk_linux_kapi::print::DrmLogLevel::Info,
            "DRM info",
            file!(),
            line!(),
            &__message,
        );
    }};
}

/// Logs a DRM KMS debug message (`DRM_DEBUG_KMS()`) through the kapi logger.
macro_rules! drm_dbg_kms {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {
        tk_linux_kapi::drm_dbg!($device, $format $(, $argument)*)
    };
}

/// `drm_connector_enum_list[]` names, indexed by connector type.
// upstream: drm_connector.c drm_connector_enum_list[]
const DRM_CONNECTOR_ENUM_NAMES: [&[u8]; 21] = [
    b"Unknown\0", b"VGA\0", b"DVI-I\0", b"DVI-D\0", b"DVI-A\0", b"Composite\0",
    b"SVIDEO\0", b"LVDS\0", b"Component\0", b"DIN\0", b"DP\0", b"HDMI-A\0",
    b"HDMI-B\0", b"TV\0", b"eDP\0", b"Virtual\0", b"DSI\0", b"DPI\0",
    b"Writeback\0", b"SPI\0", b"USB\0",
];

/// The NUL-terminated name of connector type `i` (`drm_connector_enum_list[i].name`).
fn connector_type_name_c(i: usize) -> *const c_char {
    DRM_CONNECTOR_ENUM_NAMES[i].as_ptr().cast()
}

/// The name of connector type `i` without its terminator, for `kasprintf()`.
fn connector_type_name(i: usize) -> &'static [u8] {
    let name = DRM_CONNECTOR_ENUM_NAMES[i];
    &name[..name.len() - 1]
}

/// Per-connector-type IDA table (`drm_conn_prop_enum_list.ida`) and the
/// global connector list used by the fwnode lookup, both guarded by the
/// init flag below (Linux initialises them at module load).
struct ConnectorGlobals {
    ida: UnsafeCell<MaybeUninit<[Ida; 21]>>,
    list: UnsafeCell<MaybeUninit<ListHead>>,
    list_lock: UnsafeCell<MaybeUninit<Mutex>>,
    ready: AtomicBool,
}

// SAFETY: every access is serialised by the kernel-style locks stored in the
// same structure, mirroring the static C objects they replace.
unsafe impl Sync for ConnectorGlobals {}

static CONNECTOR_GLOBALS: ConnectorGlobals = ConnectorGlobals {
    ida: UnsafeCell::new(MaybeUninit::zeroed()),
    list: UnsafeCell::new(MaybeUninit::zeroed()),
    list_lock: UnsafeCell::new(MaybeUninit::zeroed()),
    ready: AtomicBool::new(false),
};

/// Initialises the global IDA table, list and lock on first use.
unsafe fn connector_globals_ensure() {
    if CONNECTOR_GLOBALS.ready.load(Ordering::Acquire) {
        return;
    }
    let ida = CONNECTOR_GLOBALS.ida.get().cast::<Ida>();
    for i in 0..21 {
        ida_init(&mut *ida.add(i));
    }
    INIT_LIST_HEAD(CONNECTOR_GLOBALS.list.get().cast());
    mutex_init(CONNECTOR_GLOBALS.list_lock.get().cast::<Mutex>());
    CONNECTOR_GLOBALS.ready.store(true, Ordering::Release);
}

// upstream: drm_connector.c drm_connector_ida_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_ida_init() {
    unsafe {
        connector_globals_ensure();
    }
}

// upstream: drm_connector.c drm_connector_ida_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_ida_destroy() {
    unsafe {
        connector_globals_ensure();
        let ida = CONNECTOR_GLOBALS.ida.get().cast::<Ida>();
        for i in 0..21 {
            ida_destroy(&mut *ida.add(i));
        }
    }
}

/// `drm_connector_enum_list[type].ida`.
unsafe fn connector_type_ida(connector_type: i32) -> *mut Ida {
    unsafe {
        connector_globals_ensure();
        CONNECTOR_GLOBALS.ida.get().cast::<Ida>().add(connector_type as usize)
    }
}

/// `ida_alloc_max()` (include/linux/idr.h): `[0, max]`.
unsafe fn ida_alloc_max(ida: *mut Ida, max: u32, gfp: u32) -> i32 {
    unsafe { ida_alloc_range(&mut *ida, 0, max, gfp) }
}

/// `ida_alloc_min()` (include/linux/idr.h): `[min, INT_MAX]`.
unsafe fn ida_alloc_min(ida: *mut Ida, min: u32, gfp: u32) -> i32 {
    unsafe { ida_alloc_range(&mut *ida, min, i32::MAX as u32, gfp) }
}

// upstream: drm_connector.c drm_get_connector_type_name()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_get_connector_type_name(r#type: u32) -> *const c_char {
    if (r#type as usize) < DRM_CONNECTOR_ENUM_NAMES.len() {
        connector_type_name_c(r#type as usize)
    } else {
        core::ptr::null()
    }
}

// upstream: drm_connector.c drm_connector_get_cmdline_mode()
unsafe fn drm_connector_get_cmdline_mode(connector: *mut DrmConnector) {
    unsafe {
        let mode = core::ptr::addr_of_mut!((*connector).cmdline_mode);

        let option = video_get_options((*connector).name);
        if option.is_null() {
            return;
        }

        if !drm_mode_parse_command_line_for_connector(option, connector, mode) {
            return;
        }

        if (*mode).force != DRM_FORCE_UNSPECIFIED {
            drm_info!(
                (*connector).dev,
                "forcing %s connector %s\n",
                (*connector).name,
                drm_get_connector_force_name((*mode).force)
            );
            (*connector).force = (*mode).force;
        }

        if (*mode).panel_orientation != DRM_MODE_PANEL_ORIENTATION_UNKNOWN {
            drm_info!(
                (*connector).dev,
                "cmdline forces connector %s panel_orientation to %d\n",
                (*connector).name,
                (*mode).panel_orientation
            );
            drm_connector_set_panel_orientation(connector, (*mode).panel_orientation);
        }

        drm_dbg_kms!(
            (*connector).dev,
            "cmdline mode for connector %s %s %dx%d@%dHz%s%s%s\n",
            (*connector).name,
            (*mode).name.as_ptr(),
            (*mode).xres,
            (*mode).yres,
            if (*mode).refresh_specified { (*mode).refresh } else { 60 },
            if (*mode).rb { " reduced blanking" } else { "" },
            if (*mode).margins { " with margins" } else { "" },
            if (*mode).interlace { " interlaced" } else { "" }
        );
    }
}

/// `container_of(kref, struct drm_connector, base.refcount)`.
unsafe fn connector_from_kref(kref: *mut tk_linux_kapi::types::Kref) -> *mut DrmConnector {
    kref.cast::<u8>()
        .sub(offset_of!(DrmModeObject, refcount))
        .sub(offset_of!(DrmConnector, base))
        .cast::<DrmConnector>()
}

/// `container_of(link, struct drm_connector, free_node)` for llist members.
unsafe fn connector_from_free_node(node: *mut LlistNode) -> *mut DrmConnector {
    node.cast::<u8>()
        .sub(offset_of!(DrmConnector, free_node))
        .cast::<DrmConnector>()
}

/// `container_of(link, struct drm_connector, head)`.
unsafe fn connector_from_head(link: *mut ListHead) -> *mut DrmConnector {
    link.cast::<u8>()
        .sub(offset_of!(DrmConnector, head))
        .cast::<DrmConnector>()
}

// upstream: drm_connector.c drm_connector_free()
unsafe extern "C" fn drm_connector_free(kref: *mut tk_linux_kapi::types::Kref) {
    unsafe {
        let connector = connector_from_kref(kref);
        let dev = (*connector).dev;

        drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*connector).base));
        if let Some(destroy) = (*(*connector).funcs).destroy {
            destroy(connector);
        }
    }
}

// upstream: drm_connector.c drm_connector_free_work_fn()
pub unsafe extern "C" fn drm_connector_free_work_fn(work: *mut WorkStruct) {
    unsafe {
        // container_of(work, struct drm_device, mode_config.connector_free_work)
        let config = work
            .cast::<u8>()
            .sub(offset_of!(crate::drm_mode_config_types::DrmModeConfig, connector_free_work))
            .cast::<crate::drm_mode_config_types::DrmModeConfig>();
        let dev = (config.cast::<u8>())
            .sub(offset_of!(DrmDevice, mode_config))
            .cast::<DrmDevice>();
        let config_ptr = core::ptr::addr_of_mut!((*dev).mode_config);

        let mut flags: u64 = 0;
        spin_lock_irqsave(&mut (*config_ptr).connector_list_lock, &mut flags);
        let freed = tk_linux_kapi::list::llist_del_all(core::ptr::addr_of_mut!((*config_ptr).connector_free_list));
        spin_unlock_irqrestore(&mut (*config_ptr).connector_list_lock, flags);

        // llist_for_each_entry_safe(connector, n, freed, free_node)
        let mut node = freed;
        while !node.is_null() {
            let next = (*node).next;
            let connector = connector_from_free_node(node);
            drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*connector).base));
            if let Some(destroy) = (*(*connector).funcs).destroy {
                destroy(connector);
            }
            node = next;
        }
    }
}

/// Linux `drm_connector_init_only()`: object, IDs, names and default
/// properties, without linking the connector into the device list.
// upstream: drm_connector.c drm_connector_init_only()
unsafe fn drm_connector_init_only(
    dev: *mut DrmDevice,
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorFuncs,
    connector_type: i32,
    ddc: *mut c_void,
) -> i32 {
    unsafe {
        let config = core::ptr::addr_of_mut!((*dev).mode_config);
        let mut ret: i32;
        let connector_ida = connector_type_ida(connector_type);

        warn_on(
            drm_drv_uses_atomic_modeset(dev)
                && ((*funcs).atomic_destroy_state.is_none() || (*funcs).atomic_duplicate_state.is_none()),
        );

        ret = __drm_mode_object_add(
            dev,
            core::ptr::addr_of_mut!((*connector).base),
            DRM_MODE_OBJECT_CONNECTOR,
            false,
            Some(drm_connector_free),
        );
        if ret != 0 {
            return ret;
        }

        (*connector).base.properties = core::ptr::addr_of_mut!((*connector).properties);
        (*connector).dev = dev;
        (*connector).funcs = funcs.cast_mut();

        ret = ida_alloc_max(core::ptr::addr_of_mut!((*config).connector_ida), 31, GFP_KERNEL);
        if ret < 0 {
            drm_dbg_kms!(
                dev,
                "Failed to allocate %s connector index: %d\n",
                connector_type_name_c(connector_type as usize),
                ret
            );
            // out_put:
            drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*connector).base));
            return ret;
        }
        (*connector).index = ret as u32;

        (*connector).connector_type = connector_type;
        (*connector).connector_type_id = ida_alloc_min(connector_ida, 1, GFP_KERNEL);
        if (*connector).connector_type_id < 0 {
            ret = (*connector).connector_type_id;
            // out_put_id:
            ida_free(&mut (*config).connector_ida, (*connector).index);
            drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*connector).base));
            return ret;
        }

        (*connector).name = kasprintf_name_id(
            connector_type_name(connector_type as usize),
            (*connector).connector_type_id as u32,
        );
        if (*connector).name.is_null() {
            ret = -ENOMEM;
            // out_put_type_id:
            ida_free(&mut *connector_ida, (*connector).connector_type_id as u32);
            ida_free(&mut (*config).connector_ida, (*connector).index);
            drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*connector).base));
            return ret;
        }

        (*connector).ddc = ddc;
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*connector).head));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*connector).global_connector_list_entry));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*connector).probed_modes));
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*connector).modes));
        mutex_init(core::ptr::addr_of_mut!((*connector).mutex));
        mutex_init(core::ptr::addr_of_mut!((*connector).cec.mutex));
        mutex_init(core::ptr::addr_of_mut!((*connector).eld_mutex));
        mutex_init(core::ptr::addr_of_mut!((*connector).edid_override_mutex));
        mutex_init(core::ptr::addr_of_mut!((*connector).hdmi.infoframes.lock));
        mutex_init(core::ptr::addr_of_mut!((*connector).hdmi_audio.lock));

        (*connector).edid_blob_ptr = core::ptr::null_mut();
        (*connector).epoch_counter = 0;
        (*connector).tile_blob_ptr = core::ptr::null_mut();
        (*connector).status = CONNECTOR_STATUS_UNKNOWN;
        (*connector).display_info.panel_orientation = DRM_MODE_PANEL_ORIENTATION_UNKNOWN;

        drm_connector_get_cmdline_mode(connector);

        if connector_type != DRM_MODE_CONNECTOR_VIRTUAL as i32
            && connector_type != DRM_MODE_CONNECTOR_WRITEBACK as i32
        {
            drm_connector_attach_edid_property(connector);
        }

        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            (*config).dpms_property,
            0,
        );
        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            (*config).link_status_property,
            0,
        );
        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            (*config).non_desktop_property,
            0,
        );
        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            (*config).tile_property,
            0,
        );
        if drm_core_check_feature(dev, DRIVER_ATOMIC) {
            drm_object_attach_property(
                core::ptr::addr_of_mut!((*connector).base),
                (*config).prop_crtc_id,
                0,
            );
        }

        (*connector).debugfs_entry = core::ptr::null_mut();

        0
    }
}

// upstream: drm_connector.c drm_connector_add()
unsafe fn drm_connector_add(connector: *mut DrmConnector) {
    unsafe {
        let dev = (*connector).dev;
        let config = core::ptr::addr_of_mut!((*dev).mode_config);

        if warn_on(!list_empty(&(*connector).head)) {
            return;
        }

        spin_lock_irq(&mut (*config).connector_list_lock);
        list_add_tail(
            core::ptr::addr_of_mut!((*connector).head),
            core::ptr::addr_of_mut!((*config).connector_list),
        );
        (*config).num_connector += 1;
        spin_unlock_irq(&mut (*config).connector_list_lock);
    }
}

// upstream: drm_connector.c drm_connector_remove()
unsafe fn drm_connector_remove(connector: *mut DrmConnector) {
    unsafe {
        let dev = (*connector).dev;

        if list_empty(&(*connector).head) {
            return;
        }

        spin_lock_irq(&mut (*dev).mode_config.connector_list_lock);
        list_del_init(core::ptr::addr_of_mut!((*connector).head));
        (*dev).mode_config.num_connector -= 1;
        spin_unlock_irq(&mut (*dev).mode_config.connector_list_lock);
    }
}

// upstream: drm_connector.c drm_connector_init_and_add()
unsafe fn drm_connector_init_and_add(
    dev: *mut DrmDevice,
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorFuncs,
    connector_type: i32,
    ddc: *mut c_void,
) -> i32 {
    unsafe {
        let ret = drm_connector_init_only(dev, connector, funcs, connector_type, ddc);
        if ret != 0 {
            return ret;
        }
        drm_connector_add(connector);
        0
    }
}

// upstream: drm_connector.c drm_connector_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_init(
    dev: *mut DrmDevice,
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorFuncs,
    connector_type: i32,
) -> i32 {
    unsafe {
        if warn_on(!(!funcs.is_null() && (*funcs).destroy.is_some())) {
            return -EINVAL;
        }
        drm_connector_init_and_add(dev, connector, funcs, connector_type, core::ptr::null_mut())
    }
}

// upstream: drm_connector.c drm_connector_dynamic_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_dynamic_init(
    dev: *mut DrmDevice,
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorFuncs,
    connector_type: i32,
    ddc: *mut c_void,
) -> i32 {
    unsafe {
        if warn_on(!(!funcs.is_null() && (*funcs).destroy.is_some())) {
            return -EINVAL;
        }
        drm_connector_init_only(dev, connector, funcs, connector_type, ddc)
    }
}

// upstream: drm_connector.c drm_connector_init_with_ddc()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_init_with_ddc(
    dev: *mut DrmDevice,
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorFuncs,
    connector_type: i32,
    ddc: *mut c_void,
) -> i32 {
    unsafe {
        if warn_on(!(!funcs.is_null() && (*funcs).destroy.is_some())) {
            return -EINVAL;
        }
        drm_connector_init_and_add(dev, connector, funcs, connector_type, ddc)
    }
}

// upstream: drm_connector.c drm_connector_cleanup_action()
unsafe extern "C" fn drm_connector_cleanup_action(_dev: *mut DrmDevice, ptr: *mut c_void) {
    unsafe {
        drm_connector_cleanup(ptr.cast::<DrmConnector>());
    }
}

// upstream: drm_connector.c drmm_connector_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_connector_init(
    dev: *mut DrmDevice,
    connector: *mut DrmConnector,
    funcs: *const DrmConnectorFuncs,
    connector_type: i32,
    ddc: *mut c_void,
) -> i32 {
    unsafe {
        if warn_on(!funcs.is_null() && (*funcs).destroy.is_some()) {
            return -EINVAL;
        }

        let ret = drm_connector_init_and_add(dev, connector, funcs, connector_type, ddc);
        if ret != 0 {
            return ret;
        }

        let ret = __drmm_add_action_or_reset(
            dev,
            drm_connector_cleanup_action,
            connector.cast(),
            b"drm_connector_cleanup_action\0".as_ptr().cast(),
        );
        if ret != 0 {
            return ret;
        }

        0
    }
}

/// `strtomem_pad()`: copies `src` into a fixed byte array, zero padding.
// upstream: include/linux/string.h strtomem_pad()
unsafe fn strtomem_pad(dst: *mut u8, len: usize, src: *const c_char) {
    unsafe {
        let mut i = 0usize;
        while i < len {
            let c = *src.add(i);
            if c == 0 {
                break;
            }
            *dst.add(i) = c as u8;
            i += 1;
        }
        while i < len {
            *dst.add(i) = 0;
            i += 1;
        }
    }
}

/// `strlen()` on a NUL-terminated C string.
unsafe fn c_strlen(s: *const c_char) -> usize {
    unsafe {
        let mut len = 0usize;
        while *s.add(len) != 0 {
            len += 1;
        }
        len
    }
}

// upstream: drm_connector.c drmm_connector_hdmi_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_connector_hdmi_init(
    dev: *mut DrmDevice,
    connector: *mut DrmConnector,
    vendor: *const c_char,
    product: *const c_char,
    funcs: *const DrmConnectorFuncs,
    hdmi_funcs: *const DrmConnectorHdmiFuncs,
    connector_type: i32,
    ddc: *mut c_void,
    supported_formats: u64,
    max_bpc: u32,
) -> i32 {
    unsafe {
        if vendor.is_null() || product.is_null() {
            return -EINVAL;
        }

        if c_strlen(vendor) > DRM_CONNECTOR_HDMI_VENDOR_LEN
            || c_strlen(product) > DRM_CONNECTOR_HDMI_PRODUCT_LEN
        {
            return -EINVAL;
        }

        if !(connector_type == DRM_MODE_CONNECTOR_HDMIA as i32
            || connector_type == DRM_MODE_CONNECTOR_HDMIB as i32)
        {
            return -EINVAL;
        }

        if supported_formats == 0
            || supported_formats & (1u64 << DRM_OUTPUT_COLOR_FORMAT_RGB444) == 0
        {
            return -EINVAL;
        }

        if (*connector).ycbcr_420_allowed
            != (supported_formats & (1u64 << DRM_OUTPUT_COLOR_FORMAT_YCBCR420) != 0)
        {
            return -EINVAL;
        }

        if !(max_bpc == 8 || max_bpc == 10 || max_bpc == 12) {
            return -EINVAL;
        }

        if (*hdmi_funcs).avi.clear_infoframe.is_none()
            || (*hdmi_funcs).avi.write_infoframe.is_none()
            || (*hdmi_funcs).hdmi.clear_infoframe.is_none()
            || (*hdmi_funcs).hdmi.write_infoframe.is_none()
        {
            return -EINVAL;
        }

        let ret = drmm_connector_init(dev, connector, funcs, connector_type, ddc);
        if ret != 0 {
            return ret;
        }

        (*connector).hdmi.supported_formats = supported_formats;
        strtomem_pad((*connector).hdmi.vendor.as_mut_ptr(), DRM_CONNECTOR_HDMI_VENDOR_LEN, vendor);
        strtomem_pad((*connector).hdmi.product.as_mut_ptr(), DRM_CONNECTOR_HDMI_PRODUCT_LEN, product);

        if let Some(reset) = (*(*connector).funcs).reset {
            reset(connector);
        }

        drm_connector_attach_max_bpc_property(connector, 8, max_bpc as i32);
        (*connector).max_bpc = max_bpc;

        if max_bpc > 8 {
            drm_connector_attach_hdr_output_metadata_property(connector);
        }

        (*connector).hdmi.funcs = hdmi_funcs.cast_mut();

        0
    }
}

// upstream: drm_connector.c drm_connector_attach_edid_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_edid_property(connector: *mut DrmConnector) {
    unsafe {
        let config = core::ptr::addr_of_mut!((*(*connector).dev).mode_config);
        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            (*config).edid_property,
            0,
        );
    }
}

// upstream: drm_connector.c drm_connector_attach_encoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_encoder(
    connector: *mut DrmConnector,
    encoder: *mut DrmEncoder,
) -> i32 {
    unsafe {
        if warn_on(!(*connector).encoder.is_null()) {
            return -EINVAL;
        }
        (*connector).possible_encoders |= crate::drm_encoder_upstream::drm_encoder_mask(encoder);
        0
    }
}

// upstream: drm_connector.c drm_connector_has_possible_encoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_has_possible_encoder(
    connector: *mut DrmConnector,
    encoder: *mut DrmEncoder,
) -> bool {
    unsafe {
        (*connector).possible_encoders & crate::drm_encoder_upstream::drm_encoder_mask(encoder) != 0
    }
}

// upstream: drm_connector.c drm_mode_remove()
unsafe fn drm_mode_remove(connector: *mut DrmConnector, mode: *mut DrmDisplayMode) {
    unsafe {
        list_del(core::ptr::addr_of_mut!((*mode).head));
        drm_mode_destroy((*connector).dev, mode);
    }
}

// upstream: drm_connector.c drm_connector_cec_phys_addr_invalidate()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_cec_phys_addr_invalidate(connector: *mut DrmConnector) {
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*connector).cec.mutex));
        if !(*connector).cec.funcs.is_null() {
            if let Some(phys_addr_invalidate) = (*(*connector).cec.funcs).phys_addr_invalidate {
                phys_addr_invalidate(connector);
            }
        }
        mutex_unlock(core::ptr::addr_of_mut!((*connector).cec.mutex));
    }
}

// upstream: drm_connector.c drm_connector_cec_phys_addr_set()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_cec_phys_addr_set(connector: *mut DrmConnector) {
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*connector).cec.mutex));
        let addr = (*connector).display_info.source_physical_address;
        if !(*connector).cec.funcs.is_null() {
            if let Some(phys_addr_set) = (*(*connector).cec.funcs).phys_addr_set {
                phys_addr_set(connector, addr);
            }
        }
        mutex_unlock(core::ptr::addr_of_mut!((*connector).cec.mutex));
    }
}

// upstream: drm_connector.c drm_connector_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_cleanup(connector: *mut DrmConnector) {
    unsafe {
        let dev = (*connector).dev;

        if warn_on((*connector).registration_state == DRM_CONNECTOR_REGISTERED) {
            drm_connector_unregister(connector);
        }

        // platform_device_unregister(codec_pdev): HDMI audio codec devices are
        // not created on this target, so the pointer is NULL here.
        // privacy_screen_put(): the privacy screen is never attached on this target.
        // fwnode_handle_put(): fwnode is NULL on this target.

        if !(*connector).tile_group.is_null() {
            drm_mode_put_tile_group(dev, (*connector).tile_group);
            (*connector).tile_group = core::ptr::null_mut();
        }

        let head = core::ptr::addr_of_mut!((*connector).probed_modes);
        let mut cur = (*head).next;
        while cur != head {
            let next = (*cur).next;
            drm_mode_remove(connector, cur.cast::<u8>().sub(offset_of!(DrmDisplayMode, head)).cast());
            cur = next;
        }

        let head = core::ptr::addr_of_mut!((*connector).modes);
        let mut cur = (*head).next;
        while cur != head {
            let next = (*cur).next;
            drm_mode_remove(connector, cur.cast::<u8>().sub(offset_of!(DrmDisplayMode, head)).cast());
            cur = next;
        }

        ida_free(&mut *connector_type_ida((*connector).connector_type), (*connector).connector_type_id as u32);
        ida_free(&mut (*dev).mode_config.connector_ida, (*connector).index);

        kfree((*connector).display_info.bus_formats);
        kfree((*connector).display_info.vics);

        drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*connector).base));

        kfree((*connector).name);
        (*connector).name = core::ptr::null_mut();

        drm_connector_remove(connector);

        warn_on(!(*connector).state.is_null() && (*(*connector).funcs).atomic_destroy_state.is_none());
        if !(*connector).state.is_null() {
            if let Some(atomic_destroy_state) = (*(*connector).funcs).atomic_destroy_state {
                atomic_destroy_state(connector, (*connector).state);
            }
        }

        mutex_destroy_c(core::ptr::addr_of_mut!((*connector).hdmi_audio.lock));
        mutex_destroy_c(core::ptr::addr_of_mut!((*connector).hdmi.infoframes.lock));
        mutex_destroy_c(core::ptr::addr_of_mut!((*connector).mutex));

        core::ptr::write_bytes(connector.cast::<u8>(), 0, size_of::<DrmConnector>());

        // drm_sysfs_hotplug_event(dev) when registered: sysfs is not built.
    }
}

/// `mutex_destroy()` on a raw mutex pointer.
unsafe fn mutex_destroy_c(lock: *mut Mutex) {
    unsafe {
        tk_linux_kapi::mutex::mutex_destroy(&mut *lock);
    }
}

// upstream: drm_connector.c drm_connector_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_register(connector: *mut DrmConnector) -> i32 {
    unsafe {
        let mut ret = 0;

        if !(*(*connector).dev).registered {
            return 0;
        }

        mutex_lock(core::ptr::addr_of_mut!((*connector).mutex));
        'unlock: {
            if (*connector).registration_state != DRM_CONNECTOR_INITIALIZING {
                break 'unlock;
            }

            // sysfs/debugfs connector registration is not built on this target.

            if let Some(late_register) = (*(*connector).funcs).late_register {
                ret = late_register(connector);
                if ret != 0 {
                    break 'unlock;
                }
            }

            drm_mode_object_register((*connector).dev, core::ptr::addr_of_mut!((*connector).base));
            (*connector).registration_state = DRM_CONNECTOR_REGISTERED;

            // privacy-screen notifier registration: no privacy screen on this target.

            connector_globals_ensure();
            mutex_lock(CONNECTOR_GLOBALS.list_lock.get().cast::<Mutex>());
            list_add_tail(
                core::ptr::addr_of_mut!((*connector).global_connector_list_entry),
                CONNECTOR_GLOBALS.list.get().cast::<ListHead>(),
            );
            mutex_unlock(CONNECTOR_GLOBALS.list_lock.get().cast::<Mutex>());
        }
        mutex_unlock(core::ptr::addr_of_mut!((*connector).mutex));
        ret
    }
}

// upstream: drm_connector.c drm_connector_dynamic_register()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_dynamic_register(connector: *mut DrmConnector) -> i32 {
    unsafe {
        let ok = !(*connector).funcs.is_null() && (*(*connector).funcs).destroy.is_some();
        if warn_on(!ok) {
            return -EINVAL;
        }
        drm_connector_add(connector);
        drm_connector_register(connector)
    }
}

// upstream: drm_connector.c drm_connector_unregister()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_unregister(connector: *mut DrmConnector) {
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*connector).mutex));
        if (*connector).registration_state != DRM_CONNECTOR_REGISTERED {
            mutex_unlock(core::ptr::addr_of_mut!((*connector).mutex));
            return;
        }

        connector_globals_ensure();
        mutex_lock(CONNECTOR_GLOBALS.list_lock.get().cast::<Mutex>());
        list_del_init(core::ptr::addr_of_mut!((*connector).global_connector_list_entry));
        mutex_unlock(CONNECTOR_GLOBALS.list_lock.get().cast::<Mutex>());

        // privacy-screen notifier removal and sysfs/debugfs teardown: not built.

        if let Some(early_unregister) = (*(*connector).funcs).early_unregister {
            early_unregister(connector);
        }

        (*connector).registration_state = DRM_CONNECTOR_UNREGISTERED;
        mutex_unlock(core::ptr::addr_of_mut!((*connector).mutex));
    }
}

// upstream: drm_connector.c drm_connector_unregister_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_unregister_all(dev: *mut DrmDevice) {
    unsafe {
        let mut conn_iter = core::mem::MaybeUninit::<DrmConnectorListIter>::zeroed();
        let conn_iter = conn_iter.as_mut_ptr();
        drm_connector_list_iter_begin(dev, conn_iter);
        loop {
            let connector = drm_connector_list_iter_next(conn_iter);
            if connector.is_null() {
                break;
            }
            drm_connector_unregister(connector);
        }
        drm_connector_list_iter_end(conn_iter);
    }
}

// upstream: drm_connector.c drm_connector_register_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_register_all(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let mut conn_iter = core::mem::MaybeUninit::<DrmConnectorListIter>::zeroed();
        let conn_iter = conn_iter.as_mut_ptr();
        let mut ret = 0;

        drm_connector_list_iter_begin(dev, conn_iter);
        loop {
            let connector = drm_connector_list_iter_next(conn_iter);
            if connector.is_null() {
                break;
            }
            ret = drm_connector_register(connector);
            if ret != 0 {
                break;
            }
        }
        drm_connector_list_iter_end(conn_iter);

        if ret != 0 {
            drm_connector_unregister_all(dev);
        }
        ret
    }
}

/// One `struct drm_prop_enum_list` entry as a Sync table row; the name is
/// NUL-terminated for C consumers.
type EnumEntry = (i32, &'static [u8]);

/// Builds a C `drm_prop_enum_list[]` view of a static table on the stack.
// upstream: include/drm/drm_property.h struct drm_prop_enum_list
fn to_prop_enum_list<const N: usize>(table: &[EnumEntry; N]) -> [DrmPropEnumList; N] {
    core::array::from_fn(|i| DrmPropEnumList {
        r#type: table[i].0,
        name: table[i].1.as_ptr().cast::<c_char>() as *mut c_char,
    })
}

/// `DRM_ENUM_NAME_FN()`: the name for `val`, or "(null)".
// upstream: include/drm/drm_property.h DRM_ENUM_NAME_FN()
fn enum_name_fn<const N: usize>(table: &[EnumEntry; N], val: i32) -> *const c_char {
    for &(ty, name) in table.iter() {
        if ty == val {
            return name.as_ptr().cast();
        }
    }
    b"(null)\0".as_ptr().cast()
}

// upstream: drm_connector.c drm_get_connector_status_name()
#[unsafe(no_mangle)]
pub extern "C" fn drm_get_connector_status_name(status: DrmConnectorStatus) -> *const c_char {
    if status == CONNECTOR_STATUS_CONNECTED {
        b"connected\0".as_ptr().cast()
    } else if status == CONNECTOR_STATUS_DISCONNECTED {
        b"disconnected\0".as_ptr().cast()
    } else {
        b"unknown\0".as_ptr().cast()
    }
}

// upstream: drm_connector.c drm_get_connector_force_name()
#[unsafe(no_mangle)]
pub extern "C" fn drm_get_connector_force_name(force: DrmConnectorForce) -> *const c_char {
    match force {
        DRM_FORCE_UNSPECIFIED => b"unspecified\0".as_ptr().cast(),
        DRM_FORCE_OFF => b"off\0".as_ptr().cast(),
        DRM_FORCE_ON => b"on\0".as_ptr().cast(),
        DRM_FORCE_ON_DIGITAL => b"digital\0".as_ptr().cast(),
        _ => b"unknown\0".as_ptr().cast(),
    }
}

// upstream: drm_connector.c drm_connector_list_iter_begin()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_list_iter_begin(dev: *mut DrmDevice, iter: *mut DrmConnectorListIter) {
    unsafe {
        (*iter).dev = dev;
        (*iter).conn = core::ptr::null_mut();
    }
}

// upstream: drm_connector.c __drm_connector_put_safe()
unsafe fn __drm_connector_put_safe(conn: *mut DrmConnector) {
    unsafe {
        let config = core::ptr::addr_of_mut!((*(*conn).dev).mode_config);

        if !tk_linux_kapi::memory::refcount_dec_and_test(&mut (*conn).base.refcount.refcount) {
            return;
        }
        tk_linux_kapi::list::llist_add(
            core::ptr::addr_of_mut!((*conn).free_node),
            core::ptr::addr_of_mut!((*config).connector_free_list),
        );
        tk_linux_kapi::workqueue::queue_work(tk_linux_kapi::workqueue::system_dfl_wq, core::ptr::addr_of_mut!((*config).connector_free_work));
    }
}

// upstream: drm_connector.c drm_connector_list_iter_next()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_list_iter_next(iter: *mut DrmConnectorListIter) -> *mut DrmConnector {
    unsafe {
        let old_conn = (*iter).conn;
        let config = core::ptr::addr_of_mut!((*(*iter).dev).mode_config);
        let mut flags: u64 = 0;

        spin_lock_irqsave(&mut (*config).connector_list_lock, &mut flags);
        let list_head_ptr = core::ptr::addr_of_mut!((*config).connector_list);
        let mut lhead: *mut ListHead = if !old_conn.is_null() {
            core::ptr::addr_of_mut!((*old_conn).head)
        } else {
            list_head_ptr
        };
        loop {
            if (*lhead).next == list_head_ptr {
                (*iter).conn = core::ptr::null_mut();
                break;
            }
            lhead = (*lhead).next;
            (*iter).conn = connector_from_head(lhead);
            if tk_linux_kapi::memory::kref_get_unless_zero(&mut (*(*iter).conn).base.refcount) {
                break;
            }
        }

        if !old_conn.is_null() {
            __drm_connector_put_safe(old_conn);
        }
        spin_unlock_irqrestore(&mut (*config).connector_list_lock, flags);

        (*iter).conn
    }
}

// upstream: drm_connector.c drm_connector_list_iter_end()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_list_iter_end(iter: *mut DrmConnectorListIter) {
    unsafe {
        let config = core::ptr::addr_of_mut!((*(*iter).dev).mode_config);
        let mut flags: u64 = 0;

        (*iter).dev = core::ptr::null_mut();
        if !(*iter).conn.is_null() {
            spin_lock_irqsave(&mut (*config).connector_list_lock, &mut flags);
            __drm_connector_put_safe((*iter).conn);
            spin_unlock_irqrestore(&mut (*config).connector_list_lock, flags);
        }
    }
}

// upstream: drm_connector.c drm_get_subpixel_order_name()
static DRM_SUBPIXEL_ENUM_LIST: [EnumEntry; 6] = [
    (SUBPIXELUNKNOWN as i32, b"Unknown\0"),
    (SUBPIXELHORIZONTALRGB as i32, b"Horizontal RGB\0"),
    (SUBPIXELHORIZONTALBGR as i32, b"Horizontal BGR\0"),
    (SUBPIXELVERTICALRGB as i32, b"Vertical RGB\0"),
    (SUBPIXELVERTICALBGR as i32, b"Vertical BGR\0"),
    (SUBPIXELNONE as i32, b"None\0"),
];

#[unsafe(no_mangle)]
pub extern "C" fn drm_get_subpixel_order_name(order: SubpixelOrder) -> *const c_char {
    DRM_SUBPIXEL_ENUM_LIST[order as usize].1.as_ptr().cast()
}

// upstream: drm_connector.c drm_dpms_enum_list[] / drm_get_dpms_name()
static DRM_DPMS_ENUM_LIST: [EnumEntry; 4] = [
    (DRM_MODE_DPMS_ON as i32, b"On\0"),
    (DRM_MODE_DPMS_STANDBY as i32, b"Standby\0"),
    (DRM_MODE_DPMS_SUSPEND as i32, b"Suspend\0"),
    (DRM_MODE_DPMS_OFF as i32, b"Off\0"),
];

#[unsafe(no_mangle)]
pub extern "C" fn drm_get_dpms_name(val: i32) -> *const c_char {
    enum_name_fn(&DRM_DPMS_ENUM_LIST, val)
}

// upstream: drm_connector.c drm_link_status_enum_list[]
static DRM_LINK_STATUS_ENUM_LIST: [EnumEntry; 2] = [
    (DRM_MODE_LINK_STATUS_GOOD as i32, b"Good\0"),
    (DRM_MODE_LINK_STATUS_BAD as i32, b"Bad\0"),
];

// upstream: drm_connector.c drm_panel_type_enum_list[]
static DRM_PANEL_TYPE_ENUM_LIST: [EnumEntry; 2] = [
    (DRM_MODE_PANEL_TYPE_UNKNOWN as i32, b"unknown\0"),
    (DRM_MODE_PANEL_TYPE_OLED as i32, b"OLED\0"),
];

// upstream: drm_connector.c drm_display_info_set_bus_formats()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_display_info_set_bus_formats(
    info: *mut DrmDisplayInfo,
    formats: *const u32,
    num_formats: u32,
) -> i32 {
    unsafe {
        let mut fmts: *mut u32 = core::ptr::null_mut();

        if formats.is_null() && num_formats != 0 {
            return -EINVAL;
        }

        if !formats.is_null() && num_formats != 0 {
            let bytes = size_of::<u32>() * num_formats as usize;
            fmts = tk_linux_kapi::memory::kmalloc(bytes, GFP_KERNEL).cast::<u32>();
            if fmts.is_null() {
                return -ENOMEM;
            }
            core::ptr::copy_nonoverlapping(formats, fmts, num_formats as usize);
        }

        kfree((*info).bus_formats);
        (*info).bus_formats = fmts;
        (*info).num_bus_formats = num_formats;

        0
    }
}

// upstream: drm_connector.c scaling/aspect/content/orientation tables
static DRM_SCALING_MODE_ENUM_LIST: [EnumEntry; 4] = [
    (DRM_MODE_SCALE_NONE as i32, b"None\0"),
    (DRM_MODE_SCALE_FULLSCREEN as i32, b"Full\0"),
    (DRM_MODE_SCALE_CENTER as i32, b"Center\0"),
    (DRM_MODE_SCALE_ASPECT as i32, b"Full aspect\0"),
];

static DRM_ASPECT_RATIO_ENUM_LIST: [EnumEntry; 3] = [
    (DRM_MODE_PICTURE_ASPECT_NONE as i32, b"Automatic\0"),
    (DRM_MODE_PICTURE_ASPECT_4_3 as i32, b"4:3\0"),
    (DRM_MODE_PICTURE_ASPECT_16_9 as i32, b"16:9\0"),
];

static DRM_CONTENT_TYPE_ENUM_LIST: [EnumEntry; 5] = [
    (DRM_MODE_CONTENT_TYPE_NO_DATA as i32, b"No Data\0"),
    (DRM_MODE_CONTENT_TYPE_GRAPHICS as i32, b"Graphics\0"),
    (DRM_MODE_CONTENT_TYPE_PHOTO as i32, b"Photo\0"),
    (DRM_MODE_CONTENT_TYPE_CINEMA as i32, b"Cinema\0"),
    (DRM_MODE_CONTENT_TYPE_GAME as i32, b"Game\0"),
];

static DRM_PANEL_ORIENTATION_ENUM_LIST: [EnumEntry; 4] = [
    (DRM_MODE_PANEL_ORIENTATION_NORMAL, b"Normal\0"),
    (DRM_MODE_PANEL_ORIENTATION_BOTTOM_UP, b"Upside Down\0"),
    (DRM_MODE_PANEL_ORIENTATION_LEFT_UP, b"Left Side Up\0"),
    (DRM_MODE_PANEL_ORIENTATION_RIGHT_UP, b"Right Side Up\0"),
];

static DRM_DVI_I_SELECT_ENUM_LIST: [EnumEntry; 3] = [
    (DRM_MODE_SUBCONNECTOR_AUTOMATIC as i32, b"Automatic\0"),
    (DRM_MODE_SUBCONNECTOR_DVID as i32, b"DVI-D\0"),
    (DRM_MODE_SUBCONNECTOR_DVIA as i32, b"DVI-A\0"),
];

static DRM_DVI_I_SUBCONNECTOR_ENUM_LIST: [EnumEntry; 3] = [
    (DRM_MODE_SUBCONNECTOR_UNKNOWN as i32, b"Unknown\0"),
    (DRM_MODE_SUBCONNECTOR_DVID as i32, b"DVI-D\0"),
    (DRM_MODE_SUBCONNECTOR_DVIA as i32, b"DVI-A\0"),
];

static DRM_TV_MODE_ENUM_LIST: [EnumEntry; 8] = [
    (DRM_MODE_TV_MODE_NTSC as i32, b"NTSC\0"),
    (DRM_MODE_TV_MODE_NTSC_443 as i32, b"NTSC-443\0"),
    (DRM_MODE_TV_MODE_NTSC_J as i32, b"NTSC-J\0"),
    (DRM_MODE_TV_MODE_PAL as i32, b"PAL\0"),
    (DRM_MODE_TV_MODE_PAL_M as i32, b"PAL-M\0"),
    (DRM_MODE_TV_MODE_PAL_N as i32, b"PAL-N\0"),
    (DRM_MODE_TV_MODE_SECAM as i32, b"SECAM\0"),
    (DRM_MODE_TV_MODE_MONOCHROME as i32, b"Mono\0"),
];

#[unsafe(no_mangle)]
pub extern "C" fn drm_get_tv_mode_name(val: i32) -> *const c_char {
    enum_name_fn(&DRM_TV_MODE_ENUM_LIST, val)
}

// upstream: drm_connector.c drm_get_tv_mode_from_name()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_get_tv_mode_from_name(name: *const c_char, len: usize) -> i32 {
    unsafe {
        for &(ty, item) in DRM_TV_MODE_ENUM_LIST.iter() {
            // strlen(item->name) == len && !strncmp(item->name, name, len)
            let item_len = item.len() - 1;
            if item_len == len && core::slice::from_raw_parts(name.cast::<u8>(), len) == &item[..len] {
                return ty;
            }
        }
        -EINVAL
    }
}

static DRM_TV_SELECT_ENUM_LIST: [EnumEntry; 5] = [
    (DRM_MODE_SUBCONNECTOR_AUTOMATIC as i32, b"Automatic\0"),
    (DRM_MODE_SUBCONNECTOR_COMPOSITE as i32, b"Composite\0"),
    (DRM_MODE_SUBCONNECTOR_SVIDEO as i32, b"SVIDEO\0"),
    (DRM_MODE_SUBCONNECTOR_COMPONENT as i32, b"Component\0"),
    (DRM_MODE_SUBCONNECTOR_SCART as i32, b"SCART\0"),
];

static DRM_TV_SUBCONNECTOR_ENUM_LIST: [EnumEntry; 5] = [
    (DRM_MODE_SUBCONNECTOR_UNKNOWN as i32, b"Unknown\0"),
    (DRM_MODE_SUBCONNECTOR_COMPOSITE as i32, b"Composite\0"),
    (DRM_MODE_SUBCONNECTOR_SVIDEO as i32, b"SVIDEO\0"),
    (DRM_MODE_SUBCONNECTOR_COMPONENT as i32, b"Component\0"),
    (DRM_MODE_SUBCONNECTOR_SCART as i32, b"SCART\0"),
];

static DRM_DP_SUBCONNECTOR_ENUM_LIST: [EnumEntry; 7] = [
    (DRM_MODE_SUBCONNECTOR_UNKNOWN as i32, b"Unknown\0"),
    (DRM_MODE_SUBCONNECTOR_VGA as i32, b"VGA\0"),
    (DRM_MODE_SUBCONNECTOR_DVID as i32, b"DVI-D\0"),
    (DRM_MODE_SUBCONNECTOR_HDMIA as i32, b"HDMI\0"),
    (DRM_MODE_SUBCONNECTOR_DISPLAYPORT as i32, b"DP\0"),
    (DRM_MODE_SUBCONNECTOR_WIRELESS as i32, b"Wireless\0"),
    (DRM_MODE_SUBCONNECTOR_NATIVE as i32, b"Native\0"),
];

// upstream: drm_connector.c colorspace_names[] (sparse, indexed by colorimetry)
const COLORSPACE_NAMES: [&[u8]; DRM_MODE_COLORIMETRY_COUNT as usize] = [
    b"Default\0",
    b"SMPTE_170M_YCC\0",
    b"BT709_YCC\0",
    b"XVYCC_601\0",
    b"XVYCC_709\0",
    b"SYCC_601\0",
    b"opYCC_601\0",
    b"opRGB\0",
    b"BT2020_CYCC\0",
    b"BT2020_RGB\0",
    b"BT2020_YCC\0",
    b"DCI-P3_RGB_D65\0",
    b"DCI-P3_RGB_Theater\0",
    b"RGB_WIDE_FIXED\0",
    b"RGB_WIDE_FLOAT\0",
    b"BT601_YCC\0",
];

// upstream: drm_connector.c drm_get_colorspace_name()
#[unsafe(no_mangle)]
pub extern "C" fn drm_get_colorspace_name(colorspace: DrmColorspace) -> *const c_char {
    if (colorspace as usize) < COLORSPACE_NAMES.len() {
        return COLORSPACE_NAMES[colorspace as usize].as_ptr().cast();
    }
    b"(null)\0".as_ptr().cast()
}

/// `hdmi_colorspaces` mask (drm_connector.c).
const HDMI_COLORSPACES: u32 = (1 << DRM_MODE_COLORIMETRY_SMPTE_170M_YCC)
    | (1 << DRM_MODE_COLORIMETRY_BT709_YCC)
    | (1 << DRM_MODE_COLORIMETRY_XVYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_XVYCC_709)
    | (1 << DRM_MODE_COLORIMETRY_SYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_OPYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_OPRGB)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_CYCC)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_RGB)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_YCC)
    | (1 << DRM_MODE_COLORIMETRY_DCI_P3_RGB_D65)
    | (1 << DRM_MODE_COLORIMETRY_DCI_P3_RGB_THEATER);

/// `dp_colorspaces` mask (drm_connector.c).
const DP_COLORSPACES: u32 = (1 << DRM_MODE_COLORIMETRY_RGB_WIDE_FIXED)
    | (1 << DRM_MODE_COLORIMETRY_RGB_WIDE_FLOAT)
    | (1 << DRM_MODE_COLORIMETRY_OPRGB)
    | (1 << DRM_MODE_COLORIMETRY_DCI_P3_RGB_D65)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_RGB)
    | (1 << DRM_MODE_COLORIMETRY_BT601_YCC)
    | (1 << DRM_MODE_COLORIMETRY_BT709_YCC)
    | (1 << DRM_MODE_COLORIMETRY_XVYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_XVYCC_709)
    | (1 << DRM_MODE_COLORIMETRY_SYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_OPYCC_601)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_CYCC)
    | (1 << DRM_MODE_COLORIMETRY_BT2020_YCC);

// upstream: drm_connector.c broadcast_rgb_names[]
static BROADCAST_RGB_NAMES: [EnumEntry; 3] = [
    (DRM_HDMI_BROADCAST_RGB_AUTO as i32, b"Automatic\0"),
    (DRM_HDMI_BROADCAST_RGB_FULL as i32, b"Full\0"),
    (DRM_HDMI_BROADCAST_RGB_LIMITED as i32, b"Limited 16:235\0"),
];

// upstream: drm_connector.c drm_hdmi_connector_get_broadcast_rgb_name()
#[unsafe(no_mangle)]
pub extern "C" fn drm_hdmi_connector_get_broadcast_rgb_name(broadcast_rgb: DrmHdmiBroadcastRgb) -> *const c_char {
    if broadcast_rgb as usize >= BROADCAST_RGB_NAMES.len() {
        return core::ptr::null();
    }
    BROADCAST_RGB_NAMES[broadcast_rgb as usize].1.as_ptr().cast()
}

// upstream: drm_connector.c output_format_str[]
const OUTPUT_FORMAT_STR: [Option<&[u8]>; 4] = [
    Some(b"RGB\0"),
    Some(b"YUV 4:4:4\0"),
    Some(b"YUV 4:2:2\0"),
    Some(b"YUV 4:2:0\0"),
];

// upstream: drm_connector.c drm_hdmi_connector_get_output_format_name()
#[unsafe(no_mangle)]
pub extern "C" fn drm_hdmi_connector_get_output_format_name(fmt: DrmOutputColorFormat) -> *const c_char {
    if fmt as usize >= OUTPUT_FORMAT_STR.len() {
        return core::ptr::null();
    }
    match OUTPUT_FORMAT_STR[fmt as usize] {
        Some(name) => name.as_ptr().cast(),
        None => core::ptr::null(),
    }
}

/// Creates a property and stores it in `mode_config`, or returns `-ENOMEM`.
/// Mirrors the repeated `prop = ...; if (!prop) return -ENOMEM; config.x = prop;`
/// blocks of the C file.
macro_rules! store_prop_or_enomem {
    ($slot:expr, $create:expr) => {{
        let prop: *mut DrmProperty = $create;
        if prop.is_null() {
            return -ENOMEM;
        }
        $slot = prop;
    }};
}

// upstream: drm_connector.c drm_connector_create_standard_properties()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_create_standard_properties(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);

        store_prop_or_enomem!(
            (*mc).edid_property,
            drm_property_create(dev, DRM_MODE_PROP_BLOB | DRM_MODE_PROP_IMMUTABLE, b"EDID\0".as_ptr().cast(), 0)
        );
        store_prop_or_enomem!(
            (*mc).dpms_property,
            drm_property_create_enum(dev, 0, b"DPMS\0".as_ptr().cast(), to_prop_enum_list(&DRM_DPMS_ENUM_LIST).as_ptr(), DRM_DPMS_ENUM_LIST.len() as i32)
        );
        store_prop_or_enomem!(
            (*mc).path_property,
            drm_property_create(dev, DRM_MODE_PROP_BLOB | DRM_MODE_PROP_IMMUTABLE, b"PATH\0".as_ptr().cast(), 0)
        );
        store_prop_or_enomem!(
            (*mc).tile_property,
            drm_property_create(dev, DRM_MODE_PROP_BLOB | DRM_MODE_PROP_IMMUTABLE, b"TILE\0".as_ptr().cast(), 0)
        );
        store_prop_or_enomem!(
            (*mc).link_status_property,
            drm_property_create_enum(dev, 0, b"link-status\0".as_ptr().cast(), to_prop_enum_list(&DRM_LINK_STATUS_ENUM_LIST).as_ptr(), DRM_LINK_STATUS_ENUM_LIST.len() as i32)
        );
        store_prop_or_enomem!(
            (*mc).panel_type_property,
            drm_property_create_enum(dev, DRM_MODE_PROP_IMMUTABLE, b"panel_type\0".as_ptr().cast(), to_prop_enum_list(&DRM_PANEL_TYPE_ENUM_LIST).as_ptr(), DRM_PANEL_TYPE_ENUM_LIST.len() as i32)
        );
        store_prop_or_enomem!(
            (*mc).non_desktop_property,
            drm_property_create_bool(dev, DRM_MODE_PROP_IMMUTABLE, b"non-desktop\0".as_ptr().cast())
        );
        store_prop_or_enomem!(
            (*mc).hdr_output_metadata_property,
            drm_property_create(dev, DRM_MODE_PROP_BLOB, b"HDR_OUTPUT_METADATA\0".as_ptr().cast(), 0)
        );

        0
    }
}

// upstream: drm_connector.c drm_mode_create_dvi_i_properties()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_dvi_i_properties(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);

        if !(*mc).dvi_i_select_subconnector_property.is_null() {
            return 0;
        }

        let dvi_i_selector = drm_property_create_enum(
            dev,
            0,
            b"select subconnector\0".as_ptr().cast(),
            to_prop_enum_list(&DRM_DVI_I_SELECT_ENUM_LIST).as_ptr(),
            DRM_DVI_I_SELECT_ENUM_LIST.len() as i32,
        );
        (*mc).dvi_i_select_subconnector_property = dvi_i_selector;

        let dvi_i_subconnector = drm_property_create_enum(
            dev,
            DRM_MODE_PROP_IMMUTABLE,
            b"subconnector\0".as_ptr().cast(),
            to_prop_enum_list(&DRM_DVI_I_SUBCONNECTOR_ENUM_LIST).as_ptr(),
            DRM_DVI_I_SUBCONNECTOR_ENUM_LIST.len() as i32,
        );
        (*mc).dvi_i_subconnector_property = dvi_i_subconnector;

        0
    }
}

// upstream: drm_connector.c drm_connector_attach_dp_subconnector_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_dp_subconnector_property(connector: *mut DrmConnector) {
    unsafe {
        let mode_config = core::ptr::addr_of_mut!((*(*connector).dev).mode_config);

        if (*mode_config).dp_subconnector_property.is_null() {
            (*mode_config).dp_subconnector_property = drm_property_create_enum(
                (*connector).dev,
                DRM_MODE_PROP_IMMUTABLE,
                b"subconnector\0".as_ptr().cast(),
                to_prop_enum_list(&DRM_DP_SUBCONNECTOR_ENUM_LIST).as_ptr(),
                DRM_DP_SUBCONNECTOR_ENUM_LIST.len() as i32,
            );
        }

        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            (*mode_config).dp_subconnector_property,
            DRM_MODE_SUBCONNECTOR_UNKNOWN as u64,
        );
    }
}

// upstream: drm_connector.c drm_connector_attach_content_type_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_content_type_property(connector: *mut DrmConnector) -> i32 {
    unsafe {
        if drm_mode_create_content_type_property((*connector).dev) == 0 {
            drm_object_attach_property(
                core::ptr::addr_of_mut!((*connector).base),
                (*(*connector).dev).mode_config.content_type_property,
                DRM_MODE_CONTENT_TYPE_NO_DATA as u64,
            );
        }
        0
    }
}

// upstream: drm_connector.c drm_connector_attach_tv_margin_properties()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_tv_margin_properties(connector: *mut DrmConnector) {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*(*connector).dev).mode_config);
        let base = core::ptr::addr_of_mut!((*connector).base);
        drm_object_attach_property(base, (*mc).tv_left_margin_property, 0);
        drm_object_attach_property(base, (*mc).tv_right_margin_property, 0);
        drm_object_attach_property(base, (*mc).tv_top_margin_property, 0);
        drm_object_attach_property(base, (*mc).tv_bottom_margin_property, 0);
    }
}

// upstream: drm_connector.c drm_mode_create_tv_margin_properties()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_tv_margin_properties(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);

        if !(*mc).tv_left_margin_property.is_null() {
            return 0;
        }

        (*mc).tv_left_margin_property = drm_property_create_range(dev, 0, b"left margin\0".as_ptr().cast(), 0, 100);
        if (*mc).tv_left_margin_property.is_null() {
            return -ENOMEM;
        }

        (*mc).tv_right_margin_property = drm_property_create_range(dev, 0, b"right margin\0".as_ptr().cast(), 0, 100);
        if (*mc).tv_right_margin_property.is_null() {
            return -ENOMEM;
        }

        (*mc).tv_top_margin_property = drm_property_create_range(dev, 0, b"top margin\0".as_ptr().cast(), 0, 100);
        if (*mc).tv_top_margin_property.is_null() {
            return -ENOMEM;
        }

        (*mc).tv_bottom_margin_property = drm_property_create_range(dev, 0, b"bottom margin\0".as_ptr().cast(), 0, 100);
        if (*mc).tv_bottom_margin_property.is_null() {
            return -ENOMEM;
        }

        0
    }
}

/// Creates a TV range property (`0..=100`) into `slot`; returns true on failure.
unsafe fn tv_range_failed(dev: *mut DrmDevice, slot: *mut *mut DrmProperty, name: &[u8]) -> bool {
    unsafe {
        *slot = drm_property_create_range(dev, 0, name.as_ptr().cast(), 0, 100);
        (*slot).is_null()
    }
}

// upstream: drm_connector.c drm_mode_create_tv_properties_legacy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_tv_properties_legacy(
    dev: *mut DrmDevice,
    num_modes: u32,
    modes: *const *const c_char,
) -> i32 {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);

        if !(*mc).tv_select_subconnector_property.is_null() {
            return 0;
        }

        // Each failing step jumps to `nomem`, which returns -ENOMEM.
        let tv_selector = drm_property_create_enum(
            dev,
            0,
            b"select subconnector\0".as_ptr().cast(),
            to_prop_enum_list(&DRM_TV_SELECT_ENUM_LIST).as_ptr(),
            DRM_TV_SELECT_ENUM_LIST.len() as i32,
        );
        if tv_selector.is_null() {
            return -ENOMEM;
        }
        (*mc).tv_select_subconnector_property = tv_selector;

        let tv_subconnector = drm_property_create_enum(
            dev,
            DRM_MODE_PROP_IMMUTABLE,
            b"subconnector\0".as_ptr().cast(),
            to_prop_enum_list(&DRM_TV_SUBCONNECTOR_ENUM_LIST).as_ptr(),
            DRM_TV_SUBCONNECTOR_ENUM_LIST.len() as i32,
        );
        if tv_subconnector.is_null() {
            return -ENOMEM;
        }
        (*mc).tv_subconnector_property = tv_subconnector;

        if drm_mode_create_tv_margin_properties(dev) != 0 {
            return -ENOMEM;
        }

        if num_modes != 0 {
            (*mc).legacy_tv_mode_property =
                drm_property_create(dev, DRM_MODE_PROP_ENUM, b"mode\0".as_ptr().cast(), num_modes as i32);
            if (*mc).legacy_tv_mode_property.is_null() {
                return -ENOMEM;
            }
            for i in 0..num_modes {
                drm_property_add_enum((*mc).legacy_tv_mode_property, i as u64, *modes.add(i as usize));
            }
        }

        if tv_range_failed(dev, core::ptr::addr_of_mut!((*mc).tv_brightness_property), b"brightness\0") {
            return -ENOMEM;
        }
        if tv_range_failed(dev, core::ptr::addr_of_mut!((*mc).tv_contrast_property), b"contrast\0") {
            return -ENOMEM;
        }
        if tv_range_failed(dev, core::ptr::addr_of_mut!((*mc).tv_flicker_reduction_property), b"flicker reduction\0") {
            return -ENOMEM;
        }
        if tv_range_failed(dev, core::ptr::addr_of_mut!((*mc).tv_overscan_property), b"overscan\0") {
            return -ENOMEM;
        }
        if tv_range_failed(dev, core::ptr::addr_of_mut!((*mc).tv_saturation_property), b"saturation\0") {
            return -ENOMEM;
        }
        if tv_range_failed(dev, core::ptr::addr_of_mut!((*mc).tv_hue_property), b"hue\0") {
            return -ENOMEM;
        }

        0
    }
}

// upstream: drm_connector.c drm_mode_create_tv_properties()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_tv_properties(dev: *mut DrmDevice, supported_tv_modes: u32) -> i32 {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);

        if !(*mc).tv_mode_property.is_null() {
            return 0;
        }

        let mut tv_mode_list: [DrmPropEnumList; DRM_MODE_TV_MODE_MAX as usize] = core::array::from_fn(|_| DrmPropEnumList {
            r#type: 0,
            name: core::ptr::null_mut(),
        });
        let mut len = 0usize;
        for i in 0..DRM_MODE_TV_MODE_MAX {
            if supported_tv_modes & (1u32 << i) == 0 {
                continue;
            }
            tv_mode_list[len].r#type = i as i32;
            tv_mode_list[len].name = drm_get_tv_mode_name(i as i32) as *mut c_char;
            len += 1;
        }

        let tv_mode = drm_property_create_enum(dev, 0, b"TV mode\0".as_ptr().cast(), tv_mode_list.as_ptr(), len as i32);
        if tv_mode.is_null() {
            return -ENOMEM;
        }
        (*mc).tv_mode_property = tv_mode;

        drm_mode_create_tv_properties_legacy(dev, 0, core::ptr::null())
    }
}

// upstream: drm_connector.c drm_mode_create_scaling_mode_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_scaling_mode_property(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);

        if !(*mc).scaling_mode_property.is_null() {
            return 0;
        }

        let scaling_mode = drm_property_create_enum(
            dev,
            0,
            b"scaling mode\0".as_ptr().cast(),
            to_prop_enum_list(&DRM_SCALING_MODE_ENUM_LIST).as_ptr(),
            DRM_SCALING_MODE_ENUM_LIST.len() as i32,
        );
        (*mc).scaling_mode_property = scaling_mode;

        0
    }
}

// upstream: drm_connector.c drm_connector_attach_vrr_capable_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_vrr_capable_property(connector: *mut DrmConnector) -> i32 {
    unsafe {
        let dev = (*connector).dev;

        if (*connector).vrr_capable_property.is_null() {
            let prop = drm_property_create_bool(dev, DRM_MODE_PROP_IMMUTABLE, b"vrr_capable\0".as_ptr().cast());
            if prop.is_null() {
                return -ENOMEM;
            }
            (*connector).vrr_capable_property = prop;
            drm_object_attach_property(core::ptr::addr_of_mut!((*connector).base), prop, 0);
        }

        0
    }
}

// upstream: drm_connector.c drm_connector_attach_scaling_mode_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_scaling_mode_property(
    connector: *mut DrmConnector,
    scaling_mode_mask: u32,
) -> i32 {
    unsafe {
        let dev = (*connector).dev;
        let valid_scaling_mode_mask: u32 = (1u32 << DRM_SCALING_MODE_ENUM_LIST.len()) - 1;

        if warn_on(scaling_mode_mask.count_ones() < 2 || scaling_mode_mask & !valid_scaling_mode_mask != 0) {
            return -EINVAL;
        }

        let scaling_mode_property = drm_property_create(
            dev,
            DRM_MODE_PROP_ENUM,
            b"scaling mode\0".as_ptr().cast(),
            scaling_mode_mask.count_ones() as i32,
        );
        if scaling_mode_property.is_null() {
            return -ENOMEM;
        }

        for (i, &(ty, name)) in DRM_SCALING_MODE_ENUM_LIST.iter().enumerate() {
            if (1u32 << i) & scaling_mode_mask == 0 {
                continue;
            }

            let ret = drm_property_add_enum(scaling_mode_property, ty as u64, name.as_ptr().cast());
            if ret != 0 {
                drm_property_destroy(dev, scaling_mode_property);
                return ret;
            }
        }

        drm_object_attach_property(core::ptr::addr_of_mut!((*connector).base), scaling_mode_property, 0);
        (*connector).scaling_mode_property = scaling_mode_property;

        0
    }
}

// upstream: drm_connector.c drm_mode_create_aspect_ratio_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_aspect_ratio_property(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);

        if !(*mc).aspect_ratio_property.is_null() {
            return 0;
        }

        (*mc).aspect_ratio_property = drm_property_create_enum(
            dev,
            0,
            b"aspect ratio\0".as_ptr().cast(),
            to_prop_enum_list(&DRM_ASPECT_RATIO_ENUM_LIST).as_ptr(),
            DRM_ASPECT_RATIO_ENUM_LIST.len() as i32,
        );
        if (*mc).aspect_ratio_property.is_null() {
            return -ENOMEM;
        }

        0
    }
}

// upstream: drm_connector.c drm_mode_create_colorspace_property()
unsafe fn drm_mode_create_colorspace_property(connector: *mut DrmConnector, supported_colorspaces: u32) -> i32 {
    unsafe {
        let dev = (*connector).dev;
        let colorspaces = supported_colorspaces | (1u32 << DRM_MODE_COLORIMETRY_DEFAULT);
        let mut enum_list: [DrmPropEnumList; DRM_MODE_COLORIMETRY_COUNT as usize] =
            core::array::from_fn(|_| DrmPropEnumList { r#type: 0, name: core::ptr::null_mut() });

        if !(*connector).colorspace_property.is_null() {
            return 0;
        }

        if supported_colorspaces == 0 {
            drm_err!(
                dev,
                "No supported colorspaces provded on [CONNECTOR:%d:%s]\n",
                (*connector).base.id,
                (*connector).name
            );
            return -EINVAL;
        }

        if supported_colorspaces & !((1u32 << DRM_MODE_COLORIMETRY_COUNT) - 1) != 0 {
            drm_err!(
                dev,
                "Unknown colorspace provded on [CONNECTOR:%d:%s]\n",
                (*connector).base.id,
                (*connector).name
            );
            return -EINVAL;
        }

        let mut len = 0usize;
        for i in 0..DRM_MODE_COLORIMETRY_COUNT {
            if colorspaces & (1u32 << i) == 0 {
                continue;
            }
            enum_list[len].r#type = i as i32;
            enum_list[len].name = COLORSPACE_NAMES[i as usize].as_ptr().cast::<c_char>() as *mut c_char;
            len += 1;
        }

        (*connector).colorspace_property = drm_property_create_enum(
            dev,
            DRM_MODE_PROP_ENUM,
            b"Colorspace\0".as_ptr().cast(),
            enum_list.as_ptr(),
            len as i32,
        );
        if (*connector).colorspace_property.is_null() {
            return -ENOMEM;
        }

        0
    }
}

// upstream: drm_connector.c drm_mode_create_hdmi_colorspace_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_hdmi_colorspace_property(
    connector: *mut DrmConnector,
    supported_colorspaces: u32,
) -> i32 {
    unsafe {
        let colorspaces = if supported_colorspaces != 0 {
            supported_colorspaces & HDMI_COLORSPACES
        } else {
            HDMI_COLORSPACES
        };
        drm_mode_create_colorspace_property(connector, colorspaces)
    }
}

// upstream: drm_connector.c drm_mode_create_dp_colorspace_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_dp_colorspace_property(
    connector: *mut DrmConnector,
    supported_colorspaces: u32,
) -> i32 {
    unsafe {
        let colorspaces = if supported_colorspaces != 0 {
            supported_colorspaces & DP_COLORSPACES
        } else {
            DP_COLORSPACES
        };
        drm_mode_create_colorspace_property(connector, colorspaces)
    }
}

// upstream: drm_connector.c drm_mode_create_content_type_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_content_type_property(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);

        if !(*mc).content_type_property.is_null() {
            return 0;
        }

        (*mc).content_type_property = drm_property_create_enum(
            dev,
            0,
            b"content type\0".as_ptr().cast(),
            to_prop_enum_list(&DRM_CONTENT_TYPE_ENUM_LIST).as_ptr(),
            DRM_CONTENT_TYPE_ENUM_LIST.len() as i32,
        );
        if (*mc).content_type_property.is_null() {
            return -ENOMEM;
        }

        0
    }
}

// upstream: drm_connector.c drm_mode_create_suggested_offset_properties()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_suggested_offset_properties(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let mc = core::ptr::addr_of_mut!((*dev).mode_config);

        if !(*mc).suggested_x_property.is_null() && !(*mc).suggested_y_property.is_null() {
            return 0;
        }

        (*mc).suggested_x_property =
            drm_property_create_range(dev, DRM_MODE_PROP_IMMUTABLE, b"suggested X\0".as_ptr().cast(), 0, 0xffff_ffff);
        (*mc).suggested_y_property =
            drm_property_create_range(dev, DRM_MODE_PROP_IMMUTABLE, b"suggested Y\0".as_ptr().cast(), 0, 0xffff_ffff);

        if (*mc).suggested_x_property.is_null() || (*mc).suggested_y_property.is_null() {
            return -ENOMEM;
        }

        0
    }
}

// upstream: drm_connector.c drm_connector_set_path_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_set_path_property(connector: *mut DrmConnector, path: *const c_char) -> i32 {
    unsafe {
        let dev = (*connector).dev;
        drm_property_replace_global_blob(
            dev,
            core::ptr::addr_of_mut!((*connector).path_blob_ptr),
            c_strlen(path) + 1,
            path.cast(),
            core::ptr::addr_of_mut!((*connector).base),
            (*dev).mode_config.path_property,
        )
    }
}

/// Appends the decimal form of `value` to `buf` at `*len` (no bounds growth
/// beyond the 256-byte `tile[]` of the C code; the caller caps `*len`).
unsafe fn tile_push_u32(buf: *mut u8, len: &mut usize, value: u32) {
    let mut digits = [0u8; 10];
    let mut n = value;
    let mut count = 0usize;
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for i in (0..count).rev() {
        if *len + 1 < 256 {
            *buf.add(*len) = digits[i];
            *len += 1;
        }
    }
}

// upstream: drm_connector.c drm_connector_set_tile_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_set_tile_property(connector: *mut DrmConnector) -> i32 {
    unsafe {
        let dev = (*connector).dev;

        if !(*connector).has_tile {
            return drm_property_replace_global_blob(
                dev,
                core::ptr::addr_of_mut!((*connector).tile_blob_ptr),
                0,
                core::ptr::null(),
                core::ptr::addr_of_mut!((*connector).base),
                (*dev).mode_config.tile_property,
            );
        }

        // snprintf(tile, 256, "%d:%d:%d:%d:%d:%d:%d:%d", ...)
        let mut tile = [0u8; 256];
        let mut len = 0usize;
        let fields = [
            (*(*connector).tile_group).id as u32,
            (*connector).tile_is_single_monitor as u32,
            (*connector).num_h_tile as u32,
            (*connector).num_v_tile as u32,
            (*connector).tile_h_loc as u32,
            (*connector).tile_v_loc as u32,
            (*connector).tile_h_size as u32,
            (*connector).tile_v_size as u32,
        ];
        for (i, &field) in fields.iter().enumerate() {
            if i != 0 && len + 1 < 256 {
                tile[len] = b':';
                len += 1;
            }
            tile_push_u32(tile.as_mut_ptr(), &mut len, field);
        }
        tile[len] = 0;

        drm_property_replace_global_blob(
            dev,
            core::ptr::addr_of_mut!((*connector).tile_blob_ptr),
            len + 1,
            tile.as_ptr().cast(),
            core::ptr::addr_of_mut!((*connector).base),
            (*dev).mode_config.tile_property,
        )
    }
}

// upstream: drm_connector.c drm_connector_set_link_status_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_set_link_status_property(connector: *mut DrmConnector, link_status: u64) {
    unsafe {
        let dev = (*connector).dev;
        drm_modeset_lock(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex), core::ptr::null_mut());
        (*(*connector).state).link_status = link_status as u32;
        drm_modeset_unlock(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex));
    }
}

// upstream: drm_connector.c drm_connector_attach_max_bpc_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_max_bpc_property(connector: *mut DrmConnector, min: i32, max: i32) -> i32 {
    unsafe {
        let dev = (*connector).dev;

        let mut prop = (*connector).max_bpc_property;
        if prop.is_null() {
            prop = drm_property_create_range(dev, 0, b"max bpc\0".as_ptr().cast(), min as u64, max as u64);
            if prop.is_null() {
                return -ENOMEM;
            }
            (*connector).max_bpc_property = prop;
        }

        drm_object_attach_property(core::ptr::addr_of_mut!((*connector).base), prop, max as u64);
        (*(*connector).state).max_requested_bpc = max as u8;
        (*(*connector).state).max_bpc = max as u8;

        0
    }
}

// upstream: drm_connector.c drm_connector_attach_hdr_output_metadata_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_hdr_output_metadata_property(connector: *mut DrmConnector) {
    unsafe {
        let dev = (*connector).dev;
        let prop = (*dev).mode_config.hdr_output_metadata_property;
        drm_object_attach_property(core::ptr::addr_of_mut!((*connector).base), prop, 0);
    }
}

// upstream: drm_connector.c drm_connector_attach_broadcast_rgb_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_broadcast_rgb_property(connector: *mut DrmConnector) -> i32 {
    unsafe {
        let dev = (*connector).dev;

        let mut prop = (*connector).broadcast_rgb_property;
        if prop.is_null() {
            prop = drm_property_create_enum(
                dev,
                DRM_MODE_PROP_ENUM,
                b"Broadcast RGB\0".as_ptr().cast(),
                to_prop_enum_list(&BROADCAST_RGB_NAMES).as_ptr(),
                BROADCAST_RGB_NAMES.len() as i32,
            );
            if prop.is_null() {
                return -EINVAL;
            }
            (*connector).broadcast_rgb_property = prop;
        }

        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            prop,
            DRM_HDMI_BROADCAST_RGB_AUTO as u64,
        );
        0
    }
}

// upstream: drm_connector.c drm_connector_attach_colorspace_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_colorspace_property(connector: *mut DrmConnector) -> i32 {
    unsafe {
        let prop = (*connector).colorspace_property;
        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            prop,
            DRM_MODE_COLORIMETRY_DEFAULT as u64,
        );
        0
    }
}

// upstream: drm_connector.c drm_connector_atomic_hdr_metadata_equal()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_atomic_hdr_metadata_equal(
    old_state: *mut DrmConnectorState,
    new_state: *mut DrmConnectorState,
) -> bool {
    unsafe {
        let old_blob = (*old_state).hdr_output_metadata;
        let new_blob = (*new_state).hdr_output_metadata;

        if old_blob.is_null() || new_blob.is_null() {
            return old_blob == new_blob;
        }

        if (*old_blob).length != (*new_blob).length {
            return false;
        }

        let len = (*old_blob).length;
        core::slice::from_raw_parts((*old_blob).data.cast::<u8>(), len)
            == core::slice::from_raw_parts((*new_blob).data.cast::<u8>(), len)
    }
}

// upstream: drm_connector.c drm_connector_set_vrr_capable_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_set_vrr_capable_property(connector: *mut DrmConnector, capable: bool) {
    unsafe {
        if (*connector).vrr_capable_property.is_null() {
            return;
        }
        drm_object_property_set_value(
            core::ptr::addr_of_mut!((*connector).base),
            (*connector).vrr_capable_property,
            capable as u64,
        );
    }
}

// upstream: drm_connector.c drm_connector_set_panel_orientation()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_set_panel_orientation(
    connector: *mut DrmConnector,
    panel_orientation: DrmPanelOrientation,
) -> i32 {
    unsafe {
        let dev = (*connector).dev;
        let info = core::ptr::addr_of_mut!((*connector).display_info);

        if (*info).panel_orientation != DRM_MODE_PANEL_ORIENTATION_UNKNOWN {
            return 0;
        }

        if panel_orientation == DRM_MODE_PANEL_ORIENTATION_UNKNOWN {
            return 0;
        }

        (*info).panel_orientation = panel_orientation;

        let mut prop = (*dev).mode_config.panel_orientation_property;
        if prop.is_null() {
            prop = drm_property_create_enum(
                dev,
                DRM_MODE_PROP_IMMUTABLE,
                b"panel orientation\0".as_ptr().cast(),
                to_prop_enum_list(&DRM_PANEL_ORIENTATION_ENUM_LIST).as_ptr(),
                DRM_PANEL_ORIENTATION_ENUM_LIST.len() as i32,
            );
            if prop.is_null() {
                return -ENOMEM;
            }
            (*dev).mode_config.panel_orientation_property = prop;
        }

        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            prop,
            (*info).panel_orientation as u64,
        );
        0
    }
}

// upstream: drm_connector.c drm_connector_set_panel_orientation_with_quirk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_set_panel_orientation_with_quirk(
    connector: *mut DrmConnector,
    mut panel_orientation: DrmPanelOrientation,
    width: i32,
    height: i32,
) -> i32 {
    unsafe {
        let orientation_quirk = drm_get_panel_orientation_quirk(width, height);
        if orientation_quirk != DRM_MODE_PANEL_ORIENTATION_UNKNOWN {
            panel_orientation = orientation_quirk;
        }
        drm_connector_set_panel_orientation(connector, panel_orientation)
    }
}

// upstream: drm_connector.c drm_connector_set_orientation_from_panel()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_set_orientation_from_panel(
    connector: *mut DrmConnector,
    _panel: *mut c_void,
) -> i32 {
    // Deviation: struct drm_panel is not ported in this batch, so
    // panel->funcs->get_orientation() cannot be called; the orientation is
    // reported as UNKNOWN, which is what the C code returns for a NULL panel.
    unsafe { drm_connector_set_panel_orientation(connector, DRM_MODE_PANEL_ORIENTATION_UNKNOWN) }
}

/// `drm_connector_create_privacy_screen_properties()`: the sw/hw state enums.
// upstream: drm_connector.c privacy_screen_enum[]
static PRIVACY_SCREEN_ENUM: [EnumEntry; 4] = [
    (PRIVACY_SCREEN_DISABLED as i32, b"Disabled\0"),
    (PRIVACY_SCREEN_ENABLED as i32, b"Enabled\0"),
    (PRIVACY_SCREEN_DISABLED_LOCKED as i32, b"Disabled-locked\0"),
    (PRIVACY_SCREEN_ENABLED_LOCKED as i32, b"Enabled-locked\0"),
];

// upstream: drm_connector.c drm_connector_create_privacy_screen_properties()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_create_privacy_screen_properties(connector: *mut DrmConnector) {
    unsafe {
        if !(*connector).privacy_screen_sw_state_property.is_null() {
            return;
        }

        (*connector).privacy_screen_sw_state_property = drm_property_create_enum(
            (*connector).dev,
            DRM_MODE_PROP_ENUM,
            b"privacy-screen sw-state\0".as_ptr().cast(),
            to_prop_enum_list(&PRIVACY_SCREEN_ENUM).as_ptr(),
            2,
        );
        (*connector).privacy_screen_hw_state_property = drm_property_create_enum(
            (*connector).dev,
            DRM_MODE_PROP_IMMUTABLE | DRM_MODE_PROP_ENUM,
            b"privacy-screen hw-state\0".as_ptr().cast(),
            to_prop_enum_list(&PRIVACY_SCREEN_ENUM).as_ptr(),
            PRIVACY_SCREEN_ENUM.len() as i32,
        );
    }
}

// upstream: drm_connector.c drm_connector_attach_privacy_screen_properties()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_privacy_screen_properties(connector: *mut DrmConnector) {
    unsafe {
        if (*connector).privacy_screen_sw_state_property.is_null() {
            return;
        }
        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            (*connector).privacy_screen_sw_state_property,
            PRIVACY_SCREEN_DISABLED as u64,
        );
        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            (*connector).privacy_screen_hw_state_property,
            PRIVACY_SCREEN_DISABLED as u64,
        );
    }
}

// upstream: drm_connector.c drm_connector_update_privacy_screen_properties()
unsafe fn drm_connector_update_privacy_screen_properties(connector: *mut DrmConnector, set_sw_state: bool) {
    unsafe {
        let mut sw_state: DrmPrivacyScreenStatus = 0;
        let mut hw_state: DrmPrivacyScreenStatus = 0;
        drm_privacy_screen_get_state((*connector).privacy_screen, &mut sw_state, &mut hw_state);

        if set_sw_state {
            (*(*connector).state).privacy_screen_sw_state = sw_state;
        }
        drm_object_property_set_value(
            core::ptr::addr_of_mut!((*connector).base),
            (*connector).privacy_screen_hw_state_property,
            hw_state as u64,
        );
    }
}

// upstream: drm_connector.c drm_connector_privacy_screen_notifier()
unsafe extern "C" fn drm_connector_privacy_screen_notifier(
    nb: *mut tk_linux_kapi::gem_memory::NotifierBlock,
    _action: u64,
    _data: *mut c_void,
) -> i32 {
    unsafe {
        // container_of(nb, struct drm_connector, privacy_screen_notifier)
        let connector = nb
            .cast::<u8>()
            .sub(offset_of!(DrmConnector, privacy_screen_notifier))
            .cast::<DrmConnector>();
        let dev = (*connector).dev;

        drm_modeset_lock(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex), core::ptr::null_mut());
        drm_connector_update_privacy_screen_properties(connector, true);
        drm_modeset_unlock(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex));

        // drm_sysfs_connector_property_event() is not built on this target.

        NOTIFY_DONE
    }
}

/// `NOTIFY_DONE` (include/linux/notifier.h).
const NOTIFY_DONE: i32 = 0;

// upstream: drm_connector.c drm_connector_attach_privacy_screen_provider()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_privacy_screen_provider(
    connector: *mut DrmConnector,
    priv_: *mut c_void,
) {
    unsafe {
        (*connector).privacy_screen = priv_;
        (*connector).privacy_screen_notifier.notifier_call = Some(drm_connector_privacy_screen_notifier);

        drm_connector_create_privacy_screen_properties(connector);
        drm_connector_update_privacy_screen_properties(connector, true);
        drm_connector_attach_privacy_screen_properties(connector);
    }
}

// upstream: drm_connector.c drm_connector_update_privacy_screen()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_update_privacy_screen(connector_state: *const DrmConnectorState) {
    unsafe {
        let connector = (*connector_state).connector;

        if (*connector).privacy_screen.is_null() {
            return;
        }

        let ret = drm_privacy_screen_set_sw_state(
            (*connector).privacy_screen,
            (*connector_state).privacy_screen_sw_state,
        );
        if ret != 0 {
            drm_err!((*connector).dev, "Error updating privacy-screen sw_state\n");
            return;
        }

        drm_connector_update_privacy_screen_properties(connector, false);
    }
}

// upstream: drm_connector.c drm_connector_set_obj_prop()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_set_obj_prop(
    obj: *mut DrmModeObject,
    property: *mut DrmProperty,
    value: u64,
) -> i32 {
    unsafe {
        let mut ret = -EINVAL;
        let connector = obj_to_connector(obj);

        if property == (*(*connector).dev).mode_config.dpms_property {
            if let Some(dpms) = (*(*connector).funcs).dpms {
                ret = dpms(connector, value as i32);
            }
        } else if let Some(set_property) = (*(*connector).funcs).set_property {
            ret = set_property(connector, property, value);
        }

        if ret == 0 {
            drm_object_property_set_value(core::ptr::addr_of_mut!((*connector).base), property, value);
        }
        ret
    }
}

// upstream: drm_connector.c drm_connector_property_set_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_property_set_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    unsafe {
        let conn_set_prop = data.cast::<DrmModeConnectorSetProperty>();
        let mut obj_set_prop = DrmModeObjSetProperty {
            value: (*conn_set_prop).value,
            prop_id: (*conn_set_prop).prop_id,
            obj_id: (*conn_set_prop).connector_id,
            obj_type: DRM_MODE_OBJECT_CONNECTOR,
        };
        crate::drm_mode_object_upstream::drm_mode_obj_set_property_ioctl(
            dev,
            core::ptr::addr_of_mut!(obj_set_prop).cast(),
            file_priv,
        )
    }
}

// upstream: drm_connector.c drm_connector_get_encoder()
unsafe fn drm_connector_get_encoder(connector: *mut DrmConnector) -> *mut DrmEncoder {
    unsafe {
        if !(*connector).state.is_null() {
            return (*(*connector).state).best_encoder;
        }
        (*connector).encoder
    }
}

/// `DRM_MODE_FLAG_3D_MASK` (include/uapi/drm/drm_mode.h).
const DRM_MODE_FLAG_3D_MASK: u32 = 0x1f << 14;

/// `DRM_MODE_MATCH_*` bits (include/drm/drm_modes.h).
const DRM_MODE_MATCH_TIMINGS: u32 = 1 << 0;
const DRM_MODE_MATCH_CLOCK: u32 = 1 << 1;
const DRM_MODE_MATCH_FLAGS: u32 = 1 << 2;
const DRM_MODE_MATCH_3D_FLAGS: u32 = 1 << 3;

/// `drm_mode_is_stereo()` (include/drm/drm_modes.h).
// upstream: include/drm/drm_modes.h drm_mode_is_stereo()
unsafe fn drm_mode_is_stereo(mode: *const DrmDisplayMode) -> bool {
    unsafe { (*mode).flags & DRM_MODE_FLAG_3D_MASK != 0 }
}

/// `drm_connector_lookup()` (include/drm/drm_connector.h).
// upstream: include/drm/drm_connector.h drm_connector_lookup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_lookup(
    dev: *mut DrmDevice,
    file_priv: *mut DrmFile,
    id: u32,
) -> *mut DrmConnector {
    unsafe {
        let mo = drm_mode_object_find(dev, file_priv, id, DRM_MODE_OBJECT_CONNECTOR);
        if mo.is_null() {
            core::ptr::null_mut()
        } else {
            obj_to_connector(mo)
        }
    }
}

// upstream: drm_connector.c drm_mode_expose_to_userspace()
unsafe fn drm_mode_expose_to_userspace(
    mode: *const DrmDisplayMode,
    modes: *const ListHead,
    file_priv: *const DrmFile,
) -> bool {
    unsafe {
        if !(*file_priv).stereo_allowed && drm_mode_is_stereo(mode) {
            return false;
        }

        if !(*file_priv).aspect_ratio_allowed {
            let head = modes as *mut ListHead;
            let mut cur = (*head).next;
            while cur != head {
                let mode_itr = cur
                    .cast::<u8>()
                    .sub(offset_of!(DrmDisplayMode, head))
                    .cast::<DrmDisplayMode>();
                if (*mode_itr).expose_to_userspace
                    && drm_mode_match(
                        mode_itr,
                        mode,
                        DRM_MODE_MATCH_TIMINGS
                            | DRM_MODE_MATCH_CLOCK
                            | DRM_MODE_MATCH_FLAGS
                            | DRM_MODE_MATCH_3D_FLAGS,
                    )
                {
                    return false;
                }
                cur = (*cur).next;
            }
        }

        true
    }
}

// upstream: drm_connector.c drm_mode_getconnector()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_getconnector(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    unsafe {
        let out_resp = data.cast::<DrmModeGetConnector>();
        let mut mode_count: u32 = 0;
        let ret: i32;
        let mut u_mode: DrmModeModeinfo = core::mem::zeroed();

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        let connector = drm_connector_lookup(dev, file_priv, (*out_resp).connector_id);
        if connector.is_null() {
            return -ENOENT;
        }

        let encoders_count = (*connector).possible_encoders.count_ones();
        'out: {
            if (*out_resp).count_encoders >= encoders_count && encoders_count != 0 {
                let mut copied: u32 = 0;
                let encoder_ptr = (*out_resp).encoders_ptr as usize as *mut u32;
                // drm_connector_for_each_possible_encoder()
                let head = core::ptr::addr_of_mut!((*dev).mode_config.encoder_list);
                let mut cur = (*head).next;
                while cur != head {
                    let encoder = encoder_from_head(cur);
                    cur = (*cur).next;
                    if (*connector).possible_encoders & drm_encoder_mask(encoder) == 0 {
                        continue;
                    }
                    if put_user(encoder_ptr.add(copied as usize), (*encoder).base.id) != 0 {
                        ret = -EFAULT;
                        break 'out;
                    }
                    copied += 1;
                }
            }

            (*out_resp).count_encoders = encoders_count;
            (*out_resp).connector_id = (*connector).base.id;
            (*out_resp).connector_type = (*connector).connector_type as u32;
            (*out_resp).connector_type_id = (*connector).connector_type_id as u32;

            let is_current_master = drm_is_current_master(file_priv);

            mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.mutex));
            if (*out_resp).count_modes == 0 {
                if is_current_master {
                    if let Some(fill_modes) = (*(*connector).funcs).fill_modes {
                        fill_modes(
                            connector,
                            (*dev).mode_config.max_width,
                            (*dev).mode_config.max_height,
                        );
                    }
                } else {
                    drm_dbg_kms!(
                        dev,
                        "User-space requested a forced probe on [CONNECTOR:%d:%s] but is not the DRM master, demoting to read-only probe\n",
                        (*connector).base.id,
                        (*connector).name
                    );
                }
            }

            (*out_resp).mm_width = (*connector).display_info.width_mm;
            (*out_resp).mm_height = (*connector).display_info.height_mm;
            (*out_resp).subpixel = (*connector).display_info.subpixel_order;
            (*out_resp).connection = (*connector).status;

            let modes_head = core::ptr::addr_of_mut!((*connector).modes);
            let mut cur = (*modes_head).next;
            while cur != modes_head {
                let mode = cur
                    .cast::<u8>()
                    .sub(offset_of!(DrmDisplayMode, head))
                    .cast::<DrmDisplayMode>();
                warn_on((*mode).expose_to_userspace);
                if drm_mode_expose_to_userspace(mode, modes_head, file_priv) {
                    (*mode).expose_to_userspace = true;
                    mode_count += 1;
                }
                cur = (*cur).next;
            }

            if (*out_resp).count_modes >= mode_count && mode_count != 0 {
                let mut copied: u32 = 0;
                let mode_ptr = (*out_resp).modes_ptr as usize as *mut DrmModeModeinfo;
                let mut cur = (*modes_head).next;
                while cur != modes_head {
                    let mode = cur
                        .cast::<u8>()
                        .sub(offset_of!(DrmDisplayMode, head))
                        .cast::<DrmDisplayMode>();
                    cur = (*cur).next;
                    if !(*mode).expose_to_userspace {
                        continue;
                    }
                    (*mode).expose_to_userspace = false;
                    drm_mode_convert_to_umode(&mut u_mode, mode);
                    if !(*file_priv).aspect_ratio_allowed {
                        u_mode.flags &= !DRM_MODE_FLAG_PIC_AR_MASK;
                    }
                    if copy_to_user_c(
                        mode_ptr.add(copied as usize).cast(),
                        core::ptr::addr_of!(u_mode).cast(),
                        size_of::<DrmModeModeinfo>(),
                    ) != 0
                    {
                        ret = -EFAULT;
                        // list_for_each_entry_continue(): clear the rest.
                        let mut rest = cur;
                        while rest != modes_head {
                            let m = rest
                                .cast::<u8>()
                                .sub(offset_of!(DrmDisplayMode, head))
                                .cast::<DrmDisplayMode>();
                            (*m).expose_to_userspace = false;
                            rest = (*rest).next;
                        }
                        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.mutex));
                        break 'out;
                    }
                    copied += 1;
                }
            } else {
                let mut cur = (*modes_head).next;
                while cur != modes_head {
                    let mode = cur
                        .cast::<u8>()
                        .sub(offset_of!(DrmDisplayMode, head))
                        .cast::<DrmDisplayMode>();
                    (*mode).expose_to_userspace = false;
                    cur = (*cur).next;
                }
            }

            (*out_resp).count_modes = mode_count;
            mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.mutex));

            drm_modeset_lock(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex), core::ptr::null_mut());
            let encoder = drm_connector_get_encoder(connector);
            if !encoder.is_null() {
                (*out_resp).encoder_id = (*encoder).base.id;
            } else {
                (*out_resp).encoder_id = 0;
            }

            ret = drm_mode_object_get_properties(
                core::ptr::addr_of_mut!((*connector).base),
                (*file_priv).atomic,
                (*file_priv).plane_color_pipeline,
                (*out_resp).props_ptr as usize as *mut u32,
                (*out_resp).prop_values_ptr as usize as *mut u64,
                core::ptr::addr_of_mut!((*out_resp).count_props),
            );
            drm_modeset_unlock(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex));
        }

        drm_connector_put(connector);
        ret
    }
}

/// `drm_connector_get()` (include/drm/drm_connector.h).
// upstream: include/drm/drm_connector.h drm_connector_get()
pub(crate) unsafe fn drm_connector_get(connector: *mut DrmConnector) {
    unsafe { drm_mode_object_get(core::ptr::addr_of_mut!((*connector).base)) }
}

/// `drm_connector_put()` (include/drm/drm_connector.h).
// upstream: include/drm/drm_connector.h drm_connector_put()
pub(crate) unsafe fn drm_connector_put(connector: *mut DrmConnector) {
    unsafe { drm_mode_object_put(core::ptr::addr_of_mut!((*connector).base)) }
}

/// `drm_connector_find_by_fwnode()`.
///
/// Deviation: fwnode handles are not ported (`fwnode` is always NULL on this
/// target), so only the `connector->fwnode == fwnode` identity match is kept
/// and the `->secondary` fallback is omitted.
// upstream: drm_connector.c drm_connector_find_by_fwnode()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_find_by_fwnode(fwnode: *mut c_void) -> *mut DrmConnector {
    unsafe {
        if fwnode.is_null() {
            return ERR_PTR_C(-ENODEV);
        }

        let mut found: *mut DrmConnector = ERR_PTR_C(-ENODEV);
        connector_globals_ensure();
        mutex_lock(CONNECTOR_GLOBALS.list_lock.get().cast::<Mutex>());
        let head = CONNECTOR_GLOBALS.list.get().cast::<ListHead>();
        let mut cur = (*head).next;
        while cur != head {
            let connector = cur
                .cast::<u8>()
                .sub(offset_of!(DrmConnector, global_connector_list_entry))
                .cast::<DrmConnector>();
            if (*connector).fwnode == fwnode {
                drm_connector_get(connector);
                found = connector;
                break;
            }
            cur = (*cur).next;
        }
        mutex_unlock(CONNECTOR_GLOBALS.list_lock.get().cast::<Mutex>());
        found
    }
}

/// `ENODEV` error pointer helper for the connector lookup.
const ENODEV: i32 = 19;

// upstream: drm_connector.c drm_connector_oob_hotplug_event()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_oob_hotplug_event(
    connector_fwnode: *mut c_void,
    status: DrmConnectorStatus,
) {
    unsafe {
        let connector = drm_connector_find_by_fwnode(connector_fwnode);
        if IS_ERR_C(connector) {
            return;
        }

        if let Some(oob_hotplug_event) = (*(*connector).funcs).oob_hotplug_event {
            oob_hotplug_event(connector, status);
        }

        drm_connector_put(connector);
    }
}

/// `IS_ERR()` in pointer form.
fn IS_ERR_C<T>(ptr: *mut T) -> bool {
    tk_linux_kapi::config::IS_ERR(ptr)
}

/// `ERR_PTR()` in pointer form.
fn ERR_PTR_C<T>(error: i32) -> *mut T {
    tk_linux_kapi::config::ERR_PTR::<T>(error)
}

// upstream: drm_connector.c drm_tile_group_free()
unsafe extern "C" fn drm_tile_group_free(kref: *mut tk_linux_kapi::types::Kref) {
    unsafe {
        let tg = kref.cast::<DrmTileGroup>();
        let dev = (*tg).dev;

        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        idr_remove(core::ptr::addr_of_mut!((*dev).mode_config.tile_idr), (*tg).id);
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));

        kfree(tg);
    }
}

// upstream: drm_connector.c drm_mode_put_tile_group()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_put_tile_group(_dev: *mut DrmDevice, tg: *mut DrmTileGroup) {
    unsafe {
        tk_linux_kapi::memory::kref_put(core::ptr::addr_of_mut!((*tg).refcount), drm_tile_group_free);
    }
}

// upstream: drm_connector.c drm_mode_get_tile_group()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_get_tile_group(dev: *mut DrmDevice, topology_id: *const u8) -> *mut DrmTileGroup {
    unsafe {
        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        // idr_for_each_entry(&tile_idr, tg, id): entries in ascending id order.
        for (_id, entry) in tk_linux_kapi::xarray::xa_snapshot(&mut (*dev).mode_config.tile_idr.idr_rt) {
            let mut tg = entry.cast::<DrmTileGroup>();
            if core::slice::from_raw_parts((*tg).group_data.as_ptr(), 9) == core::slice::from_raw_parts(topology_id, 9) {
                if !tk_linux_kapi::memory::kref_get_unless_zero(&mut (*tg).refcount) {
                    tg = core::ptr::null_mut();
                }
                mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
                return tg;
            }
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        core::ptr::null_mut()
    }
}

// upstream: drm_connector.c drm_mode_create_tile_group()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_create_tile_group(dev: *mut DrmDevice, topology_id: *const u8) -> *mut DrmTileGroup {
    unsafe {
        let mut tg = kzalloc(size_of::<DrmTileGroup>(), GFP_KERNEL).cast::<DrmTileGroup>();
        if tg.is_null() {
            return core::ptr::null_mut();
        }

        tk_linux_kapi::memory::kref_init(core::ptr::addr_of_mut!((*tg).refcount));
        core::ptr::copy_nonoverlapping(topology_id, (*tg).group_data.as_mut_ptr(), 9);
        (*tg).dev = dev;

        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));
        let ret = idr_alloc(core::ptr::addr_of_mut!((*dev).mode_config.tile_idr), tg.cast(), 1, 0);
        if ret >= 0 {
            (*tg).id = ret;
        } else {
            kfree(tg);
            tg = core::ptr::null_mut();
        }
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.idr_mutex));

        tg
    }
}

// upstream: drm_connector.c drm_connector_attach_panel_type_property()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_connector_attach_panel_type_property(connector: *mut DrmConnector) {
    unsafe {
        let dev = (*connector).dev;
        let prop = (*dev).mode_config.panel_type_property;
        if prop.is_null() {
            return;
        }
        drm_object_attach_property(
            core::ptr::addr_of_mut!((*connector).base),
            prop,
            DRM_MODE_PANEL_TYPE_UNKNOWN as u64,
        );
    }
}

// upstream: drm_connector.c drm_get_dvi_i_select_name() and siblings (DRM_ENUM_NAME_FN)
#[unsafe(no_mangle)]
pub extern "C" fn drm_get_dvi_i_select_name(val: i32) -> *const c_char {
    enum_name_fn(&DRM_DVI_I_SELECT_ENUM_LIST, val)
}

#[unsafe(no_mangle)]
pub extern "C" fn drm_get_dvi_i_subconnector_name(val: i32) -> *const c_char {
    enum_name_fn(&DRM_DVI_I_SUBCONNECTOR_ENUM_LIST, val)
}

#[unsafe(no_mangle)]
pub extern "C" fn drm_get_tv_select_name(val: i32) -> *const c_char {
    enum_name_fn(&DRM_TV_SELECT_ENUM_LIST, val)
}

#[unsafe(no_mangle)]
pub extern "C" fn drm_get_tv_subconnector_name(val: i32) -> *const c_char {
    enum_name_fn(&DRM_TV_SUBCONNECTOR_ENUM_LIST, val)
}

#[unsafe(no_mangle)]
pub extern "C" fn drm_get_dp_subconnector_name(val: i32) -> *const c_char {
    enum_name_fn(&DRM_DP_SUBCONNECTOR_ENUM_LIST, val)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDrm;
    use crate::uapi_drm_consts::DRM_MODE_CONNECTOR_DisplayPort;

    unsafe extern "C" fn noop_destroy(_connector: *mut DrmConnector) {}

    fn funcs() -> DrmConnectorFuncs {
        DrmConnectorFuncs {
            dpms: None,
            reset: None,
            detect: None,
            force: None,
            fill_modes: None,
            set_property: None,
            late_register: None,
            early_unregister: None,
            destroy: Some(noop_destroy),
            atomic_duplicate_state: None,
            atomic_destroy_state: None,
            atomic_set_property: None,
            atomic_get_property: None,
            atomic_print_state: None,
            oob_hotplug_event: None,
            debugfs_init: None,
        }
    }

    fn name_of(connector: *mut DrmConnector) -> &'static str {
        unsafe { core::ffi::CStr::from_ptr((*connector).name).to_str().unwrap() }
    }

    #[test]
    fn connector_init_assigns_name_ids_and_stays_initializing() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        assert_eq!(t.init_mode_config(), 0);
        let dev = t.ptr();
        let f = funcs();
        let mut conn: DrmConnector = unsafe { core::mem::zeroed() };
        unsafe {
            let ret = drm_connector_init(dev, &mut conn, &f, DRM_MODE_CONNECTOR_DisplayPort as i32);
            assert_eq!(ret, 0);
            assert_eq!(name_of(&mut conn), "DP-1");
            assert_eq!(conn.connector_type_id, 1);
            assert_eq!(conn.index, 0);
            assert_eq!(conn.registration_state, DRM_CONNECTOR_INITIALIZING);
            assert_eq!((*dev).mode_config.num_connector, 1);
            assert_eq!(conn.status, CONNECTOR_STATUS_UNKNOWN);
            assert_eq!(conn.display_info.panel_orientation, DRM_MODE_PANEL_ORIENTATION_UNKNOWN);
            // The standard connector properties were attached.
            assert!(conn.properties.count >= 4);

            drm_connector_cleanup(&mut conn);
            assert_eq!((*dev).mode_config.num_connector, 0);
        }
        t.cleanup_mode_config();
    }

    #[test]
    fn same_type_connectors_get_increasing_type_ids() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        assert_eq!(t.init_mode_config(), 0);
        let dev = t.ptr();
        let f = funcs();
        let mut a: DrmConnector = unsafe { core::mem::zeroed() };
        let mut b: DrmConnector = unsafe { core::mem::zeroed() };
        unsafe {
            assert_eq!(drm_connector_init(dev, &mut a, &f, DRM_MODE_CONNECTOR_DisplayPort as i32), 0);
            assert_eq!(drm_connector_init(dev, &mut b, &f, DRM_MODE_CONNECTOR_DisplayPort as i32), 0);
            assert_eq!(name_of(&mut a), "DP-1");
            assert_eq!(name_of(&mut b), "DP-2");
            assert_eq!(b.index, 1);

            // The list iterator walks both connectors and then ends.
            let mut iter: DrmConnectorListIter = core::mem::zeroed();
            drm_connector_list_iter_begin(dev, &mut iter);
            let mut seen = 0;
            loop {
                let c = drm_connector_list_iter_next(&mut iter);
                if c.is_null() {
                    break;
                }
                // The iterator owns the reference it returned; no put here.
                seen += 1;
            }
            drm_connector_list_iter_end(&mut iter);
            assert_eq!(seen, 2);

            drm_connector_cleanup(&mut a);
            drm_connector_cleanup(&mut b);
        }
        t.cleanup_mode_config();
    }

    #[test]
    fn register_state_machine_follows_device_registration() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        assert_eq!(t.init_mode_config(), 0);
        let dev = t.ptr();
        let f = funcs();
        let mut conn: DrmConnector = unsafe { core::mem::zeroed() };
        unsafe {
            assert_eq!(drm_connector_init(dev, &mut conn, &f, DRM_MODE_CONNECTOR_DisplayPort as i32), 0);

            // Device not registered yet: the connector stays INITIALIZING.
            assert_eq!(drm_connector_register(&mut conn), 0);
            assert_eq!(conn.registration_state, DRM_CONNECTOR_INITIALIZING);

            (*dev).registered = true;
            assert_eq!(drm_connector_register(&mut conn), 0);
            assert_eq!(conn.registration_state, DRM_CONNECTOR_REGISTERED);

            drm_connector_unregister(&mut conn);
            assert_eq!(conn.registration_state, DRM_CONNECTOR_UNREGISTERED);
            // Unregistering twice is a no-op.
            drm_connector_unregister(&mut conn);
            assert_eq!(conn.registration_state, DRM_CONNECTOR_UNREGISTERED);

            (*dev).registered = false;
            drm_connector_cleanup(&mut conn);
        }
        t.cleanup_mode_config();
    }

    #[test]
    fn subpixel_and_status_names_match_the_c_tables() {
        unsafe {
            let name = |p: *const c_char| core::ffi::CStr::from_ptr(p).to_str().unwrap();
            assert_eq!(name(drm_get_connector_status_name(CONNECTOR_STATUS_CONNECTED)), "connected");
            assert_eq!(name(drm_get_connector_status_name(CONNECTOR_STATUS_UNKNOWN)), "unknown");
            assert_eq!(name(drm_get_connector_force_name(DRM_FORCE_ON_DIGITAL)), "digital");
            assert_eq!(name(drm_get_subpixel_order_name(SUBPIXELNONE)), "None");
            assert_eq!(name(drm_get_colorspace_name(DRM_MODE_COLORIMETRY_BT601_YCC)), "BT601_YCC");
            assert_eq!(name(drm_get_connector_type_name(DRM_MODE_CONNECTOR_DisplayPort)), "DP");
        }
    }
}
