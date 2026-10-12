// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_encoder.c` (batch B1a).
//!
//! Encoder registration, `drm_encoder_init()` / `drm_encoder_cleanup()`,
//! the managed `drmm_encoder_init()` / `__drmm_encoder_alloc()` pair, and the
//! `GETENCODER` ioctl.
//!
//! Deviation: the C `...` format arguments of `drm_encoder_init()`,
//! `__drmm_encoder_alloc()` and `drmm_encoder_init()` are not expanded here
//! (stable Rust has no C variadics in this `no_std` crate). The `name`
//! parameter is taken as an already-formatted C string, duplicated verbatim.
//! The default name (`name == NULL`) is still built as `"%s-%d"`.
//!
//! Deviation: the debugfs hooks (`drm_debugfs_encoder_add/remove`) are
//! omitted; they are dead with `CONFIG_DEBUG_FS` off on this target.

#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use crate::drm_mode_object_upstream::{mutex_lock, mutex_unlock};

use core::ffi::{c_char, c_void};
use core::mem::{offset_of, size_of};

use tk_linux_kapi::{
    assert::warn_on,
    config::{EINVAL, ENOENT, ENOMEM, EOPNOTSUPP, GFP_KERNEL},
    list::{INIT_LIST_HEAD, list_add_tail, list_del, list_splice_init},
    memory::{kfree, kmalloc},
    mutex::{mutex_destroy, mutex_init},
    types::ListHead,
};

use crate::{
    drm_connector_upstream::{
        drm_connector_list_iter_begin, drm_connector_list_iter_end,
        drm_connector_list_iter_next,
    },
    drm_connector_types::{DrmConnector, DrmConnectorListIter},
    drm_crtc_types::DrmCrtc,
    drm_device_types::DrmDevice,
    drm_encoder_types::{DrmEncoder, DrmEncoderFuncs},
    drm_file_types::DrmFile,
    drm_mode_object_types::DrmModeObject,
    drm_modeset_lock_types::{DrmModesetAcquireCtx, DrmModesetLock},
    drm_mode_object_upstream::{
        drm_core_check_feature, drm_mode_object_add, drm_mode_object_find,
        drm_mode_object_unregister, DRIVER_MODESET,
    },
    uapi_drm_consts::DRM_MODE_OBJECT_ENCODER,
    uapi_drm_mode_types::DrmModeGetEncoder,
};

// Cross-batch callees: managed resources (drm_managed.c), modeset lock
// (drm_modeset_lock.c), lease helpers (drm_lease.c), and bridge detach
// (drm_bridge.c).
unsafe extern "C" {
    fn drmm_kmalloc(dev: *mut DrmDevice, size: usize, gfp: u32) -> *mut c_void;
    fn __drmm_add_action_or_reset(
        dev: *mut DrmDevice,
        action: unsafe extern "C" fn(*mut DrmDevice, *mut c_void),
        data: *mut c_void,
        name: *const c_char,
    ) -> i32;
    fn drm_modeset_lock(lock: *mut DrmModesetLock, ctx: *mut DrmModesetAcquireCtx) -> i32;
    fn drm_modeset_unlock(lock: *mut DrmModesetLock);
    fn drm_lease_held(file_priv: *mut DrmFile, id: i32) -> bool;
    fn drm_lease_filter_crtcs(file_priv: *mut DrmFile, crtcs: u32) -> u32;
    fn drm_bridge_detach(bridge: *mut c_void);
}

/// Linux `DRM_MODE_OBJECT_*` string names for the default encoder name.
// upstream: drm_encoder.c drm_encoder_enum_list[]
static DRM_ENCODER_ENUM_NAMES: [&[u8]; 9] = [
    b"None", b"DAC", b"TMDS", b"LVDS", b"TV", b"Virtual", b"DSI", b"DP MST", b"DPI",
];

