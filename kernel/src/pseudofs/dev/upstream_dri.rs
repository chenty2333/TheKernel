//! VFS bridge for the imported Linux DRM file/event/mmap core.

use alloc::{borrow::Cow, format, sync::Arc, vec, vec::Vec};
use core::{
    any::Any,
    cell::UnsafeCell,
    ffi::{c_int, c_ulong, c_void},
    mem::offset_of,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, NodeFlags, VfsResult};
use axio::prelude::*;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use kspin::SpinNoIrq;
use linux_raw_sys::general::{O_ACCMODE, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, S_IFCHR};
use memory_addr::PhysAddr;
use tk_drm_upstream::{
    drm_file_types::{DrmFile, DrmMinor},
    drm_gem_shmem_helper_types_upstream::DrmGemShmemObject,
    drm_gem_types::DrmGemObject,
};
use tk_linux_kapi::{
    locks::{read_lock, read_unlock},
    mm::{VM_MAYWRITE, VM_READ, VM_SHARED, VM_WRITE, VmAreaStruct, vm_get_page_prot},
    shmem::try_page_to_phys,
    types::{File as LinuxFile, WaitQueueEntry},
    wait::{add_wait_queue, remove_wait_queue},
};

use super::dri::{
    DRM_PRIMARY_DEVICE_ID, DRM_PRIMARY_NODE_MODE, DRM_RENDER_DEVICE_ID, DRM_RENDER_NODE_MODE,
    attach_drm_minor, attribute, mode_attribute, publish_pci_parent,
};
use crate::{
    drm::{Mode, upstream::UpstreamDrmDevice},
    file::{
        FileLike, FileMmapProtection, FileMmapRequest, FileMmapSharing, FixedSharedMmapRegion,
        IoDst, IoSrc, IoctlContext, Kstat, OfdIoStatus, PreparedFileMmap, SharedMmapOps,
    },
    mm::{ExternalPageLease, SharedPages},
    pseudofs::{
        DeviceOpen, DeviceOps,
        device_registry::{
            DeviceHandle, DeviceIdentity, DeviceRegistration, DeviceReservation, MAX_DEVICES,
            global_device_registry,
        },
    },
    readiness::block_on_poll_io,
};

const PAGE_SIZE: usize = 4096;
const EFAULT: i32 = 14;
const ENODEV: i32 = 19;
const EPOLLIN: u32 = 0x0001;
const EPOLLRDNORM: u32 = 0x0040;
const EPOLLERR: u32 = 0x0008;
const EPOLLHUP: u32 = 0x0010;

/// Linux `drm_open()` in this import resolves the primary by non-null inode and
/// obtains the minor from the global DRM xarray. It currently does not inspect
/// inode fields, but retain a real stable byte as its non-null discriminator.
#[derive(Default)]
struct OpenInode(u8);

struct UpstreamDrmNode {
    device: Arc<UpstreamDrmDevice>,
    render: bool,
    _registry_handles: Vec<DeviceHandle<'static, MAX_DEVICES>>,
}

pub(crate) fn primary_node(
    fs: Arc<crate::pseudofs::SimpleFs>,
    device: Arc<UpstreamDrmDevice>,
) -> VfsResult<Arc<crate::pseudofs::Device>> {
    let registry_handles = publish_sysfs(&device)?;
    Ok(node(fs, device, false, registry_handles))
}

pub(crate) fn render_node(
    fs: Arc<crate::pseudofs::SimpleFs>,
    device: Arc<UpstreamDrmDevice>,
) -> VfsResult<Option<Arc<crate::pseudofs::Device>>> {
    let has_render = unsafe { !(*device.raw()).render.is_null() };
    if has_render {
        Ok(Some(node(fs, device, true, Vec::new())))
    } else {
        Ok(None)
    }
}

