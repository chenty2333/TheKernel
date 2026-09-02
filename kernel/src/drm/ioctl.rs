//! Typed DRM ioctl decoding.  Devfs only adapts [`IoctlContext`] to [`UserCopy`].

use alloc::vec::Vec;
use core::{
    mem::{MaybeUninit, size_of},
    slice,
};

use axerrno::{AxError, AxResult};
use thekernel_linux_drm as abi;

use super::{DrmFile, DumbRequest, Mode, dmabuf, property, syncobj, uapi};

const MAX_ATOMIC_OBJECTS: usize = 3;
const MAX_ATOMIC_PROPERTIES: usize = 32;
const MAX_BLOB_BYTES: usize = 4096;
const MAX_SYNCOBJ_HANDLES: usize = 1024;
const XRGB8888: u32 = 0x3432_5258;
const ARGB8888: u32 = 0x3432_5241;
const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
const DRM_MODE_ENCODER_VIRTUAL: u32 = 5;

/// The small, testable user-memory capability needed by DRM ioctl decoding.
pub trait UserCopy {
    fn read(&self, address: usize, dst: &mut [MaybeUninit<u8>]) -> AxResult<()>;
    fn write(&self, address: usize, src: &[u8]) -> AxResult<()>;
}

impl UserCopy for crate::file::IoctlContext {
    fn read(&self, address: usize, dst: &mut [MaybeUninit<u8>]) -> AxResult<()> {
        self.user_memory()
            .read_bytes(address, dst)
            .map_err(crate::mm::map_usercopy_error)
    }
    fn write(&self, address: usize, src: &[u8]) -> AxResult<()> {
        self.user_memory()
            .write_bytes(address, src)
            .map_err(crate::mm::map_usercopy_error)
    }
}

fn read_pod<T: Copy>(copy: &impl UserCopy, address: usize) -> AxResult<T> {
    // `MaybeUninit<T>` avoids a generic-length stack array (not available on
    // stable Rust) while still reserving exactly the ABI record's bytes.
    // Zero first, then let UserCopy overwrite every byte before `assume_init`.
    let mut value = MaybeUninit::<T>::zeroed();
    // SAFETY: `MaybeUninit<u8>` has byte alignment and the range is exactly
    // the allocation occupied by `value`; UserCopy fills the whole range.
    let bytes = unsafe {
        slice::from_raw_parts_mut(value.as_mut_ptr().cast::<MaybeUninit<u8>>(), size_of::<T>())
    };
    copy.read(address, bytes)?;
    // SAFETY: all DRM UAPI records passed here are repr(C) integer records;
    // UserCopy completed an exact-width copy into the zero-initialized value.
    Ok(unsafe { value.assume_init() })
}

fn write_pod<T>(copy: &impl UserCopy, address: usize, value: &T) -> AxResult<()> {
    // SAFETY: DRM UAPI types are repr(C) scalar records and this only exposes
    // their exact ABI-sized representation to the requesting process.
    let bytes = unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy.write(address, bytes)
}

fn write_u32(copy: &impl UserCopy, address: u64, value: u32) -> AxResult<()> {
    let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
    copy.write(address, &value.to_ne_bytes())
}
fn write_u64(copy: &impl UserCopy, address: u64, value: u64) -> AxResult<()> {
    let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
    copy.write(address, &value.to_ne_bytes())
}
fn write_u16(copy: &impl UserCopy, address: u64, value: u16) -> AxResult<()> {
    let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
    copy.write(address, &value.to_ne_bytes())
}
fn write_i32(copy: &impl UserCopy, address: u64, value: i32) -> AxResult<()> {
    let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
    copy.write(address, &value.to_ne_bytes())
}
fn array_at(base: u64, index: usize, element_size: u64) -> AxResult<u64> {
    base.checked_add(
        (index as u64)
            .checked_mul(element_size)
            .ok_or(AxError::BadAddress)?,
    )
    .ok_or(AxError::BadAddress)
}
fn read_array<T: Copy>(
    copy: &impl UserCopy,
    address: u64,
    count: usize,
    max: usize,
) -> AxResult<Vec<T>> {
    if count > max {
        return Err(AxError::InvalidInput);
    }
    let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..count {
        result.push(read_pod(
            copy,
            address
                .checked_add(
                    index
                        .checked_mul(size_of::<T>())
                        .ok_or(AxError::InvalidInput)?,
                )
                .ok_or(AxError::BadAddress)?,
        )?);
    }
    Ok(result)
}
fn read_bytes(copy: &impl UserCopy, address: u64, length: usize, max: usize) -> AxResult<Vec<u8>> {
    if length > max {
        return Err(AxError::InvalidInput);
    }
    let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| AxError::NoMemory)?;
    bytes.resize(length, 0);
    // SAFETY: the initialized byte vector has exactly `length` contiguous bytes.
    let dst =
        unsafe { slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<MaybeUninit<u8>>(), length) };
    copy.read(address, dst)?;
    Ok(bytes)
}

