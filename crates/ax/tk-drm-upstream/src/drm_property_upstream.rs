// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_property.c` (batch B1a).
//!
//! Property creation (plain, enum, bitmask, range, object, bool), enum value
//! registration, property ioctl (`GETPROPERTY`), refcounted property blobs
//! (create, lookup, replace, get/put, `GETBLOB`/`CREATEBLOB`/`DESTROYBLOB`),
//! and the atomic-free `drm_property_change_valid_get()` / `_put()` pair.

#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use crate::drm_mode_object_upstream::{mutex_lock, mutex_unlock};

use core::ffi::{c_char, c_void};
use core::mem::{offset_of, size_of};

use tk_linux_kapi::{
    assert::warn_on,
    config::{EFAULT, EINVAL, ENOENT, ENOMEM, EOPNOTSUPP, EPERM, GFP_KERNEL},
    list::{INIT_LIST_HEAD, list_add_tail, list_del, list_del_init},
    memory::{kfree, kzalloc, kvfree},
    mm_native::{copy_from_user_c, copy_to_user_c},
    types::{Kref, ListHead},
};

use crate::{
    drm_device_types::DrmDevice,
    drm_file_types::DrmFile,
    drm_mode_object_types::DrmModeObject,
    drm_mode_object_upstream::{
        __drm_mode_object_add, __drm_mode_object_find, drm_core_check_feature,
        drm_mode_object_add, drm_mode_object_get, drm_mode_object_put,
        drm_mode_object_unregister, drm_mode_object_find, drm_object_property_set_value,
        DRIVER_MODESET,
    },
    drm_property_types::{
        DrmProperty, DrmPropertyBlob, DrmPropertyEnum, DrmPropEnumList,
    },
    uapi_drm_consts::{
        DRM_MODE_OBJECT_BLOB, DRM_MODE_OBJECT_PROPERTY, DRM_MODE_PROP_ATOMIC,
        DRM_MODE_PROP_BITMASK, DRM_MODE_PROP_BLOB, DRM_MODE_PROP_ENUM,
        DRM_MODE_PROP_EXTENDED_TYPE, DRM_MODE_PROP_IMMUTABLE, DRM_MODE_PROP_LEGACY_TYPE,
        DRM_MODE_PROP_OBJECT, DRM_MODE_PROP_RANGE, DRM_MODE_PROP_SIGNED_RANGE,
    },
    uapi_drm_mode_types::{
        DrmModeCreateBlob, DrmModeDestroyBlob, DrmModeGetBlob, DrmModeGetProperty,
        DrmModePropertyEnum,
    },
};

/// `DRM_PROP_NAME_LEN` (include/drm/drm_property.h).
pub const DRM_PROP_NAME_LEN: usize = 32;

/// Size of `struct drm_mode_property_enum` in user space (uapi/drm/drm_mode.h).
const USER_PROPERTY_ENUM_SIZE: usize = size_of::<DrmModePropertyEnum>();

// upstream: include/drm/drm_property.h drm_property_type_is()
pub(crate) unsafe fn drm_property_type_is(property: *mut DrmProperty, r#type: u32) -> bool {
    let flags = (*property).flags;
    // instanceof for props.. handles extended type vs original types:
    if flags & DRM_MODE_PROP_EXTENDED_TYPE != 0 {
        return (flags & DRM_MODE_PROP_EXTENDED_TYPE) == r#type;
    }
    flags & r#type != 0
}

// upstream: include/drm/drm_property.h obj_to_property()
pub(crate) unsafe fn obj_to_property(obj: *mut DrmModeObject) -> *mut DrmProperty {
    obj.cast::<u8>()
        .sub(offset_of!(DrmProperty, base))
        .cast::<DrmProperty>()
}

// upstream: include/drm/drm_property.h obj_to_blob()
pub(crate) unsafe fn obj_to_blob(obj: *mut DrmModeObject) -> *mut DrmPropertyBlob {
    obj.cast::<u8>()
        .sub(offset_of!(DrmPropertyBlob, base))
        .cast::<DrmPropertyBlob>()
}

// upstream: include/drm/drm_property.h drm_property_find()
pub(crate) unsafe fn drm_property_find(
    dev: *mut DrmDevice,
    file_priv: *mut DrmFile,
    id: u32,
) -> *mut DrmProperty {
    let mo = drm_mode_object_find(dev, file_priv, id, DRM_MODE_OBJECT_PROPERTY);
    if mo.is_null() {
        core::ptr::null_mut()
    } else {
        obj_to_property(mo)
    }
}

/// Logs a DRM atomic debug message (`drm_dbg_atomic()`), via the kapi logger.
macro_rules! drm_dbg_atomic {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {
        tk_linux_kapi::drm_dbg!($device, $format $(, $argument)*)
    };
}

