/*
 * Copyright (c) 2016 Intel Corporation
 *
 * Permission to use, copy, modify, distribute, and sell this software and its
 * documentation for any purpose is hereby granted without fee, provided that
 * the above copyright notice appear in all copies and that both that copyright
 * notice and this permission notice appear in supporting documentation, and
 * that the name of the copyright holders not be used in advertising or
 * publicity pertaining to distribution of the software without specific,
 * written prior permission.  The copyright holders make no representations
 * about the suitability of this software for any purpose.  It is provided "as
 * is" without express or implied warranty.
 *
 * THE COPYRIGHT HOLDERS DISCLAIM ALL WARRANTIES WITH REGARD TO THIS SOFTWARE,
 * INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS, IN NO
 * EVENT SHALL THE COPYRIGHT HOLDERS BE LIABLE FOR ANY SPECIAL, INDIRECT OR
 * CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE,
 * DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
 * TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
 * OF THIS SOFTWARE.
 */

//! Function-level Rust translation of Linux v7.2.3
//! `drivers/gpu/drm/drm_property.c`.
//!
//! Property/blob policy and lifetime ordering are kept here. The DRM object
//! registry, blob mutex, warning/debug sinks, and userspace copies are modeled
//! as explicit state and `PropertyUapiIo` operations so callers can bind the
//! algorithms to the kernel framework without changing their decisions.

#![no_std]
extern crate alloc;

use alloc::vec::Vec;

pub type DeviceId = u64;
pub type ObjectId = u32;
pub type PropertyId = u32;
pub type BlobId = u32;
pub type UserPtr = u64;
pub type KResult<T> = Result<T, i32>;

pub const EFAULT: i32 = -14;
pub const EINVAL: i32 = -22;
pub const ENOENT: i32 = -2;
pub const ENOMEM: i32 = -12;
pub const EPERM: i32 = -1;
pub const EOPNOTSUPP: i32 = -95;
pub const DRIVER_MODESET: u32 = 1 << 0;
pub const DRM_PROP_NAME_LEN: usize = 32;

pub const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
pub const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub const DRM_MODE_PROP_BITMASK: u32 = 1 << 5;
pub const DRM_MODE_PROP_LEGACY_TYPE: u32 = DRM_MODE_PROP_RANGE
    | DRM_MODE_PROP_ENUM
    | DRM_MODE_PROP_BLOB
    | DRM_MODE_PROP_BITMASK;
pub const DRM_MODE_PROP_EXTENDED_TYPE: u32 = 0x0000_ffc0;
pub const DRM_MODE_PROP_OBJECT: u32 = 1 << 6;
pub const DRM_MODE_PROP_SIGNED_RANGE: u32 = 2 << 6;
pub const DRM_MODE_PROP_ATOMIC: u32 = 0x8000_0000;
pub const DRM_MODE_OBJECT_PROPERTY: u32 = 0xb0b0_b0b0;
pub const DRM_MODE_OBJECT_BLOB: u32 = 0xbbbb_bbbb;