fn node(
    fs: Arc<crate::pseudofs::SimpleFs>,
    device: Arc<UpstreamDrmDevice>,
    render: bool,
    registry_handles: Vec<DeviceHandle<'static, MAX_DEVICES>>,
) -> Arc<crate::pseudofs::Device> {
    crate::pseudofs::Device::new_with_permissions(
        fs,
        axfs_ng_vfs::NodeType::CharacterDevice,
        if render {
            DRM_RENDER_DEVICE_ID
        } else {
            DRM_PRIMARY_DEVICE_ID
        },
        axfs_ng_vfs::NodePermission::from_bits_truncate(if render {
            DRM_RENDER_NODE_MODE
        } else {
            DRM_PRIMARY_NODE_MODE
        }),
        Arc::new(UpstreamDrmNode {
            device,
            render,
            _registry_handles: registry_handles,
        }),
    )
}

fn publish_sysfs(device: &UpstreamDrmDevice) -> VfsResult<Vec<DeviceHandle<'static, MAX_DEVICES>>> {
    let identity = device
        .pci_identity()
        .ok_or(axfs_ng_vfs::VfsError::NotFound)?;
    let mut parents = Vec::new();
    let transport = publish_pci_parent(identity, &mut parents)?;
    let attach = |identity| attach_drm_minor(identity, &Some(transport.clone()));
    let card = DeviceRegistration::try_new(
        attach(
            DeviceIdentity::new(
                "virtio0".into(),
                "drm".into(),
                "card0".into(),
                DRM_PRIMARY_DEVICE_ID,
            )?
            .with_devname("dri/card0".into())?,
        )?,
        "drm_minor".into(),
        Vec::new(),
        None,
    )?;
    let connector = DeviceRegistration::try_new(
        DeviceIdentity::without_dev("virtio0".into(), "drm".into(), "card0-Virtual-1".into())?
            .child_of_path(format!("{transport}/drm"), "card0".into())?,
        "drm_connector".into(),
        vec![
            attribute("status", "connected\n"),
            mode_attribute(Mode {
                width: device.mode().0,
                height: device.mode().1,
                refresh_millihz: 60_000,
            }),
        ],
        None,
    )?;
    let card_reservation = global_device_registry().reserve(card.identity().clone())?;
    let connector_reservation = global_device_registry().reserve(connector.identity().clone())?;
    let render_minor = unsafe { !(*device.raw()).render.is_null() };
    if !render_minor {
        let (card, connector) = DeviceReservation::publish_pair(
            card_reservation,
            card,
            connector_reservation,
            connector,
        )?;
        parents.extend([card, connector]);
        return Ok(parents);
    }
    let render = DeviceRegistration::try_new(
        attach(
            DeviceIdentity::new(
                "virtio0".into(),
                "drm".into(),
                "renderD128".into(),
                DRM_RENDER_DEVICE_ID,
            )?
            .with_devname("dri/renderD128".into())?,
        )?,
        "drm_minor".into(),
        Vec::new(),
        None,
    )?;
    let render_reservation = global_device_registry().reserve(render.identity().clone())?;
    parents.extend(
        DeviceReservation::publish_many([
            (card_reservation, card),
            (connector_reservation, connector),
            (render_reservation, render),
        ])?
        .into_iter(),
    );
    Ok(parents)
}

impl DeviceOps for UpstreamDrmNode {
    fn open_description(&self, _location: &Location, flags: u32) -> VfsResult<Option<DeviceOpen>> {
        let minor = unsafe {
            if self.render {
                (*self.device.raw()).render
            } else {
                (*self.device.raw()).primary
            }
        };
        if minor.is_null() {
            return Err(AxError::NoSuchDevice.into());
        }
        let mut file = Arc::try_new(UpstreamDrmFile::new(
            self.device.clone(),
            self.render,
            flags,
        ))
        .map_err(|_| AxError::NoMemory)?;
        Arc::get_mut(&mut file)
            .ok_or(AxError::InvalidInput)?
            .open(minor)?;
        let file: Arc<dyn FileLike> = file;
        Ok(Some(DeviceOpen::new(file, None)))
    }

    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput.into())
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput.into())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM | NodeFlags::NO_SEEK
    }
}

struct UpstreamDrmFile {
    device: Arc<UpstreamDrmDevice>,
    render: bool,
    raw: UnsafeCell<LinuxFile>,
    inode: UnsafeCell<OpenInode>,
    wait_entry: UnsafeCell<WaitQueueEntry>,
    waiters: Arc<PollSet>,
    nonblocking: AtomicBool,
    access_mode: u32,
    flags_lock: SpinNoIrq<()>,
}