/// `strlen()` on a NUL-terminated C string.
unsafe fn c_strlen(s: *const c_char) -> usize {
    let mut len = 0usize;
    while *s.add(len) != 0 {
        len += 1;
    }
    len
}

/// `strscpy_pad()`: copies at most `dst.len() - 1` bytes and zero-fills the rest.
unsafe fn strscpy_pad(dst: *mut c_char, src: *const c_char, size: usize) {
    let mut i = 0usize;
    while i + 1 < size {
        let c = *src.add(i);
        *dst.add(i) = c;
        if c == 0 {
            break;
        }
        i += 1;
    }
    while i < size {
        *dst.add(i) = 0;
        i += 1;
    }
}

/// `put_user()` for a single scalar: returns 0 or -EFAULT.
// upstream: include/linux/uaccess.h put_user()
unsafe fn put_user<T: Copy>(dst: *mut T, value: T) -> i32 {
    let rest = copy_to_user_c(
        dst.cast::<c_void>(),
        core::ptr::addr_of!(value).cast::<c_void>(),
        size_of::<T>(),
    );
    if rest == 0 {
        0
    } else {
        -EFAULT
    }
}

/// `container_of(kref, struct drm_property_blob, base.refcount)`.
unsafe fn blob_from_kref(kref: *mut Kref) -> *mut DrmPropertyBlob {
    kref.cast::<u8>()
        .sub(offset_of!(DrmModeObject, refcount))
        .sub(offset_of!(DrmPropertyBlob, base))
        .cast::<DrmPropertyBlob>()
}

// upstream: drm_property.c drm_property_flags_valid()
fn drm_property_flags_valid(flags: u32) -> bool {
    let legacy_type = flags & DRM_MODE_PROP_LEGACY_TYPE;
    let ext_type = flags & DRM_MODE_PROP_EXTENDED_TYPE;

    // Reject undefined/deprecated flags
    if flags
        & !(DRM_MODE_PROP_LEGACY_TYPE
            | DRM_MODE_PROP_EXTENDED_TYPE
            | DRM_MODE_PROP_IMMUTABLE
            | DRM_MODE_PROP_ATOMIC)
        != 0
    {
        return false;
    }

    // We want either a legacy type or an extended type, but not both
    if legacy_type == 0 && ext_type == 0 || legacy_type != 0 && ext_type != 0 {
        return false;
    }

    // Only one legacy type at a time please
    if legacy_type != 0 && !legacy_type.is_power_of_two() {
        return false;
    }

    true
}

// upstream: drm_property.c drm_property_create()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_create(
    dev: *mut DrmDevice,
    flags: u32,
    name: *const c_char,
    num_values: i32,
) -> *mut DrmProperty {
    unsafe {
        if warn_on(!drm_property_flags_valid(flags)) {
            return core::ptr::null_mut();
        }
        if warn_on(c_strlen(name) >= DRM_PROP_NAME_LEN) {
            return core::ptr::null_mut();
        }

        let property = tk_linux_kapi::memory::kzalloc_obj::<DrmProperty>();
        if property.is_null() {
            return core::ptr::null_mut();
        }

        (*property).dev = dev;

        if num_values != 0 {
            (*property).values = kzalloc(
                num_values as usize * size_of::<u64>(),
                GFP_KERNEL,
            )
            .cast::<u64>();
            if (*property).values.is_null() {
                return fail_property(property);
            }
        }

        let ret = drm_mode_object_add_property(dev, property);
        if ret != 0 {
            return fail_property(property);
        }

        (*property).flags = flags;
        (*property).num_values = num_values as u32;
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*property).enum_list));

        strscpy_pad(
            (*property).name.as_mut_ptr(),
            name,
            DRM_PROP_NAME_LEN,
        );

        list_add_tail(
            core::ptr::addr_of_mut!((*property).head),
            core::ptr::addr_of_mut!((*dev).mode_config.property_list),
        );

        property
    }
}

/// `fail:` label of `drm_property_create()`.
unsafe fn fail_property(property: *mut DrmProperty) -> *mut DrmProperty {
    unsafe {
        kfree((*property).values);
        kfree(property);
        core::ptr::null_mut()
    }
}

/// `drm_mode_object_add(dev, &property->base, DRM_MODE_OBJECT_PROPERTY)`.
unsafe fn drm_mode_object_add_property(dev: *mut DrmDevice, property: *mut DrmProperty) -> i32 {
    unsafe {
        drm_mode_object_add(
            dev,
            core::ptr::addr_of_mut!((*property).base),
            DRM_MODE_OBJECT_PROPERTY,
        )
    }
}

