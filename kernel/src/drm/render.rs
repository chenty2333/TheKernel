//! Unprivileged legacy-virgl render-node ABI.
//!
//! This deliberately implements only the old VIRGL path.  Blob, Venus,
//! context-init and UUID ioctls are not accepted because the transport cannot
//! truthfully provide their lifetime or memory-domain guarantees.

use alloc::{sync::Arc, vec::Vec};
use core::{
    mem::{MaybeUninit, size_of},
    slice,
};

use axerrno::{AxError, AxResult};
use axhal::paging::PageSize;

use super::{DrmError, DrmFile, DrmResult, GemBacking, fence::Fence, ioctl::UserCopy, syncobj};
use crate::mm::{SharedPages, checked_align_up};

const MAP: u64 = 0xc010_6441;
const EXECBUFFER: u64 = 0xc040_6442;
const GETPARAM: u64 = 0xc010_6443;
const RESOURCE_CREATE: u64 = 0xc038_6444;
const RESOURCE_INFO: u64 = 0xc010_6445;
const TRANSFER_FROM_HOST: u64 = 0xc02c_6446;
const TRANSFER_TO_HOST: u64 = 0xc02c_6447;
const WAIT: u64 = 0xc008_6448;
const GET_CAPS: u64 = 0xc018_6449;
const PARAM_3D_FEATURES: u64 = 1;
const PARAM_CAPSET_QUERY_FIX: u64 = 2;
const PARAM_SUPPORTED_CAPSET_IDS: u64 = 7;
const CAPSET_VIRGL: u32 = 1;
/// One virtio control-buffer payload.  This is the negotiated driver limit,
/// not an arbitrary userspace-visible 4 KiB restriction.
const MAX_COMMAND: usize = 1024 * 1024;
const MAX_HANDLES: usize = 128;
// The only formats for which this legacy path can prove a linear four-byte
// layout. These are the first four virgl/Gallium BGRA/ARGB 8:8:8:8 formats.
const LINEAR_4BPP_FORMATS: core::ops::RangeInclusive<u32> = 1..=4;
const PIPE_TEXTURE_2D: u32 = 2;