// SAFETY: upstream owns all mutable DRM private-data under its locks; the VFS
// adapter only mutates f_flags under flags_lock and intrusive wait state is
// removed before release.
unsafe impl Send for UpstreamDrmFile {}
unsafe impl Sync for UpstreamDrmFile {}

impl UpstreamDrmFile {
    fn new(device: Arc<UpstreamDrmDevice>, render: bool, flags: u32) -> Self {
        let mut raw: LinuxFile = unsafe { core::mem::zeroed() };
        raw.f_flags = flags;
        Self {
            device,
            render,
            raw: UnsafeCell::new(raw),
            inode: UnsafeCell::new(OpenInode::default()),
            wait_entry: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            waiters: Arc::new(PollSet::new()),
            nonblocking: AtomicBool::new(flags & O_NONBLOCK != 0),
            access_mode: flags & O_ACCMODE,
            flags_lock: SpinNoIrq::new(()),
        }
    }

    fn raw(&self) -> *mut LinuxFile {
        self.raw.get()
    }

    fn allows_read(&self) -> bool {
        matches!(self.access_mode, O_RDONLY | O_RDWR)
    }

    fn allows_write(&self) -> bool {
        matches!(self.access_mode, O_WRONLY | O_RDWR)
    }

    fn open(&mut self, minor: *mut DrmMinor) -> AxResult {
        if unsafe { (*minor).dev != self.device.raw() } {
            return Err(AxError::NoSuchDevice);
        }
        // Use the common upstream open path for the primary minor; the
        // render-node minor is acquired through the companion exported helper.
        let ret = if unsafe { (*minor).r#type == 0 } {
            let inode = self.inode.get();
            unsafe { tk_drm_upstream::drm_file_upstream::drm_open(inode.cast(), self.raw()) }
        } else {
            unsafe {
                tk_drm_upstream::drm_file_upstream::drm_open_minor_index(
                    (*minor).index as u32,
                    self.raw(),
                )
            }
        };
        if ret != 0 {
            return Err(errno_to_ax(ret));
        }
        let private = unsafe { (*self.raw()).private_data.cast::<DrmFile>() };
        if private.is_null() {
            unsafe { tk_drm_upstream::drm_file_upstream::drm_release(ptr::null_mut(), self.raw()) };
            return Err(AxError::NoSuchDevice);
        }
        let entry = self.wait_entry.get();
        unsafe {
            (*entry).private = Arc::as_ptr(&self.waiters).cast_mut().cast();
            (*entry).func = Some(wake_poll_waiters);
            tk_linux_kapi::list::INIT_LIST_HEAD(ptr::addr_of_mut!((*entry).entry));
            add_wait_queue(&mut *ptr::addr_of_mut!((*private).event_wait), &mut *entry);
        }
        Ok(())
    }

    fn read_events(&self, dst: &mut IoDst) -> AxResult<usize> {
        if dst.remaining_mut() == 0 {
            return Ok(0);
        }
        let remaining = dst.remaining_mut();
        let mut sink = IoDstSink { dst, remaining };
        let ret = unsafe {
            tk_drm_upstream::drm_file_upstream::drm_read_kernel_to(
                self.raw(),
                sink.remaining,
                (&mut sink as *mut IoDstSink<'_>).cast(),
                copy_event_to_io_dst,
            )
        };
        if ret < 0 {
            Err(errno_to_ax(ret as i32))
        } else {
            Ok(ret as usize)
        }
    }

    fn poll_events(&self) -> IoEvents {
        let mask =
            unsafe { tk_drm_upstream::drm_file_upstream::drm_poll(self.raw(), ptr::null_mut()) };
        linux_poll_events(mask)
    }
}

impl Drop for UpstreamDrmFile {
    fn drop(&mut self) {
        let raw = self.raw();
        unsafe {
            let private = (*raw).private_data.cast::<DrmFile>();
            if !private.is_null() {
                remove_wait_queue(
                    &mut *ptr::addr_of_mut!((*private).event_wait),
                    &mut *self.wait_entry.get(),
                );
                tk_drm_upstream::drm_file_upstream::drm_release(ptr::null_mut(), raw);
            }
        }
        self.waiters.close();
    }
}

struct IoDstSink<'a> {
    dst: &'a mut IoDst<'a>,
    remaining: usize,
}

