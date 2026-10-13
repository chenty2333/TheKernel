//! `/dev/fb0` backed by the registered upstream `fb_info` and deferred-I/O VM ops.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    any::Any,
    ffi::{c_int, c_ulong},
    mem::{offset_of, size_of},
    ptr,
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{DeviceId, Location, NodeFlags, VfsResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use linux_raw_sys::general::S_IFCHR;
use memory_addr::PhysAddr;
use tk_drm_upstream::{
    fb_types::{FbFixScreenInfo, FbInfo, FbVarScreenInfo},
    fbmem_upstream::{fb_deferred_io_get_page, get_fb_info, put_fb_info},
};
use tk_linux_kapi::{
    mm::{VM_MAYWRITE, VM_READ, VM_SHARED, VM_WRITE, VmAreaStruct, vm_get_page_prot},
    shmem::{put_page, try_page_to_phys},
    types::{File as LinuxFile, Page},
};

use crate::{
    drm::upstream::UpstreamDrmDevice,
    file::{
        FileLike, FileMmapProtection, FileMmapRequest, FileMmapSharing, FixedSharedMmapRegion,
        IoDst, IoSrc, IoctlContext, Kstat, PreparedFileMmap, SharedMmapOps,
    },
    mm::SharedPages,
    pseudofs::{DeviceOpen, DeviceOps},
};

const PAGE_SIZE: usize = 4096;
const FB_DEVICE_ID: DeviceId = DeviceId::new(29, 0);
const FB_NODE_MODE: u16 = 0o660;
const FBIOGET_VSCREENINFO: u32 = 0x4600;
const FBIOPUT_VSCREENINFO: u32 = 0x4601;
const FBIOGET_FSCREENINFO: u32 = 0x4602;
const FBIOPAN_DISPLAY: u32 = 0x4606;
const FBIOBLANK: u32 = 0x4611;

struct FbInfoOwner {
    info: *mut FbInfo,
    _drm: Arc<UpstreamDrmDevice>,
    /// Stable Linux address_space cookie shared by every fb VMA. This file
    /// record remains alive until `SharedMmapOps` has called vm_close.
    file: LinuxFile,
}

unsafe impl Send for FbInfoOwner {}
unsafe impl Sync for FbInfoOwner {}

impl Drop for FbInfoOwner {
    fn drop(&mut self) {
        unsafe { put_fb_info(self.info) };
    }
}

pub(crate) fn device(
    fs: Arc<crate::pseudofs::SimpleFs>,
    drm: Arc<UpstreamDrmDevice>,
) -> VfsResult<Option<Arc<crate::pseudofs::Device>>> {
    let info = unsafe { get_fb_info(0) };
    if info.is_null() {
        return Ok(None);
    }
    if unsafe { (*info).fix.smem_len == 0 || (*info).fbops.is_null() } {
        unsafe { put_fb_info(info) };
        return Err(AxError::OperationNotSupported.into());
    }
    let mut file: LinuxFile = unsafe { core::mem::zeroed() };
    // The registered fb_info is the backing object identity, stable across
    // all opens and mmaps of this one framebuffer node.
    file.f_mapping = info.cast();
    let owner = match Arc::try_new(FbInfoOwner {
        info,
        _drm: drm,
        file,
    }) {
        Ok(owner) => owner,
        Err(_) => {
            unsafe { put_fb_info(info) };
            return Err(AxError::NoMemory.into());
        }
    };
    Ok(Some(crate::pseudofs::Device::new_with_permissions(
        fs,
        axfs_ng_vfs::NodeType::CharacterDevice,
        FB_DEVICE_ID,
        axfs_ng_vfs::NodePermission::from_bits_truncate(FB_NODE_MODE),
        Arc::new(FbInfoNode { owner }),
    )))
}

struct FbInfoNode {
    owner: Arc<FbInfoOwner>,
}

impl DeviceOps for FbInfoNode {
    fn open_description(&self, _location: &Location, _flags: u32) -> VfsResult<Option<DeviceOpen>> {
        if self.owner._drm.is_unplugged() {
            return Err(AxError::NoSuchDevice.into());
        }
        let info = self.owner.info;
        let ops = unsafe { &*(*info).fbops };
        if let Some(open) = ops.fb_open {
            let ret = unsafe { open(info, 0) };
            if ret != 0 {
                return Err(errno_to_ax(ret).into());
            }
        }
        let file: Arc<dyn FileLike> = Arc::try_new(FbInfoFile {
            owner: self.owner.clone(),
            opened: ops.fb_open.is_some(),
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok(Some(DeviceOpen::new(file, None)))
    }

    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::OperationNotSupported.into())
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::OperationNotSupported.into())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::NO_SEEK
    }
}