// upstream: drm_property.c drm_property_create_enum()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_create_enum(
    dev: *mut DrmDevice,
    mut flags: u32,
    name: *const c_char,
    props: *const DrmPropEnumList,
    num_values: i32,
) -> *mut DrmProperty {
    unsafe {
        flags |= DRM_MODE_PROP_ENUM;

        let property = drm_property_create(dev, flags, name, num_values);
        if property.is_null() {
            return core::ptr::null_mut();
        }

        for i in 0..num_values as usize {
            let ret = drm_property_add_enum(
                property,
                (*props.add(i)).r#type as u64,
                (*props.add(i)).name,
            );
            if ret != 0 {
                drm_property_destroy(dev, property);
                return core::ptr::null_mut();
            }
        }

        property
    }
}

// upstream: drm_property.c drm_property_create_bitmask()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_create_bitmask(
    dev: *mut DrmDevice,
    mut flags: u32,
    name: *const c_char,
    props: *const DrmPropEnumList,
    num_props: i32,
    supported_bits: u64,
) -> *mut DrmProperty {
    unsafe {
        let num_values = supported_bits.count_ones() as i32;

        flags |= DRM_MODE_PROP_BITMASK;

        let property = drm_property_create(dev, flags, name, num_values);
        if property.is_null() {
            return core::ptr::null_mut();
        }

        for i in 0..num_props as usize {
            if supported_bits & (1u64 << (*props.add(i)).r#type) == 0 {
                continue;
            }

            let ret = drm_property_add_enum(
                property,
                (*props.add(i)).r#type as u64,
                (*props.add(i)).name,
            );
            if ret != 0 {
                drm_property_destroy(dev, property);
                return core::ptr::null_mut();
            }
        }

        property
    }
}

// upstream: drm_property.c property_create_range()
unsafe fn property_create_range(
    dev: *mut DrmDevice,
    flags: u32,
    name: *const c_char,
    min: u64,
    max: u64,
) -> *mut DrmProperty {
    unsafe {
        let property = drm_property_create(dev, flags, name, 2);
        if property.is_null() {
            return core::ptr::null_mut();
        }

        *(*property).values.add(0) = min;
        *(*property).values.add(1) = max;

        property
    }
}

// upstream: drm_property.c drm_property_create_range()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_create_range(
    dev: *mut DrmDevice,
    flags: u32,
    name: *const c_char,
    min: u64,
    max: u64,
) -> *mut DrmProperty {
    unsafe { property_create_range(dev, DRM_MODE_PROP_RANGE | flags, name, min, max) }
}

// upstream: drm_property.c drm_property_create_signed_range()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_create_signed_range(
    dev: *mut DrmDevice,
    flags: u32,
    name: *const c_char,
    min: i64,
    max: i64,
) -> *mut DrmProperty {
    unsafe {
        property_create_range(
            dev,
            DRM_MODE_PROP_SIGNED_RANGE | flags,
            name,
            min as u64,
            max as u64,
        )
    }
}

// upstream: drm_property.c drm_property_create_object()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_create_object(
    dev: *mut DrmDevice,
    mut flags: u32,
    name: *const c_char,
    r#type: u32,
) -> *mut DrmProperty {
    unsafe {
        flags |= DRM_MODE_PROP_OBJECT;

        if warn_on(flags & DRM_MODE_PROP_ATOMIC == 0) {
            return core::ptr::null_mut();
        }

        let property = drm_property_create(dev, flags, name, 1);
        if property.is_null() {
            return core::ptr::null_mut();
        }

        *(*property).values.add(0) = r#type as u64;

        property
    }
}

// upstream: drm_property.c drm_property_create_bool()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_create_bool(
    dev: *mut DrmDevice,
    flags: u32,
    name: *const c_char,
) -> *mut DrmProperty {
    unsafe { drm_property_create_range(dev, flags, name, 0, 1) }
}