/// `__drmm_add_action_or_reset()` through the `drmm_add_action_or_reset()`
/// macro (drm_managed.h), which passes the action name as a string.
unsafe fn drmm_add_action_or_reset(
    dev: *mut DrmDevice,
    action: unsafe extern "C" fn(*mut DrmDevice, *mut c_void),
    data: *mut c_void,
) -> i32 {
    __drmm_add_action_or_reset(dev, action, data, b"drmm_encoder_alloc_release\0".as_ptr().cast())
}

/// `drm_encoder_find()` (drm_encoder.h): object lookup for an encoder id.
// upstream: include/drm/drm_encoder.h drm_encoder_find()
pub(crate) unsafe fn drm_encoder_find(
    dev: *mut DrmDevice,
    file_priv: *mut DrmFile,
    id: u32,
) -> *mut DrmEncoder {
    let obj = drm_mode_object_find(dev, file_priv, id, DRM_MODE_OBJECT_ENCODER);
    obj_to_encoder(obj)
}

/// `drm_encoder_mask()`: one bit per encoder index (drm_encoder.h).
// upstream: include/drm/drm_encoder.h drm_encoder_mask()
pub(crate) unsafe fn drm_encoder_mask(encoder: *const DrmEncoder) -> u32 {
    1u32 << (*encoder).index
}

/// `obj_to_encoder()` (drm_encoder.h); NULL-safe like the C container_of
/// helper's callers.
unsafe fn obj_to_encoder(obj: *mut DrmModeObject) -> *mut DrmEncoder {
    if obj.is_null() {
        return core::ptr::null_mut();
    }
    obj.cast::<u8>()
        .sub(offset_of!(DrmEncoder, base))
        .cast::<DrmEncoder>()
}

/// `list_entry(link, struct drm_encoder, head)`.
unsafe fn encoder_from_head(link: *mut ListHead) -> *mut DrmEncoder {
    link.cast::<u8>()
        .sub(offset_of!(DrmEncoder, head))
        .cast::<DrmEncoder>()
}

/// `kstrdup()`: a kmalloc'd NUL-terminated copy of `src`.
// upstream: mm/util.c kstrdup()
pub(crate) unsafe fn kstrdup_c(src: *const c_char) -> *mut c_char {
    let mut len = 0usize;
    while *src.add(len) != 0 {
        len += 1;
    }
    let dst = kmalloc(len + 1, GFP_KERNEL).cast::<c_char>();
    if dst.is_null() {
        return dst;
    }
    core::ptr::copy_nonoverlapping(src, dst, len);
    *dst.add(len) = 0;
    dst
}

/// `kasprintf(GFP_KERNEL, "%s-%d", prefix, id)`.
// upstream: lib/kasprintf.c kasprintf()
pub(crate) unsafe fn kasprintf_name_id(prefix: &[u8], id: u32) -> *mut c_char {
    // Decimal digits of `id`, most significant first.
    let mut digits = [0u8; 10];
    let mut n = id;
    let mut count = 0usize;
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let total = prefix.len() + 1 + count;
    let dst = kmalloc(total + 1, GFP_KERNEL).cast::<u8>();
    if dst.is_null() {
        return dst.cast();
    }
    let mut at = 0usize;
    for &b in prefix {
        *dst.add(at) = b;
        at += 1;
    }
    *dst.add(at) = b'-';
    at += 1;
    for i in (0..count).rev() {
        *dst.add(at) = digits[i];
        at += 1;
    }
    *dst.add(at) = 0;
    dst.cast()
}

// upstream: drm_encoder.c drm_encoder_register_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_encoder_register_all(dev: *mut DrmDevice) -> i32 {
    unsafe {
        let head = core::ptr::addr_of_mut!((*dev).mode_config.encoder_list);
        let mut cur = (*head).next;
        let mut ret = 0;
        while cur != head {
            let encoder = encoder_from_head(cur);
            if !(*encoder).funcs.is_null() {
                if let Some(late_register) = (*(*encoder).funcs).late_register {
                    ret = late_register(encoder);
                }
            }
            if ret != 0 {
                return ret;
            }
            cur = (*cur).next;
        }
        0
    }
}