struct FbInfoFile {
    owner: Arc<FbInfoOwner>,
    opened: bool,
}

impl Drop for FbInfoFile {
    fn drop(&mut self) {
        if self.opened {
            if let Some(release) = unsafe { (*(*self.owner.info).fbops).fb_release } {
                unsafe { release(self.owner.info, 0) };
            }
        }
    }
}

impl FileLike for FbInfoFile {
    fn read(&self, _dst: &mut IoDst) -> AxResult<usize> {
        Err(AxError::OperationNotSupported)
    }

    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        Err(AxError::OperationNotSupported)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        if self.owner._drm.is_unplugged() {
            return Err(AxError::NoSuchDevice);
        }
        let info = self.owner.info;
        let ops = unsafe { &*(*info).fbops };
        match cmd {
            FBIOGET_VSCREENINFO => {
                let value = unsafe { ptr::addr_of!((*info).var) };
                let bytes = unsafe {
                    core::slice::from_raw_parts(value.cast::<u8>(), size_of::<FbVarScreenInfo>())
                };
                context
                    .user_memory()
                    .write_bytes(arg, bytes)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            FBIOPUT_VSCREENINFO | FBIOPAN_DISPLAY => {
                let mut bytes = [0u8; size_of::<FbVarScreenInfo>()];
                context
                    .user_memory()
                    .read_into(arg as *const u8, &mut bytes)
                    .map_err(crate::mm::map_usercopy_error)?;
                // FbVarScreenInfo is an all-u32 C record; every bit pattern is
                // a valid field value and the copied buffer is properly sized.
                let mut value =
                    unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<FbVarScreenInfo>()) };
                let old = unsafe { (*info).var };
                if let Some(check) = ops.fb_check_var {
                    let ret = unsafe { check(&mut value, info) };
                    if ret != 0 {
                        return Err(errno_to_ax(ret));
                    }
                }
                if cmd == FBIOPAN_DISPLAY {
                    let pan = ops.fb_pan_display.ok_or(AxError::OperationNotSupported)?;
                    let ret = unsafe { pan(&mut value, info) };
                    if ret != 0 {
                        return Err(errno_to_ax(ret));
                    }
                } else {
                    unsafe { (*info).var = value };
                    if let Some(set_par) = ops.fb_set_par {
                        let ret = unsafe { set_par(info) };
                        if ret != 0 {
                            unsafe { (*info).var = old };
                            return Err(errno_to_ax(ret));
                        }
                    }
                }
                Ok(0)
            }
            FBIOGET_FSCREENINFO => {
                let fix = unsafe { ptr::addr_of!((*info).fix).read() };
                let bytes = fix_to_bytes(fix);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            FBIOBLANK => {
                let blank_cb = ops.fb_blank.ok_or(AxError::OperationNotSupported)?;
                let ret = unsafe { blank_cb(arg as c_int, info) };
                if ret != 0 {
                    Err(errno_to_ax(ret))
                } else {
                    Ok(0)
                }
            }
            _ => {
                if let Some(ioctl) = ops.fb_ioctl {
                    let ret = unsafe { ioctl(info, cmd, arg as c_ulong) };
                    if ret < 0 {
                        Err(errno_to_ax(ret))
                    } else {
                        Ok(ret as usize)
                    }
                } else {
                    Err(AxError::OperationNotSupported)
                }
            }
        }
    }

    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        if self.owner._drm.is_unplugged() {
            return Err(AxError::NoSuchDevice);
        }
        prepare_fb_mmap(&self.owner, request).map(Some)
    }

    fn nonblocking(&self) -> bool {
        false
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult<()> {
        Ok(())
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(Kstat {
            rdev: FB_DEVICE_ID,
            mode: S_IFCHR | FB_NODE_MODE as u32,
            nlink: 1,
            blksize: PAGE_SIZE as u32,
            ..Kstat::default()
        })
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(b"/dev/fb0")))
    }
}