unsafe extern "C" fn copy_event_to_io_dst(
    context: *mut c_void,
    bytes: *const c_void,
    len: usize,
) -> isize {
    if context.is_null() || bytes.is_null() {
        return -(EFAULT as isize);
    }
    let sink = unsafe { &mut *context.cast::<IoDstSink<'_>>() };
    if len > sink.remaining {
        return -(EFAULT as isize);
    }
    let source = unsafe { core::slice::from_raw_parts(bytes.cast::<u8>(), len) };
    match sink.dst.write_all(source) {
        Ok(()) => {
            sink.remaining -= len;
            len as isize
        }
        Err(_) => -(EFAULT as isize),
    }
}

unsafe extern "C" fn wake_poll_waiters(
    entry: *mut WaitQueueEntry,
    _mode: u32,
    _sync: c_int,
    _key: *mut c_void,
) -> c_int {
    if entry.is_null() {
        return 0;
    }
    let poll = unsafe { (*entry).private.cast::<PollSet>() };
    if !poll.is_null() {
        // PollSet uses a no-IRQ spin lock, matching Linux waitqueue wake
        // context. The waker callbacks run after that internal lock is dropped.
        unsafe { &*poll }.wake();
    }
    1
}

impl FileLike for UpstreamDrmFile {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        if !self.allows_read() {
            return Err(AxError::BadFileDescriptor);
        }
        block_on_poll_io(self, IoEvents::READABLE, self.nonblocking(), || match self
            .read_events(dst)
        {
            Ok(0) => Err(AxError::WouldBlock),
            other => other,
        })
    }

    fn read_with_operation_status(&self, status: OfdIoStatus, dst: &mut IoDst) -> AxResult<usize> {
        if !self.allows_read() {
            return Err(AxError::BadFileDescriptor);
        }
        block_on_poll_io(
            self,
            IoEvents::READABLE,
            self.nonblocking() || status.rwf_nowait(),
            || match self.read_events(dst) {
                Ok(0) => Err(AxError::WouldBlock),
                other => other,
            },
        )
    }

    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        if !self.allows_write() {
            return Err(AxError::BadFileDescriptor);
        }
        Err(AxError::OperationNotSupported)
    }

    fn ioctl(&self, _context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        let result = unsafe {
            tk_drm_upstream::drm_ioctl_upstream::drm_ioctl(self.raw(), cmd, arg as c_ulong)
        };
        if result < 0 {
            Err(errno_to_ax(result))
        } else {
            Ok(result as usize)
        }
    }

    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        upstream_gem_mmap(self, request).map(Some)
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        let _guard = self.flags_lock.lock();
        self.nonblocking.store(nonblocking, Ordering::Release);
        unsafe {
            let flags = &mut (*self.raw()).f_flags;
            if nonblocking {
                *flags |= O_NONBLOCK;
            } else {
                *flags &= !O_NONBLOCK;
            }
        }
        Ok(())
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(Kstat {
            rdev: if self.render {
                DRM_RENDER_DEVICE_ID
            } else {
                DRM_PRIMARY_DEVICE_ID
            },
            mode: S_IFCHR
                | if self.render {
                    DRM_RENDER_NODE_MODE as u32
                } else {
                    DRM_PRIMARY_NODE_MODE as u32
                },
            nlink: 1,
            blksize: PAGE_SIZE as u32,
            ..Kstat::default()
        })
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(if self.render {
            b"/dev/dri/renderD128"
        } else {
            b"/dev/dri/card0"
        })))
    }
}