// upstream: drm_encoder.c drm_encoder_unregister_all()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_encoder_unregister_all(dev: *mut DrmDevice) {
    unsafe {
        let head = core::ptr::addr_of_mut!((*dev).mode_config.encoder_list);
        let mut cur = (*head).next;
        while cur != head {
            let encoder = encoder_from_head(cur);
            if !(*encoder).funcs.is_null() {
                if let Some(early_unregister) = (*(*encoder).funcs).early_unregister {
                    early_unregister(encoder);
                }
            }
            cur = (*cur).next;
        }
    }
}

// upstream: drm_encoder.c __drm_encoder_init()
unsafe fn __drm_encoder_init(
    dev: *mut DrmDevice,
    encoder: *mut DrmEncoder,
    funcs: *const DrmEncoderFuncs,
    encoder_type: i32,
    name: *const c_char,
) -> i32 {
    unsafe {
        let mut ret: i32;

        // encoder index is used with 32bit bitmasks
        if warn_on((*dev).mode_config.num_encoder >= 32) {
            return -EINVAL;
        }

        ret = drm_mode_object_add(
            dev,
            core::ptr::addr_of_mut!((*encoder).base),
            DRM_MODE_OBJECT_ENCODER,
        );
        if ret != 0 {
            return ret;
        }

        (*encoder).dev = dev;
        (*encoder).encoder_type = encoder_type;
        (*encoder).funcs = funcs.cast_mut();

        if !name.is_null() {
            (*encoder).name = kstrdup_c(name);
        } else {
            let prefix = DRM_ENCODER_ENUM_NAMES[encoder_type as usize];
            (*encoder).name = kasprintf_name_id(prefix, (*encoder).base.id);
        }

        if (*encoder).name.is_null() {
            ret = -ENOMEM;
            // out_put:
            drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*encoder).base));
            return ret;
        }

        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*encoder).bridge_chain));
        mutex_init(core::ptr::addr_of_mut!((*encoder).bridge_chain_mutex));
        list_add_tail(
            core::ptr::addr_of_mut!((*encoder).head),
            core::ptr::addr_of_mut!((*dev).mode_config.encoder_list),
        );
        (*encoder).index = (*dev).mode_config.num_encoder as u32;
        (*dev).mode_config.num_encoder += 1;

        0
    }
}

// upstream: drm_encoder.c drm_encoder_init()
//
// The C variadic `name` format is not expanded; see the module comment.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_encoder_init(
    dev: *mut DrmDevice,
    encoder: *mut DrmEncoder,
    funcs: *const DrmEncoderFuncs,
    encoder_type: i32,
    name: *const c_char,
) -> i32 {
    unsafe {
        warn_on((*funcs).destroy.is_none());
        __drm_encoder_init(dev, encoder, funcs, encoder_type, name)
    }
}

// upstream: drm_encoder.c drm_encoder_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_encoder_cleanup(encoder: *mut DrmEncoder) {
    unsafe {
        let dev = (*encoder).dev;

        // Note that the encoder_list is considered to be static; should we
        // remove the drm_encoder at runtime we would have to decrement all
        // the indices on the drm_encoder after us in the encoder_list.

        let mut tmplist = core::mem::MaybeUninit::<ListHead>::uninit();
        let tmplist = tmplist.as_mut_ptr();
        INIT_LIST_HEAD(tmplist);

        mutex_lock(core::ptr::addr_of_mut!((*encoder).bridge_chain_mutex));
        list_splice_init(core::ptr::addr_of_mut!((*encoder).bridge_chain), tmplist);
        mutex_unlock(core::ptr::addr_of_mut!((*encoder).bridge_chain_mutex));

        // list_for_each_entry_safe(bridge, next, &tmplist, chain_node)
        //         drm_bridge_detach(bridge);
        let mut cur = (*tmplist).next;
        while cur != tmplist {
            let next = (*cur).next;
            drm_bridge_detach(cur.cast());
            cur = next;
        }

        drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*encoder).base));

        kfree((*encoder).name);
        list_del(core::ptr::addr_of_mut!((*encoder).head));
        (*dev).mode_config.num_encoder -= 1;

        mutex_destroy(&mut (*encoder).bridge_chain_mutex);

        core::ptr::write_bytes(encoder.cast::<u8>(), 0, size_of::<DrmEncoder>());
    }
}