impl Pollable for FbInfoFile {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}

struct FbMapOwner {
    fb: Arc<FbInfoOwner>,
    page_refs: Vec<*mut Page>,
}
unsafe impl Send for FbMapOwner {}
unsafe impl Sync for FbMapOwner {}

impl Drop for FbMapOwner {
    fn drop(&mut self) {
        for page in self.page_refs.drain(..) {
            if !page.is_null() {
                unsafe { put_page(page) };
            }
        }
    }
}

struct PageRefGuard(Vec<*mut Page>);
impl Drop for PageRefGuard {
    fn drop(&mut self) {
        for page in self.0.drain(..) {
            if !page.is_null() {
                unsafe { put_page(page) };
            }
        }
    }
}

fn prepare_fb_mmap(
    owner: &Arc<FbInfoOwner>,
    request: FileMmapRequest,
) -> AxResult<PreparedFileMmap> {
    if request.sharing() != FileMmapSharing::Shared
        || request.page_size() != PAGE_SIZE
        || request.protection().contains(FileMmapProtection::EXECUTE)
    {
        return Err(AxError::OperationNotSupported);
    }
    let info = owner.info;
    let length = unsafe { (*info).fix.smem_len as usize };
    if request
        .offset()
        .checked_add(request.length() as u64)
        .is_none_or(|end| end > length as u64)
    {
        return Err(AxError::InvalidInput);
    }
    let page_count = length
        .checked_add(PAGE_SIZE - 1)
        .ok_or(AxError::InvalidInput)?
        / PAGE_SIZE;
    if page_count == 0 {
        return Err(AxError::InvalidInput);
    }
    let mut page_refs = PageRefGuard(Vec::new());
    page_refs
        .0
        .try_reserve_exact(page_count)
        .map_err(|_| AxError::NoMemory)?;
    let mut physical_pages = Vec::new();
    physical_pages
        .try_reserve_exact(page_count)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..page_count {
        let offset = index.checked_mul(PAGE_SIZE).ok_or(AxError::InvalidInput)?;
        let page = unsafe { fb_deferred_io_get_page(info, offset as c_ulong) };
        if page.is_null() {
            return Err(AxError::NoMemory);
        }
        page_refs.0.push(page);
        let physical = unsafe { try_page_to_phys(page) }.ok_or(AxError::BadAddress)?;
        physical_pages.push(PhysAddr::from_usize(physical));
    }
    let map_owner = Arc::try_new(FbMapOwner {
        fb: owner.clone(),
        page_refs: core::mem::take(&mut page_refs.0),
    })
    .map_err(|_| AxError::NoMemory)?;
    let backing_owner: Arc<dyn Send + Sync> = map_owner.clone();
    let pages =
        Arc::try_new(unsafe { SharedPages::new_page_cache_4k(physical_pages, backing_owner)? })
            .map_err(|_| AxError::NoMemory)?;

    let mut vma = VmAreaStruct::empty();
    let mut flags = VM_SHARED | VM_READ | VM_MAYWRITE;
    if request.protection().contains(FileMmapProtection::WRITE) {
        flags |= VM_WRITE;
    }
    vma.vm_start = 0;
    vma.vm_end = request.length() as c_ulong;
    vma.vm_flags = flags;
    vma.vm_page_prot = unsafe { vm_get_page_prot(flags) };
    vma.vm_pgoff = (request.offset() / PAGE_SIZE as u64) as c_ulong;
    vma.vm_file = (&owner.file as *const LinuxFile).cast_mut();
    let ops = unsafe { &*(*info).fbops };
    let ret = if let Some(mmap) = ops.fb_mmap {
        unsafe { mmap(info, &mut vma) }
    } else {
        unsafe { tk_drm_upstream::fbmem_upstream::fb_deferred_io_mmap(info, &mut vma) }
    };
    if ret != 0 {
        return Err(errno_to_ax(ret));
    }
    let mut close_guard = FbVmaCloseGuard { vma, armed: true };
    let region = FixedSharedMmapRegion::try_new(
        0,
        pages.clone(),
        FileMmapProtection::READ | FileMmapProtection::WRITE,
    )?;
    let plan = region.prepare(request)?.ok_or(AxError::InvalidInput)?;
    let owner_any: Arc<dyn Any + Send + Sync> = map_owner;
    let mmap_ops = unsafe { SharedMmapOps::from_vma(pages, &close_guard.vma, owner_any)? };
    close_guard.armed = false;
    let plan = plan.with_shared_mmap_ops(mmap_ops)?;
    Ok(plan)
}