// upstream: drm_property.c drm_property_add_enum()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_add_enum(
    property: *mut DrmProperty,
    value: u64,
    name: *const c_char,
) -> i32 {
    unsafe {
        let mut index: i32 = 0;

        if warn_on(c_strlen(name) >= DRM_PROP_NAME_LEN) {
            return -EINVAL;
        }

        if warn_on(
            !drm_property_type_is(property, DRM_MODE_PROP_ENUM)
                && !drm_property_type_is(property, DRM_MODE_PROP_BITMASK),
        ) {
            return -EINVAL;
        }

        // Bitmask bit positions are limited to the 64-bit mask.
        if warn_on(drm_property_type_is(property, DRM_MODE_PROP_BITMASK) && value > 63) {
            return -EINVAL;
        }

        let head = core::ptr::addr_of_mut!((*property).enum_list);
        let mut cur = (*head).next;
        while cur != head {
            let prop_enum = enum_from_link(cur);
            if warn_on((*prop_enum).value == value) {
                return -EINVAL;
            }
            index += 1;
            cur = (*cur).next;
        }

        if warn_on(index >= (*property).num_values as i32) {
            return -EINVAL;
        }

        let prop_enum = tk_linux_kapi::memory::kzalloc_obj::<DrmPropertyEnum>();
        if prop_enum.is_null() {
            return -ENOMEM;
        }

        strscpy_pad((*prop_enum).name.as_mut_ptr(), name, DRM_PROP_NAME_LEN);
        (*prop_enum).value = value;
        *(*property).values.add(index as usize) = value;
        list_add_tail(core::ptr::addr_of_mut!((*prop_enum).head), head);

        0
    }
}

/// `list_entry(link, struct drm_property_enum, head)`.
unsafe fn enum_from_link(link: *mut ListHead) -> *mut DrmPropertyEnum {
    link.cast::<u8>()
        .sub(offset_of!(DrmPropertyEnum, head))
        .cast::<DrmPropertyEnum>()
}

// upstream: drm_property.c drm_property_destroy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_destroy(dev: *mut DrmDevice, property: *mut DrmProperty) {
    unsafe {
        let head = core::ptr::addr_of_mut!((*property).enum_list);
        let mut cur = (*head).next;
        while cur != head {
            // list_for_each_entry_safe(): grab the successor before freeing.
            let next = (*cur).next;
            let prop_enum = enum_from_link(cur);
            list_del(cur);
            kfree(prop_enum);
            cur = next;
        }

        if (*property).num_values != 0 {
            kfree((*property).values);
        }

        drm_mode_object_unregister(dev, core::ptr::addr_of_mut!((*property).base));
        list_del(core::ptr::addr_of_mut!((*property).head));
        kfree(property);
    }
}

// upstream: drm_property.c drm_mode_getproperty_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_getproperty_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    unsafe {
        let out_resp = data.cast::<DrmModeGetProperty>();
        let mut enum_count: u32 = 0;
        let mut copied: u32 = 0;

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        let property = drm_property_find(dev, file_priv, (*out_resp).prop_id);
        if property.is_null() {
            return -ENOENT;
        }

        strscpy_pad(
            (*out_resp).name.as_mut_ptr(),
            (*property).name.as_ptr(),
            DRM_PROP_NAME_LEN,
        );
        (*out_resp).flags = (*property).flags;

        let value_count = (*property).num_values;
        let values_ptr = (*out_resp).values_ptr as usize as *mut u64;
        for i in 0..value_count {
            if i < (*out_resp).count_values
                && put_user(values_ptr.add(i as usize), *(*property).values.add(i as usize)) != 0
            {
                return -EFAULT;
            }
        }
        (*out_resp).count_values = value_count;

        let enum_ptr = (*out_resp).enum_blob_ptr as usize as *mut u8;
        if drm_property_type_is(property, DRM_MODE_PROP_ENUM)
            || drm_property_type_is(property, DRM_MODE_PROP_BITMASK)
        {
            let head = core::ptr::addr_of_mut!((*property).enum_list);
            let mut cur = (*head).next;
            while cur != head {
                let prop_enum = enum_from_link(cur);
                enum_count += 1;
                if (*out_resp).count_enum_blobs < enum_count {
                    cur = (*cur).next;
                    continue;
                }

                // Each user entry is a `struct drm_mode_property_enum`
                // (value at offset 0, name following it).
                let user = enum_ptr
                    .add(copied as usize * USER_PROPERTY_ENUM_SIZE)
                    .cast::<DrmModePropertyEnum>();
                let value = (*prop_enum).value;
                if copy_to_user_c(
                    core::ptr::addr_of_mut!((*user).value).cast::<c_void>(),
                    core::ptr::addr_of!(value).cast::<c_void>(),
                    size_of::<u64>(),
                ) != 0
                {
                    return -EFAULT;
                }
                if copy_to_user_c(
                    core::ptr::addr_of_mut!((*user).name).cast::<c_void>(),
                    (*prop_enum).name.as_ptr().cast::<c_void>(),
                    DRM_PROP_NAME_LEN,
                ) != 0
                {
                    return -EFAULT;
                }
                copied += 1;
                cur = (*cur).next;
            }
            (*out_resp).count_enum_blobs = enum_count;
        }

        if drm_property_type_is(property, DRM_MODE_PROP_BLOB) {
            (*out_resp).count_enum_blobs = 0;
        }

        0
    }
}