impl Pollable for UpstreamDrmFile {
    fn poll(&self) -> IoEvents {
        self.poll_events()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.intersects(IoEvents::READABLE | IoEvents::READ_NORMAL) {
            PollRegistration::single_owned(self.waiters.clone(), context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}

fn errno_to_ax(result: i32) -> AxError {
    let errno = result.saturating_neg();
    LinuxError::try_from(errno)
        .map(AxError::from)
        .unwrap_or(AxError::Io)
}

fn linux_poll_events(mask: u32) -> IoEvents {
    let mut events = IoEvents::empty();
    if mask & EPOLLIN != 0 {
        events |= IoEvents::READABLE;
    }
    if mask & EPOLLRDNORM != 0 {
        events |= IoEvents::READ_NORMAL;
    }
    if mask & EPOLLERR != 0 {
        events |= IoEvents::ERROR;
    }
    if mask & EPOLLHUP != 0 {
        events |= IoEvents::HANGUP;
    }
    events
}

struct UpstreamGemOwner {
    _device: Arc<UpstreamDrmDevice>,
    _object: *mut DrmGemObject,
}
unsafe impl Send for UpstreamGemOwner {}
unsafe impl Sync for UpstreamGemOwner {}

struct ExternalGemMmapOwner {
    _device: Arc<UpstreamDrmDevice>,
    _object: GemObjectLookupRef,
}
unsafe impl Send for ExternalGemMmapOwner {}
unsafe impl Sync for ExternalGemMmapOwner {}

struct GemObjectLookupRef(*mut DrmGemObject);
impl Drop for GemObjectLookupRef {
    fn drop(&mut self) {
        unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_put(self.0) };
    }
}

struct VmaCloseGuard {
    vma: VmAreaStruct,
    armed: bool,
}

impl Drop for VmaCloseGuard {
    fn drop(&mut self) {
        if self.armed && !self.vma.vm_ops.is_null() {
            if let Some(close) = unsafe { (*self.vma.vm_ops).close } {
                unsafe { close(&mut self.vma) };
            }
        }
    }
}

fn upstream_gem_mmap(
    file: &UpstreamDrmFile,
    request: FileMmapRequest,
) -> AxResult<PreparedFileMmap> {
    if request.sharing() != FileMmapSharing::Shared
        || request.page_size() != PAGE_SIZE
        || request.protection().contains(FileMmapProtection::EXECUTE)
    {
        return Err(AxError::OperationNotSupported);
    }
    let dev = file.device.raw();
    if unsafe { (*dev).unplugged || (*dev).vma_offset_manager.is_null() } {
        return Err(AxError::NoSuchDevice);
    }
    let file_page = request.offset() / PAGE_SIZE as u64;
    let map_pages = request.length() / PAGE_SIZE;
    let manager = unsafe { (*dev).vma_offset_manager };
    let manager_lock = unsafe { ptr::addr_of_mut!((*manager).vm_lock).cast() };
    unsafe { read_lock(manager_lock) };
    let node = unsafe {
        tk_drm_upstream::drm_vma_manager_upstream::drm_vma_offset_lookup_locked(
            manager,
            file_page as c_ulong,
            map_pages as c_ulong,
        )
    };
    if node.is_null() {
        unsafe { read_unlock(manager_lock) };
        return Err(AxError::InvalidInput);
    }
    let object = node
        .cast::<u8>()
        .wrapping_sub(offset_of!(DrmGemObject, vma_node))
        .cast::<DrmGemObject>();
    if object.is_null() || unsafe { (*object).dev != dev } {
        unsafe { read_unlock(manager_lock) };
        return Err(AxError::InvalidInput);
    }
    unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_get(object) };
    unsafe { read_unlock(manager_lock) };
    let _object_ref = GemObjectLookupRef(object);
    let private = unsafe { (*file.raw()).private_data.cast::<DrmFile>() };
    if private.is_null()
        || !unsafe {
            tk_drm_upstream::drm_vma_manager_upstream::drm_vma_node_is_allowed(node, private)
        }
    {
        return Err(AxError::PermissionDenied);
    }
    if unsafe { (*object).size == 0 || !(*object).size.is_multiple_of(PAGE_SIZE) } {
        return Err(AxError::InvalidInput);
    }
    let start_page = unsafe { (*node).vm_node.start };
    let region_offset = start_page
        .checked_mul(PAGE_SIZE as u64)
        .ok_or(AxError::InvalidInput)?;
    let relative_offset = request
        .offset()
        .checked_sub(region_offset)
        .ok_or(AxError::InvalidInput)?;
    if relative_offset
        .checked_add(request.length() as u64)
        .is_none_or(|end| end > unsafe { (*object).size as u64 })
    {
        return Err(AxError::InvalidInput);
    }
    let object_file = unsafe { (*object).filp.cast::<LinuxFile>() };
    if object_file.is_null() || unsafe { (*object_file).f_mapping.is_null() } {
        return Err(AxError::InvalidInput);
    }
    if request.protection().contains(FileMmapProtection::WRITE) && !file.allows_write() {
        return Err(AxError::PermissionDenied);
    }

    let count = unsafe { (*object).size / PAGE_SIZE };
    if let Some(mapping) = file
        .device
        .map_blob_gem_object(object)
        .map_err(blob_map_error)?
    {
        return upstream_external_blob_mmap(file, request, region_offset, object, count, mapping);
    }

    let funcs = unsafe { (*object).funcs.as_ref() }.ok_or(AxError::OperationNotSupported)?;
    if funcs.mmap.is_none()
        || funcs.mmap
            != Some(tk_drm_upstream::drm_gem_shmem_helper_upstream::drm_gem_shmem_object_mmap)
    {
        // Only the upstream shmem implementation has a source-compatible page
        // list and paired vm_ops here. Blob imports are mapped by the device's
        // typed aperture provider, never reinterpreted as RAM page-cache pages.
        return Err(AxError::OperationNotSupported);
    }

    let mut vma = VmAreaStruct::empty();
    let mut vm_flags = VM_SHARED | VM_READ;
    if file.allows_write() {
        vm_flags |= VM_MAYWRITE;
    }
    if request.protection().contains(FileMmapProtection::WRITE) {
        if !file.allows_write() {
            return Err(AxError::PermissionDenied);
        }
        vm_flags |= VM_WRITE;
    }
    vma.vm_start = 0;
    vma.vm_end = request.length() as c_ulong;
    vma.vm_flags = vm_flags;
    vma.vm_page_prot = unsafe { vm_get_page_prot(vm_flags) };
    vma.vm_pgoff = (relative_offset / PAGE_SIZE as u64) as c_ulong;
    vma.vm_file = unsafe { (*object).filp.cast() };
    if vma.vm_file.is_null() || unsafe { (*vma.vm_file).f_mapping.is_null() } {
        return Err(AxError::InvalidInput);
    }
    let ret = unsafe {
        tk_drm_upstream::drm_gem_upstream::drm_gem_mmap_obj(
            object,
            (*object).size as c_ulong,
            (&mut vma as *mut VmAreaStruct).cast(),
        )
    };
    if ret != 0 {
        return Err(errno_to_ax(ret));
    }
    let mut close_guard = VmaCloseGuard { vma, armed: true };
    if close_guard.vma.vm_private_data != object.cast() || close_guard.vma.vm_ops.is_null() {
        return Err(AxError::OperationNotSupported);
    }

    let shmem = object.cast::<DrmGemShmemObject>();
    let page_ptrs = unsafe { (*shmem).pages };
    if page_ptrs.is_null() {
        return Err(AxError::InvalidInput);
    }
    let mut phys_pages = Vec::new();
    phys_pages
        .try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..count {
        let page = unsafe { *page_ptrs.add(index) };
        let physical = unsafe { try_page_to_phys(page) }.ok_or(AxError::BadAddress)?;
        phys_pages.push(PhysAddr::from_usize(physical));
    }

    let owner = Arc::try_new(UpstreamGemOwner {
        _device: file.device.clone(),
        _object: object,
    })
    .map_err(|_| AxError::NoMemory)?;
    let owner_shared: Arc<dyn Send + Sync> = owner.clone();
    let pages = Arc::try_new(unsafe { SharedPages::new_page_cache_4k(phys_pages, owner_shared)? })
        .map_err(|_| AxError::NoMemory)?;
    let region = FixedSharedMmapRegion::try_new(
        region_offset,
        pages.clone(),
        FileMmapProtection::READ | FileMmapProtection::WRITE,
    )?;
    let mut plan = region.prepare(request)?.ok_or(AxError::InvalidInput)?;
    let owner_any: Arc<dyn Any + Send + Sync> = owner;
    let ops = unsafe { SharedMmapOps::from_vma(pages, &close_guard.vma, owner_any)? };
    plan = plan.with_shared_mmap_ops(ops)?;
    close_guard.armed = false;
    Ok(plan)
}

fn upstream_external_blob_mmap(
    file: &UpstreamDrmFile,
    request: FileMmapRequest,
    region_offset: u64,
    object: *mut DrmGemObject,
    object_pages: usize,
    mapping: axdriver_display::BlobMapping,
) -> AxResult<PreparedFileMmap> {
    if mapping.pages.len() != object_pages {
        return Err(AxError::InvalidInput);
    }
    const MAP_CACHE_MASK: u32 = 0x0f;
    let cache_flags = match mapping.cache_policy & MAP_CACHE_MASK {
        0 | 1 => axhal::paging::MappingFlags::empty(),
        2 => axhal::paging::MappingFlags::UNCACHED,
        3 => axhal::paging::MappingFlags::WRITE_COMBINING,
        _ => return Err(AxError::InvalidInput),
    };
    let mut phys_pages = Vec::new();
    phys_pages
        .try_reserve_exact(mapping.pages.len())
        .map_err(|_| AxError::NoMemory)?;
    for page in mapping.pages {
        let page = usize::try_from(page).map_err(|_| AxError::InvalidInput)?;
        if !page.is_multiple_of(PAGE_SIZE) {
            return Err(AxError::InvalidInput);
        }
        phys_pages.push(PhysAddr::from_usize(page));
    }

    let lease = Arc::try_new(ExternalPageLease::new_with_transport(
        mapping.lease,
        file.device.mapping_owner(),
    ))
    .map_err(|_| AxError::NoMemory)?;
    let shared = SharedPages::new_external_4k(
        phys_pages,
        lease.clone(),
        axhal::paging::MappingFlags::DEVICE | cache_flags,
    )?;
    let pages = Arc::try_new(shared).map_err(|_| AxError::NoMemory)?;
    file.device.track_external_pages(&lease, &pages)?;

    // The C mmap_obj callback is intentionally not invoked for BAR mappings:
    // tk-virtgpu's generic shmem callback would construct RAM pages instead
    // of the validated MAP_BLOB PFNs. The fixed SharedPages backend publishes
    // those PFNs directly and owns their external lease across VMA splits.
    unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_get(object) };
    let owner = Arc::try_new(ExternalGemMmapOwner {
        _device: file.device.clone(),
        _object: GemObjectLookupRef(object),
    })
    .map_err(|_| AxError::NoMemory)?;
    let owner: Arc<dyn Any + Send + Sync> = owner;
    let region = FixedSharedMmapRegion::try_new(
        region_offset,
        pages,
        FileMmapProtection::READ | FileMmapProtection::WRITE,
    )?;
    let plan = region
        .prepare(request)?
        .ok_or(AxError::InvalidInput)?
        .with_mapping_lifetime(owner);
    Ok(plan)
}