struct FbVmaCloseGuard {
    vma: VmAreaStruct,
    armed: bool,
}
impl Drop for FbVmaCloseGuard {
    fn drop(&mut self) {
        if self.armed && !self.vma.vm_ops.is_null() {
            if let Some(close) = unsafe { (*self.vma.vm_ops).close } {
                unsafe { close(&mut self.vma) };
            }
        }
    }
}

fn fix_to_bytes(value: FbFixScreenInfo) -> [u8; size_of::<FbFixScreenInfo>()] {
    let mut bytes = [0u8; size_of::<FbFixScreenInfo>()];
    for (index, byte) in value.id.into_iter().enumerate() {
        bytes[index] = byte as u8;
    }
    put_u64(
        &mut bytes,
        offset_of!(FbFixScreenInfo, smem_start),
        value.smem_start as u64,
    );
    put_u32(
        &mut bytes,
        offset_of!(FbFixScreenInfo, smem_len),
        value.smem_len,
    );
    put_u32(&mut bytes, offset_of!(FbFixScreenInfo, type_), value.type_);
    put_u32(
        &mut bytes,
        offset_of!(FbFixScreenInfo, type_aux),
        value.type_aux,
    );
    put_u32(
        &mut bytes,
        offset_of!(FbFixScreenInfo, visual),
        value.visual,
    );
    put_u16(
        &mut bytes,
        offset_of!(FbFixScreenInfo, xpanstep),
        value.xpanstep,
    );
    put_u16(
        &mut bytes,
        offset_of!(FbFixScreenInfo, ypanstep),
        value.ypanstep,
    );
    put_u16(
        &mut bytes,
        offset_of!(FbFixScreenInfo, ywrapstep),
        value.ywrapstep,
    );
    put_u32(
        &mut bytes,
        offset_of!(FbFixScreenInfo, line_length),
        value.line_length,
    );
    put_u64(
        &mut bytes,
        offset_of!(FbFixScreenInfo, mmio_start),
        value.mmio_start as u64,
    );
    put_u32(
        &mut bytes,
        offset_of!(FbFixScreenInfo, mmio_len),
        value.mmio_len,
    );
    put_u32(&mut bytes, offset_of!(FbFixScreenInfo, accel), value.accel);
    put_u16(
        &mut bytes,
        offset_of!(FbFixScreenInfo, capabilities),
        value.capabilities,
    );
    for (index, item) in value.reserved.into_iter().enumerate() {
        put_u16(
            &mut bytes,
            offset_of!(FbFixScreenInfo, reserved) + index * 2,
            item,
        );
    }
    bytes
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..][..2].copy_from_slice(&value.to_ne_bytes());
}
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..][..4].copy_from_slice(&value.to_ne_bytes());
}
fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..][..8].copy_from_slice(&value.to_ne_bytes());
}

fn errno_to_ax(ret: i32) -> AxError {
    LinuxError::try_from(ret.saturating_neg())
        .map(AxError::from)
        .unwrap_or(AxError::Io)
}