// upstream: drm_property.c drm_property_free_blob()
unsafe extern "C" fn drm_property_free_blob(kref: *mut Kref) {
    unsafe {
        let blob = blob_from_kref(kref);

        mutex_lock(core::ptr::addr_of_mut!((*(*blob).dev).mode_config.blob_lock));
        list_del(core::ptr::addr_of_mut!((*blob).head_global));
        mutex_unlock(core::ptr::addr_of_mut!((*(*blob).dev).mode_config.blob_lock));

        drm_mode_object_unregister((*blob).dev, core::ptr::addr_of_mut!((*blob).base));
        kvfree(blob.cast::<c_void>());
    }
}

// upstream: drm_property.c drm_property_create_blob()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_create_blob(
    dev: *mut DrmDevice,
    length: usize,
    data: *const c_void,
) -> *mut DrmPropertyBlob {
    unsafe {
        if length == 0 || length > (i32::MAX as usize) - size_of::<DrmPropertyBlob>() {
            return ERR_PTR_C(-EINVAL);
        }

        let blob = kzalloc(size_of::<DrmPropertyBlob>() + length, GFP_KERNEL)
            .cast::<DrmPropertyBlob>();
        if blob.is_null() {
            return ERR_PTR_C(-ENOMEM);
        }

        // This must be explicitly initialised, so we can safely call list_del
        // on it in the error path.
        INIT_LIST_HEAD(core::ptr::addr_of_mut!((*blob).head_file));

        (*blob).data = blob.cast::<u8>().add(size_of::<DrmPropertyBlob>()).cast::<c_void>();
        (*blob).length = length;
        (*blob).dev = dev;

        if !data.is_null() {
            core::ptr::copy_nonoverlapping(data.cast::<u8>(), (*blob).data.cast::<u8>(), length);
        }

        let ret = __drm_mode_object_add(
            dev,
            core::ptr::addr_of_mut!((*blob).base),
            DRM_MODE_OBJECT_BLOB,
            true,
            Some(drm_property_free_blob),
        );
        if ret != 0 {
            kvfree(blob.cast::<c_void>());
            return ERR_PTR_C(-EINVAL);
        }

        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.blob_lock));
        list_add_tail(
            core::ptr::addr_of_mut!((*blob).head_global),
            core::ptr::addr_of_mut!((*dev).mode_config.property_blob_list),
        );
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.blob_lock));

        blob
    }
}

/// `ERR_PTR()` in pointer form, as used by the blob constructors.
fn ERR_PTR_C<T>(error: i32) -> *mut T {
    tk_linux_kapi::config::ERR_PTR::<T>(error)
}

/// `PTR_ERR()`/`IS_ERR()` in pointer form.
fn IS_ERR_C<T>(ptr: *mut T) -> bool {
    tk_linux_kapi::config::IS_ERR(ptr)
}

fn PTR_ERR_C<T>(ptr: *mut T) -> i32 {
    tk_linux_kapi::config::PTR_ERR(ptr)
}

// upstream: drm_property.c drm_property_blob_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_blob_put(blob: *mut DrmPropertyBlob) {
    unsafe {
        if blob.is_null() {
            return;
        }
        drm_mode_object_put(core::ptr::addr_of_mut!((*blob).base));
    }
}

// upstream: drm_property.c drm_property_destroy_user_blobs()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_destroy_user_blobs(_dev: *mut DrmDevice, file_priv: *mut DrmFile) {
    unsafe {
        let head = core::ptr::addr_of_mut!((*file_priv).blobs);
        let mut cur = (*head).next;
        while cur != head {
            // list_for_each_entry_safe(): grab the successor before unlinking.
            let next = (*cur).next;
            let blob = cur
                .cast::<u8>()
                .sub(offset_of!(DrmPropertyBlob, head_file))
                .cast::<DrmPropertyBlob>();
            list_del_init(cur);
            drm_property_blob_put(blob);
            cur = next;
        }
    }
}

// upstream: drm_property.c drm_property_blob_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_blob_get(blob: *mut DrmPropertyBlob) -> *mut DrmPropertyBlob {
    unsafe {
        drm_mode_object_get(core::ptr::addr_of_mut!((*blob).base));
        blob
    }
}

// upstream: drm_property.c drm_property_lookup_blob()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_lookup_blob(dev: *mut DrmDevice, id: u32) -> *mut DrmPropertyBlob {
    unsafe {
        let obj = __drm_mode_object_find(dev, core::ptr::null_mut(), id, DRM_MODE_OBJECT_BLOB);
        let mut blob: *mut DrmPropertyBlob = core::ptr::null_mut();
        if !obj.is_null() {
            blob = obj_to_blob(obj);
        }
        blob
    }
}

