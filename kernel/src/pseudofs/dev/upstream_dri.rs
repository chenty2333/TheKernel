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
use axfs_ng_vfs::{DeviceId, Location, NodeFlags, VfsError, VfsResult};
use axio::prelude::*;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use kspin::SpinNoIrq;
use linux_raw_sys::general::{O_ACCMODE, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, S_IFCHR};
use memory_addr::PhysAddr;
use tk_drm_upstream::{
    drm_connector_types::{CONNECTOR_STATUS_CONNECTED, DrmConnector},
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
    DRM_PRIMARY_NODE_MODE, DRM_RENDER_NODE_MODE, attach_drm_minor, attribute, mode_attribute,
    platform_registration, publish_pci_parent,
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
            DeviceHandle, DeviceIdentity, DeviceRegistration, MAX_DEVICES,
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
    // SAFETY: `device.raw()` returns a valid, pinned Linux `drm_device` pointer owned by
    // `UpstreamDrmDevice`. Reading the `render` minor pointer is safe and race-free as minor
    // assignments are established during device registration and remain immutable for the device lifetime.
    let has_render = unsafe { !(*device.raw()).render.is_null() };
    if has_render {
        Ok(Some(node(fs, device, true, Vec::new())))
    } else {
        Ok(None)
    }
}

/// `/dev/dri` as a live directory: one `card<N>` and one `renderD<N>` entry per
/// device on the upstream list at lookup time. A device can publish after devfs
/// has mounted (i915 probes from the rootfs-ready callback), so the directory
/// is never a snapshot. Each name's node is created once and reused, because
/// `primary_node()` publishes the device's sysfs identity.
pub(crate) struct DriDirectory {
    fs: Arc<crate::pseudofs::SimpleFs>,
    /// name -> (raw `drm_device` the node serves, node).
    nodes: axsync::Mutex<alloc::collections::BTreeMap<Vec<u8>, (usize, crate::pseudofs::NodeOpsMux)>>,
}

impl DriDirectory {
    pub(crate) fn new(fs: Arc<crate::pseudofs::SimpleFs>) -> Self {
        Self {
            fs,
            nodes: axsync::Mutex::new(alloc::collections::BTreeMap::new()),
        }
    }
}