fn blob_map_error(error: axdriver_display::DevError) -> AxError {
    match error {
        axdriver_display::DevError::InvalidParam => AxError::InvalidInput,
        axdriver_display::DevError::NoMemory => AxError::NoMemory,
        axdriver_display::DevError::Unsupported => AxError::OperationNotSupported,
        _ => AxError::Io,
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{Wake, Waker},
    };

    use super::*;

    struct WakeCounter(AtomicUsize);
    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn linux_poll_flags_translate_to_vfs_events() {
        assert_eq!(
            linux_poll_events(EPOLLIN | EPOLLRDNORM | EPOLLERR | EPOLLHUP),
            IoEvents::READABLE | IoEvents::READ_NORMAL | IoEvents::ERROR | IoEvents::HANGUP,
        );
        assert!(linux_poll_events(0).is_empty());
    }

    #[test]
    fn drm_event_wait_wakeup_reaches_vfs_poll_registration() {
        let poll: Arc<PollSet> = Arc::new(PollSet::new());
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let _token = poll.register(&waker).unwrap();
        let mut entry: WaitQueueEntry = unsafe { core::mem::zeroed() };
        entry.private = Arc::as_ptr(&poll).cast_mut().cast();
        assert_eq!(
            unsafe { wake_poll_waiters(&mut entry, u32::MAX, 0, ptr::null_mut()) },
            1
        );
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    }
}