// upstream: drm_property.c drm_property_replace_global_blob()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_replace_global_blob(
    dev: *mut DrmDevice,
    replace: *mut *mut DrmPropertyBlob,
    length: usize,
    data: *const c_void,
    obj_holds_id: *mut DrmModeObject,
    prop_holds_id: *mut DrmProperty,
) -> i32 {
    unsafe {
        let mut new_blob: *mut DrmPropertyBlob = core::ptr::null_mut();

        warn_on(replace.is_null());

        let old_blob = *replace;

        if length != 0 && !data.is_null() {
            new_blob = drm_property_create_blob(dev, length, data);
            if IS_ERR_C(new_blob) {
                return PTR_ERR_C(new_blob);
            }
        }

        if !obj_holds_id.is_null() {
            let ret = drm_object_property_set_value(
                obj_holds_id,
                prop_holds_id,
                if new_blob.is_null() {
                    0
                } else {
                    (*new_blob).base.id as u64
                },
            );
            if ret != 0 {
                drm_property_blob_put(new_blob);
                return ret;
            }
        }

        drm_property_blob_put(old_blob);
        *replace = new_blob;

        0
    }
}

// upstream: drm_property.c drm_property_replace_blob()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_replace_blob(
    blob: *mut *mut DrmPropertyBlob,
    new_blob: *mut DrmPropertyBlob,
) -> bool {
    unsafe {
        let old_blob = *blob;

        if old_blob == new_blob {
            return false;
        }

        drm_property_blob_put(old_blob);
        if !new_blob.is_null() {
            drm_property_blob_get(new_blob);
        }

        *blob = new_blob;
        true
    }
}

// upstream: drm_property.c drm_property_replace_blob_from_id()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_replace_blob_from_id(
    dev: *mut DrmDevice,
    blob: *mut *mut DrmPropertyBlob,
    blob_id: u64,
    max_size: isize,
    expected_size: isize,
    expected_elem_size: isize,
    replaced: *mut bool,
) -> i32 {
    unsafe {
        let mut new_blob: *mut DrmPropertyBlob = core::ptr::null_mut();

        if blob_id != 0 {
            new_blob = drm_property_lookup_blob(dev, blob_id as u32);
            if new_blob.is_null() {
                drm_dbg_atomic!(dev, "cannot find blob ID %llu\n", blob_id);
                return -EINVAL;
            }

            if max_size > 0 && (*new_blob).length as isize > max_size {
                drm_dbg_atomic!(
                    dev,
                    "[BLOB:%d] length %zu greater than max %zu\n",
                    (*new_blob).base.id,
                    (*new_blob).length,
                    max_size
                );
                drm_property_blob_put(new_blob);
                return -EINVAL;
            }

            if expected_size > 0 && (*new_blob).length as isize != expected_size {
                drm_dbg_atomic!(
                    dev,
                    "[BLOB:%d] length %zu different from expected %zu\n",
                    (*new_blob).base.id,
                    (*new_blob).length,
                    expected_size
                );
                drm_property_blob_put(new_blob);
                return -EINVAL;
            }

            if expected_elem_size > 0 && (*new_blob).length as isize % expected_elem_size != 0 {
                drm_dbg_atomic!(
                    dev,
                    "[BLOB:%d] length %zu not divisible by element size %zu\n",
                    (*new_blob).base.id,
                    (*new_blob).length,
                    expected_elem_size
                );
                drm_property_blob_put(new_blob);
                return -EINVAL;
            }
        }

        *replaced |= drm_property_replace_blob(blob, new_blob);
        drm_property_blob_put(new_blob);

        0
    }
}

// upstream: drm_property.c drm_mode_getblob_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_getblob_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    _file_priv: *mut DrmFile,
) -> i32 {
    unsafe {
        let out_resp = data.cast::<DrmModeGetBlob>();
        let mut ret = 0;

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        let blob = drm_property_lookup_blob(dev, (*out_resp).blob_id);
        if blob.is_null() {
            return -ENOENT;
        }

        'unref: {
            if (*out_resp).length as usize == (*blob).length {
                if copy_to_user_c(
                    ((*out_resp).data as usize) as *mut c_void,
                    (*blob).data,
                    (*blob).length,
                ) != 0
                {
                    ret = -EFAULT;
                    break 'unref;
                }
            }
            (*out_resp).length = (*blob).length as u32;
        }

        drm_property_blob_put(blob);
        ret
    }
}