pub(super) fn dispatch(
    file: &DrmFile,
    context: &crate::file::IoctlContext,
    cmd: u32,
    arg: usize,
) -> AxResult<usize> {
    let copy = context;
    let command = cmd as u64;
    // Keep ioctl framing policy in linux-drm.  The individual arms below
    // retain device-specific usercopy and resource operations.
    if !abi::DecodedIoctl::decode(cmd).is_drm() {
        return Err(AxError::InvalidInput);
    }
    match command {
        uapi::DRM_IOCTL_VERSION => version(copy, arg)?,
        uapi::DRM_IOCTL_GET_MAGIC => get_magic(file, copy, arg)?,
        uapi::DRM_IOCTL_AUTH_MAGIC => auth_magic(file, copy, arg)?,
        uapi::DRM_IOCTL_SET_MASTER => file.become_master().map_err(AxError::from)?,
        uapi::DRM_IOCTL_DROP_MASTER => file.drop_master(),
        uapi::DRM_IOCTL_GEM_CLOSE => {
            let request: uapi::DrmGemClose = read_pod(copy, arg)?;
            file.close_handle(request.handle).map_err(AxError::from)?;
        }
        uapi::DRM_IOCTL_MODE_DESTROY_DUMB => destroy_dumb(file, copy, arg)?,
        uapi::DRM_IOCTL_GET_CAP => {
            let mut request: uapi::DrmGetCap = read_pod(copy, arg)?;
            request.value = match request.capability {
                uapi::DRM_CAP_DUMB_BUFFER
                | uapi::DRM_CAP_TIMESTAMP_MONOTONIC
                | uapi::DRM_CAP_SYNCOBJ
                | uapi::DRM_CAP_SYNCOBJ_TIMELINE => 1,
                uapi::DRM_CAP_PRIME => uapi::DRM_PRIME_CAP_IMPORT | uapi::DRM_PRIME_CAP_EXPORT,
                uapi::DRM_CAP_DUMB_PREFERRED_DEPTH => 24,
                uapi::DRM_CAP_DUMB_PREFER_SHADOW => 0,
                _ => return Err(AxError::InvalidInput),
            };
            write_pod(copy, arg, &request)?;
        }
        uapi::DRM_IOCTL_PRIME_HANDLE_TO_FD => prime_handle_to_fd(file, context, arg)?,
        uapi::DRM_IOCTL_PRIME_FD_TO_HANDLE => prime_fd_to_handle(file, context, arg)?,
        uapi::DRM_IOCTL_SYNCOBJ_CREATE => syncobj_create(file, context, arg)?,
        uapi::DRM_IOCTL_SYNCOBJ_DESTROY => syncobj_destroy(file, copy, arg)?,
        uapi::DRM_IOCTL_SYNCOBJ_RESET => syncobj_array(file, copy, arg, false)?,
        uapi::DRM_IOCTL_SYNCOBJ_SIGNAL => syncobj_array(file, copy, arg, true)?,
        uapi::DRM_IOCTL_SYNCOBJ_WAIT => syncobj_wait(file, copy, arg)?,
        uapi::DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD => syncobj_handle_to_fd(file, context, arg)?,
        uapi::DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE => syncobj_fd_to_handle(file, context, arg)?,
        uapi::DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT => syncobj_timeline_wait(file, copy, arg)?,
        uapi::DRM_IOCTL_SYNCOBJ_QUERY => syncobj_query(file, copy, arg)?,
        uapi::DRM_IOCTL_SYNCOBJ_TRANSFER => syncobj_transfer(file, copy, arg)?,
        uapi::DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL => syncobj_timeline_signal(file, copy, arg)?,
        uapi::DRM_IOCTL_SET_CLIENT_CAP => {
            let request: uapi::DrmSetClientCap = read_pod(copy, arg)?;
            match request.capability {
                // The first-stage core has exactly one always-present primary
                // plane; exposing it as a universal plane is truthful.
                uapi::DRM_CLIENT_CAP_UNIVERSAL_PLANES if request.value == 1 => {}
                uapi::DRM_CLIENT_CAP_ATOMIC if request.value == 1 => file.enable_atomic(),
                uapi::DRM_CLIENT_CAP_PLANE_COLOR_PIPELINE | uapi::DRM_CLIENT_CAP_OBJECT_COLOROP => {
                    return Err(AxError::OperationNotSupported);
                }
                _ => return Err(AxError::InvalidInput),
            }
        }
        uapi::DRM_IOCTL_MODE_CREATE_DUMB => {
            let mut request: uapi::DrmModeCreateDumb = read_pod(copy, arg)?;
            if request.flags != 0 {
                return Err(AxError::InvalidInput);
            }
            let dumb = file
                .create_dumb(DumbRequest {
                    width: request.width,
                    height: request.height,
                    bpp: request.bpp,
                })
                .map_err(AxError::from)?;
            request.handle = dumb.handle;
            request.pitch = dumb.pitch;
            request.size = dumb.size;
            write_pod(copy, arg, &request)?;
        }
        uapi::DRM_IOCTL_MODE_MAP_DUMB => {
            let mut request: uapi::DrmModeMapDumb = read_pod(copy, arg)?;
            request.offset = file.map_dumb(request.handle).map_err(AxError::from)?;
            write_pod(copy, arg, &request)?;
        }
        uapi::DRM_IOCTL_MODE_ADDFB2 => {
            let mut request: uapi::DrmModeFbCmd2 = read_pod(copy, arg)?;
            if request.flags & !uapi::DRM_MODE_FB_MODIFIERS != 0
                || request.handles[1..].iter().any(|&id| id != 0)
                || request.pitches[1..].iter().any(|&pitch| pitch != 0)
                || request.offsets[1..].iter().any(|&offset| offset != 0)
                || request.modifier[1..].iter().any(|&modifier| modifier != 0)
                || request.modifier[0] != 0
                || request.pixel_format != XRGB8888 && request.pixel_format != ARGB8888
            {
                return Err(AxError::InvalidInput);
            }
            // The sole supported modifier is DRM_FORMAT_MOD_LINEAR (zero).
            // Keeping this explicit makes a missing MODIFIERS flag harmless
            // and rejects every layout the backing cannot faithfully scan out.
            request.fb_id = file
                .add_framebuffer_format_offset(
                    request.handles[0],
                    request.width,
                    request.height,
                    request.pitches[0],
                    32,
                    request.pixel_format,
                    u64::from(request.offsets[0]),
                )
                .map_err(AxError::from)?;
            write_pod(copy, arg, &request)?;
        }
        uapi::DRM_IOCTL_MODE_ADDFB => addfb(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_GETFB => getfb(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_RMFB => {
            let id: u32 = read_pod(copy, arg)?;
            file.rm_framebuffer(id).map_err(AxError::from)?;
        }
        uapi::DRM_IOCTL_MODE_SETCRTC => set_crtc(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_CURSOR => cursor(file, copy, arg, false)?,
        uapi::DRM_IOCTL_MODE_CURSOR2 => cursor(file, copy, arg, true)?,
        uapi::DRM_IOCTL_MODE_GETGAMMA => get_gamma(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_SETGAMMA => set_gamma(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_GETCRTC => get_crtc(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_GETENCODER => get_encoder(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_GETCONNECTOR => get_connector(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_DIRTYFB => dirtyfb(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_SETPLANE => set_plane(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_PAGE_FLIP => {
            let request: uapi::DrmModeCrtcPageFlip = read_pod(copy, arg)?;
            let resources = file.resources();
            if request.crtc_id != resources.crtc.id
                || request.reserved != 0
                || request.flags & !uapi::DRM_MODE_PAGE_FLIP_EVENT != 0
            {
                return Err(AxError::InvalidInput);
            }
            let changes = [change(
                resources.primary_plane_id,
                property::PLANE_FB_ID,
                request.fb_id as u64,
            )];
            file.submit_legacy_atomic(
                &changes,
                None,
                (request.flags & uapi::DRM_MODE_PAGE_FLIP_EVENT != 0).then_some(request.user_data),
                true,
            )
            .map_err(AxError::from)?;
        }
        uapi::DRM_IOCTL_WAIT_VBLANK => wait_vblank(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_GETRESOURCES => resources(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_GETPLANERESOURCES => plane_resources(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_GETPLANE => get_plane(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_OBJ_GETPROPERTIES => object_properties(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_OBJ_SETPROPERTY => object_set_property(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_SETPROPERTY => connector_set_property(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_GETPROPERTY => get_property(copy, arg)?,
        uapi::DRM_IOCTL_MODE_CREATEPROPBLOB => create_blob(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_GETPROPBLOB => get_blob(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_DESTROYPROPBLOB => destroy_blob(file, copy, arg)?,
        uapi::DRM_IOCTL_MODE_ATOMIC => atomic(file, context, arg)?,
        uapi::DRM_IOCTL_MODE_GETFB2 => getfb2(file, copy, arg)?,
        _ => return Err(AxError::NotATty),
    }
    Ok(0)
}

/// Dispatch the narrow set of DRM core ioctls which Linux makes available on
/// an unprivileged render node, then the driver-specific virgl ABI.  This is
/// intentionally an allowlist: KMS, master/magic, legacy authentication, and
/// all other primary-node controls must remain unreachable through renderD*.
pub(super) fn render_dispatch(
    file: &DrmFile,
    context: &crate::file::IoctlContext,
    cmd: u32,
    arg: usize,
) -> AxResult<usize> {
    if render_allows_core_ioctl(cmd as u64) {
        dispatch(file, context, cmd, arg)
    } else {
        super::render::dispatch(file, context, cmd, arg)
    }
}

const fn render_allows_core_ioctl(command: u64) -> bool {
    matches!(
        command,
        // These are the DRM_RENDER_ALLOW core ioctls implemented here.  They
        // are per-file-handle operations and do not expose KMS or master state.
        uapi::DRM_IOCTL_VERSION
            | uapi::DRM_IOCTL_GET_CAP
            | uapi::DRM_IOCTL_GEM_CLOSE
            | uapi::DRM_IOCTL_PRIME_HANDLE_TO_FD
            | uapi::DRM_IOCTL_PRIME_FD_TO_HANDLE
            | uapi::DRM_IOCTL_SYNCOBJ_CREATE
            | uapi::DRM_IOCTL_SYNCOBJ_DESTROY
            | uapi::DRM_IOCTL_SYNCOBJ_RESET
            | uapi::DRM_IOCTL_SYNCOBJ_SIGNAL
            | uapi::DRM_IOCTL_SYNCOBJ_WAIT
            | uapi::DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD
            | uapi::DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE
            | uapi::DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT
            | uapi::DRM_IOCTL_SYNCOBJ_QUERY
            | uapi::DRM_IOCTL_SYNCOBJ_TRANSFER
            | uapi::DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL
    )
}

fn destroy_dumb(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    // drm_mode_destroy_dumb is exactly one u32.  Do not decode it as
    // drm_gem_close: a valid four-byte userspace buffer can end at a page
    // boundary, where an erroneous eight-byte read would return EFAULT.
    let request: uapi::DrmModeDestroyDumb = read_pod(copy, arg)?;
    file.close_handle(request.handle).map_err(AxError::from)
}

fn fd_flags(flags: u32) -> AxResult<bool> {
    if flags & !(uapi::DRM_CLOEXEC | uapi::DRM_RDWR) != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(flags & uapi::DRM_CLOEXEC != 0)
}

fn addfb(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeFbCmd = read_pod(copy, arg)?;
    if r.bpp != 32 || !matches!(r.depth, 24 | 32) {
        return Err(AxError::InvalidInput);
    }
    r.fb_id = file
        .add_framebuffer_format_offset(
            r.handle,
            r.width,
            r.height,
            r.pitch,
            r.bpp,
            if r.depth == 24 { XRGB8888 } else { ARGB8888 },
            0,
        )
        .map_err(AxError::from)?;
    write_pod(copy, arg, &r)
}

fn getfb(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeFbCmd = read_pod(copy, arg)?;
    let fb = file.framebuffer(r.fb_id).map_err(AxError::from)?;
    if fb.owner != file.id() {
        return Err(AxError::PermissionDenied);
    }
    r.width = fb.width;
    r.height = fb.height;
    r.pitch = fb.pitch;
    r.bpp = fb.bpp;
    r.depth = 24;
    r.handle = fb.handle;
    write_pod(copy, arg, &r)
}

fn write_truncated(
    copy: &impl UserCopy,
    address: u64,
    capacity: u64,
    bytes: &[u8],
) -> AxResult<()> {
    if address != 0 && capacity != 0 {
        let length = usize::try_from(capacity)
            .map_err(|_| AxError::InvalidInput)?
            .min(bytes.len());
        copy.write(
            usize::try_from(address).map_err(|_| AxError::BadAddress)?,
            &bytes[..length],
        )?;
    }
    Ok(())
}

fn version(copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmVersion = read_pod(copy, arg)?;
    const NAME: &[u8] = b"virtio_gpu";
    const DATE: &[u8] = b"20260830";
    const DESC: &[u8] = b"TheKernel virtio GPU";
    write_truncated(copy, r.name, r.name_len, NAME)?;
    write_truncated(copy, r.date, r.date_len, DATE)?;
    write_truncated(copy, r.desc, r.desc_len, DESC)?;
    r.version_major = 0;
    r.version_minor = 1;
    r.version_patchlevel = 0;
    r.name_len = NAME.len() as u64;
    r.date_len = DATE.len() as u64;
    r.desc_len = DESC.len() as u64;
    write_pod(copy, arg, &r)
}

fn get_magic(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    if file.is_render_node() {
        return Err(AxError::OperationNotSupported);
    }
    let mut r: uapi::DrmAuth = read_pod(copy, arg)?;
    r.magic = u32::try_from(file.id()).map_err(|_| AxError::InvalidInput)?;
    if r.magic == 0 || !file.has_open_id(r.magic as u64) {
        return Err(AxError::InvalidInput);
    }
    write_pod(copy, arg, &r)
}

fn auth_magic(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    if file.is_render_node() {
        return Err(AxError::OperationNotSupported);
    }
    let r: uapi::DrmAuth = read_pod(copy, arg)?;
    file.require_master().map_err(AxError::from)?;
    // Primary-node clients are already trusted by the devfs open policy.  We
    // still enforce the legacy master's authority and reject the invalid zero
    // token; there is no render-node authentication path in this device.
    if r.magic == 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}
fn prime_handle_to_fd(
    file: &DrmFile,
    context: &crate::file::IoctlContext,
    arg: usize,
) -> AxResult<()> {
    let copy = context;
    let mut request: uapi::DrmPrimeHandle = read_pod(copy, arg)?;
    let cloexec = fd_flags(request.flags)?;
    let object = file.gem(request.handle).map_err(AxError::from)?;
    request.fd = dmabuf::export(object, context, cloexec)?;
    write_pod(copy, arg, &request)
}
fn prime_fd_to_handle(
    file: &DrmFile,
    context: &crate::file::IoctlContext,
    arg: usize,
) -> AxResult<()> {
    let copy = context;
    let mut request: uapi::DrmPrimeHandle = read_pod(copy, arg)?;
    if request.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let object = dmabuf::import(context, request.fd)?;
    request.handle = file.import_gem(object).map_err(AxError::from)?;
    write_pod(copy, arg, &request)
}

fn syncobj_create(file: &DrmFile, context: &crate::file::IoctlContext, arg: usize) -> AxResult<()> {
    let mut request: uapi::DrmSyncobjCreate = read_pod(context, arg)?;
    let signaled = request.validate().map_err(|_| AxError::InvalidInput)?;
    request.handle = file.create_syncobj(signaled).map_err(AxError::from)?;
    write_pod(context, arg, &request)
}

fn syncobj_destroy(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let request: uapi::DrmSyncobjDestroy = read_pod(copy, arg)?;
    if request.pad != 0 {
        return Err(AxError::InvalidInput);
    }
    let handle = request.validate().map_err(|_| AxError::InvalidInput)?;
    file.destroy_syncobj(handle).map_err(AxError::from)
}

fn syncobj_array(file: &DrmFile, copy: &impl UserCopy, arg: usize, signal: bool) -> AxResult<()> {
    let request: uapi::DrmSyncobjArray = read_pod(copy, arg)?;
    if request.pad != 0 || request.count_handles == 0 {
        return Err(AxError::InvalidInput);
    }
    let handles = read_array::<u32>(
        copy,
        request.handles,
        request.count_handles as usize,
        MAX_SYNCOBJ_HANDLES,
    )?;
    let mut objects = Vec::new();
    objects
        .try_reserve_exact(handles.len())
        .map_err(|_| AxError::NoMemory)?;
    for handle in handles {
        objects.push(file.syncobj(handle).map_err(AxError::from)?);
    }
    for object in objects {
        if signal {
            object.signal();
        } else {
            object.reset();
        }
    }
    Ok(())
}

fn syncobj_wait(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut request: uapi::DrmSyncobjWait = read_pod(copy, arg)?;
    let allowed = uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL
        | uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT
        | uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE;
    if request.count_handles == 0 || request.flags & !allowed != 0 || request.pad != 0 {
        return Err(AxError::InvalidInput);
    }
    let deadline_hint = syncobj_deadline_hint(request.flags, request.deadline_nsec)?;
    let handles = read_array::<u32>(
        copy,
        request.handles,
        request.count_handles as usize,
        MAX_SYNCOBJ_HANDLES,
    )?;
    let mut objects = Vec::new();
    objects
        .try_reserve_exact(handles.len())
        .map_err(|_| AxError::NoMemory)?;
    for handle in handles {
        objects.push((file.syncobj(handle).map_err(AxError::from)?, 0));
    }
    request.first_signaled = syncobj::timeline_wait(
        &objects,
        request.flags & uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0,
        request.flags & uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT != 0,
        false,
        request.timeout_nsec,
        deadline_hint,
    )? as u32;
    write_pod(copy, arg, &request)
}

fn syncobj_handle_to_fd(
    file: &DrmFile,
    context: &crate::file::IoctlContext,
    arg: usize,
) -> AxResult<()> {
    let mut request: uapi::DrmSyncobjHandle = read_pod(context, arg)?;
    if request.pad != 0 {
        return Err(AxError::InvalidInput);
    }
    let object = file.syncobj(request.handle).map_err(AxError::from)?;
    request.fd = match request.flags {
        0 => syncobj::export_syncobj(object, context, false)?,
        uapi::DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE => {
            syncobj::export(object.fence()?, context, false)?
        }
        _ => return Err(AxError::InvalidInput),
    };
    write_pod(context, arg, &request)
}

fn syncobj_fd_to_handle(
    file: &DrmFile,
    context: &crate::file::IoctlContext,
    arg: usize,
) -> AxResult<()> {
    let mut request: uapi::DrmSyncobjHandle = read_pod(context, arg)?;
    if request.pad != 0 {
        return Err(AxError::InvalidInput);
    }
    match request.flags {
        0 => {
            request.handle = file
                .import_syncobj(syncobj::import_syncobj(context, request.fd)?)
                .map_err(AxError::from)?;
        }
        uapi::DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE => {
            file.syncobj(request.handle)
                .map_err(AxError::from)?
                .import_fence(syncobj::import(context, request.fd)?);
        }
        _ => return Err(AxError::InvalidInput),
    }
    write_pod(context, arg, &request)
}

fn syncobj_timeline_wait(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut request: uapi::DrmSyncobjTimelineWait = read_pod(copy, arg)?;
    let allowed = uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL
        | uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT
        | uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE
        | uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE;
    if request.count_handles == 0 || request.flags & !allowed != 0 || request.pad != 0 {
        return Err(AxError::InvalidInput);
    }
    let deadline_hint = syncobj_deadline_hint(request.flags, request.deadline_nsec)?;
    let handles = read_array::<u32>(
        copy,
        request.handles,
        request.count_handles as usize,
        MAX_SYNCOBJ_HANDLES,
    )?;
    let points = read_array::<u64>(
        copy,
        request.points,
        request.count_handles as usize,
        MAX_SYNCOBJ_HANDLES,
    )?;
    let mut objects = Vec::new();
    objects
        .try_reserve_exact(handles.len())
        .map_err(|_| AxError::NoMemory)?;
    for (handle, point) in handles.into_iter().zip(points) {
        objects.push((file.syncobj(handle).map_err(AxError::from)?, point));
    }
    request.first_signaled = syncobj::timeline_wait(
        &objects,
        request.flags & uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0,
        request.flags & uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT != 0,
        request.flags & uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0,
        request.timeout_nsec,
        deadline_hint,
    )? as u32;
    write_pod(copy, arg, &request)
}

fn syncobj_query(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let request: uapi::DrmSyncobjTimelineArray = read_pod(copy, arg)?;
    if request.count_handles == 0
        || request.flags & !uapi::DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0
    {
        return Err(AxError::InvalidInput);
    }
    let handles = read_array::<u32>(
        copy,
        request.handles,
        request.count_handles as usize,
        MAX_SYNCOBJ_HANDLES,
    )?;
    for (index, handle) in handles.into_iter().enumerate() {
        let point = file
            .syncobj(handle)
            .map_err(AxError::from)?
            .query_point(request.flags & uapi::DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0);
        write_u64(
            copy,
            array_at(request.points, index, size_of::<u64>() as u64)?,
            point,
        )?;
    }
    Ok(())
}

fn syncobj_transfer(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let request: uapi::DrmSyncobjTransfer = read_pod(copy, arg)?;
    if request.flags & !uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT != 0 || request.pad != 0 {
        return Err(AxError::InvalidInput);
    }
    let source = file.syncobj(request.src_handle).map_err(AxError::from)?;
    let destination = file.syncobj(request.dst_handle).map_err(AxError::from)?;
    let fence = source.wait_fence_for_submit(
        request.src_point,
        request.flags & uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT != 0,
        None,
    )?;
    destination.submit_point(request.dst_point, fence)
}

fn syncobj_deadline_hint(
    flags: u32,
    deadline_nsec: u64,
) -> AxResult<Option<axhal::time::TimeValue>> {
    if flags & uapi::DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE == 0 {
        if deadline_nsec != 0 {
            return Err(AxError::InvalidInput);
        }
        return Ok(None);
    }
    if deadline_nsec == 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(Some(core::time::Duration::from_nanos(deadline_nsec)))
}

fn syncobj_timeline_signal(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let request: uapi::DrmSyncobjTimelineArray = read_pod(copy, arg)?;
    if request.count_handles == 0 || request.flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let handles = read_array::<u32>(
        copy,
        request.handles,
        request.count_handles as usize,
        MAX_SYNCOBJ_HANDLES,
    )?;
    let points = read_array::<u64>(
        copy,
        request.points,
        request.count_handles as usize,
        MAX_SYNCOBJ_HANDLES,
    )?;
    let mut objects = Vec::new();
    objects
        .try_reserve_exact(handles.len())
        .map_err(|_| AxError::NoMemory)?;
    for (handle, point) in handles.into_iter().zip(points) {
        objects.push((file.syncobj(handle).map_err(AxError::from)?, point));
    }
    for (object, point) in objects {
        object.signal_point(point)?;
    }
    Ok(())
}

fn plane_resources(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeGetPlaneRes = read_pod(copy, arg)?;
    let capacity = r.count_planes;
    r.count_planes = 2;
    if r.plane_id_ptr != 0 && capacity != 0 {
        write_u32(copy, r.plane_id_ptr, file.resources().primary_plane_id)?;
        if capacity > 1 {
            write_u32(
                copy,
                r.plane_id_ptr.checked_add(4).ok_or(AxError::BadAddress)?,
                file.resources().cursor_plane_id,
            )?;
        }
    }
    write_pod(copy, arg, &r)
}
fn get_plane(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeGetPlane = read_pod(copy, arg)?;
    let x = file.resources();
    if r.plane_id != x.primary_plane_id && r.plane_id != x.cursor_plane_id {
        return Err(AxError::InvalidInput);
    }
    let s = file.device_state();
    let cursor = r.plane_id == x.cursor_plane_id;
    r.crtc_id = if cursor {
        s.atomic.cursor_crtc
    } else {
        s.atomic.plane_crtc
    };
    r.fb_id = if cursor {
        s.atomic.cursor_fb
    } else {
        s.atomic.fb
    };
    r.possible_crtcs = 1;
    r.gamma_size = 0;
    r.count_format_types = if cursor { 1 } else { 2 };
    drop(s);
    if r.format_type_ptr != 0 {
        write_u32(
            copy,
            r.format_type_ptr,
            if cursor { 0x3432_5241 } else { 0x3432_5258 },
        )?;
        if !cursor {
            write_u32(
                copy,
                r.format_type_ptr
                    .checked_add(4)
                    .ok_or(AxError::BadAddress)?,
                0x3432_5241,
            )?;
        }
    }
    write_pod(copy, arg, &r)
}
fn set_plane(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let r: uapi::DrmModeSetPlane = read_pod(copy, arg)?;
    let resources = file.resources();
    if r.plane_id != resources.primary_plane_id
        || r.crtc_id != resources.crtc.id
        || r.flags != 0
        || r.crtc_x != 0
        || r.crtc_y != 0
        || r.src_x != 0
        || r.src_y != 0
        || r.fb_id == 0
        || r.crtc_w == 0
        || r.crtc_h == 0
        || r.src_w != r.crtc_w.checked_shl(16).ok_or(AxError::InvalidInput)?
        || r.src_h != r.crtc_h.checked_shl(16).ok_or(AxError::InvalidInput)?
    {
        return Err(AxError::InvalidInput);
    }
    // SETPLANE changes plane geometry only.  In particular it never invents
    // a CRTC mode from the destination dimensions; the shared atomic proposal
    // below validates this full-frame linear geometry against the active mode.
    let changes = [
        change(
            resources.primary_plane_id,
            property::PLANE_FB_ID,
            r.fb_id as u64,
        ),
        change(
            resources.primary_plane_id,
            property::PLANE_CRTC_ID,
            r.crtc_id as u64,
        ),
        change(
            resources.primary_plane_id,
            property::PLANE_SRC_X,
            r.src_x as u64,
        ),
        change(
            resources.primary_plane_id,
            property::PLANE_SRC_Y,
            r.src_y as u64,
        ),
        change(
            resources.primary_plane_id,
            property::PLANE_SRC_W,
            r.src_w as u64,
        ),
        change(
            resources.primary_plane_id,
            property::PLANE_SRC_H,
            r.src_h as u64,
        ),
        change(
            resources.primary_plane_id,
            property::PLANE_CRTC_X,
            r.crtc_x as u64,
        ),
        change(
            resources.primary_plane_id,
            property::PLANE_CRTC_Y,
            r.crtc_y as u64,
        ),
        change(
            resources.primary_plane_id,
            property::PLANE_CRTC_W,
            r.crtc_w as u64,
        ),
        change(
            resources.primary_plane_id,
            property::PLANE_CRTC_H,
            r.crtc_h as u64,
        ),
    ];
    file.submit_legacy_atomic(&changes, None, None, false)
        .map_err(AxError::from)
}

fn getfb2(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeFbCmd2 = read_pod(copy, arg)?;
    let fb = file.framebuffer(r.fb_id).map_err(AxError::from)?;
    if fb.owner != file.id() {
        return Err(AxError::PermissionDenied);
    }
    r.width = fb.width;
    r.height = fb.height;
    r.pixel_format = fb.format;
    r.flags = 0;
    r.handles = [0; 4];
    r.handles[0] = fb.handle;
    r.pitches = [0; 4];
    r.pitches[0] = fb.pitch;
    r.offsets = [0; 4];
    r.offsets[0] = u32::try_from(fb.offset).map_err(|_| AxError::InvalidInput)?;
    r.modifier = [0; 4];
    write_pod(copy, arg, &r)
}

fn object_set_property(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let r: uapi::DrmModeObjSetProperty = read_pod(copy, arg)?;
    file.set_legacy_property(r.obj_id, r.obj_type, r.prop_id, r.value)
        .map_err(AxError::from)
}
fn connector_set_property(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let r: uapi::DrmModeConnectorSetProperty = read_pod(copy, arg)?;
    file.set_legacy_property(
        r.connector_id,
        uapi::DRM_MODE_OBJECT_CONNECTOR,
        r.prop_id,
        r.value,
    )
    .map_err(AxError::from)
}
fn object_properties(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeObjGetProperties = read_pod(copy, arg)?;
    let resources = file.resources();
    let valid = match r.obj_type {
        uapi::DRM_MODE_OBJECT_CONNECTOR => r.obj_id == resources.connector.id,
        uapi::DRM_MODE_OBJECT_CRTC => r.obj_id == resources.crtc.id,
        uapi::DRM_MODE_OBJECT_PLANE => {
            r.obj_id == resources.primary_plane_id || r.obj_id == resources.cursor_plane_id
        }
        _ => false,
    };
    if !valid {
        return Err(AxError::InvalidInput);
    };
    // IN_FENCE_FD is implemented by the scanout plane only.  The cursorq
    // completion is internal to a single transaction and has no user-facing
    // plane-fence ABI.
    let ids: Vec<u32> = property::object_properties(r.obj_type)
        .iter()
        .copied()
        .filter(|id| r.obj_id != resources.cursor_plane_id || *id != property::PLANE_IN_FENCE_FD)
        .collect();
    r.count_props = ids.len() as u32;
    let state = file.device_state().atomic;
    if r.props_ptr != 0 {
        for (i, id) in ids.iter().enumerate() {
            write_u32(copy, array_at(r.props_ptr, i, 4)?, *id)?;
        }
    }
    if r.prop_values_ptr != 0 {
        for (i, id) in ids.iter().enumerate() {
            write_u64(
                copy,
                array_at(r.prop_values_ptr, i, 8)?,
                super::atomic::value_for_object(&resources, &state, r.obj_id, *id)
                    .ok_or(AxError::InvalidInput)?,
            )?;
        }
    }
    write_pod(copy, arg, &r)
}
fn get_property(copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeGetProperty = read_pod(copy, arg)?;
    let p = property::get(r.prop_id).ok_or(AxError::InvalidInput)?;
    r.flags = p.flags;
    r.name = [0; uapi::DRM_PROP_NAME_LEN];
    let n = p.name.as_bytes();
    r.name[..n.len()].copy_from_slice(n);
    r.count_values =
        if p.flags & (uapi::DRM_MODE_PROP_RANGE | uapi::DRM_MODE_PROP_SIGNED_RANGE) != 0 {
            2
        } else {
            0
        };
    r.count_enum_blobs = match p.id {
        property::PLANE_TYPE => 2,
        property::CONNECTOR_DPMS => 4,
        _ => 0,
    };
    if r.values_ptr != 0 && r.count_values != 0 {
        write_u64(copy, r.values_ptr, p.min)?;
        write_u64(copy, r.values_ptr + 8, p.max)?;
    }
    if r.enum_blob_ptr != 0 {
        let enums: &[(u64, &[u8])] = match p.id {
            property::PLANE_TYPE => &[(1, b"Primary"), (2, b"Cursor")],
            property::CONNECTOR_DPMS => {
                &[(0, b"On"), (1, b"Standby"), (2, b"Suspend"), (3, b"Off")]
            }
            _ => &[],
        };
        for (index, (value, name)) in enums.iter().enumerate() {
            let mut entry = uapi::DrmModePropertyEnum {
                value: *value,
                name: [0; uapi::DRM_PROP_NAME_LEN],
            };
            entry.name[..name.len()].copy_from_slice(name);
            write_pod(
                copy,
                usize::try_from(array_at(
                    r.enum_blob_ptr,
                    index,
                    size_of::<uapi::DrmModePropertyEnum>() as u64,
                )?)
                .map_err(|_| AxError::BadAddress)?,
                &entry,
            )?;
        }
    }
    write_pod(copy, arg, &r)
}
fn create_blob(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeCreateBlob = read_pod(copy, arg)?;
    r.blob_id = file
        .create_blob(read_bytes(copy, r.data, r.length as usize, MAX_BLOB_BYTES)?)
        .map_err(AxError::from)?;
    write_pod(copy, arg, &r)
}
fn get_blob(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeGetBlob = read_pod(copy, arg)?;
    let b = file.blob(r.blob_id).ok_or(AxError::NotFound)?;
    let capacity = r.length as usize;
    r.length = b.len() as u32;
    if r.data != 0 {
        let a = usize::try_from(r.data).map_err(|_| AxError::BadAddress)?;
        copy.write(a, &b[..b.len().min(capacity)])?;
    }
    write_pod(copy, arg, &r)
}
fn destroy_blob(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let r: uapi::DrmModeDestroyBlob = read_pod(copy, arg)?;
    file.destroy_blob(r.blob_id).map_err(AxError::from)
}
fn atomic(file: &DrmFile, context: &crate::file::IoctlContext, arg: usize) -> AxResult<()> {
    let copy = context;
    let r: uapi::DrmModeAtomic = read_pod(copy, arg)?;
    if !file.atomic_enabled()
        || r.reserved != 0
        || r.flags
            & !(uapi::DRM_MODE_PAGE_FLIP_EVENT
                | uapi::DRM_MODE_ATOMIC_TEST_ONLY
                | uapi::DRM_MODE_ATOMIC_NONBLOCK
                | uapi::DRM_MODE_ATOMIC_ALLOW_MODESET)
            != 0
    {
        return Err(AxError::InvalidInput);
    };
    r.validate(MAX_ATOMIC_OBJECTS as u32)
        .map_err(|_| AxError::InvalidInput)?;
    file.require_master().map_err(AxError::from)?;
    let objects = read_array::<u32>(copy, r.objs_ptr, r.count_objs as usize, MAX_ATOMIC_OBJECTS)?;
    let counts = read_array::<u32>(
        copy,
        r.count_props_ptr,
        r.count_objs as usize,
        MAX_ATOMIC_OBJECTS,
    )?;
    let total = counts.iter().try_fold(0usize, |x, y| {
        x.checked_add(*y as usize).ok_or(AxError::InvalidInput)
    })?;
    if total > MAX_ATOMIC_PROPERTIES {
        return Err(AxError::InvalidInput);
    }
    let props = read_array::<u32>(copy, r.props_ptr, total, MAX_ATOMIC_PROPERTIES)?;
    let values = read_array::<u64>(copy, r.prop_values_ptr, total, MAX_ATOMIC_PROPERTIES)?;
    let mut changes = Vec::new();
    changes
        .try_reserve_exact(total)
        .map_err(|_| AxError::NoMemory)?;
    let mut at = 0;
    for (o, n) in objects.iter().zip(counts) {
        for _ in 0..n {
            changes.push(super::atomic::Change {
                object: *o,
                property: props[at],
                value: values[at],
            });
            at += 1;
        }
    }
    let mode_id = changes
        .iter()
        .rev()
        .find(|x| x.property == property::CRTC_MODE_ID)
        .map(|x| x.value as u32);
    let mode_blob = match mode_id {
        Some(0) | None => None,
        Some(id) => Some((
            id,
            mode_from_blob(&file.live_blob(id).ok_or(AxError::NotFound)?)?,
        )),
    };
    let (generation, current, next, fb) =
        super::atomic::propose(file, &changes, mode_blob).map_err(AxError::from)?;
    if r.flags & uapi::DRM_MODE_ATOMIC_ALLOW_MODESET == 0
        && (current.active != next.active
            || current.mode != next.mode
            || current.connector_crtc != next.connector_crtc)
    {
        return Err(AxError::InvalidInput);
    }
    // These two standard atomic properties are request-local and never
    // become part of `State`.  Importing first validates a sync_file without
    // publishing a dependency or touching the framebuffer reservation.
    let in_fence = changes
        .iter()
        .rev()
        .find(|change| change.property == property::PLANE_IN_FENCE_FD)
        .map(|change| change.value);
    let mut inputs = Vec::new();
    if let Some(value) = in_fence {
        if value != u64::MAX {
            let fd = i32::try_from(value).map_err(|_| AxError::InvalidInput)?;
            inputs.push(syncobj::import(context, fd)?);
        }
    }
    // Atomic check has no reservation side effects. submit_atomic performs
    // the exact predecessor replacement at commit admission, after all
    // request validation and user copies have completed.
    let predecessors = Vec::new();
    let out_fence_ptr = changes
        .iter()
        .rev()
        .find(|change| change.property == property::CRTC_OUT_FENCE_PTR)
        .map(|change| change.value)
        .unwrap_or(0);
    if r.flags & uapi::DRM_MODE_ATOMIC_TEST_ONLY != 0 {
        return Ok(());
    }
    let completion = super::fence::Fence::new(false);
    let exported_fd = if out_fence_ptr != 0 {
        let fd = syncobj::export(completion.clone(), context, false)?;
        if let Err(error) = write_i32(copy, out_fence_ptr, fd) {
            completion.signal_error();
            let _ = crate::file::close_file_like(fd);
            return Err(error);
        }
        Some(fd)
    } else {
        None
    };
    let result = file.submit_atomic(
        generation,
        next,
        fb,
        (r.flags & uapi::DRM_MODE_PAGE_FLIP_EVENT != 0).then_some(r.user_data),
        r.flags & uapi::DRM_MODE_ATOMIC_NONBLOCK != 0,
        super::file::AtomicSync {
            inputs,
            predecessors,
            completion: Some(completion),
        },
    );
    if let Err(error) = result {
        if let Some(fd) = exported_fd {
            let _ = crate::file::close_file_like(fd);
        }
        return Err(AxError::from(error));
    }
    Ok(())
}

fn mode_from_blob(b: &[u8]) -> AxResult<Mode> {
    if b.len() != core::mem::size_of::<uapi::DrmModeModeInfo>() {
        return Err(AxError::InvalidInput);
    }
    let width = u16::from_ne_bytes([b[4], b[5]]) as u32;
    // hskew is at byte 12; vdisplay follows at byte 14.
    let height = u16::from_ne_bytes([b[14], b[15]]) as u32;
    let refresh = u32::from_ne_bytes([b[24], b[25], b[26], b[27]]);
    if width == 0 || height == 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(Mode {
        width,
        height,
        refresh_millihz: refresh.saturating_mul(1000),
    })
}

fn resources(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut request: uapi::DrmModeCardRes = read_pod(copy, arg)?;
    let resources = file.resources();
    let state = file.device_state();
    let mut framebuffer_ids = Vec::new();
    framebuffer_ids
        .try_reserve_exact(state.framebuffers.len())
        .map_err(|_| AxError::NoMemory)?;
    framebuffer_ids.extend(state.framebuffers.keys().copied());
    drop(state);
    let fb_capacity = request.count_fbs as usize;
    let crtc_capacity = request.count_crtcs as usize;
    let connector_capacity = request.count_connectors as usize;
    let encoder_capacity = request.count_encoders as usize;
    if request.fb_id_ptr != 0 {
        for (index, id) in framebuffer_ids.iter().take(fb_capacity).enumerate() {
            write_u32(copy, array_at(request.fb_id_ptr, index, 4)?, *id)?;
        }
    }
    if request.crtc_id_ptr != 0 && crtc_capacity != 0 {
        write_u32(copy, request.crtc_id_ptr, resources.crtc.id)?;
    }
    if request.connector_id_ptr != 0 && connector_capacity != 0 {
        write_u32(copy, request.connector_id_ptr, resources.connector.id)?;
    }
    if request.encoder_id_ptr != 0 && encoder_capacity != 0 {
        write_u32(copy, request.encoder_id_ptr, resources.encoder_id)?;
    }
    request.count_fbs = framebuffer_ids.len() as u32;
    request.count_crtcs = 1;
    request.count_connectors = 1;
    request.count_encoders = 1;
    request.min_width = 1;
    request.min_height = 1;
    request.max_width = u32::MAX;
    request.max_height = u32::MAX;
    write_pod(copy, arg, &request)
}

fn mode_info(mode: Mode) -> uapi::DrmModeModeInfo {
    // A fixed VESA-like timing is sufficient for the virtual scanout.  The
    // active dimensions are retained so GETCRTC reports the committed state.
    let hdisplay = mode.width.min(u16::MAX as u32) as u16;
    let vdisplay = mode.height.min(u16::MAX as u32) as u16;
    let hsync_start = hdisplay.saturating_add(24);
    let hsync_end = hsync_start.saturating_add(136);
    let htotal = hsync_end.saturating_add(160);
    let vsync_start = vdisplay.saturating_add(3);
    let vsync_end = vsync_start.saturating_add(6);
    let vtotal = vsync_end.saturating_add(29);
    let refresh = (mode.refresh_millihz / 1000).max(1);
    let clock = (u32::from(htotal) * u32::from(vtotal) * refresh) / 1000;
    let mut name = [0; uapi::DRM_DISPLAY_MODE_LEN];
    let mut decimal = [0u8; 10];
    let mut n = 0;
    let mut value = refresh;
    loop {
        decimal[n] = b'0' + (value % 10) as u8;
        n += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let mut at = 0;
    for (dimension, value) in [mode.width, mode.height].into_iter().enumerate() {
        let mut digits = [0u8; 10];
        let mut count = 0;
        let mut current = value;
        loop {
            digits[count] = b'0' + (current % 10) as u8;
            count += 1;
            current /= 10;
            if current == 0 {
                break;
            }
        }
        for digit in digits[..count].iter().rev() {
            name[at] = *digit;
            at += 1;
        }
        if dimension == 0 {
            name[at] = b'x';
            at += 1;
        }
    }
    name[at] = b'@';
    at += 1;
    for digit in decimal[..n].iter().rev() {
        name[at] = *digit;
        at += 1;
    }
    uapi::DrmModeModeInfo {
        clock,
        hdisplay,
        hsync_start,
        hsync_end,
        htotal,
        hskew: 0,
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        vscan: 0,
        vrefresh: refresh,
        flags: 0,
        type_: 1,
        name,
    }
}

fn advertised_mode(file: &DrmFile) -> Mode {
    let resources = file.resources();
    resources.crtc.mode.unwrap_or(resources.preferred_mode)
}

fn get_crtc(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeCrtc = read_pod(copy, arg)?;
    let resources = file.resources();
    if r.crtc_id != resources.crtc.id {
        return Err(AxError::InvalidInput);
    }
    let capacity = r.count_connectors as usize;
    let active = resources.crtc.framebuffer.is_some() && resources.crtc.mode.is_some();
    if active && r.set_connectors_ptr != 0 && capacity != 0 {
        write_u32(copy, r.set_connectors_ptr, resources.connector.id)?;
    }
    r.count_connectors = active as u32;
    r.fb_id = resources.crtc.framebuffer.unwrap_or(0);
    r.x = 0;
    r.y = 0;
    r.gamma_size = file
        .gamma_lut(resources.crtc.id)
        .map_err(AxError::from)?
        .len() as u32
        / 3;
    r.mode_valid = active as u32;
    r.mode = resources.crtc.mode.map(mode_info).unwrap_or_default();
    write_pod(copy, arg, &r)
}

fn get_encoder(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeGetEncoder = read_pod(copy, arg)?;
    let resources = file.resources();
    if r.encoder_id != resources.encoder_id {
        return Err(AxError::InvalidInput);
    }
    r.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
    r.crtc_id = resources.crtc.id;
    r.possible_crtcs = 1;
    r.possible_clones = 0;
    write_pod(copy, arg, &r)
}

fn get_connector(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeGetConnector = read_pod(copy, arg)?;
    let resources = file.resources();
    if r.connector_id != resources.connector.id {
        return Err(AxError::InvalidInput);
    }
    let encoders_capacity = r.count_encoders as usize;
    let modes_capacity = r.count_modes as usize;
    let props_capacity = r.count_props as usize;
    let properties = property::object_properties(uapi::DRM_MODE_OBJECT_CONNECTOR);
    if r.encoders_ptr != 0 && encoders_capacity != 0 {
        write_u32(copy, r.encoders_ptr, resources.encoder_id)?;
    }
    if resources.connector.connected && r.modes_ptr != 0 {
        for (index, mode) in resources.modes.iter().take(modes_capacity).enumerate() {
            write_pod(
                copy,
                usize::try_from(array_at(
                    r.modes_ptr,
                    index,
                    size_of::<uapi::DrmModeModeInfo>() as u64,
                )?)
                .map_err(|_| AxError::BadAddress)?,
                &mode_info(*mode),
            )?;
        }
    }
    let atomic = file.device_state().atomic;
    if r.props_ptr != 0 {
        for (index, id) in properties.iter().take(props_capacity).enumerate() {
            write_u32(copy, array_at(r.props_ptr, index, 4)?, *id)?;
        }
    }
    if r.prop_values_ptr != 0 {
        for (index, id) in properties.iter().take(props_capacity).enumerate() {
            write_u64(
                copy,
                array_at(r.prop_values_ptr, index, 8)?,
                super::atomic::value_with_resources(&resources, &atomic, *id)
                    .ok_or(AxError::InvalidInput)?,
            )?;
        }
    }
    r.count_encoders = 1;
    r.count_modes = resources.connector.connected as u32 * resources.modes.len() as u32;
    r.count_props = properties.len() as u32;
    r.encoder_id = resources.encoder_id;
    r.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
    r.connector_type_id = 1;
    r.connection = if resources.connector.connected {
        uapi::DRM_MODE_CONNECTED
    } else {
        uapi::DRM_MODE_DISCONNECTED
    };
    r.mm_width = 0;
    r.mm_height = 0;
    r.subpixel = 0;
    r.pad = 0;
    write_pod(copy, arg, &r)
}

/// Legacy cursor ioctls are translated directly to the dedicated cursor
/// transport.  The primary plane remains the only atomic plane: VirtIO's
/// cursorq has no atomic plane state and must never be exposed as one.
fn cursor(file: &DrmFile, copy: &impl UserCopy, arg: usize, cursor2: bool) -> AxResult<()> {
    let (flags, crtc_id, x, y, width, height, handle, hot_x, hot_y) = if cursor2 {
        let request: uapi::DrmModeCursor2 = read_pod(copy, arg)?;
        if request.hot_x < 0 || request.hot_y < 0 {
            return Err(AxError::InvalidInput);
        }
        (
            request.flags,
            request.crtc_id,
            request.x,
            request.y,
            request.width,
            request.height,
            request.handle,
            request.hot_x as u32,
            request.hot_y as u32,
        )
    } else {
        let request: uapi::DrmModeCursor = read_pod(copy, arg)?;
        (
            request.flags,
            request.crtc_id,
            request.x,
            request.y,
            request.width,
            request.height,
            request.handle,
            0,
            0,
        )
    };
    let resources = file.resources();
    if crtc_id != resources.crtc.id || flags == 0 || flags & !uapi::DRM_MODE_CURSOR_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    let plane = resources.cursor_plane_id;
    let changes = if flags & uapi::DRM_MODE_CURSOR_BO != 0 {
        if handle == 0 || width != 64 || height != 64 || hot_x >= 64 || hot_y >= 64 {
            return Err(AxError::InvalidInput);
        }
        [
            change(
                plane,
                property::PLANE_FB_ID,
                file.cursor_framebuffer(handle).map_err(AxError::from)? as u64,
            ),
            change(plane, property::PLANE_CRTC_ID, resources.crtc.id as u64),
            change(plane, property::PLANE_SRC_X, 0),
            change(plane, property::PLANE_SRC_Y, 0),
            change(plane, property::PLANE_SRC_W, 64 << 16),
            change(plane, property::PLANE_SRC_H, 64 << 16),
            change(plane, property::PLANE_CRTC_X, x as u32 as u64),
            change(plane, property::PLANE_CRTC_Y, y as u32 as u64),
            change(plane, property::PLANE_CRTC_W, 64),
            change(plane, property::PLANE_CRTC_H, 64),
        ]
    } else if flags & uapi::DRM_MODE_CURSOR_MOVE != 0 {
        let state = file.device_state().atomic;
        if state.cursor_fb == 0 {
            return Err(AxError::InvalidInput);
        }
        [
            change(plane, property::PLANE_FB_ID, state.cursor_fb as u64),
            change(plane, property::PLANE_CRTC_ID, resources.crtc.id as u64),
            change(plane, property::PLANE_SRC_X, 0),
            change(plane, property::PLANE_SRC_Y, 0),
            change(plane, property::PLANE_SRC_W, 64 << 16),
            change(plane, property::PLANE_SRC_H, 64 << 16),
            change(plane, property::PLANE_CRTC_X, x as u32 as u64),
            change(plane, property::PLANE_CRTC_Y, y as u32 as u64),
            change(plane, property::PLANE_CRTC_W, 64),
            change(plane, property::PLANE_CRTC_H, 64),
        ]
    } else {
        return Err(AxError::InvalidInput);
    };
    file.submit_legacy_cursor_atomic(&changes, hot_x, hot_y)
        .map_err(AxError::from)
}

fn dirtyfb(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let r: uapi::DrmModeFbDirtyCmd = read_pod(copy, arg)?;
    if r.flags != 0 || r.color != 0 || r.num_clips != 0 || r.clips_ptr != 0 {
        return Err(AxError::InvalidInput);
    }
    let state = file.device_state();
    if !state.framebuffers.contains_key(&r.fb_id) {
        return Err(AxError::NotFound);
    }
    // Scanout is synchronous; there is no deferred shadow buffer to flush.
    Ok(())
}

fn get_gamma(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let mut r: uapi::DrmModeCrtcLut = read_pod(copy, arg)?;
    let gamma = file.gamma_lut(r.crtc_id).map_err(AxError::from)?;
    let size = gamma.len() / 3;
    let capacity = r.gamma_size as usize;
    if r.red != 0 && r.green != 0 && r.blue != 0 {
        for index in 0..size.min(capacity) {
            write_u16(copy, array_at(r.red, index, 2)?, gamma[index * 3])?;
            write_u16(copy, array_at(r.green, index, 2)?, gamma[index * 3 + 1])?;
            write_u16(copy, array_at(r.blue, index, 2)?, gamma[index * 3 + 2])?;
        }
    } else if r.red != 0 || r.green != 0 || r.blue != 0 {
        return Err(AxError::InvalidInput);
    }
    r.gamma_size = size as u32;
    write_pod(copy, arg, &r)
}

fn set_gamma(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let r: uapi::DrmModeCrtcLut = read_pod(copy, arg)?;
    let expected = file.gamma_lut(r.crtc_id).map_err(AxError::from)?.len() / 3;
    if r.gamma_size as usize != expected || r.red == 0 || r.green == 0 || r.blue == 0 {
        return Err(AxError::InvalidInput);
    }
    // Complete all user reads before changing the device LUT.
    let red = read_array::<u16>(copy, r.red, expected, expected)?;
    let green = read_array::<u16>(copy, r.green, expected, expected)?;
    let blue = read_array::<u16>(copy, r.blue, expected, expected)?;
    let mut gamma = Vec::new();
    gamma
        .try_reserve_exact(expected.checked_mul(3).ok_or(AxError::InvalidInput)?)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..expected {
        gamma.extend_from_slice(&[red[index], green[index], blue[index]]);
    }
    file.set_gamma_lut(r.crtc_id, &gamma).map_err(AxError::from)
}

fn set_crtc(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let request: uapi::DrmModeCrtc = read_pod(copy, arg)?;
    let resources = file.resources();
    if request.crtc_id != resources.crtc.id || request.x != 0 || request.y != 0 {
        return Err(AxError::InvalidInput);
    }
    let (changes, mode) = if request.fb_id == 0 {
        if request.count_connectors != 0 || request.mode_valid != 0 {
            return Err(AxError::InvalidInput);
        }
        (
            [
                change(resources.connector.id, property::CONNECTOR_CRTC_ID, 0),
                change(resources.crtc.id, property::CRTC_ACTIVE, 0),
                change(resources.crtc.id, property::CRTC_MODE_ID, 0),
                change(resources.primary_plane_id, property::PLANE_FB_ID, 0),
                change(resources.primary_plane_id, property::PLANE_CRTC_ID, 0),
                change(resources.primary_plane_id, property::PLANE_SRC_X, 0),
                change(resources.primary_plane_id, property::PLANE_SRC_Y, 0),
                change(resources.primary_plane_id, property::PLANE_SRC_W, 0),
                change(resources.primary_plane_id, property::PLANE_SRC_H, 0),
                change(resources.primary_plane_id, property::PLANE_CRTC_X, 0),
                change(resources.primary_plane_id, property::PLANE_CRTC_Y, 0),
                change(resources.primary_plane_id, property::PLANE_CRTC_W, 0),
                change(resources.primary_plane_id, property::PLANE_CRTC_H, 0),
            ],
            None,
        )
    } else {
        if request.count_connectors != 1 || request.mode_valid == 0 {
            return Err(AxError::InvalidInput);
        }
        let connector: u32 = read_pod(
            copy,
            usize::try_from(request.set_connectors_ptr).map_err(|_| AxError::BadAddress)?,
        )?;
        if connector != resources.connector.id
            || request.mode.hdisplay == 0
            || request.mode.vdisplay == 0
        {
            return Err(AxError::InvalidInput);
        }
        let mode = Mode {
            width: request.mode.hdisplay as u32,
            height: request.mode.vdisplay as u32,
            refresh_millihz: request.mode.vrefresh.saturating_mul(1000),
        };
        let source_width = mode.width.checked_shl(16).ok_or(AxError::InvalidInput)?;
        let source_height = mode.height.checked_shl(16).ok_or(AxError::InvalidInput)?;
        (
            [
                change(
                    resources.connector.id,
                    property::CONNECTOR_CRTC_ID,
                    resources.crtc.id as u64,
                ),
                change(resources.crtc.id, property::CRTC_ACTIVE, 1),
                change(resources.crtc.id, property::CRTC_MODE_ID, 0),
                change(
                    resources.primary_plane_id,
                    property::PLANE_FB_ID,
                    request.fb_id as u64,
                ),
                change(
                    resources.primary_plane_id,
                    property::PLANE_CRTC_ID,
                    resources.crtc.id as u64,
                ),
                change(resources.primary_plane_id, property::PLANE_SRC_X, 0),
                change(resources.primary_plane_id, property::PLANE_SRC_Y, 0),
                change(
                    resources.primary_plane_id,
                    property::PLANE_SRC_W,
                    source_width as u64,
                ),
                change(
                    resources.primary_plane_id,
                    property::PLANE_SRC_H,
                    source_height as u64,
                ),
                change(resources.primary_plane_id, property::PLANE_CRTC_X, 0),
                change(resources.primary_plane_id, property::PLANE_CRTC_Y, 0),
                change(
                    resources.primary_plane_id,
                    property::PLANE_CRTC_W,
                    mode.width as u64,
                ),
                change(
                    resources.primary_plane_id,
                    property::PLANE_CRTC_H,
                    mode.height as u64,
                ),
            ],
            Some(mode),
        )
    };
    file.submit_legacy_atomic(&changes, mode, None, false)
        .map_err(AxError::from)
}

const fn change(object: u32, property: u32, value: u64) -> super::atomic::Change {
    super::atomic::Change {
        object,
        property,
        value,
    }
}

fn wait_vblank(file: &DrmFile, copy: &impl UserCopy, arg: usize) -> AxResult<()> {
    let request: uapi::DrmWaitVblank = read_pod(copy, arg)?;
    // SAFETY: request is copied from a 24-byte Linux union; both variants use
    // the first 16 bytes compatibly for this request path.
    let request = unsafe { request.request };
    let supported =
        uapi::DRM_VBLANK_RELATIVE | uapi::DRM_VBLANK_EVENT | uapi::DRM_VBLANK_NEXTONMISS;
    if request.type_ & !supported != 0
        || request.signal != 0 && request.type_ & uapi::DRM_VBLANK_EVENT == 0
    {
        return Err(AxError::InvalidInput);
    }
    let current = file.vblank_sequence();
    let relative = request.type_ & uapi::DRM_VBLANK_RELATIVE != 0;
    let mut target = if relative {
        current
            .checked_add(u64::from(request.sequence))
            .ok_or(AxError::InvalidInput)?
    } else {
        resolve_vblank_sequence(current, request.sequence)
    };
    if !relative && target < current && request.type_ & uapi::DRM_VBLANK_NEXTONMISS != 0 {
        target = current.saturating_add(1);
    }
    let sequence = file
        .wait_vblank_target(
            target,
            request.signal,
            request.type_ & uapi::DRM_VBLANK_EVENT != 0,
        )
        .map_err(AxError::from)?;
    let now = axhal::time::monotonic_time_nanos();
    let reply = uapi::DrmWaitVblankReply {
        type_: request.type_,
        sequence: sequence as u32,
        tval_sec: (now / 1_000_000_000) as i64,
        tval_usec: ((now / 1_000) % 1_000_000) as i64,
    };
    // SAFETY: writing the reply union member initializes the complete 24-byte union.
    let response = uapi::DrmWaitVblank { reply };
    write_pod(copy, arg, &response)
}

/// Linux exposes a 32-bit vblank sequence; resolve it into the closest epoch
/// around the device's monotonic 64-bit counter.
fn resolve_vblank_sequence(current: u64, sequence: u32) -> u64 {
    // Interpret the userspace u32 as a signed displacement from the low word
    // of the monotonic sequence.  This is the usual Linux-style half-range
    // ordering and remains correct across every 32-bit wrap.
    let delta = sequence.wrapping_sub(current as u32) as i32 as i64;
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as u64)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec};
    use core::{cell::RefCell, mem::MaybeUninit};

    use super::*;

    struct Image(RefCell<alloc::vec::Vec<u8>>);
    impl UserCopy for Image {
        fn read(&self, address: usize, dst: &mut [MaybeUninit<u8>]) -> AxResult<()> {
            let bytes = self.0.borrow();
            let source = bytes
                .get(address..address.checked_add(dst.len()).ok_or(AxError::BadAddress)?)
                .ok_or(AxError::BadAddress)?;
            for (to, from) in dst.iter_mut().zip(source) {
                to.write(*from);
            }
            Ok(())
        }
        fn write(&self, address: usize, src: &[u8]) -> AxResult<()> {
            let mut bytes = self.0.borrow_mut();
            let target = bytes
                .get_mut(address..address.checked_add(src.len()).ok_or(AxError::BadAddress)?)
                .ok_or(AxError::BadAddress)?;
            target.copy_from_slice(src);
            Ok(())
        }
    }

    #[test]
    fn version_reports_full_lengths_and_truncates_caller_buffers() {
        let copy = Image(RefCell::new(vec![0; 256]));
        let request = uapi::DrmVersion {
            name_len: 3,
            name: 128,
            date_len: 0,
            date: 0,
            desc_len: 2,
            desc: 160,
            ..Default::default()
        };
        write_pod(&copy, 0, &request).unwrap();
        version(&copy, 0).unwrap();
        let response: uapi::DrmVersion = read_pod(&copy, 0).unwrap();
        assert_eq!(response.name_len, 10);
        assert_eq!(response.desc_len, 20);
        let image = copy.0.borrow();
        assert_eq!(&image[128..131], b"vir");
        assert_eq!(&image[160..162], b"Th");
    }

    #[test]
    fn virtual_mode_info_has_complete_timing_and_name() {
        let info = mode_info(Mode {
            width: 1024,
            height: 768,
            refresh_millihz: 60_000,
        });
        assert!(info.clock != 0 && info.htotal > info.hsync_end && info.vtotal > info.vsync_end);
        assert_eq!(&info.name[..11], b"1024x768@60");
    }

    #[test]
    fn mode_blob_reads_vdisplay_not_hskew() {
        let info = uapi::DrmModeModeInfo {
            hdisplay: 1280,
            hskew: 37,
            vdisplay: 800,
            vrefresh: 60,
            ..Default::default()
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&info as *const uapi::DrmModeModeInfo).cast::<u8>(),
                core::mem::size_of_val(&info),
            )
        };
        assert_eq!(
            mode_from_blob(bytes).unwrap(),
            Mode {
                width: 1280,
                height: 800,
                refresh_millihz: 60_000
            }
        );
    }

    #[test]
    fn vblank_u32_sequence_resolves_across_wrap_in_both_directions() {
        assert_eq!(
            resolve_vblank_sequence(0x0000_0001_ffff_fffe, 1),
            0x0000_0002_0000_0001
        );
        assert_eq!(
            resolve_vblank_sequence(0x0000_0002_0000_0001, 0xffff_fffe),
            0x0000_0001_ffff_fffe
        );
    }

    #[test]
    fn render_core_allowlist_excludes_primary_node_controls() {
        for command in [
            uapi::DRM_IOCTL_VERSION,
            uapi::DRM_IOCTL_GET_CAP,
            uapi::DRM_IOCTL_GEM_CLOSE,
            uapi::DRM_IOCTL_PRIME_HANDLE_TO_FD,
            uapi::DRM_IOCTL_PRIME_FD_TO_HANDLE,
            uapi::DRM_IOCTL_SYNCOBJ_CREATE,
            uapi::DRM_IOCTL_SYNCOBJ_DESTROY,
            uapi::DRM_IOCTL_SYNCOBJ_RESET,
            uapi::DRM_IOCTL_SYNCOBJ_SIGNAL,
            uapi::DRM_IOCTL_SYNCOBJ_WAIT,
            uapi::DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD,
            uapi::DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE,
            uapi::DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
            uapi::DRM_IOCTL_SYNCOBJ_QUERY,
            uapi::DRM_IOCTL_SYNCOBJ_TRANSFER,
            uapi::DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
        ] {
            assert!(render_allows_core_ioctl(command));
        }
        for command in [
            uapi::DRM_IOCTL_GET_MAGIC,
            uapi::DRM_IOCTL_AUTH_MAGIC,
            uapi::DRM_IOCTL_SET_MASTER,
            uapi::DRM_IOCTL_DROP_MASTER,
            uapi::DRM_IOCTL_MODE_GETRESOURCES,
            uapi::DRM_IOCTL_MODE_CREATE_DUMB,
            uapi::DRM_IOCTL_MODE_DESTROY_DUMB,
            uapi::DRM_IOCTL_MODE_PAGE_FLIP,
        ] {
            assert!(!render_allows_core_ioctl(command));
        }
    }

    struct Backing;
    impl crate::drm::GemBacking for Backing {
        fn shared_pages(&self) -> crate::drm::DrmResult<Arc<crate::mm::SharedPages>> {
            Err(crate::drm::DrmError::Unsupported)
        }
    }

    struct Adapter;
    impl crate::drm::DisplayAdapter for Adapter {
        fn create_dumb(
            &self,
            _: DumbRequest,
            _: u32,
            _: u64,
        ) -> crate::drm::DrmResult<Arc<dyn crate::drm::GemBacking>> {
            Ok(Arc::new(Backing))
        }

        fn present(
            &self,
            _: crate::drm::Scanout,
        ) -> crate::drm::DrmResult<alloc::sync::Arc<crate::drm::fence::Fence>> {
            Ok(crate::drm::fence::Fence::new(true))
        }
    }

    #[test]
    fn destroy_dumb_reads_only_its_four_byte_payload() {
        let device = crate::drm::DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4);
        let file = device.open_primary();
        let dumb = file
            .create_dumb(DumbRequest {
                width: 1,
                height: 1,
                bpp: 32,
            })
            .unwrap();
        let copy = Image(RefCell::new(dumb.handle.to_ne_bytes().to_vec()));

        destroy_dumb(&file, &copy, 0).unwrap();
        assert!(file.gem(dumb.handle).is_err());
    }
}