// upstream: drm_encoder.c drmm_encoder_alloc_release()
unsafe extern "C" fn drmm_encoder_alloc_release(_dev: *mut DrmDevice, ptr: *mut c_void) {
    unsafe {
        let encoder = ptr.cast::<DrmEncoder>();
        if warn_on((*encoder).dev.is_null()) {
            return;
        }
        drm_encoder_cleanup(encoder);
    }
}

// upstream: drm_encoder.c __drmm_encoder_init()
unsafe fn __drmm_encoder_init(
    dev: *mut DrmDevice,
    encoder: *mut DrmEncoder,
    funcs: *const DrmEncoderFuncs,
    encoder_type: i32,
    name: *const c_char,
) -> i32 {
    unsafe {
        if !funcs.is_null() && (*funcs).destroy.is_some() {
            // drm_WARN_ON(dev, funcs && funcs->destroy)
            warn_on(true);
            return -EINVAL;
        }

        let ret = __drm_encoder_init(dev, encoder, funcs, encoder_type, name);
        if ret != 0 {
            return ret;
        }

        let ret = drmm_add_action_or_reset(dev, drmm_encoder_alloc_release, encoder.cast());
        if ret != 0 {
            return ret;
        }

        0
    }
}

// upstream: drm_encoder.c __drmm_encoder_alloc()
//
// The C variadic `name` format is not expanded; see the module comment.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __drmm_encoder_alloc(
    dev: *mut DrmDevice,
    size: usize,
    offset: usize,
    funcs: *const DrmEncoderFuncs,
    encoder_type: i32,
    name: *const c_char,
) -> *mut c_void {
    unsafe {
        let container = drmm_kmalloc(dev, size, GFP_KERNEL | 0x100 /* __GFP_ZERO */);
        if container.is_null() {
            return tk_linux_kapi::config::ERR_PTR::<c_void>(-ENOMEM);
        }

        let encoder = container.cast::<u8>().add(offset).cast::<DrmEncoder>();

        let ret = __drmm_encoder_init(dev, encoder, funcs, encoder_type, name);
        if ret != 0 {
            return tk_linux_kapi::config::ERR_PTR::<c_void>(ret);
        }

        container
    }
}

// upstream: drm_encoder.c drmm_encoder_init()
//
// The C variadic `name` format is not expanded; see the module comment.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drmm_encoder_init(
    dev: *mut DrmDevice,
    encoder: *mut DrmEncoder,
    funcs: *const DrmEncoderFuncs,
    encoder_type: i32,
    name: *const c_char,
) -> i32 {
    unsafe { __drmm_encoder_init(dev, encoder, funcs, encoder_type, name) }
}

// upstream: drm_encoder.c drm_encoder_get_crtc()
unsafe fn drm_encoder_get_crtc(encoder: *mut DrmEncoder) -> *mut DrmCrtc {
    unsafe {
        let dev = (*encoder).dev;
        let mut uses_atomic = false;
        let mut conn_iter = core::mem::MaybeUninit::<DrmConnectorListIter>::zeroed();
        let conn_iter = conn_iter.as_mut_ptr();

        drm_connector_list_iter_begin(dev, conn_iter);
        loop {
            let connector: *mut DrmConnector = drm_connector_list_iter_next(conn_iter);
            if connector.is_null() {
                break;
            }
            if (*connector).state.is_null() {
                continue;
            }

            uses_atomic = true;
            if (*(*connector).state).best_encoder != encoder {
                continue;
            }

            drm_connector_list_iter_end(conn_iter);
            return (*(*connector).state).crtc;
        }
        drm_connector_list_iter_end(conn_iter);

        // Don't return stale data (e.g. pending async disable).
        if uses_atomic {
            return core::ptr::null_mut();
        }
        (*encoder).crtc
    }
}