// upstream: drm_property.c drm_mode_createblob_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_createblob_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    unsafe {
        let out_resp = data.cast::<DrmModeCreateBlob>();

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        let blob = drm_property_create_blob(dev, (*out_resp).length as usize, core::ptr::null());
        if IS_ERR_C(blob) {
            return PTR_ERR_C(blob);
        }

        if copy_from_user_c(
            (*blob).data,
            ((*out_resp).data as usize) as *const c_void,
            (*out_resp).length as usize,
        ) != 0
        {
            drm_property_blob_put(blob);
            return -EFAULT;
        }

        // Dropping the lock between create_blob and our access here is safe
        // because we only ever add to the list, and the blob is ours.
        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.blob_lock));
        (*out_resp).blob_id = (*blob).base.id;
        list_add_tail(
            core::ptr::addr_of_mut!((*blob).head_file),
            core::ptr::addr_of_mut!((*file_priv).blobs),
        );
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.blob_lock));

        0
    }
}

// upstream: drm_property.c drm_mode_destroyblob_ioctl()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_mode_destroyblob_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file_priv: *mut DrmFile,
) -> i32 {
    unsafe {
        let out_resp = data.cast::<DrmModeDestroyBlob>();
        let mut found = false;

        if !drm_core_check_feature(dev, DRIVER_MODESET) {
            return -EOPNOTSUPP;
        }

        let blob = drm_property_lookup_blob(dev, (*out_resp).blob_id);
        if blob.is_null() {
            return -ENOENT;
        }

        mutex_lock(core::ptr::addr_of_mut!((*dev).mode_config.blob_lock));

        // Ensure the property was actually created by this user.
        let head = core::ptr::addr_of_mut!((*file_priv).blobs);
        let mut cur = (*head).next;
        while cur != head {
            let bt = cur
                .cast::<u8>()
                .sub(offset_of!(DrmPropertyBlob, head_file))
                .cast::<DrmPropertyBlob>();
            if bt == blob {
                found = true;
                break;
            }
            cur = (*cur).next;
        }

        if !found {
            mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.blob_lock));
            drm_property_blob_put(blob);
            return -EPERM;
        }

        // We must drop head_file here, because we may not be the last
        // reference holder.
        list_del_init(core::ptr::addr_of_mut!((*blob).head_file));
        mutex_unlock(core::ptr::addr_of_mut!((*dev).mode_config.blob_lock));

        // One reference from lookup, and one from the filp.
        drm_property_blob_put(blob);
        drm_property_blob_put(blob);
        0
    }
}

// upstream: drm_property.c drm_property_change_valid_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_change_valid_get(
    property: *mut DrmProperty,
    value: u64,
    r#ref: *mut *mut DrmModeObject,
) -> bool {
    unsafe {
        if (*property).flags & DRM_MODE_PROP_IMMUTABLE != 0 {
            return false;
        }

        *r#ref = core::ptr::null_mut();

        if drm_property_type_is(property, DRM_MODE_PROP_RANGE) {
            if value < *(*property).values.add(0) || value > *(*property).values.add(1) {
                return false;
            }
            return true;
        } else if drm_property_type_is(property, DRM_MODE_PROP_SIGNED_RANGE) {
            let svalue = value as i64;
            if svalue < *(*property).values.add(0) as i64 || svalue > *(*property).values.add(1) as i64 {
                return false;
            }
            return true;
        } else if drm_property_type_is(property, DRM_MODE_PROP_BITMASK) {
            let mut valid_mask: u64 = 0;
            for i in 0..(*property).num_values as usize {
                valid_mask |= 1u64 << *(*property).values.add(i);
            }
            return value & !valid_mask == 0;
        } else if drm_property_type_is(property, DRM_MODE_PROP_BLOB) {
            if value == 0 {
                return true;
            }
            let blob = drm_property_lookup_blob((*property).dev, value as u32);
            if !blob.is_null() {
                *r#ref = core::ptr::addr_of_mut!((*blob).base);
                return true;
            } else {
                return false;
            }
        } else if drm_property_type_is(property, DRM_MODE_PROP_OBJECT) {
            // a zero value for an object property translates to null:
            if value == 0 {
                return true;
            }

            *r#ref = __drm_mode_object_find(
                (*property).dev,
                core::ptr::null_mut(),
                value as u32,
                *(*property).values.add(0) as u32,
            );
            return !(*r#ref).is_null();
        }

        for i in 0..(*property).num_values as usize {
            if *(*property).values.add(i) == value {
                return true;
            }
        }
        false
    }
}