const INT_MAX: usize = i32::MAX as usize;
// sizeof(struct drm_property_blob) for the 64-bit Linux ABI targeted here.
const DRM_PROPERTY_BLOB_C_HEADER_SIZE: usize = 88;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyEnum {
    pub value: u64,
    pub name: [u8; DRM_PROP_NAME_LEN],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyEnumSpec<'a> {
    pub value: u64,
    pub name: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrmProperty {
    pub id: PropertyId,
    pub device: DeviceId,
    pub flags: u32,
    pub name: [u8; DRM_PROP_NAME_LEN],
    pub values: Vec<u64>,
    pub enum_list: Vec<PropertyEnum>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrmPropertyBlob {
    pub id: BlobId,
    pub device: DeviceId,
    pub length: usize,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeObject {
    pub id: ObjectId,
    pub object_type: u32,
    pub references: usize,
    /// Values attached to this mode object, in the same order as its property list.
    pub property_values: Vec<(PropertyId, u64)>,
    /// Blob objects use the property-blob kref free callback.
    pub blob_free_callback: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrmFile {
    pub id: u64,
    /// Mirrors drm_file::blobs. Removing an ID drops the file's blob reference.
    pub blobs: Vec<BlobId>,
    /// `None` means unrestricted; `Some` models lease visibility for lookups.
    pub visible_objects: Option<Vec<ObjectId>>,
}

impl DrmFile {
    pub fn new(id: u64) -> Self {
        Self { id, blobs: Vec::new(), visible_objects: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrmDevice {
    pub id: DeviceId,
    pub driver_features: u32,
    pub next_object_id: ObjectId,
    pub properties: Vec<DrmProperty>,
    /// Mirrors mode_config.property_list.
    pub property_list: Vec<PropertyId>,
    pub blobs: Vec<DrmPropertyBlob>,
    /// Mirrors mode_config.property_blob_list.
    pub property_blob_list: Vec<BlobId>,
    pub objects: Vec<ModeObject>,
}

impl DrmDevice {
    pub fn new(id: DeviceId, driver_features: u32) -> Self {
        Self {
            id,
            driver_features,
            next_object_id: 1,
            properties: Vec::new(),
            property_list: Vec::new(),
            blobs: Vec::new(),
            property_blob_list: Vec::new(),
            objects: Vec::new(),
        }
    }

    fn alloc_object_id(&mut self) -> KResult<ObjectId> {
        let id = self.next_object_id;
        self.next_object_id = id.checked_add(1).ok_or(ENOMEM)?;
        Ok(id)
    }

    fn register_object(
        &mut self,
        object_type: u32,
        blob_free_callback: bool,
    ) -> KResult<ObjectId> {
        self.objects.try_reserve(1).map_err(|_| ENOMEM)?;
        let id = self.alloc_object_id()?;
        self.objects.push(ModeObject {
            id,
            object_type,
            references: 1,
            property_values: Vec::new(),
            blob_free_callback,
        });
        Ok(id)
    }

    fn object_index(&self, id: ObjectId) -> Option<usize> {
        self.objects.iter().position(|object| object.id == id)
    }

    fn property_index(&self, id: PropertyId) -> Option<usize> {
        self.properties.iter().position(|property| property.id == id)
    }

    fn blob_index(&self, id: BlobId) -> Option<usize> {
        self.blobs.iter().position(|blob| blob.id == id)
    }

    fn unregister_object(&mut self, id: ObjectId) {
        if let Some(index) = self.object_index(id) {
            self.objects.remove(index);
        }
    }

    /// Test/kernel integration hook corresponding to drm_object_attach_property().
    pub fn attach_property_value(&mut self, object: ObjectId, property: PropertyId, value: u64) -> KResult<()> {
        let index = self.object_index(object).ok_or(ENOENT)?;
        self.objects[index].property_values.try_reserve(1).map_err(|_| ENOMEM)?;
        self.objects[index].property_values.push((property, value));
        Ok(())
    }
}

/// Framework boundaries used by the source's warning, lock, logging and
/// userspace-access primitives. Copy methods return `true` when a user fault
/// occurred, matching Linux's `copy_{to,from}_user()` / `put_user()` tests.
pub trait PropertyUapiIo {
    fn warn_on(&mut self, condition: bool, _site: &'static str) -> bool { condition }
    fn lock_blob(&mut self, _device: DeviceId) {}
    fn unlock_blob(&mut self, _device: DeviceId) {}
    fn put_user_u64(&mut self, ptr: UserPtr, value: u64) -> bool;
    fn copy_to_user(&mut self, ptr: UserPtr, source: &[u8]) -> bool;
    fn copy_from_user(&mut self, ptr: UserPtr, destination: &mut [u8]) -> bool;
    fn debug_atomic(&mut self, _site: &'static str, _blob: BlobId, _length: usize, _limit: isize) {}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeGetProperty {
    pub values_ptr: UserPtr,
    pub enum_blob_ptr: UserPtr,
    pub prop_id: PropertyId,
    pub flags: u32,
    pub name: [u8; DRM_PROP_NAME_LEN],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeGetBlob {
    pub blob_id: BlobId,
    pub length: u32,
    pub data: UserPtr,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeCreateBlob {
    pub data: UserPtr,
    pub length: u32,
    pub blob_id: BlobId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeDestroyBlob {
    pub blob_id: BlobId,
}

fn c_name_len(name: &str) -> usize {
    name.as_bytes().iter().position(|byte| *byte == 0).unwrap_or(name.len())
}

fn strscpy_pad(destination: &mut [u8; DRM_PROP_NAME_LEN], source: &str) {
    *destination = [0; DRM_PROP_NAME_LEN];
    let bytes = source.as_bytes();
    let source_len = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    let copied = core::cmp::min(source_len, DRM_PROP_NAME_LEN - 1);
    destination[..copied].copy_from_slice(&bytes[..copied]);
}

fn property_type_is(property: &DrmProperty, requested: u32) -> bool {
    if property.flags & DRM_MODE_PROP_EXTENDED_TYPE != 0 {
        property.flags & DRM_MODE_PROP_EXTENDED_TYPE == requested
    } else {
        property.flags & requested != 0
    }
}

fn find_property(dev: &DrmDevice, file: &DrmFile, id: PropertyId) -> Option<usize> {
    let index = dev.property_index(id)?;
    if let Some(visible) = &file.visible_objects {
        if !visible.contains(&id) {
            return None;
        }
    }
    Some(index)
}

fn user_ptr_add(ptr: UserPtr, offset: usize) -> UserPtr {
    ptr.wrapping_add(offset as u64)
}

fn object_get(dev: &mut DrmDevice, id: ObjectId) -> bool {
    let Some(index) = dev.object_index(id) else { return false; };
    let Some(references) = dev.objects[index].references.checked_add(1) else { return false; };
    dev.objects[index].references = references;
    true
}

fn object_put(dev: &mut DrmDevice, io: &mut impl PropertyUapiIo, id: ObjectId) {
    let Some(index) = dev.object_index(id) else { return; };
    if dev.objects[index].references == 0 {
        return;
    }
    dev.objects[index].references -= 1;
    if dev.objects[index].references != 0 {
        return;
    }
    if dev.objects[index].blob_free_callback {
        drm_property_free_blob(dev, io, id);
    } else {
        dev.objects.remove(index);
    }
}

// upstream: drm_property.c drm_property_flags_valid()
fn drm_property_flags_valid(flags: u32) -> bool {
    let legacy_type = flags & DRM_MODE_PROP_LEGACY_TYPE;
    let ext_type = flags & DRM_MODE_PROP_EXTENDED_TYPE;

    // Reject undefined/deprecated flags.
    if flags & !(DRM_MODE_PROP_LEGACY_TYPE
        | DRM_MODE_PROP_EXTENDED_TYPE
        | DRM_MODE_PROP_IMMUTABLE
        | DRM_MODE_PROP_ATOMIC) != 0
    {
        return false;
    }

    // Exactly one of a legacy type and an extended type must be present.
    if (legacy_type == 0) == (ext_type == 0) {
        return false;
    }

    // Only one legacy type at a time.
    if legacy_type != 0 && legacy_type.count_ones() != 1 {
        return false;
    }
    true
}

// upstream: drm_property.c drm_property_create()
pub fn drm_property_create(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    flags: u32,
    name: &str,
    num_values: i32,
) -> Option<PropertyId> {
    if io.warn_on(!drm_property_flags_valid(flags), "drm_property_create:flags") {
        return None;
    }
    if io.warn_on(c_name_len(name) >= DRM_PROP_NAME_LEN, "drm_property_create:name") {
        return None;
    }

    let mut values = Vec::new();
    if num_values != 0 {
        if num_values < 0 || values.try_reserve_exact(num_values as usize).is_err() {
            return None;
        }
        values.resize(num_values as usize, 0);
    }

    // Reserve the intrusive-list equivalents before object registration, so
    // an allocation failure cannot leave a registered but unlisted property.
    if dev.properties.try_reserve(1).is_err() || dev.property_list.try_reserve(1).is_err() {
        return None;
    }
    let id = dev.register_object(DRM_MODE_OBJECT_PROPERTY, false).ok()?;
    let mut property = DrmProperty {
        id,
        device: dev.id,
        flags,
        name: [0; DRM_PROP_NAME_LEN],
        values,
        enum_list: Vec::new(),
    };
    strscpy_pad(&mut property.name, name);
    dev.properties.push(property);
    dev.property_list.push(id);
    Some(id)
}

// upstream: drm_property.c drm_property_create_enum()
pub fn drm_property_create_enum(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    flags: u32,
    name: &str,
    props: &[PropertyEnumSpec<'_>],
    num_values: i32,
) -> Option<PropertyId> {
    let flags = flags | DRM_MODE_PROP_ENUM;
    let property = drm_property_create(dev, io, flags, name, num_values)?;
    for item in props.iter().take(core::cmp::max(num_values, 0) as usize) {
        if drm_property_add_enum(dev, io, property, item.value, item.name) != 0 {
            drm_property_destroy(dev, io, property);
            return None;
        }
    }
    Some(property)
}

// upstream: drm_property.c drm_property_create_bitmask()
pub fn drm_property_create_bitmask(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    flags: u32,
    name: &str,
    props: &[PropertyEnumSpec<'_>],
    num_props: i32,
    supported_bits: u64,
) -> Option<PropertyId> {
    let num_values = supported_bits.count_ones() as i32;
    let property = drm_property_create(dev, io, flags | DRM_MODE_PROP_BITMASK, name, num_values)?;
    for item in props.iter().take(core::cmp::max(num_props, 0) as usize) {
        if item.value >= 64 || supported_bits & (1u64 << item.value) == 0 {
            continue;
        }
        if drm_property_add_enum(dev, io, property, item.value, item.name) != 0 {
            drm_property_destroy(dev, io, property);
            return None;
        }
    }
    Some(property)
}

// upstream: drm_property.c property_create_range()
fn property_create_range(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    flags: u32,
    name: &str,
    min: u64,
    max: u64,
) -> Option<PropertyId> {
    let property = drm_property_create(dev, io, flags, name, 2)?;
    if let Some(index) = dev.property_index(property) {
        dev.properties[index].values[0] = min;
        dev.properties[index].values[1] = max;
    }
    Some(property)
}

// upstream: drm_property.c drm_property_create_range()
pub fn drm_property_create_range(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    flags: u32,
    name: &str,
    min: u64,
    max: u64,
) -> Option<PropertyId> {
    property_create_range(dev, io, DRM_MODE_PROP_RANGE | flags, name, min, max)
}

// upstream: drm_property.c drm_property_create_signed_range()
pub fn drm_property_create_signed_range(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    flags: u32,
    name: &str,
    min: i64,
    max: i64,
) -> Option<PropertyId> {
    property_create_range(
        dev,
        io,
        DRM_MODE_PROP_SIGNED_RANGE | flags,
        name,
        min as u64,
        max as u64,
    )
}

// upstream: drm_property.c drm_property_create_object()
pub fn drm_property_create_object(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    flags: u32,
    name: &str,
    object_type: u32,
) -> Option<PropertyId> {
    let flags = flags | DRM_MODE_PROP_OBJECT;
    if io.warn_on(flags & DRM_MODE_PROP_ATOMIC == 0, "drm_property_create_object:atomic") {
        return None;
    }
    let property = drm_property_create(dev, io, flags, name, 1)?;
    if let Some(index) = dev.property_index(property) {
        dev.properties[index].values[0] = object_type as u64;
    }
    Some(property)
}

// upstream: drm_property.c drm_property_create_bool()
pub fn drm_property_create_bool(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    flags: u32,
    name: &str,
) -> Option<PropertyId> {
    drm_property_create_range(dev, io, flags, name, 0, 1)
}

// upstream: drm_property.c drm_property_add_enum()
pub fn drm_property_add_enum(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    property_id: PropertyId,
    value: u64,
    name: &str,
) -> i32 {
    if io.warn_on(c_name_len(name) >= DRM_PROP_NAME_LEN, "drm_property_add_enum:name") {
        return EINVAL;
    }
    let Some(property_index) = dev.property_index(property_id) else { return EINVAL; };
    if io.warn_on(
        !property_type_is(&dev.properties[property_index], DRM_MODE_PROP_ENUM)
            && !property_type_is(&dev.properties[property_index], DRM_MODE_PROP_BITMASK),
        "drm_property_add_enum:type",
    ) {
        return EINVAL;
    }

    // Bitmask enum values are single bit positions in [0, 63].
    if io.warn_on(
        property_type_is(&dev.properties[property_index], DRM_MODE_PROP_BITMASK) && value > 63,
        "drm_property_add_enum:bit",
    ) {
        return EINVAL;
    }

    let property = &dev.properties[property_index];
    let mut index = 0usize;
    for item in &property.enum_list {
        if io.warn_on(item.value == value, "drm_property_add_enum:duplicate") {
            return EINVAL;
        }
        index += 1;
    }
    if io.warn_on(index >= property.values.len(), "drm_property_add_enum:capacity") {
        return EINVAL;
    }

    if dev.properties[property_index].enum_list.try_reserve(1).is_err() {
        return ENOMEM;
    }
    let mut enum_name = [0; DRM_PROP_NAME_LEN];
    strscpy_pad(&mut enum_name, name);
    dev.properties[property_index].values[index] = value;
    dev.properties[property_index].enum_list.push(PropertyEnum { value, name: enum_name });
    0
}

// upstream: drm_property.c drm_property_destroy()
pub fn drm_property_destroy(
    dev: &mut DrmDevice,
    _io: &mut impl PropertyUapiIo,
    property_id: PropertyId,
) {
    let Some(index) = dev.property_index(property_id) else { return; };
    // list_for_each_entry_safe() removes every enum entry before releasing the
    // property values and unregistering its base mode object.
    while !dev.properties[index].enum_list.is_empty() {
        dev.properties[index].enum_list.remove(0);
    }
    dev.properties[index].values.clear();
    dev.unregister_object(property_id);
    if let Some(index) = dev.property_list.iter().position(|id| *id == property_id) {
        dev.property_list.remove(index);
    }
    dev.properties.remove(index);
}

// upstream: drm_property.c drm_mode_getproperty_ioctl()
pub fn drm_mode_getproperty_ioctl(
    dev: &DrmDevice,
    io: &mut impl PropertyUapiIo,
    out_resp: &mut ModeGetProperty,
    file: &DrmFile,
) -> i32 {
    if dev.driver_features & DRIVER_MODESET == 0 {
        return EOPNOTSUPP;
    }
    let Some(property_index) = find_property(dev, file, out_resp.prop_id) else { return ENOENT; };
    let property = &dev.properties[property_index];
    out_resp.name = property.name;
    out_resp.flags = property.flags;

    let value_count = property.values.len();
    let values_ptr = out_resp.values_ptr;
    for (index, value) in property.values.iter().enumerate() {
        if index < out_resp.count_values as usize
            && io.put_user_u64(user_ptr_add(values_ptr, index * core::mem::size_of::<u64>()), *value)
        {
            return EFAULT;
        }
    }
    out_resp.count_values = value_count as u32;

    let mut enum_count = 0usize;
    let mut copied = 0usize;
    let enum_ptr = out_resp.enum_blob_ptr;
    if property_type_is(property, DRM_MODE_PROP_ENUM)
        || property_type_is(property, DRM_MODE_PROP_BITMASK)
    {
        for item in &property.enum_list {
            enum_count += 1;
            if (out_resp.count_enum_blobs as usize) < enum_count {
                continue;
            }

            let base = user_ptr_add(enum_ptr, copied * 40);
            let value_bytes = item.value.to_ne_bytes();
            if io.copy_to_user(base, &value_bytes) {
                return EFAULT;
            }
            if io.copy_to_user(user_ptr_add(base, 8), &item.name) {
                return EFAULT;
            }
            copied += 1;
        }
        out_resp.count_enum_blobs = enum_count as u32;
    }

    // Historically blob values are returned by GETBLOB, never this metadata ioctl.
    if property_type_is(property, DRM_MODE_PROP_BLOB) {
        out_resp.count_enum_blobs = 0;
    }
    0
}

// upstream: drm_property.c drm_property_free_blob()
fn drm_property_free_blob(dev: &mut DrmDevice, io: &mut impl PropertyUapiIo, id: BlobId) {
    io.lock_blob(dev.id);
    if let Some(index) = dev.property_blob_list.iter().position(|blob| *blob == id) {
        dev.property_blob_list.remove(index);
    }
    io.unlock_blob(dev.id);

    // drm_mode_object_unregister() follows the global-list unlink; kvfree()
    // then releases the blob and its embedded data allocation.
    dev.unregister_object(id);
    if let Some(index) = dev.blob_index(id) {
        dev.blobs.remove(index);
    }
}

// upstream: drm_property.c drm_property_create_blob()
pub fn drm_property_create_blob(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    length: usize,
    data: Option<&[u8]>,
) -> KResult<BlobId> {
    if length == 0 || length > INT_MAX - DRM_PROPERTY_BLOB_C_HEADER_SIZE {
        return Err(EINVAL);
    }

    let mut blob_data = Vec::new();
    blob_data.try_reserve_exact(length).map_err(|_| ENOMEM)?;
    blob_data.resize(length, 0);
    if let Some(source) = data {
        if source.len() < length {
            return Err(EINVAL);
        }
        blob_data.copy_from_slice(&source[..length]);
    }

    // Reserve both list nodes before registering the mode object. The C
    // allocation is one zeroed header+payload block; `blob_data` models the
    // embedded payload while the object/list records model its header links.
    dev.blobs.try_reserve(1).map_err(|_| ENOMEM)?;
    dev.property_blob_list.try_reserve(1).map_err(|_| ENOMEM)?;
    let id = dev.register_object(DRM_MODE_OBJECT_BLOB, true).map_err(|_| EINVAL)?;
    dev.blobs.push(DrmPropertyBlob { id, device: dev.id, length, data: blob_data });

    io.lock_blob(dev.id);
    dev.property_blob_list.push(id);
    io.unlock_blob(dev.id);
    Ok(id)
}

// upstream: drm_property.c drm_property_blob_put()
pub fn drm_property_blob_put(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    blob: Option<BlobId>,
) {
    if let Some(id) = blob {
        object_put(dev, io, id);
    }
}

// upstream: drm_property.c drm_property_destroy_user_blobs()
pub fn drm_property_destroy_user_blobs(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    file: &mut DrmFile,
) {
    // File release has exclusive access to its blob list, so as in the source
    // this path deliberately does not take blob_lock.
    while !file.blobs.is_empty() {
        let blob = file.blobs.remove(0);
        drm_property_blob_put(dev, io, Some(blob));
    }
}

// upstream: drm_property.c drm_property_blob_get()
pub fn drm_property_blob_get(dev: &mut DrmDevice, blob: BlobId) -> BlobId {
    let _ = object_get(dev, blob);
    blob
}

// upstream: drm_property.c drm_property_lookup_blob()
pub fn drm_property_lookup_blob(dev: &mut DrmDevice, id: BlobId) -> Option<BlobId> {
    let index = dev.object_index(id)?;
    if dev.objects[index].object_type != DRM_MODE_OBJECT_BLOB || !object_get(dev, id) {
        return None;
    }
    Some(id)
}

// upstream: drm_property.c drm_property_replace_global_blob()
pub fn drm_property_replace_global_blob(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    replace: &mut Option<BlobId>,
    length: usize,
    data: Option<&[u8]>,
    object_holds_id: Option<ObjectId>,
    property_holds_id: Option<PropertyId>,
) -> i32 {
    let mut new_blob = None;
    let old_blob = *replace;

    if length != 0 && data.is_some() {
        match drm_property_create_blob(dev, io, length, data) {
            Ok(blob) => new_blob = Some(blob),
            Err(error) => return error,
        }
    }

    if let Some(object) = object_holds_id {
        let Some(property) = property_holds_id else {
            drm_property_blob_put(dev, io, new_blob);
            return EINVAL;
        };
        let Some(index) = dev.object_index(object) else {
            drm_property_blob_put(dev, io, new_blob);
            return EINVAL;
        };
        let Some(value_index) = dev.objects[index]
            .property_values
            .iter()
            .position(|(property_id, _)| *property_id == property)
        else {
            drm_property_blob_put(dev, io, new_blob);
            return EINVAL;
        };
        dev.objects[index].property_values[value_index].1 = new_blob.unwrap_or(0) as u64;
    }

    drm_property_blob_put(dev, io, old_blob);
    *replace = new_blob;
    0
}

// upstream: drm_property.c drm_property_replace_blob()
pub fn drm_property_replace_blob(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    blob: &mut Option<BlobId>,
    new_blob: Option<BlobId>,
) -> bool {
    let old_blob = *blob;
    if old_blob == new_blob {
        return false;
    }

    drm_property_blob_put(dev, io, old_blob);
    if let Some(id) = new_blob {
        let _ = drm_property_blob_get(dev, id);
    }
    *blob = new_blob;
    true
}

// upstream: drm_property.c drm_property_replace_blob_from_id()
pub fn drm_property_replace_blob_from_id(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    blob: &mut Option<BlobId>,
    blob_id: u64,
    max_size: isize,
    expected_size: isize,
    expected_elem_size: isize,
    replaced: &mut bool,
) -> i32 {
    let mut new_blob = None;
    if blob_id != 0 {
        new_blob = drm_property_lookup_blob(dev, blob_id as BlobId);
        let Some(id) = new_blob else {
            io.debug_atomic("cannot-find", blob_id as BlobId, 0, 0);
            return EINVAL;
        };
        let length = dev.blobs[dev.blob_index(id).expect("registered blob")].length;

        if max_size > 0 && length > max_size as usize {
            io.debug_atomic("greater-than-max", id, length, max_size);
            drm_property_blob_put(dev, io, new_blob);
            return EINVAL;
        }
        if expected_size > 0 && length != expected_size as usize {
            io.debug_atomic("different-from-expected", id, length, expected_size);
            drm_property_blob_put(dev, io, new_blob);
            return EINVAL;
        }
        if expected_elem_size > 0 && length % expected_elem_size as usize != 0 {
            io.debug_atomic("not-divisible-by-element-size", id, length, expected_elem_size);
            drm_property_blob_put(dev, io, new_blob);
            return EINVAL;
        }
    }

    *replaced |= drm_property_replace_blob(dev, io, blob, new_blob);
    drm_property_blob_put(dev, io, new_blob);
    0
}

// upstream: drm_property.c drm_mode_getblob_ioctl()
pub fn drm_mode_getblob_ioctl(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    out_resp: &mut ModeGetBlob,
    _file: &DrmFile,
) -> i32 {
    if dev.driver_features & DRIVER_MODESET == 0 {
        return EOPNOTSUPP;
    }
    let Some(blob_id) = drm_property_lookup_blob(dev, out_resp.blob_id) else { return ENOENT; };
    let index = dev.blob_index(blob_id).expect("looked-up blob remains registered");
    let mut ret = 0;

    if out_resp.length as usize == dev.blobs[index].length {
        if io.copy_to_user(out_resp.data, &dev.blobs[index].data) {
            ret = EFAULT;
            drm_property_blob_put(dev, io, Some(blob_id));
            return ret;
        }
    }
    out_resp.length = dev.blobs[index].length as u32;
    drm_property_blob_put(dev, io, Some(blob_id));
    ret
}

// upstream: drm_property.c drm_mode_createblob_ioctl()
pub fn drm_mode_createblob_ioctl(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    out_resp: &mut ModeCreateBlob,
    file: &mut DrmFile,
) -> i32 {
    if dev.driver_features & DRIVER_MODESET == 0 {
        return EOPNOTSUPP;
    }
    if file.blobs.try_reserve(1).is_err() {
        return ENOMEM;
    }

    let blob = match drm_property_create_blob(dev, io, out_resp.length as usize, None) {
        Ok(blob) => blob,
        Err(error) => return error,
    };
    let Some(index) = dev.blob_index(blob) else { return EINVAL; };
    if io.copy_from_user(out_resp.data, &mut dev.blobs[index].data) {
        drm_property_blob_put(dev, io, Some(blob));
        return EFAULT;
    }

    // The created blob is not yet on any file list; list membership is
    // serialized with the global blob mutex exactly as in the ioctl path.
    io.lock_blob(dev.id);
    out_resp.blob_id = blob;
    file.blobs.push(blob);
    io.unlock_blob(dev.id);
    0
}

// upstream: drm_property.c drm_mode_destroyblob_ioctl()
pub fn drm_mode_destroyblob_ioctl(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    out_resp: &ModeDestroyBlob,
    file: &mut DrmFile,
) -> i32 {
    if dev.driver_features & DRIVER_MODESET == 0 {
        return EOPNOTSUPP;
    }
    let Some(blob) = drm_property_lookup_blob(dev, out_resp.blob_id) else { return ENOENT; };

    io.lock_blob(dev.id);
    let Some(file_index) = file.blobs.iter().position(|id| *id == blob) else {
        io.unlock_blob(dev.id);
        drm_property_blob_put(dev, io, Some(blob));
        return EPERM;
    };

    // Drop the file-list reference under the lock; a lookup reference keeps
    // the blob alive until the two puts after unlocking.
    file.blobs.remove(file_index);
    io.unlock_blob(dev.id);
    drm_property_blob_put(dev, io, Some(blob));
    drm_property_blob_put(dev, io, Some(blob));
    0
}

// upstream: drm_property.c drm_property_change_valid_get()
pub fn drm_property_change_valid_get(
    dev: &mut DrmDevice,
    property: &DrmProperty,
    value: u64,
    reference: &mut Option<ObjectId>,
) -> bool {
    if property.flags & DRM_MODE_PROP_IMMUTABLE != 0 {
        return false;
    }
    *reference = None;

    if property_type_is(property, DRM_MODE_PROP_RANGE) {
        if value < property.values[0] || value > property.values[1] {
            return false;
        }
        return true;
    } else if property_type_is(property, DRM_MODE_PROP_SIGNED_RANGE) {
        let signed_value = value as i64;
        if signed_value < property.values[0] as i64 || signed_value > property.values[1] as i64 {
            return false;
        }
        return true;
    } else if property_type_is(property, DRM_MODE_PROP_BITMASK) {
        let mut valid_mask = 0u64;
        for enum_value in &property.values {
            if *enum_value < 64 {
                valid_mask |= 1u64 << *enum_value;
            }
        }
        return value & !valid_mask == 0;
    } else if property_type_is(property, DRM_MODE_PROP_BLOB) {
        if value == 0 {
            return true;
        }
        if let Some(blob) = drm_property_lookup_blob(dev, value as BlobId) {
            *reference = Some(blob);
            return true;
        }
        return false;
    } else if property_type_is(property, DRM_MODE_PROP_OBJECT) {
        // A zero object-property value means a null object.
        if value == 0 {
            return true;
        }
        let expected_type = property.values[0] as u32;
        let Some(index) = dev.object_index(value as ObjectId) else { return false; };
        if dev.objects[index].object_type != expected_type || !object_get(dev, value as ObjectId) {
            return false;
        }
        *reference = Some(value as ObjectId);
        return true;
    }

    for candidate in &property.values {
        if *candidate == value {
            return true;
        }
    }
    false
}

// upstream: drm_property.c drm_property_change_valid_put()
pub fn drm_property_change_valid_put(
    dev: &mut DrmDevice,
    io: &mut impl PropertyUapiIo,
    property: &DrmProperty,
    reference: Option<ObjectId>,
) {
    let Some(id) = reference else { return; };
    if property_type_is(property, DRM_MODE_PROP_OBJECT) {
        object_put(dev, io, id);
    } else if property_type_is(property, DRM_MODE_PROP_BLOB) {
        drm_property_blob_put(dev, io, Some(id));
    }
}