/// Parses `card<N>` (primary) or `renderD<N>` (render) into `(render, index)`.
fn parse_dri_name(name: &[u8]) -> Option<(bool, u32)> {
    let (render, digits) = if let Some(rest) = name.strip_prefix(b"renderD") {
        (true, rest)
    } else {
        (false, name.strip_prefix(b"card")?)
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    core::str::from_utf8(digits).ok()?.parse().ok().map(|index| (render, index))
}

impl crate::pseudofs::SimpleDirOps for DriDirectory {
    fn child_names<'a>(&'a self) -> VfsResult<crate::pseudofs::ChildNames<'a>> {
        let mut names: Vec<Cow<'a, axfs_ng_vfs::FsName>> = Vec::new();
        for device in crate::drm::upstream::devices() {
            for (render, index) in [(false, device.primary_index()), (true, device.render_index())] {
                if let Some(index) = index {
                    let name = if render { format!("renderD{index}") } else { format!("card{index}") };
                    let name = axfs_ng_vfs::FsNameBuf::from_vec(name.into_bytes())
                        .expect("DRM node names are valid pseudo-filesystem names");
                    names.push(Cow::Owned(name));
                }
            }
        }
        crate::pseudofs::try_boxed_names(names.into_iter())
    }

    fn lookup_child(&self, name: &axfs_ng_vfs::FsName) -> VfsResult<crate::pseudofs::NodeOpsMux> {
        let (render, index) = parse_dri_name(name.as_bytes()).ok_or(VfsError::NotFound)?;
        let device = crate::drm::upstream::devices()
            .into_iter()
            .find(|device| {
                let minor = if render { device.render_index() } else { device.primary_index() };
                minor == Some(index)
            })
            .ok_or(VfsError::NotFound)?;
        let raw = device.raw() as usize;
        let mut nodes = self.nodes.lock();
        if let Some((cached, node)) = nodes.get(name.as_bytes()) {
            if *cached == raw {
                return Ok(node.clone());
            }
        }
        let node: crate::pseudofs::NodeOpsMux = if render {
            render_node(self.fs.clone(), device)?.ok_or(VfsError::NotFound)?.into()
        } else {
            primary_node(self.fs.clone(), device)?.into()
        };
        nodes.insert(name.as_bytes().to_vec(), (raw, node.clone()));
        Ok(node)
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

/// DRM character devices use major 226 with the `drm_minor.index` as minor:
/// primary minors from 0, render minors from 128 (drm_drv.c).
fn minor_device_id(device: &UpstreamDrmDevice, render: bool) -> Option<DeviceId> {
    let index = if render {
        device.render_index()?
    } else {
        device.primary_index()?
    };
    Some(DeviceId::new(226, index))
}

fn node(
    fs: Arc<crate::pseudofs::SimpleFs>,
    device: Arc<UpstreamDrmDevice>,
    render: bool,
    registry_handles: Vec<DeviceHandle<'static, MAX_DEVICES>>,
) -> Arc<crate::pseudofs::Device> {
    let device_id = minor_device_id(&device, render)
        .unwrap_or_else(|| DeviceId::new(226, if render { 128 } else { 0 }));
    crate::pseudofs::Device::new_with_permissions(
        fs,
        axfs_ng_vfs::NodeType::CharacterDevice,
        device_id,
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

/// `(connector name, connected)` for every connector on the device, as
/// `drm_sysfs_connector_add()` names them (`card<N>-<connector->name>`).
fn connectors(device: &UpstreamDrmDevice) -> Vec<(alloc::string::String, bool)> {
    let mut found = Vec::new();
    let dev = device.raw();
    // SAFETY: the connector list is initialized by drm_mode_config_init() and
    // only changes under connector_list_lock, which is held for the walk.
    unsafe {
        let config = ptr::addr_of_mut!((*dev).mode_config);
        let lock = ptr::addr_of_mut!((*config).connector_list_lock);
        tk_linux_kapi::locks::spin_lock(&mut *lock);
        let head = ptr::addr_of_mut!((*config).connector_list);
        let mut entry = (*head).next;
        while !entry.is_null() && entry != head {
            let connector = entry
                .cast::<u8>()
                .sub(core::mem::offset_of!(DrmConnector, head))
                .cast::<DrmConnector>();
            let name = (*connector).name;
            if !name.is_null() {
                let name = core::ffi::CStr::from_ptr(name).to_string_lossy().into_owned();
                if found.try_reserve(1).is_ok() {
                    found.push((name, (*connector).status == CONNECTOR_STATUS_CONNECTED));
                }
            }
            entry = (*entry).next;
        }
        tk_linux_kapi::locks::spin_unlock(&mut *lock);
    }
    found
}

fn publish_sysfs(device: &UpstreamDrmDevice) -> VfsResult<Vec<DeviceHandle<'static, MAX_DEVICES>>> {
    let mut parents = Vec::new();
    let label = device.label();
    let transport = if let Some(identity) = device.pci_identity() {
        publish_pci_parent(identity, &mut parents)?
    } else {
        let platform = platform_registration(label)?;
        let reservation = global_device_registry().reserve(platform.identity().clone())?;
        parents.push(reservation.publish(platform)?);
        format!("platform/{label}")
    };
    let transport = Some(transport);
    let attach = |identity| attach_drm_minor(identity, &transport);
    let card_index = device.primary_index().ok_or(axfs_ng_vfs::VfsError::NotFound)?;
    let card_name = format!("card{card_index}");
    let mut registrations = Vec::new();
    registrations.push(DeviceRegistration::try_new(
        attach(
            DeviceIdentity::new(
                label.into(),
                "drm".into(),
                card_name.clone(),
                minor_device_id(device, false).ok_or(axfs_ng_vfs::VfsError::NotFound)?,
            )?
            .with_devname(format!("dri/{card_name}"))?,
        )?,
        "drm_minor".into(),
        Vec::new(),
        None,
    )?);
    let connector_parent = format!("{}/drm", transport.as_deref().unwrap_or(label));
    for (index, (name, connected)) in connectors(device).into_iter().enumerate() {
        let mut attributes = vec![attribute(
            "status",
            if connected { "connected\n" } else { "disconnected\n" },
        )];
        if index == 0 && device.mode() != (0, 0) {
            attributes.push(mode_attribute(Mode {
                width: device.mode().0,
                height: device.mode().1,
                refresh_millihz: 60_000,
            }));
        }
        registrations.push(DeviceRegistration::try_new(
            DeviceIdentity::without_dev(label.into(), "drm".into(), format!("{card_name}-{name}"))?
                .child_of_path(connector_parent.clone(), card_name.clone())?,
            "drm_connector".into(),
            attributes,
            None,
        )?);
    }
    if let Some(render_id) = minor_device_id(device, true) {
        let render_name = format!("renderD{}", render_id.minor());
        registrations.push(DeviceRegistration::try_new(
            attach(
                DeviceIdentity::new(label.into(), "drm".into(), render_name.clone(), render_id)?
                    .with_devname(format!("dri/{render_name}"))?,
            )?,
            "drm_minor".into(),
            Vec::new(),
            None,
        )?);
    }
    // Card first, then its connectors and render minor, so every child finds
    // its parent already published.
    for registration in registrations {
        let reservation = global_device_registry().reserve(registration.identity().clone())?;
        parents.push(reservation.publish(registration)?);
    }
    Ok(parents)
}

impl DeviceOps for UpstreamDrmNode {
    fn open_description(&self, _location: &Location, flags: u32) -> VfsResult<Option<DeviceOpen>> {
        // SAFETY: `self.device.raw()` yields a valid, pinned Linux `drm_device` whose `primary`
        // and `render` minor pointers are initialized during probe and remain immutable while
        // the `UpstreamDrmDevice` is live.
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
    /// Kernel-side file state for GEM/syncobj aliases. It is initialized by
    /// `open()` before this object is published through `Arc<dyn FileLike>` and
    /// remains immutable until `Drop` has completed the Linux DRM release.
    native_shadow: Option<crate::drm::DrmFile>,
    /// Associates the V1-owned Linux `drm_file` with `native_shadow` without
    /// allocating a second C file or running driver open/close twice.
    native_binding: Option<crate::drm::intel::upstream_file::V1FileBinding>,
    waiters: Arc<PollSet>,
    nonblocking: AtomicBool,
    access_mode: u32,
    flags_lock: SpinNoIrq<()>,
}

// SAFETY: upstream owns all mutable DRM private-data under its locks; the VFS
// adapter only mutates f_flags under flags_lock and intrusive wait state is
// removed before release.
unsafe impl Send for UpstreamDrmFile {}
// SAFETY: Concurrent operations on `UpstreamDrmFile` are synchronized by internal
// DRM locks, `flags_lock` for file flags, atomic nonblocking state, and the
// `PollSet` spinlock for wait queue callbacks.
unsafe impl Sync for UpstreamDrmFile {}

impl UpstreamDrmFile {
    fn new(device: Arc<UpstreamDrmDevice>, render: bool, flags: u32) -> Self {
        // SAFETY: `LinuxFile` is a POD-style C struct whose all-zero bit pattern represents a valid
        // unlinked file descriptor state prior to `drm_open`.
        let mut raw: LinuxFile = unsafe { core::mem::zeroed() };
        raw.f_flags = flags;
        Self {
            device,
            render,
            raw: UnsafeCell::new(raw),
            inode: UnsafeCell::new(OpenInode::default()),
            // SAFETY: An all-zero byte pattern is valid initialization for `WaitQueueEntry`,
            // which is explicitly reinitialized with `INIT_LIST_HEAD` before use in `open()`.
            wait_entry: UnsafeCell::new(unsafe { core::mem::zeroed() }),
            native_shadow: None,
            native_binding: None,
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
        // SAFETY: `minor` was verified non-null in `open_description` and points to a valid
        // `DrmMinor` pinned by DRM device registration. Dereferencing `(*minor).dev` reads the
        // immutable backpointer to the owning `drm_device`.
        if unsafe { (*minor).dev != self.device.raw() } {
            return Err(AxError::NoSuchDevice);
        }
        // Use the common upstream open path for the primary minor; the
        // render-node minor is acquired through the companion exported helper.
        // SAFETY: `minor` points to a valid, live `DrmMinor` pinned by the DRM subsystem.
        // Reading `r#type` discriminates between primary (0) and render minors.
        let ret = if unsafe { (*minor).r#type == 0 } {
            let inode = self.inode.get();
            // SAFETY: `inode` is a valid pointer to this file's allocated `OpenInode` discriminator,
            // and `self.raw()` points to our initialized `LinuxFile`. The DRM core establishes
            // file private data and attaches the file to the primary minor under internal locks.
            unsafe { tk_drm_upstream::drm_file_upstream::drm_open(inode.cast(), self.raw()) }
        } else {
            // SAFETY: `minor` is a valid `DrmMinor` whose `index` field identifies the render minor
            // registered in the DRM xarray. `self.raw()` is our valid `LinuxFile` instance being opened.
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
        // SAFETY: `self.raw()` is our `LinuxFile` successfully initialized by `drm_open*`.
        // Reading `private_data` accesses the DRM core's allocated `DrmFile` context.
        let private = unsafe { (*self.raw()).private_data.cast::<DrmFile>() };
        if private.is_null() {
            // SAFETY: `self.raw()` is our open `LinuxFile`. On initialization failure, `drm_release`
            // cleans up partially acquired DRM state matching Linux file close semantics.
            unsafe { tk_drm_upstream::drm_file_upstream::drm_release(ptr::null_mut(), self.raw()) };
            return Err(AxError::NoSuchDevice);
        }
        // A legacy-core device exists only while the i915 V1 bridge needs a
        // kernel-side GEM/syncobj alias table; upstream-only devices dispatch
        // straight to drm_ioctl().
        if let Some(native_device) = crate::drm::primary_device() {
            // Use a non-master shadow file for both DRM node kinds: the upstream
            // Linux file remains authoritative for DRM mastership, while this
            // kernel file owns only GEM and syncobj identity aliases.
            let native = match native_device.open_render() {
                Ok(native) => native,
                Err(error) => {
                    // SAFETY: `self.raw()` points to our valid open `LinuxFile`. Calling `drm_release`
                    // performs necessary driver cleanup before aborting `open`.
                    unsafe {
                        tk_drm_upstream::drm_file_upstream::drm_release(ptr::null_mut(), self.raw())
                    };
                    return Err(AxError::from(error));
                }
            };
            // SAFETY: `self.raw()` points to our valid `LinuxFile` whose `f_flags` are initialized
            // during `new()` and modified only under `flags_lock`.
            native.set_open_flags(unsafe { (*self.raw()).f_flags });
            let binding = match crate::drm::intel::upstream_file::V1FileBinding::register(
                private.cast(),
            ) {
                Ok(binding) => binding,
                Err(error) => {
                    // SAFETY: `self.raw()` points to our valid open `LinuxFile`. Rolling back with
                    // `drm_release` frees the DRM file context upon shadow registration failure.
                    unsafe {
                        tk_drm_upstream::drm_file_upstream::drm_release(ptr::null_mut(), self.raw())
                    };
                    return Err(error);
                }
            };
            self.native_shadow = Some(native);
            self.native_binding = Some(binding);
        }
        let entry = self.wait_entry.get();
        // SAFETY: `entry` is pinned by `UpstreamDrmFile`'s allocation, `self.waiters` is a valid
        // `Arc<PollSet>` whose raw pointer remains valid until drop, and `private` was validated
        // non-null and points to the live `DrmFile` whose `event_wait` queue is initialized by DRM core.
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
        // SAFETY: `self.raw()` points to our live, open `LinuxFile`. `&mut sink` remains valid and
        // pinned on the stack for the synchronous duration of `drm_read_kernel_to`. The callback
        // `copy_event_to_io_dst` only accesses the provided sink pointer and valid event bytes.
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
        // SAFETY: `self.raw()` is a valid open `LinuxFile`. Passing a null poll_table queries the
        // current readiness mask without registering a new wait queue entry, which is already handled
        // via `wait_entry` in `open()`.
        let mask = unsafe { tk_drm_upstream::drm_file_upstream::drm_poll(self.raw(), ptr::null_mut()) };
        linux_poll_events(mask)
    }

    fn with_native_binding<T>(
        &self,
        call: impl FnOnce(&crate::drm::DrmFile) -> AxResult<T>,
    ) -> AxResult<T> {
        let native = self
            .native_shadow
            .as_ref()
            .ok_or(AxError::NoSuchDevice)?;
        let binding = self
            .native_binding
            .as_ref()
            .ok_or(AxError::NoSuchDevice)?;
        let _active = binding.bind(native)?;
        call(native)
    }
}

impl Drop for UpstreamDrmFile {
    fn drop(&mut self) {
        let raw = self.raw();
        // SAFETY: `raw` points to this file's `LinuxFile` being dropped. If `private_data` was set,
        // it points to a valid `DrmFile` whose `event_wait` queue contains `self.wait_entry`.
        // We remove the wait queue entry to prevent dangling wakeups and call `drm_release` to
        // execute driver postclose callbacks and release DRM resources under DRM locks.
        unsafe {
            let private = (*raw).private_data.cast::<DrmFile>();
            if !private.is_null() {
                remove_wait_queue(
                    &mut *ptr::addr_of_mut!((*private).event_wait),
                    &mut *self.wait_entry.get(),
                );
                if let (Some(native), Some(binding)) =
                    (self.native_shadow.as_ref(), self.native_binding.as_ref())
                {
                    let active = binding.bind(native);
                    if active.is_ok() {
                        tk_drm_upstream::drm_file_upstream::drm_release(ptr::null_mut(), raw);
                        drop(active);
                    } else {
                        // Preserve the Linux close lifecycle even if alias
                        // registration was unexpectedly lost; release remains
                        // owned by this VFS OFD and must not be skipped.
                        tk_drm_upstream::drm_file_upstream::drm_release(ptr::null_mut(), raw);
                    }
                } else {
                    tk_drm_upstream::drm_file_upstream::drm_release(ptr::null_mut(), raw);
                }
            }
        }
        // The C file has now completed postclose and all GEM/syncobj release
        // callbacks. Unregister its backpointer before the shadow file drops.
        self.native_binding.take();
        self.native_shadow.take();
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
    // SAFETY: `context` was passed from `read_events` as `&mut IoDstSink`, verified non-null above,
    // and is pinned on the caller's stack for the duration of the synchronous C helper call.
    let sink = unsafe { &mut *context.cast::<IoDstSink<'_>>() };
    if len > sink.remaining {
        return -(EFAULT as isize);
    }
    // SAFETY: `bytes` was checked non-null and points to `len` contiguous initialized bytes
    // emitted by the upstream DRM event read loop.
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
    // SAFETY: `entry` is non-null and corresponds to `UpstreamDrmFile.wait_entry`, which remains
    // valid while registered on DRM's event_wait queue. Reading `private` retrieves the pointer to
    // the owning `PollSet`.
    let poll = unsafe { (*entry).private.cast::<PollSet>() };
    if !poll.is_null() {
        // PollSet uses a no-IRQ spin lock, matching Linux waitqueue wake
        // context. The waker callbacks run after that internal lock is dropped.
        // SAFETY: `poll` was checked non-null and points to the `Arc<PollSet>` kept alive by
        // `UpstreamDrmFile`. The wake call safely acquires the internal spinlock to notify waiters.
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
        let call = || {
            // SAFETY: `self.raw()` is a valid open `LinuxFile`. Upstream `drm_ioctl` handles
            // validation, permission checks, and internal locking for the command and user
            // argument pointer.
            let result = unsafe {
                tk_drm_upstream::drm_ioctl_upstream::drm_ioctl(self.raw(), cmd, arg as c_ulong)
            };
            if result < 0 {
                Err(errno_to_ax(result))
            } else {
                Ok(result as usize)
            }
        };
        if self.native_shadow.is_some() {
            self.with_native_binding(|_| call())
        } else {
            call()
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
        if let Some(native) = self.native_shadow.as_ref() {
            native.set_nonblocking_flag(nonblocking);
        }
        // SAFETY: `self.raw()` points to this instance's `LinuxFile`. Mutating `f_flags` is synchronized
        // by `self.flags_lock`, ensuring no concurrent data races with other flag modifications.
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
            rdev: minor_device_id(&self.device, self.render)
                .unwrap_or_else(|| DeviceId::new(226, if self.render { 128 } else { 0 })),
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
// SAFETY: `UpstreamGemOwner` holds a refcounted reference to `UpstreamDrmDevice` and a raw pointer
// to a GEM object whose lifetime is guaranteed by GEM refcounting. It contains no thread-bound state
// and can safely be sent across threads.
unsafe impl Send for UpstreamGemOwner {}
// SAFETY: Access to the underlying DRM device and GEM object is synchronized by DRM core locks
// and atomic refcounts, making concurrent references across threads safe.
unsafe impl Sync for UpstreamGemOwner {}

struct ExternalGemMmapOwner {
    _device: Arc<UpstreamDrmDevice>,
    _object: GemObjectLookupRef,
}
// SAFETY: `ExternalGemMmapOwner` holds an `Arc<UpstreamDrmDevice>` and a ref-managed GEM object pointer.
// Ownership can safely be transferred between threads as both are thread-safe refcounted resources.
unsafe impl Send for ExternalGemMmapOwner {}
// SAFETY: The referenced `UpstreamDrmDevice` and GEM object are thread-safe and internally synchronized,
// allowing concurrent shared references across threads.
unsafe impl Sync for ExternalGemMmapOwner {}

struct GemObjectLookupRef(*mut DrmGemObject);
impl Drop for GemObjectLookupRef {
    fn drop(&mut self) {
        // SAFETY: `self.0` holds a reference-counted `DrmGemObject` acquired via `drm_gem_object_get`.
        // Calling `drm_gem_object_put` releases that reference using DRM's thread-safe atomic refcounting.
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
            // SAFETY: `self.vma.vm_ops` is non-null and was assigned by `drm_gem_mmap_obj` to the
            // driver's static `vm_operations_struct`. Reading function pointer `close` is safe and race-free.
            if let Some(close) = unsafe { (*self.vma.vm_ops).close } {
                // SAFETY: `self.vma` is the initialized `VmAreaStruct` passed to `drm_gem_mmap_obj`.
                // Calling the driver's `close` callback tears down the VMA mapping state on error.
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
    // SAFETY: `file.device.raw()` returns a valid, pinned Linux `drm_device` pointer owned by
    // `file.device`. Reading `unplugged` and `vma_offset_manager` inspects fields that remain
    // allocated throughout the device lifetime.
    if unsafe { (*dev).unplugged || (*dev).vma_offset_manager.is_null() } {
        return Err(AxError::NoSuchDevice);
    }
    let file_page = request.offset() / PAGE_SIZE as u64;
    let map_pages = request.length() / PAGE_SIZE;
    // SAFETY: `dev` is valid and `vma_offset_manager` was confirmed non-null above.
    let manager = unsafe { (*dev).vma_offset_manager };
    // SAFETY: `manager` is a valid `drm_vma_offset_manager` whose `vm_lock` is an initialized rwlock.
    let manager_lock = unsafe { ptr::addr_of_mut!((*manager).vm_lock).cast() };
    // SAFETY: `manager_lock` points to the valid rwlock of the VMA offset manager.
    unsafe { read_lock(manager_lock) };
    // SAFETY: `manager` is valid and locked via `read_lock(manager_lock)`. The lookup traverses
    // the VMA rb-tree safely under this lock.
    let node = unsafe {
        tk_drm_upstream::drm_vma_manager_upstream::drm_vma_offset_lookup_locked(
            manager,
            file_page as c_ulong,
            map_pages as c_ulong,
        )
    };
    if node.is_null() {
        // SAFETY: `manager_lock` is held under `read_lock` and must be released before returning error.
        unsafe { read_unlock(manager_lock) };
        return Err(AxError::InvalidInput);
    }
    let object = node
        .cast::<u8>()
        .wrapping_sub(offset_of!(DrmGemObject, vma_node))
        .cast::<DrmGemObject>();
    if object.is_null()
        // SAFETY: `node` was verified non-null above and represents the embedded `vma_node` within
        // the enclosing `DrmGemObject`. Accessing `(*object).dev` under `manager_lock` verifies
        // that the object belongs to this DRM device.
        || unsafe { (*object).dev != dev }
    {
        // SAFETY: `manager_lock` is held by `read_lock` and must be released before returning error.
        unsafe { read_unlock(manager_lock) };
        return Err(AxError::InvalidInput);
    }
    // SAFETY: `object` is confirmed valid and belongs to `dev`; incrementing its reference count
    // via `drm_gem_object_get` prevents concurrent destruction while holding `manager_lock`.
    unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_get(object) };
    // SAFETY: `manager_lock` was acquired with `read_lock` and can now be released after taking the object ref.
    unsafe { read_unlock(manager_lock) };
    let _object_ref = GemObjectLookupRef(object);
    // SAFETY: `file.raw()` points to this file's valid open `LinuxFile`. Reading `private_data`
    // retrieves the associated `DrmFile` structure.
    let private = unsafe { (*file.raw()).private_data.cast::<DrmFile>() };
    if private.is_null()
        // SAFETY: `node` is the valid `drm_vma_offset_node` looked up above, and `private` was checked
        // non-null. `drm_vma_node_is_allowed` checks the per-file offset access permissions table.
        || !unsafe {
            tk_drm_upstream::drm_vma_manager_upstream::drm_vma_node_is_allowed(node, private)
        }
    {
        return Err(AxError::PermissionDenied);
    }
    // SAFETY: `object` is kept alive by `_object_ref`. Accessing its `size` field is safe and immutable.
    if unsafe { (*object).size == 0 || !(*object).size.is_multiple_of(PAGE_SIZE) } {
        return Err(AxError::InvalidInput);
    }
    // SAFETY: `node` points to the `drm_vma_offset_node` embedded in `object`, kept alive by `_object_ref`.
    // Reading `vm_node.start` retrieves the allocated offset page address.
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
        .is_none_or(|end| {
            // SAFETY: `object` is kept alive by `_object_ref`; reading `size` is safe and immutable.
            end > unsafe { (*object).size as u64 }
        })
    {
        return Err(AxError::InvalidInput);
    }
    // SAFETY: `object` is valid and kept alive by `_object_ref`. Reading `filp` accesses its
    // underlying shmem backing file.
    let object_file = unsafe { (*object).filp.cast::<LinuxFile>() };
    if object_file.is_null()
        // SAFETY: `object_file` was checked non-null; reading its `f_mapping` pointer verifies that
        // address space operations and page cache mappings are initialized.
        || unsafe { (*object_file).f_mapping.is_null() }
    {
        return Err(AxError::InvalidInput);
    }
    if request.protection().contains(FileMmapProtection::WRITE) && !file.allows_write() {
        return Err(AxError::PermissionDenied);
    }

    // SAFETY: `object` is valid and kept alive by `_object_ref`; `size` is immutable.
    let count = unsafe { (*object).size / PAGE_SIZE };
    if let Some(mapping) = file
        .device
        .map_blob_gem_object(object)
        .map_err(blob_map_error)?
    {
        return upstream_external_blob_mmap(file, request, region_offset, object, count, mapping);
    }

    // SAFETY: `object` is valid and alive; `funcs` points to static driver GEM object operations.
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
    // SAFETY: `vm_get_page_prot` calculates architecture page protection flags for valid `vm_flags`.
    vma.vm_page_prot = unsafe { vm_get_page_prot(vm_flags) };
    vma.vm_pgoff = (relative_offset / PAGE_SIZE as u64) as c_ulong;
    // SAFETY: `object` is alive via `_object_ref`; reading `filp` assigns the VMA backing file pointer.
    vma.vm_file = unsafe { (*object).filp.cast() };
    if vma.vm_file.is_null()
        // SAFETY: `vma.vm_file` was checked non-null; verifying `f_mapping` ensures valid address space operations.
        || unsafe { (*vma.vm_file).f_mapping.is_null() }
    {
        return Err(AxError::InvalidInput);
    }
    // SAFETY: `object` is valid and kept alive by `_object_ref`. `vma` is an initialized `VmAreaStruct`
    // with valid offsets, size, flags, and file mapping. `drm_gem_mmap_obj` sets up driver VMA hooks.
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
    // SAFETY: `object` was verified to use `drm_gem_shmem_object_mmap`, guaranteeing it is an instance
    // of `DrmGemShmemObject`. After successful `drm_gem_mmap_obj`, `(*shmem).pages` points to the pinned page array.
    let page_ptrs = unsafe { (*shmem).pages };
    if page_ptrs.is_null() {
        return Err(AxError::InvalidInput);
    }
    let mut phys_pages = Vec::new();
    phys_pages
        .try_reserve_exact(count)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..count {
        // SAFETY: `page_ptrs` points to an array of at least `count` page pointers pinned by the GEM shmem object.
        let page = unsafe { *page_ptrs.add(index) };
        // SAFETY: `page` is a valid Linux page pointer from the GEM shmem object's page array.
        // `try_page_to_phys` translates it to a physical page address.
        let physical = unsafe { try_page_to_phys(page) }.ok_or(AxError::BadAddress)?;
        phys_pages.push(PhysAddr::from_usize(physical));
    }

    let owner = Arc::try_new(UpstreamGemOwner {
        _device: file.device.clone(),
        _object: object,
    })
    .map_err(|_| AxError::NoMemory)?;
    let owner_shared: Arc<dyn Send + Sync> = owner.clone();
    // SAFETY: `phys_pages` contains validated 4K physical page addresses owned and kept pinned
    // by `owner_shared` (`UpstreamGemOwner`) for the lifetime of this mapping.
    let pages = Arc::try_new(unsafe { SharedPages::new_page_cache_4k(phys_pages, owner_shared)? })
        .map_err(|_| AxError::NoMemory)?;
    let region = FixedSharedMmapRegion::try_new(
        region_offset,
        pages.clone(),
        FileMmapProtection::READ | FileMmapProtection::WRITE,
    )?;
    let mut plan = region.prepare(request)?.ok_or(AxError::InvalidInput)?;
    let owner_any: Arc<dyn Any + Send + Sync> = owner;
    // SAFETY: `close_guard.vma` contains valid `vm_ops` and `vm_private_data` installed by
    // `drm_gem_mmap_obj`. `owner_any` keeps the underlying GEM object alive while `SharedMmapOps` is active.
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
    // SAFETY: `object` points to a valid `DrmGemObject` passed from `upstream_gem_mmap`.
    // We increment its refcount before transferring ownership to `ExternalGemMmapOwner`.
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
        task::Waker,
    };
    use alloc::task::Wake;

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
        // SAFETY: An all-zero byte pattern initializes a valid, empty `WaitQueueEntry` test fixture.
        let mut entry: WaitQueueEntry = unsafe { core::mem::zeroed() };
        entry.private = Arc::as_ptr(&poll).cast_mut().cast();
        assert_eq!(
            // SAFETY: `entry` is properly initialized with `entry.private` pointing to the live `poll` Arc.
            unsafe { wake_poll_waiters(&mut entry, u32::MAX, 0, ptr::null_mut()) },
            1
        );
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    }
}