// upstream: drm_property.c drm_property_change_valid_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_property_change_valid_put(property: *mut DrmProperty, r#ref: *mut DrmModeObject) {
    unsafe {
        if r#ref.is_null() {
            return;
        }

        if drm_property_type_is(property, DRM_MODE_PROP_OBJECT) {
            drm_mode_object_put(r#ref);
        } else if drm_property_type_is(property, DRM_MODE_PROP_BLOB) {
            drm_property_blob_put(obj_to_blob(r#ref));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm_mode_object_upstream::drm_mode_object_find;
    use crate::test_support::TestDrm;
    use crate::uapi_drm_consts::DRM_MODE_OBJECT_PROPERTY;

    #[test]
    fn enum_property_has_values_in_insertion_order() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        t.init_registry();
        let dev = t.ptr();
        unsafe {
            let prop = drm_property_create(dev, DRM_MODE_PROP_ENUM, b"mode\0".as_ptr().cast(), 2);
            assert!(!prop.is_null());
            assert_eq!((*prop).num_values, 2);
            assert_eq!(drm_property_add_enum(prop, 7, b"seven\0".as_ptr().cast()), 0);
            assert_eq!(drm_property_add_enum(prop, 9, b"nine\0".as_ptr().cast()), 0);
            assert_eq!(*(*prop).values.add(0), 7);
            assert_eq!(*(*prop).values.add(1), 9);
            // A duplicate value is rejected (the C WARN_ON path).
            assert_eq!(drm_property_add_enum(prop, 7, b"again\0".as_ptr().cast()), -EINVAL);
            // The property is registered as an object and listed on the device.
            let found = drm_mode_object_find(dev, core::ptr::null_mut(), (*prop).base.id, DRM_MODE_OBJECT_PROPERTY);
            assert_eq!(obj_to_property(found), prop);
            drm_property_destroy(dev, prop);
        }
    }

    #[test]
    fn range_property_accepts_only_in_range_values() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        t.init_registry();
        let dev = t.ptr();
        unsafe {
            let prop = drm_property_create_range(dev, 0, b"range\0".as_ptr().cast(), 0, 100);
            assert!(!prop.is_null());
            let mut r: *mut DrmModeObject = core::ptr::null_mut();
            assert!(drm_property_change_valid_get(prop, 50, &mut r));
            drm_property_change_valid_put(prop, r);
            assert!(!drm_property_change_valid_get(prop, 101, &mut r));
            assert!(r.is_null());
            drm_property_destroy(dev, prop);
        }
    }

    #[test]
    fn bitmask_property_counts_its_bits() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        t.init_registry();
        let dev = t.ptr();
        let list = [
            DrmPropEnumList { r#type: 0, name: b"a\0".as_ptr().cast::<c_char>() as *mut c_char },
            DrmPropEnumList { r#type: 2, name: b"c\0".as_ptr().cast::<c_char>() as *mut c_char },
        ];
        unsafe {
            let prop = drm_property_create_bitmask(dev, 0, b"bits\0".as_ptr().cast(), list.as_ptr(), 2, 0b101);
            assert!(!prop.is_null());
            assert_eq!((*prop).num_values, 2);
            assert_ne!((*prop).flags & DRM_MODE_PROP_BITMASK, 0);
            drm_property_destroy(dev, prop);
        }
    }

    #[test]
    fn flags_must_have_exactly_one_type() {
        assert!(drm_property_flags_valid(DRM_MODE_PROP_RANGE));
        assert!(!drm_property_flags_valid(0));
        assert!(!drm_property_flags_valid(DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ENUM));
        assert!(!drm_property_flags_valid(DRM_MODE_PROP_RANGE | (1 << 3)));
    }

    #[test]
    fn blob_lives_until_its_last_reference_is_dropped() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        t.init_registry();
        let dev = t.ptr();
        let payload = [1u8, 2, 3, 4];
        unsafe {
            let blob = drm_property_create_blob(dev, payload.len(), payload.as_ptr().cast());
            assert!(!IS_ERR_C(blob));
            assert_eq!((*blob).length, 4);
            assert_eq!(core::slice::from_raw_parts((*blob).data.cast::<u8>(), 4), &payload);
            let id = (*blob).base.id;

            // Lookup takes a reference; the creator's reference is the first.
            let again = drm_property_lookup_blob(dev, id);
            assert_eq!(again, blob);
            drm_property_blob_put(again);
            assert_eq!(drm_property_lookup_blob(dev, id), blob);
            drm_property_blob_put(blob);

            drm_property_blob_put(blob);
            // Last reference dropped: the id is gone.
            assert!(drm_property_lookup_blob(dev, id).is_null());
        }
    }

    #[test]
    fn blob_creation_rejects_empty_payload() {
        let mut t = TestDrm::new(DRIVER_MODESET);
        t.init_registry();
        let dev = t.ptr();
        unsafe {
            let blob = drm_property_create_blob(dev, 0, core::ptr::null());
            assert!(IS_ERR_C(blob));
            assert_eq!(PTR_ERR_C(blob), -EINVAL);
        }
    }
}