// upstream: drm_encoder.c drm_mode_getencoder()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_getencoder(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    unsafe {
        let enc_resp = data.cast::<DrmModeGetEncoder>();

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        let encoder = drm_encoder_find(dev, file_priv, (*enc_resp).encoder_id);
        if encoder.is_null() {
            return -ENOENT;
        }

        drm_modeset_lock(
            core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex),
            core::ptr::null_mut(),
        );
        let crtc = drm_encoder_get_crtc(encoder);
        if !crtc.is_null() && drm_lease_held(file_priv, (*crtc).base.id as i32) {
            (*enc_resp).crtc_id = (*crtc).base.id;
        } else {
            (*enc_resp).crtc_id = 0;
        }
        drm_modeset_unlock(core::ptr::addr_of_mut!((*dev).mode_config.connection_mutex));

        (*enc_resp).encoder_type = (*encoder).encoder_type as u32;
        (*enc_resp).encoder_id = (*encoder).base.id;
        (*enc_resp).possible_crtcs = drm_lease_filter_crtcs(file_priv, (*encoder).possible_crtcs);
        (*enc_resp).possible_clones = (*encoder).possible_clones;

        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDrm;
    use crate::uapi_drm_consts::DRM_MODE_ENCODER_DAC;

    unsafe extern "C" fn noop_destroy(_encoder: *mut DrmEncoder) {}

    fn funcs() -> DrmEncoderFuncs {
        DrmEncoderFuncs {
            reset: None,
            destroy: Some(noop_destroy),
            late_register: None,
            early_unregister: None,
            debugfs_init: None,
        }
    }

    #[test]
    fn encoder_init_names_indexes_and_cleanup() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        assert_eq!(t.init_mode_config(), 0);
        let dev = t.ptr();
        let f = funcs();
        let mut enc: DrmEncoder = unsafe { core::mem::zeroed() };
        unsafe {
            assert_eq!(drm_encoder_init(dev, &mut enc, &f, DRM_MODE_ENCODER_DAC as i32, core::ptr::null()), 0);
            assert_eq!(enc.index, 0);
            assert_eq!((*dev).mode_config.num_encoder, 1);
            assert_eq!(drm_encoder_mask(&enc), 1);
            // Default name: "<type>-<object id>".
            let name = core::ffi::CStr::from_ptr(enc.name).to_str().unwrap();
            assert!(name.starts_with("DAC-"), "{name}");

            drm_encoder_cleanup(&mut enc);
            assert_eq!((*dev).mode_config.num_encoder, 0);
            assert!(tk_linux_kapi::list::list_empty(&(*dev).mode_config.encoder_list));
        }
        t.cleanup_mode_config();
    }

    #[test]
    fn second_encoder_gets_the_next_index_and_mask() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        assert_eq!(t.init_mode_config(), 0);
        let dev = t.ptr();
        let f = funcs();
        let mut a: DrmEncoder = unsafe { core::mem::zeroed() };
        let mut b: DrmEncoder = unsafe { core::mem::zeroed() };
        unsafe {
            assert_eq!(drm_encoder_init(dev, &mut a, &f, DRM_MODE_ENCODER_DAC as i32, b"custom\0".as_ptr().cast()), 0);
            assert_eq!(drm_encoder_init(dev, &mut b, &f, DRM_MODE_ENCODER_DAC as i32, core::ptr::null()), 0);
            assert_eq!(b.index, 1);
            assert_eq!(drm_encoder_mask(&b), 2);
            assert_eq!(core::ffi::CStr::from_ptr(a.name).to_bytes(), b"custom");
            drm_encoder_cleanup(&mut a);
            drm_encoder_cleanup(&mut b);
        }
        t.cleanup_mode_config();
    }
}