#[derive(Clone, Copy)]
pub struct RenderResource {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}
#[derive(Clone, Copy)]
pub struct RenderTransfer {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub offset: u64,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

/// The narrow safe boundary implemented by the virtio adapter. Retirement is
/// responsible for detaching before unref and retaining pages until that
/// sequence succeeds.
pub trait RenderAdapter: Send + Sync {
    fn capset_info(&self, index: u32) -> DrmResult<(u32, u32, u32)>;
    fn capset(&self, id: u32, version: u32, data: &mut [u8]) -> DrmResult<usize>;
    fn create_context(&self, name: &[u8]) -> DrmResult<u32>;
    fn destroy_context(&self, context: u32) -> DrmResult<()>;
    fn create_resource(
        &self,
        resource: RenderResource,
        entries: &[(u64, u32)],
        pages: Arc<SharedPages>,
    ) -> DrmResult<u32>;
    fn retire_resource(&self, resource: u32, pages: Arc<SharedPages>);
    fn attach_resource(&self, context: u32, resource: u32) -> DrmResult<()>;
    fn detach_resource(&self, context: u32, resource: u32) -> DrmResult<()>;
    fn transfer(
        &self,
        context: u32,
        resource: u32,
        transfer: RenderTransfer,
        to_host: bool,
    ) -> DrmResult<()>;
    fn submit(&self, context: u32, commands: &[u8], resources: &[u32]) -> DrmResult<()>;
}

struct RenderBacking {
    pages: Arc<SharedPages>,
    resource: u32,
    adapter: Arc<dyn RenderAdapter>,
}
impl GemBacking for RenderBacking {
    fn shared_pages(&self) -> DrmResult<Arc<SharedPages>> {
        Ok(self.pages.clone())
    }
}
impl Drop for RenderBacking {
    fn drop(&mut self) {
        self.adapter
            .retire_resource(self.resource, self.pages.clone());
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Map {
    offset: u64,
    handle: u32,
    pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GetParam {
    param: u64,
    value: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Create {
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    stride: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Info {
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    blob_mem: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Box3 {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Transfer {
    bo_handle: u32,
    box_: Box3,
    level: u32,
    offset: u32,
    stride: u32,
    layer_stride: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Wait {
    handle: u32,
    flags: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Caps {
    cap_set_id: u32,
    cap_set_ver: u32,
    addr: u64,
    size: u32,
    pad: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Exec {
    flags: u32,
    size: u32,
    command: u64,
    bo_handles: u64,
    num_bo_handles: u32,
    fence_fd: i32,
    ring_idx: u32,
    syncobj_stride: u32,
    num_in_syncobjs: u32,
    num_out_syncobjs: u32,
    in_syncobjs: u64,
    out_syncobjs: u64,
}

fn read<T: Copy>(copy: &impl super::ioctl::UserCopy, addr: usize) -> AxResult<T> {
    let mut v = MaybeUninit::<T>::zeroed();
    let b = unsafe {
        slice::from_raw_parts_mut(v.as_mut_ptr().cast::<MaybeUninit<u8>>(), size_of::<T>())
    };
    copy.read(addr, b)?;
    Ok(unsafe { v.assume_init() })
}
fn write<T>(copy: &impl super::ioctl::UserCopy, addr: usize, value: &T) -> AxResult<()> {
    copy.write(addr, unsafe {
        slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>())
    })
}
fn bytes(copy: &impl super::ioctl::UserCopy, addr: u64, n: usize, max: usize) -> AxResult<Vec<u8>> {
    if n > max {
        return Err(AxError::InvalidInput);
    }
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| AxError::NoMemory)?;
    v.resize(n, 0);
    copy.read(
        usize::try_from(addr).map_err(|_| AxError::BadAddress)?,
        unsafe { slice::from_raw_parts_mut(v.as_mut_ptr().cast(), n) },
    )?;
    Ok(v)
}
fn handles(copy: &impl super::ioctl::UserCopy, addr: u64, n: usize) -> AxResult<Vec<u32>> {
    if n > MAX_HANDLES {
        return Err(AxError::InvalidInput);
    };
    let mut out = Vec::new();
    out.try_reserve_exact(n).map_err(|_| AxError::NoMemory)?;
    let base = usize::try_from(addr).map_err(|_| AxError::BadAddress)?;
    for i in 0..n {
        out.push(read(
            copy,
            base.checked_add(i.checked_mul(4).ok_or(AxError::InvalidInput)?)
                .ok_or(AxError::BadAddress)?,
        )?)
    }
    Ok(out)
}
fn drm(e: DrmError) -> AxError {
    e.into()
}

pub(super) fn dispatch(
    file: &DrmFile,
    context: &crate::file::IoctlContext,
    cmd: u32,
    arg: usize,
) -> AxResult<usize> {
    let copy = context;
    match cmd as u64 {
        GETPARAM => {
            let mut r: GetParam = read(copy, arg)?;
            r.value = match r.param {
                PARAM_3D_FEATURES | PARAM_CAPSET_QUERY_FIX => 1,
                PARAM_SUPPORTED_CAPSET_IDS => 1u64 << CAPSET_VIRGL,
                _ => return Err(AxError::InvalidInput),
            };
            write(copy, arg, &r)?;
        }
        GET_CAPS => {
            let r: Caps = read(copy, arg)?;
            if r.pad != 0 || r.cap_set_id != CAPSET_VIRGL {
                return Err(AxError::InvalidInput);
            }
            let a = file.render_adapter().map_err(drm)?;
            let (id, _, max) = a.capset_info(0).map_err(drm)?;
            if id != CAPSET_VIRGL || r.size > max {
                return Err(AxError::InvalidInput);
            }
            let mut data = bytes(copy, r.addr, r.size as usize, MAX_COMMAND)?;
            let actual = a
                .capset(r.cap_set_id, r.cap_set_ver, &mut data)
                .map_err(drm)?;
            if actual != data.len() {
                return Err(AxError::InvalidInput);
            }
            copy.write(
                usize::try_from(r.addr).map_err(|_| AxError::BadAddress)?,
                &data,
            )?;
        }
        RESOURCE_CREATE => create(file, copy, arg)?,
        RESOURCE_INFO => info(file, copy, arg)?,
        MAP => map(file, copy, arg)?,
        TRANSFER_TO_HOST => transfer(file, copy, arg, true)?,
        TRANSFER_FROM_HOST => transfer(file, copy, arg, false)?,
        EXECBUFFER => exec(file, context, arg)?,
        WAIT => {
            let r: Wait = read(copy, arg)?;
            if r.flags & !1 != 0 {
                return Err(AxError::InvalidInput);
            }
            file.render_resource(r.handle).map_err(drm)?;
        }
        _ => return Err(AxError::NotATty),
    };
    Ok(0)
}
fn create(file: &DrmFile, copy: &impl super::ioctl::UserCopy, arg: usize) -> AxResult<()> {
    let mut r: Create = read(copy, arg)?;
    if r.bo_handle != 0 || r.res_handle != 0 || r.size == 0 {
        return Err(AxError::InvalidInput);
    };
    validate_linear_resource(&r)?;
    let size = r.size as u64;
    let alloc = checked_align_up(
        usize::try_from(size).map_err(|_| AxError::InvalidInput)?,
        PageSize::Size4K as usize,
    )
    .ok_or(AxError::InvalidInput)?;
    let pages = Arc::try_new(
        SharedPages::new_fixed(alloc, PageSize::Size4K).map_err(|_| AxError::NoMemory)?,
    )
    .map_err(|_| AxError::NoMemory)?;
    let mut entries = Vec::new();
    for i in 0..pages.len() {
        entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        entries.push((
            pages
                .paddr_at(i)
                .map_err(|_| AxError::InvalidInput)?
                .as_usize() as u64,
            4096,
        ));
    }
    let a = file.render_adapter().map_err(drm)?;
    let meta = RenderResource {
        target: r.target,
        format: r.format,
        bind: r.bind,
        width: r.width,
        height: r.height,
        depth: r.depth,
        array_size: r.array_size,
        last_level: r.last_level,
        nr_samples: r.nr_samples,
        flags: r.flags,
    };
    let resource = a
        .create_resource(meta, &entries, pages.clone())
        .map_err(drm)?;
    let backing: Arc<dyn GemBacking> = Arc::new(RenderBacking {
        pages,
        resource,
        adapter: a,
    });
    let handle = file
        .create_render_gem(backing, size, resource, meta)
        .map_err(drm)?;
    r.bo_handle = handle;
    r.res_handle = resource;
    write(copy, arg, &r)
}

fn validate_linear_resource(r: &Create) -> AxResult<()> {
    // Transfers below use a four-byte-per-pixel bound. Refuse every format or
    // layout for which that calculation would not prove the host DMA range.
    if !LINEAR_4BPP_FORMATS.contains(&r.format)
        || r.target != PIPE_TEXTURE_2D
        || r.nr_samples != 0
        || r.last_level != 0
        || r.width == 0
        || r.height == 0
        || r.depth != 1
        || r.array_size != 1
    {
        return Err(AxError::InvalidInput);
    }
    let bytes = u64::from(r.width)
        .checked_mul(u64::from(r.height))
        .and_then(|value| value.checked_mul(4))
        .ok_or(AxError::InvalidInput)?;
    if bytes > u64::from(r.size) {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}
fn info(file: &DrmFile, copy: &impl super::ioctl::UserCopy, arg: usize) -> AxResult<()> {
    let mut r: Info = read(copy, arg)?;
    let (res, o) = file.render_resource(r.bo_handle).map_err(drm)?;
    r.res_handle = res;
    r.size = u32::try_from(o.size).map_err(|_| AxError::InvalidInput)?;
    r.blob_mem = 0;
    write(copy, arg, &r)
}
fn map(file: &DrmFile, copy: &impl super::ioctl::UserCopy, arg: usize) -> AxResult<()> {
    let mut r: Map = read(copy, arg)?;
    r.offset = file.map_dumb(r.handle).map_err(drm)?;
    write(copy, arg, &r)
}
fn transfer(
    file: &DrmFile,
    copy: &impl super::ioctl::UserCopy,
    arg: usize,
    to_host: bool,
) -> AxResult<()> {
    let r: Transfer = read(copy, arg)?;
    if r.box_.w == 0 || r.box_.h == 0 || r.box_.d == 0 {
        return Err(AxError::InvalidInput);
    };
    let (res, obj) = file.render_resource(r.bo_handle).map_err(drm)?;
    let meta = obj.render_meta.ok_or(AxError::InvalidInput)?;
    if r.level != 0
        || r.box_
            .x
            .checked_add(r.box_.w)
            .is_none_or(|v| v > meta.width)
        || r.box_
            .y
            .checked_add(r.box_.h)
            .is_none_or(|v| v > meta.height)
        || r.box_
            .z
            .checked_add(r.box_.d)
            .is_none_or(|v| v > meta.depth)
    {
        return Err(AxError::InvalidInput);
    }
    let row = if r.stride == 0 {
        u64::from(r.box_.w).checked_mul(4)
    } else {
        Some(u64::from(r.stride))
    }
    .ok_or(AxError::InvalidInput)?;
    let layer = if r.layer_stride == 0 {
        row.checked_mul(u64::from(r.box_.h))
    } else {
        Some(u64::from(r.layer_stride))
    }
    .ok_or(AxError::InvalidInput)?;
    let end = u64::from(r.offset)
        .checked_add(
            layer
                .checked_mul(u64::from(r.box_.d - 1))
                .ok_or(AxError::InvalidInput)?,
        )
        .and_then(|v| v.checked_add(row.checked_mul(u64::from(r.box_.h - 1))?))
        .and_then(|v| v.checked_add(u64::from(r.box_.w).checked_mul(4)?))
        .ok_or(AxError::InvalidInput)?;
    if end > obj.size {
        return Err(AxError::InvalidInput);
    }
    let a = file.render_adapter().map_err(drm)?;
    let c = file.render_context().map_err(drm)?;
    a.attach_resource(c, res).map_err(drm)?;
    let result = a.transfer(
        c,
        res,
        RenderTransfer {
            x: r.box_.x,
            y: r.box_.y,
            z: r.box_.z,
            width: r.box_.w,
            height: r.box_.h,
            depth: r.box_.d,
            offset: r.offset as u64,
            level: r.level,
            stride: r.stride,
            layer_stride: r.layer_stride,
        },
        to_host,
    );
    let detach = a.detach_resource(c, res);
    result.and(detach).map_err(drm)
}
fn exec(file: &DrmFile, context: &crate::file::IoctlContext, arg: usize) -> AxResult<()> {
    let copy = context;
    let mut r: Exec = read(copy, arg)?;
    const FENCE_FD_IN: u32 = 1;
    const FENCE_FD_OUT: u32 = 2;
    if r.flags & !(FENCE_FD_IN | FENCE_FD_OUT) != 0
        || r.ring_idx != 0
        || r.syncobj_stride != 0
        || r.num_in_syncobjs != 0
        || r.num_out_syncobjs != 0
        || r.in_syncobjs != 0
        || r.out_syncobjs != 0
        || (r.flags & FENCE_FD_IN == 0 && r.fence_fd != -1)
    {
        return Err(AxError::OperationNotSupported);
    };
    if r.flags & FENCE_FD_IN != 0 {
        syncobj::import(context, r.fence_fd)?.wait(None)?;
    }
    let commands = bytes(copy, r.command, r.size as usize, MAX_COMMAND)?;
    let hs = handles(copy, r.bo_handles, r.num_bo_handles as usize)?;
    let mut resources = Vec::new();
    for h in hs {
        let resource = file.render_resource(h).map_err(drm)?.0;
        if !resources.contains(&resource) {
            resources.push(resource);
        }
    }
    let a = file.render_adapter().map_err(drm)?;
    let c = file.render_context().map_err(drm)?;
    let mut attached = Vec::new();
    for &res in &resources {
        if let Err(error) = a.attach_resource(c, res) {
            for &attached_res in attached.iter().rev() {
                let _ = a.detach_resource(c, attached_res);
            }
            return Err(drm(error));
        }
        attached.push(res);
    }
    let result = a.submit(c, &commands, &resources);
    let mut cleanup = None;
    for &res in attached.iter().rev() {
        if let Err(error) = a.detach_resource(c, res) {
            cleanup.get_or_insert(error);
        }
    }
    if let Err(error) = result {
        return Err(drm(error));
    }
    if let Some(error) = cleanup {
        return Err(drm(error));
    }
    if r.flags & FENCE_FD_OUT != 0 {
        let fence = Fence::new(true);
        r.fence_fd = syncobj::export(fence, context, false)?;
        write(copy, arg, &r)?;
    }
    Ok(())
}

const _: [(); 16] = [(); size_of::<Map>()];
const _: [(); 64] = [(); size_of::<Exec>()];
const _: [(); 16] = [(); size_of::<GetParam>()];
const _: [(); 56] = [(); size_of::<Create>()];
const _: [(); 16] = [(); size_of::<Info>()];
const _: [(); 44] = [(); size_of::<Transfer>()];
const _: [(); 8] = [(); size_of::<Wait>()];
const _: [(); 24] = [(); size_of::<Caps>()];

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_create() -> Create {
        Create {
            target: PIPE_TEXTURE_2D,
            format: 1,
            width: 16,
            height: 8,
            depth: 1,
            array_size: 1,
            size: 16 * 8 * 4,
            ..Default::default()
        }
    }

    #[test]
    fn linux_virtgpu_legacy_ioctl_layouts() {
        assert_eq!(MAP, 0xc010_6441);
        assert_eq!(EXECBUFFER, 0xc040_6442);
        assert_eq!(TRANSFER_TO_HOST, 0xc02c_6447);
        assert_eq!(size_of::<Exec>(), 64);
        assert_eq!(size_of::<Transfer>(), 44);
        // Do not accidentally expose newer UAPI command numbers via dispatch.
        assert!(RESOURCE_CREATE < 0xc040_644a);
    }

    #[test]
    fn resource_create_rejects_layouts_the_transfer_bounds_cannot_prove() {
        let valid = linear_create();
        assert!(validate_linear_resource(&valid).is_ok());

        let mut invalid = valid;
        invalid.format = 5;
        assert!(validate_linear_resource(&invalid).is_err());
        invalid = valid;
        invalid.nr_samples = 1;
        assert!(validate_linear_resource(&invalid).is_err());
        invalid = valid;
        invalid.size -= 1;
        assert!(validate_linear_resource(&invalid).is_err());
        invalid = valid;
        invalid.depth = 2;
        assert!(validate_linear_resource(&invalid).is_err());
        invalid = valid;
        invalid.array_size = 2;
        assert!(validate_linear_resource(&invalid).is_err());
    }
}
