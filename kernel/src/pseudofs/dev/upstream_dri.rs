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
use linux_raw_sys::general::{
    O_ACCMODE, O_CLOEXEC, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, S_IFCHR,
};
use memory_addr::PhysAddr;
use tk_drm_upstream::{
    drm_connector_types::{CONNECTOR_STATUS_CONNECTED, DrmConnector},
    drm_file_types::{DrmFile, DrmMinor},
    drm_gem_shmem_helper_types_upstream::DrmGemShmemObject,
    drm_gem_types::DrmGemObject,
    drm_lease_upstream::{LeaseFileOps, LesseeFileSetup, install_lease_file_ops},
};
use tk_linux_kapi::{
    locks::{read_lock, read_unlock},
    mm::{VM_MAYWRITE, VM_READ, VM_SHARED, VM_WRITE, VmAreaStruct, vm_get_page_prot},
    shmem::try_page_to_phys,
    types::{File as LinuxFile, WaitQueueEntry},
    wait::{add_wait_queue, remove_wait_queue},
};

use crate::{
    drm::upstream::UpstreamDrmDevice,
    file::{
        FileDescription, FileLike, FileMmapProtection, FileMmapRequest, FileMmapSharing,
        FixedSharedMmapRegion, IoDst, IoSrc, IoctlContext, Kstat, OfdIoStatus, PreparedFileMmap,
        ReservedFd, SharedMmapOps,
    },
    mm::{ExternalPageLease, SharedPages},
    pseudofs::{
        DeviceOpen, DeviceOps,
        device_registry::{
            DeviceAttribute, DeviceHandle, DeviceIdentity, DeviceRegistration, DeviceReservation,
            MAX_DEVICES, global_device_registry,
        },
    },
    readiness::block_on_poll_io,
};

const PAGE_SIZE: usize = 4096;
pub(crate) const DRM_PRIMARY_NODE_MODE: u16 = 0o600;
// udev grants the conventional render group after userspace starts; before
// that, do not expose a world-writable GPU node.
pub(crate) const DRM_RENDER_NODE_MODE: u16 = 0o600;

const EFAULT: i32 = 14;
const ENODEV: i32 = 19;
const EPOLLIN: u32 = 0x0001;
const EPOLLRDNORM: u32 = 0x0040;
const EPOLLERR: u32 = 0x0008;
const EPOLLHUP: u32 = 0x0010;

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

/// `/dev/dri` as a live directory: eagerly publish nodes for devices already
/// registered when devfs is built, then add devices installed later on lookup.
/// Eager publication makes their sysfs identities available before sysfs is
/// mounted and udev coldplugs. Each name's node is created once and reused,
/// because `primary_node()` publishes the device's sysfs identity.
pub(crate) struct DriDirectory {
    fs: Arc<crate::pseudofs::SimpleFs>,
    /// name -> (raw `drm_device` the node serves, node).
    nodes:
        axsync::Mutex<alloc::collections::BTreeMap<Vec<u8>, (usize, crate::pseudofs::NodeOpsMux)>>,
}

impl DriDirectory {
    pub(crate) fn new(fs: Arc<crate::pseudofs::SimpleFs>) -> Self {
        let mut nodes = alloc::collections::BTreeMap::new();
        for device in crate::drm::upstream::devices() {
            if let Err(error) = cache_device_nodes(&fs, &mut nodes, device.clone()) {
                warn!(
                    "DRM device {} initial sysfs publication failed: {error:?}",
                    device.label()
                );
            }
        }
        Self {
            fs,
            nodes: axsync::Mutex::new(nodes),
        }
    }
}

/// Populate/reuse both devnodes for a registered device. The primary node is
/// deliberately built first: it publishes the sysfs device objects (including
/// `dev` and `DEVNAME` metadata) that udev needs to set render-node ownership.
fn cache_device_nodes(
    fs: &Arc<crate::pseudofs::SimpleFs>,
    nodes: &mut alloc::collections::BTreeMap<Vec<u8>, (usize, crate::pseudofs::NodeOpsMux)>,
    device: Arc<UpstreamDrmDevice>,
) -> VfsResult<()> {
    let raw = device.raw() as usize;
    let Some(primary_index) = device.primary_index() else {
        return Ok(());
    };
    let primary_name = format!("card{primary_index}").into_bytes();
    let primary_cached = nodes
        .get(&primary_name)
        .is_some_and(|(cached, _)| *cached == raw);
    if !primary_cached {
        let node: crate::pseudofs::NodeOpsMux = primary_node(fs.clone(), device.clone())?.into();
        nodes.insert(primary_name, (raw, node));
    }

    if let Some(render_index) = device.render_index() {
        let render_name = format!("renderD{render_index}").into_bytes();
        let render_cached = nodes
            .get(&render_name)
            .is_some_and(|(cached, _)| *cached == raw);
        if !render_cached {
            let node: crate::pseudofs::NodeOpsMux = render_node(fs.clone(), device)?
                .ok_or(VfsError::NotFound)?
                .into();
            nodes.insert(render_name, (raw, node));
        }
    }
    Ok(())
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
    core::str::from_utf8(digits)
        .ok()?
        .parse()
        .ok()
        .map(|index| (render, index))
}

impl crate::pseudofs::SimpleDirOps for DriDirectory {
    fn child_names<'a>(&'a self) -> VfsResult<crate::pseudofs::ChildNames<'a>> {
        let mut names: Vec<Cow<'a, axfs_ng_vfs::FsName>> = Vec::new();
        for device in crate::drm::upstream::devices() {
            for (render, index) in [
                (false, device.primary_index()),
                (true, device.render_index()),
            ] {
                if let Some(index) = index {
                    let name = if render {
                        format!("renderD{index}")
                    } else {
                        format!("card{index}")
                    };
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
                let minor = if render {
                    device.render_index()
                } else {
                    device.primary_index()
                };
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
        if let Err(error) = cache_device_nodes(&self.fs, &mut nodes, device.clone()) {
            if !render {
                warn!("DRM primary minor {index} sysfs publication failed: {error:?}");
            }
            return Err(error);
        }
        nodes
            .get(name.as_bytes())
            .filter(|(cached, _)| *cached == raw)
            .map(|(_, node)| node.clone())
            .ok_or(VfsError::NotFound)
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
                let name = core::ffi::CStr::from_ptr(name)
                    .to_string_lossy()
                    .into_owned();
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

pub(super) fn attribute(name: &str, value: &'static str) -> DeviceAttribute {
    DeviceAttribute::try_new(name.into(), move || Ok(value)).expect("static DRM sysfs attribute")
}

pub(super) fn mode_attribute(width: u32, height: u32) -> DeviceAttribute {
    DeviceAttribute::try_new("modes".into(), move || {
        Ok(alloc::format!("{width}x{height}\n"))
    })
    .expect("static DRM sysfs mode attribute")
}

// libdrm resolves the DRM node's physical device (skipping a real virtio
// transport when present), then reads PCI_SLOT_NAME and these PCI attributes.
pub(super) fn pci_registrations(
    identity: axdriver_display::DisplayPciIdentity,
) -> VfsResult<Vec<Arc<DeviceRegistration>>> {
    let root = alloc::format!("pci0000:{:02x}", identity.bus);
    let bdf = alloc::format!(
        "0000:{:02x}:{:02x}.{}",
        identity.bus,
        identity.device,
        identity.function
    );
    let virtio = alloc::format!(
        "virtio{}",
        ((identity.bus as u32) << 8) | ((identity.device as u32) << 3) | identity.function as u32
    );
    let hex = |name: &str, value: u16| {
        DeviceAttribute::try_new(name.into(), move || Ok(alloc::format!("0x{value:04x}\n")))
            .expect("PCI attribute")
    };
    let pci = DeviceRegistration::try_bus_device(
        DeviceIdentity::without_dev(root.clone(), "pci".into(), bdf.clone())?,
        "pci_device".into(),
        vec![
            hex("vendor", identity.vendor_id),
            hex("device", identity.device_id),
            hex("subsystem_vendor", identity.subsystem_vendor),
            hex("subsystem_device", identity.subsystem_device),
            hex("revision", identity.revision as u16),
        ],
        "pci".into(),
        false,
    )?;
    // Native PCI GPUs are not VirtIO devices. In particular, an Intel render
    // node must not publish a fabricated virtio modalias/parent to udev.
    if identity.vendor_id != 0x1af4 {
        return Ok(vec![pci]);
    }
    let transport = DeviceRegistration::try_bus_device(
        DeviceIdentity::without_dev(root.clone(), "virtio".into(), virtio)?
            .child_of_path(root, bdf)?,
        "virtio_device".into(),
        vec![attribute("modalias", "virtio:d00000010v00001AF4\n")],
        "virtio".into(),
        true,
    )?;
    Ok(vec![pci, transport])
}

pub(super) fn platform_registration(name: &str) -> VfsResult<Arc<DeviceRegistration>> {
    DeviceRegistration::try_bus_device(
        DeviceIdentity::without_dev("platform".into(), "platform".into(), name.into())?,
        "platform_device".into(),
        Vec::new(),
        "platform".into(),
        false,
    )
}

pub(super) fn publish_pci_parent(
    identity: axdriver_display::DisplayPciIdentity,
    parents: &mut Vec<DeviceHandle<'static, MAX_DEVICES>>,
) -> VfsResult<alloc::string::String> {
    publish_pci_parent_in(global_device_registry(), identity, parents)
}

fn publish_pci_parent_in<'a>(
    registry: &'a crate::pseudofs::device_registry::DeviceRegistry<MAX_DEVICES>,
    identity: axdriver_display::DisplayPciIdentity,
    parents: &mut Vec<DeviceHandle<'a, MAX_DEVICES>>,
) -> VfsResult<alloc::string::String> {
    let mut registrations = pci_registrations(identity)?.into_iter();
    let pci = registrations.next().expect("PCI registration");
    let path = alloc::format!("{}/{}", pci.identity().bus, pci.identity().name);
    // Sysfs publishes the full PCI inventory before userspace can look up the
    // live /dev/dri directory. Reuse that exact, already-published BDF node:
    // pci_sysfs owns its DeviceHandle for the boot inventory lifetime. Do not
    // treat an in-flight reservation or any other collision as a reusable
    // parent.
    let pci_reservation = match registry.reserve(pci.identity().clone()) {
        Ok(reservation) => Some(reservation),
        Err(VfsError::AlreadyExists) if registry.is_published_identity(pci.identity()) => None,
        Err(error) => return Err(error),
    };
    if let Some(virtio) = registrations.next() {
        let name = virtio.identity().name.clone();
        let virtio_reservation = registry.reserve(virtio.identity().clone())?;
        if let Some(pci_reservation) = pci_reservation {
            let (pci, virtio) =
                DeviceReservation::publish_pair(pci_reservation, pci, virtio_reservation, virtio)?;
            parents.extend([pci, virtio]);
        } else {
            // The PCI inventory's retained handle owns the existing parent;
            // only the newly-created VirtIO child belongs to this DRM node.
            parents.push(virtio_reservation.publish(virtio)?);
        }
        Ok(alloc::format!("{path}/{name}"))
    } else {
        if let Some(pci_reservation) = pci_reservation {
            parents.push(pci_reservation.publish(pci)?);
        }
        Ok(path)
    }
}

pub(super) fn attach_drm_minor(
    identity: DeviceIdentity,
    transport: &Option<alloc::string::String>,
) -> VfsResult<DeviceIdentity> {
    match transport {
        Some(path) => identity.child_of_path(path.clone(), "drm".into()),
        None => Ok(identity),
    }
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
    let card_index = device
        .primary_index()
        .ok_or(axfs_ng_vfs::VfsError::NotFound)?;
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
            if connected {
                "connected\n"
            } else {
                "disconnected\n"
            },
        )];
        if index == 0 && device.mode() != (0, 0) {
            attributes.push(mode_attribute(device.mode().0, device.mode().1));
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
    fn open_description(&self, location: &Location, flags: u32) -> VfsResult<Option<DeviceOpen>> {
        if !self.render {
            // The seat grants the active session's user access to primary
            // nodes (logind's uaccess ACL), so record every one that is opened.
            crate::pseudofs::dev::tty::remember_primary_node(location)?;
        }
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
            // SAFETY: An all-zero byte pattern is valid initialization for `WaitQueueEntry`,
            // which is explicitly reinitialized with `INIT_LIST_HEAD` before use in `open()`.
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

    fn dispatch_ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
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
        if cmd != tk_drm_upstream::uapi_drm_consts::DRM_IOCTL_DROP_MASTER || self.render {
            return call();
        }
        // SAFETY: `open()` set private_data to this file's live `drm_file`.
        let private = unsafe { (*self.raw()).private_data.cast::<DrmFile>() };
        // SAFETY: `private` stays live until this file's release.
        let was_master =
            unsafe { tk_drm_upstream::drm_auth_upstream::drm_is_current_master(private) };
        let result = call();
        if was_master && result.is_ok() {
            crate::pseudofs::dev::restore_console_after_master_close();
        }
        result
    }

    fn allows_read(&self) -> bool {
        matches!(self.access_mode, O_RDONLY | O_RDWR)
    }

    fn allows_write(&self) -> bool {
        matches!(self.access_mode, O_WRONLY | O_RDWR)
    }

    fn open(&mut self, minor: *mut DrmMinor) -> AxResult {
        install_lease_file_ops(&LEASE_FILE_OPS).map_err(|_| AxError::BadState)?;
        // SAFETY: `minor` was verified non-null in `open_description` and points to a valid
        // `DrmMinor` pinned by DRM device registration. Dereferencing `(*minor).dev` reads the
        // immutable backpointer to the owning `drm_device`.
        if unsafe { (*minor).dev != self.device.raw() } {
            return Err(AxError::NoSuchDevice);
        }
        // Select this exact primary/render minor. The legacy inode shim
        // opens primary index zero and cannot clone a lease on another card.
        // SAFETY: the pinned minor identifies this device in the DRM xarray;
        // self.raw() is an initialized, unpublished LinuxFile.
        let ret = unsafe {
            tk_drm_upstream::drm_file_upstream::drm_open_minor_index(
                (*minor).index as u32,
                self.raw(),
            )
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
        let mask =
            unsafe { tk_drm_upstream::drm_file_upstream::drm_poll(self.raw(), ptr::null_mut()) };
        linux_poll_events(mask)
    }
}

static LEASE_FILE_OPS: LeaseFileOps = LeaseFileOps { open_lessee_fd };

/// Creates a fresh DRM file rather than duplicating the lessor's description.
/// The only user DRM files in this adapter embed `LinuxFile` in the stable
/// `UpstreamDrmFile` allocation, including files previously created by a lease.
unsafe extern "C" fn open_lessee_fd(
    lessor_filp: *mut LinuxFile,
    flags: c_int,
    setup: LesseeFileSetup,
    ctx: *mut c_void,
) -> c_int {
    if lessor_filp.is_null() {
        return -ENODEV;
    }
    // SAFETY: CREATE_LEASE is dispatched from a live UpstreamDrmFile. Its
    // embedded UnsafeCell<LinuxFile> has LinuxFile's transparent layout, and
    // the outer file is pinned by the caller throughout this ioctl.
    let lessor = unsafe {
        &*lessor_filp
            .cast::<u8>()
            .sub(offset_of!(UpstreamDrmFile, raw))
            .cast::<UpstreamDrmFile>()
    };
    let result = (|| {
        let reservation = crate::file::reserve_fd(flags as u32 & O_CLOEXEC != 0)?;
        // SAFETY: the DRM core supplies its setup callback and retains the
        // context until successful publication transfers it to the new file.
        unsafe { publish_lessee(lessor, flags as u32, reservation, setup, ctx) }
    })();
    result.unwrap_or_else(|error| -LinuxError::from(error).code())
}

/// All fallible work precedes `setup`: its master-reference transfer must not
/// occur on a failed fd creation. PreparedFdPublication makes the final fd
/// visibility change infallible.
unsafe fn publish_lessee(
    lessor: &UpstreamDrmFile,
    flags: u32,
    reservation: ReservedFd,
    setup: LesseeFileSetup,
    ctx: *mut c_void,
) -> AxResult<c_int> {
    let status_flags = O_RDWR | (flags & O_NONBLOCK);
    let mut file = Arc::try_new(UpstreamDrmFile::new(
        lessor.device.clone(),
        false,
        status_flags,
    ))
    .map_err(|_| AxError::NoMemory)?;
    // SAFETY: the open lessor pins its DRM private state and minor.
    let minor = unsafe { (*(*lessor.raw()).private_data.cast::<DrmFile>()).minor };
    Arc::get_mut(&mut file)
        .ok_or(AxError::BadState)?
        .open(minor)?;
    // SAFETY: open succeeded and the Arc keeps this private state live until
    // its ownership is transferred to the prepared file description below.
    let private = unsafe { (*file.raw()).private_data.cast::<DrmFile>() };
    let description = FileDescription::new_with_flags(file, status_flags)?;
    let publication = reservation.prepare_publication(description)?;
    // SAFETY: private is a fresh, unshared DRM file. No fallible operations
    // remain, so this is the unique transfer of the core's lessee reference.
    unsafe { setup(private, ctx) };
    Ok(publication.commit())
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
                let was_master = !self.render
                    && tk_drm_upstream::drm_auth_upstream::drm_is_current_master(private);
                tk_drm_upstream::drm_file_upstream::drm_release(ptr::null_mut(), raw);
                if was_master {
                    // A KMS client may take master without switching the VT to
                    // KD_GRAPHICS; its final close hands the screen back.
                    crate::pseudofs::dev::restore_console_after_master_close();
                }
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
        // A zero-length read completes without waiting for a DRM event.
        // Keep this outside the retry loop, which treats an empty queue as
        // WouldBlock rather than end-of-file.
        if dst.remaining_mut() == 0 {
            return Ok(0);
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
        if dst.remaining_mut() == 0 {
            return Ok(0);
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
        self.dispatch_ioctl(cmd, arg)
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
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        // Device removal reports ERROR/HANGUP independently of the requested
        // mask, including write-only and events=0 poll registrations.
        PollRegistration::single_owned(self.waiters.clone(), context.waker())
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
    if unsafe { (*dev).unplugged.load(Ordering::Acquire) || (*dev).vma_offset_manager.is_null() } {
        return Err(AxError::NoSuchDevice);
    }
    // i915 mmap offsets name `i915_mmap_offset` nodes, not shmem objects.
    // SAFETY: `dev` is the live device checked above; `driver` is compared by address only.
    if unsafe { (*dev).driver.cast_const() }
        == core::ptr::addr_of!(intel_gt::i915_driver_upstream::I915_DRM_DRIVER.0)
    {
        // SAFETY: `file.raw()` is this file's open `LinuxFile`; `private_data` is its `drm_file`.
        let private = unsafe { (*file.raw()).private_data.cast::<DrmFile>() };
        return crate::drm::intel::upstream_mmap::prepare_i915_gem_mmap(
            dev,
            private,
            file.allows_write(),
            request,
        );
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
    // SAFETY: `node` points to the `drm_vma_offset_node` embedded in `object`, kept alive by `_object_ref`.
    // Reading `vm_node.start` retrieves the allocated offset page address.
    let start_page = unsafe { (*node).vm_node.start };
    let region_offset = start_page
        .checked_mul(PAGE_SIZE as u64)
        .ok_or(AxError::InvalidInput)?;
    prepare_gem_object_mmap(
        &file.device,
        &_object_ref,
        file.allows_write(),
        request,
        region_offset,
    )
}

/// A PRIME fd is itself the authority to map its exported GEM backing, even
/// after the original DRM handle has been closed. Unknown anon-inode/exporter
/// types are never reinterpreted as a GEM object.
///
/// # Safety
/// `file` must be a live LinuxKPI file held by the caller during this call.
pub(crate) unsafe fn prepare_dma_buf_mmap(
    file: *mut tk_linux_kapi::shmem::File,
    request: FileMmapRequest,
) -> AxResult<Option<PreparedFileMmap>> {
    // SAFETY: AnonFile holds a live reference during this synchronous call;
    // the accessor checks the exact DMA-BUF file-operations discriminator.
    let Some(dmabuf) =
        (unsafe { tk_drm_upstream::drm_dma_buf_upstream::drm_dma_buf_from_file(file) })
    else {
        return Ok(None);
    };
    // SAFETY: the fd reference pins this DMA-BUF. Only the GEM export ops
    // table guarantees that its private pointer denotes a DrmGemObject.
    if unsafe { (*dmabuf).ops }
        != ptr::addr_of!(tk_drm_upstream::drm_prime_upstream::drm_gem_prime_dmabuf_ops)
    {
        return Err(AxError::OperationNotSupported);
    }
    // SAFETY: the exporter identity above establishes the private-data type.
    let object = unsafe { (*dmabuf).r#priv.cast::<DrmGemObject>() };
    if object.is_null() {
        return Err(AxError::InvalidInput);
    }
    // SAFETY: the GEM exporter retains this object until the final fd put.
    let dev = unsafe { (*object).dev };
    let device = crate::drm::upstream::device_by_raw(dev).ok_or(AxError::NoSuchDevice)?;
    // SAFETY: device pins the registered DRM core and transport.
    if device.is_unplugged() || unsafe { (*dev).unplugged.load(Ordering::Acquire) } {
        return Err(AxError::NoSuchDevice);
    }
    // SAFETY: take an independent lookup reference while the fd pins object.
    unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_get(object) };
    let object_ref = GemObjectLookupRef(object);
    // SAFETY: the caller's file reference keeps the immutable access mode live.
    let writable = unsafe { (*file).f_flags & O_ACCMODE != O_RDONLY };
    prepare_gem_object_mmap(&device, &object_ref, writable, request, 0).map(Some)
}

/// Map an already authorized, referenced GEM object. DRM handles authorize by
/// VMA-node ownership; PRIME descriptors authorize through their exported file.
fn prepare_gem_object_mmap(
    device: &Arc<UpstreamDrmDevice>,
    object_ref: &GemObjectLookupRef,
    writable: bool,
    request: FileMmapRequest,
    region_offset: u64,
) -> AxResult<PreparedFileMmap> {
    let object = object_ref.0;
    if request.sharing() != FileMmapSharing::Shared
        || request.page_size() != PAGE_SIZE
        || request.protection().contains(FileMmapProtection::EXECUTE)
    {
        return Err(AxError::OperationNotSupported);
    }
    // SAFETY: object_ref retains the live GEM object throughout preparation.
    if unsafe { (*object).size == 0 || !(*object).size.is_multiple_of(PAGE_SIZE) } {
        return Err(AxError::InvalidInput);
    }
    let relative_offset = request
        .offset()
        .checked_sub(region_offset)
        .ok_or(AxError::InvalidInput)?;
    if relative_offset
        .checked_add(request.length() as u64)
        .is_none_or(|end| {
            // SAFETY: `object` is kept alive by `object_ref`; reading `size` is safe and immutable.
            end > unsafe { (*object).size as u64 }
        })
    {
        return Err(AxError::InvalidInput);
    }
    // SAFETY: `object` is valid and kept alive by `object_ref`. Reading `filp` accesses its
    // underlying shmem backing file.
    let object_file = unsafe { (*object).filp.cast::<LinuxFile>() };
    if object_file.is_null()
        // SAFETY: `object_file` was checked non-null; reading its `f_mapping` pointer verifies that
        // address space operations and page cache mappings are initialized.
        || unsafe { (*object_file).f_mapping.is_null() }
    {
        return Err(AxError::InvalidInput);
    }
    if request.protection().contains(FileMmapProtection::WRITE) && !writable {
        return Err(AxError::PermissionDenied);
    }

    // SAFETY: `object` is valid and kept alive by `object_ref`; `size` is immutable.
    let count = unsafe { (*object).size / PAGE_SIZE };
    if let Some(mapping) = device.map_blob_gem_object(object).map_err(blob_map_error)? {
        return upstream_external_blob_mmap(device, request, region_offset, object, count, mapping);
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
    if writable {
        vm_flags |= VM_MAYWRITE;
    }
    if request.protection().contains(FileMmapProtection::WRITE) {
        if !writable {
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
    // SAFETY: `object` is alive via `object_ref`; reading `filp` assigns the VMA backing file pointer.
    vma.vm_file = unsafe { (*object).filp.cast() };
    if vma.vm_file.is_null()
        // SAFETY: `vma.vm_file` was checked non-null; verifying `f_mapping` ensures valid address space operations.
        || unsafe { (*vma.vm_file).f_mapping.is_null() }
    {
        return Err(AxError::InvalidInput);
    }
    // SAFETY: `object` is valid and kept alive by `object_ref`. `vma` is an initialized `VmAreaStruct`
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
        _device: device.clone(),
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
    device: &Arc<UpstreamDrmDevice>,
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
        device.mapping_owner(),
    ))
    .map_err(|_| AxError::NoMemory)?;
    let shared = SharedPages::new_external_4k(
        phys_pages,
        lease.clone(),
        axhal::paging::MappingFlags::DEVICE | cache_flags,
    )?;
    let pages = Arc::try_new(shared).map_err(|_| AxError::NoMemory)?;
    device.track_external_pages(&lease, &pages)?;

    // The C mmap_obj callback is intentionally not invoked for BAR mappings:
    // tk-virtgpu's generic shmem callback would construct RAM pages instead
    // of the validated MAP_BLOB PFNs. The fixed SharedPages backend publishes
    // those PFNs directly and owns their external lease across VMA splits.
    // SAFETY: `object` points to a valid `DrmGemObject` passed from `upstream_gem_mmap`.
    // We increment its refcount before transferring ownership to `ExternalGemMmapOwner`.
    unsafe { tk_drm_upstream::drm_gem_upstream::drm_gem_object_get(object) };
    let owner = Arc::try_new(ExternalGemMmapOwner {
        _device: device.clone(),
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
    use alloc::{boxed::Box, sync::Arc, task::Wake};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };

    use axfs_ng_vfs::NodeOps;
    use tk_drm_upstream::{
        drm_auth_upstream::{drm_is_current_master, drm_master_open},
        drm_drv_types::DrmDriver,
        drm_drv_upstream::{
            drm_dev_alloc, drm_dev_get, drm_dev_put, drm_dev_register, drm_dev_unregister,
        },
        drm_file_upstream::drm_file_alloc,
        uapi_drm_consts::DRM_IOCTL_DROP_MASTER,
    };
    use tk_linux_kapi::{list::INIT_LIST_HEAD, memory::atomic_inc, wait::add_wait_queue};

    use super::*;
    use crate::{
        drm::upstream::test_unregistered_device,
        pseudofs::{
            dev::{
                console_fb::{self, DisplayCore, tests::MemorySurface},
                scanout::ScanoutSurface,
                tty::set_active_graphics_for_test,
            },
            device_registry::{DeviceRegistry, MAX_DEVICES},
        },
    };

    struct WakeCounter(AtomicUsize);
    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct ConsoleGuard(Option<Arc<DisplayCore>>);

    impl ConsoleGuard {
        fn install(display: Arc<DisplayCore>) -> Self {
            Self(console_fb::replace_console_for_test(Some(display)))
        }
    }

    impl Drop for ConsoleGuard {
        fn drop(&mut self) {
            let previous = self.0.take();
            let _ = console_fb::replace_console_for_test(previous);
        }
    }

    struct VtModeGuard(bool);

    impl VtModeGuard {
        fn set_graphics(graphics: bool) -> Self {
            Self(set_active_graphics_for_test(graphics))
        }
    }

    impl Drop for VtModeGuard {
        fn drop(&mut self) {
            set_active_graphics_for_test(self.0);
        }
    }

    fn test_device() -> (
        Arc<UpstreamDrmDevice>,
        *mut tk_drm_upstream::drm_device_types::DrmDevice,
        Box<DrmDriver>,
    ) {
        let driver = Box::new(unsafe { core::mem::zeroed::<DrmDriver>() });
        let raw = unsafe { drm_dev_alloc(&*driver, ptr::null_mut()) };
        assert!(!raw.is_null());
        let owner: Arc<dyn core::any::Any + Send + Sync> = Arc::new(());
        let device = test_unregistered_device(raw, owner).expect("test DRM device wrapper");
        (device, raw, driver)
    }

    fn q35_virtio_gpu_pci_identity() -> axdriver_display::DisplayPciIdentity {
        axdriver_display::DisplayPciIdentity {
            bus: 0,
            device: 1,
            function: 0,
            vendor_id: 0x1af4,
            device_id: 0x1050,
            subsystem_vendor: 0x1af4,
            subsystem_device: 0x1100,
            revision: 0,
        }
    }

    fn minor_sysfs_identity(device: &UpstreamDrmDevice, render: bool) -> VfsResult<DeviceIdentity> {
        let index = if render {
            device.render_index().expect("render minor registered")
        } else {
            device.primary_index().expect("primary minor registered")
        };
        let name = if render {
            format!("renderD{index}")
        } else {
            format!("card{index}")
        };
        attach_drm_minor(
            DeviceIdentity::new(
                device.label().into(),
                "drm".into(),
                name.clone(),
                DeviceId::new(226, index),
            )?
            .with_devname(format!("dri/{name}"))?,
            &Some(format!("platform/{}", device.label())),
        )
    }

    #[test]
    fn dri_directory_publishes_initial_sysfs_before_first_lookup() {
        use crate::pseudofs::SimpleDirOps;

        let _context = crate::test_support::scheduler_test_context();
        // SAFETY: the driver table consists of nullable callbacks, pointers,
        // and scalar fields; the core accepts this empty fixture driver.
        let driver = Box::new(unsafe { core::mem::zeroed::<DrmDriver>() });
        // SAFETY: `driver` remains live until after the core device is released.
        let raw = unsafe { drm_dev_alloc(&*driver, ptr::null_mut()) };
        assert!(!raw.is_null());
        // SAFETY: the zero-feature fixture has initialized DRM core state and
        // the driver table remains live for the registered device lifetime.
        assert_eq!(unsafe { drm_dev_register(raw, 0) }, 0);
        let owner: Arc<dyn core::any::Any + Send + Sync> = Arc::new(());
        let device =
            crate::drm::upstream::install(raw, owner, (0, 0), None, "dri-eager-test", None)
                .expect("install test DRM device");

        let mut inner_fs = None;
        let _filesystem = crate::pseudofs::SimpleFs::new_with("dri-eager-test".into(), 0, |fs| {
            inner_fs = Some(fs.clone());
            crate::pseudofs::SimpleDir::new_maker(fs, Arc::new(crate::pseudofs::DirMapping::new()))
        });
        let fs = inner_fs.expect("test filesystem initialized");
        let card_index = device.primary_index().expect("primary minor registered");
        let render_index = device.render_index().expect("render minor registered");
        let card_name = format!("card{card_index}");
        let render_name = format!("renderD{render_index}");
        let registry = global_device_registry();

        {
            let directory = DriDirectory::new(fs);
            // Construction happens while devfs is built, before `/sys` is
            // mounted. Both the udev-facing identities and their /dev names
            // must already be registered at this point.
            assert!(registry.is_published_identity(
                &minor_sysfs_identity(&device, false).expect("primary sysfs identity")
            ));
            assert!(registry.is_published_identity(
                &minor_sysfs_identity(&device, true).expect("render sysfs identity")
            ));
            assert_eq!(directory.nodes.lock().len(), 2);

            // Looking up the primary again must use the cached node: calling
            // `primary_node()` here would collide with the just-published DRM
            // sysfs identity. The character-device metadata must stay intact.
            for (name, expected_mode, expected_id) in [
                (
                    card_name.as_str(),
                    DRM_PRIMARY_NODE_MODE,
                    DeviceId::new(226, card_index),
                ),
                (
                    render_name.as_str(),
                    DRM_RENDER_NODE_MODE,
                    DeviceId::new(226, render_index),
                ),
            ] {
                let node = directory
                    .lookup_child(axfs_ng_vfs::FsName::new(name.as_bytes()))
                    .expect("eagerly cached DRM node");
                let crate::pseudofs::NodeOpsMux::File(node) = node else {
                    panic!("DRM devnode is not a file node");
                };
                let metadata = node.metadata().expect("DRM node metadata");
                assert_eq!(metadata.rdev, expected_id);
                assert_eq!(metadata.mode.bits() & 0o777, expected_mode);
            }
        }

        crate::drm::upstream::unplug(raw).expect("unplug test DRM device");
        drop(device);
        // SAFETY: the directory and installed wrapper have been dropped, the
        // transport is unplugged, and the fixture's driver is still alive.
        unsafe {
            drm_dev_unregister(raw);
            drm_dev_put(raw);
        }
    }

    #[test]
    fn pci_sysfs_inventory_parent_is_reused_but_virtio_child_conflicts_fail() {
        let _context = crate::test_support::scheduler_test_context();
        let registry = DeviceRegistry::<MAX_DEVICES>::new();
        let identity = q35_virtio_gpu_pci_identity();

        // This mirrors pci_sysfs::publish_inventory(): its boot-owned handle
        // keeps the exact BDF parent published before DRM's lazy devfs lookup.
        let pci = pci_registrations(identity).unwrap().remove(0);
        let pci_identity = pci.identity().clone();
        let _inventory_handle = registry
            .reserve(pci_identity.clone())
            .unwrap()
            .publish(pci)
            .unwrap();

        let mut drm_handles = Vec::new();
        let path = publish_pci_parent_in(&registry, identity, &mut drm_handles).unwrap();
        assert_eq!(path, "pci0000:00/0000:00:01.0/virtio8");
        assert!(registry.is_published_identity(&pci_identity));
        assert_eq!(drm_handles.len(), 1);

        let virtio_identity = pci_registrations(identity)
            .unwrap()
            .remove(1)
            .identity()
            .clone();
        assert!(registry.is_published_identity(&virtio_identity));

        assert!(matches!(
            publish_pci_parent_in(&registry, identity, &mut drm_handles),
            Err(VfsError::AlreadyExists)
        ));
    }

    #[test]
    fn pci_drm_parent_publishes_new_pci_and_virtio_nodes_together() {
        let _context = crate::test_support::scheduler_test_context();
        let registry = DeviceRegistry::<MAX_DEVICES>::new();
        let identity = q35_virtio_gpu_pci_identity();
        let mut drm_handles = Vec::new();

        assert_eq!(
            publish_pci_parent_in(&registry, identity, &mut drm_handles).unwrap(),
            "pci0000:00/0000:00:01.0/virtio8"
        );
        assert_eq!(drm_handles.len(), 2);
        for registration in pci_registrations(identity).unwrap() {
            assert!(registry.is_published_identity(registration.identity()));
        }
    }

    fn test_open_file(device: Arc<UpstreamDrmDevice>, render: bool) -> Box<UpstreamDrmFile> {
        let dev = device.raw();
        let minor = unsafe {
            if render {
                (*dev).render
            } else {
                (*dev).primary
            }
        };
        assert!(!minor.is_null());
        let file = Box::new(UpstreamDrmFile::new(device, render, O_RDWR));
        unsafe {
            // Match the references/accounting normally acquired by
            // drm_minor_acquire() before the helper allocates file-private state.
            drm_dev_get(dev);
            atomic_inc(&mut (*dev).open_count);
            let private = drm_file_alloc(minor);
            assert!(!private.is_null());
            (*file.raw()).private_data = private.cast();
            (*private).filp = file.raw();
            if !render {
                assert_eq!(drm_master_open(private), 0);
            }
            // Match open_helper's publication only after all open callbacks
            // (including master creation) have succeeded.
            tk_linux_kapi::mutex::mutex_lock(ptr::addr_of_mut!((*dev).filelist_mutex));
            tk_linux_kapi::list::list_add_tail(
                ptr::addr_of_mut!((*private).lhead),
                ptr::addr_of_mut!((*dev).filelist),
            );
            tk_linux_kapi::mutex::mutex_unlock(ptr::addr_of_mut!((*dev).filelist_mutex));
            let entry = file.wait_entry.get();
            (*entry).private = Arc::as_ptr(&file.waiters).cast_mut().cast();
            (*entry).func = Some(wake_poll_waiters);
            INIT_LIST_HEAD(ptr::addr_of_mut!((*entry).entry));
            add_wait_queue(&mut *ptr::addr_of_mut!((*private).event_wait), &mut *entry);
        }
        file
    }

    fn console_fixture() -> (Arc<MemorySurface>, ConsoleGuard, VtModeGuard) {
        let mode = VtModeGuard::set_graphics(false);
        let surface = MemorySurface::new(64, 48);
        let core = Arc::new(DisplayCore::new(surface.clone()));
        let console = ConsoleGuard::install(core);
        (surface, console, mode)
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
    fn dma_buf_mmap_does_not_interpret_unrelated_anon_file_private_data() {
        // SAFETY: an all-zero LinuxKPI file is a valid unpublished record;
        // the mmap discriminator only reads its absent operations pointer.
        let mut file: tk_linux_kapi::shmem::File = unsafe { core::mem::zeroed() };
        file._private_data = core::ptr::NonNull::<u8>::dangling().as_ptr().cast();
        let request = FileMmapRequest::try_new(
            0,
            PAGE_SIZE,
            PAGE_SIZE,
            FileMmapProtection::READ,
            FileMmapSharing::Shared,
        )
        .unwrap();
        // SAFETY: this stack file remains live for the synchronous lookup;
        // its unrelated private pointer must never be dereferenced.
        assert!(
            unsafe { prepare_dma_buf_mmap(&mut file, request) }
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn empty_drm_reads_do_not_wait_for_events() {
        let _context = crate::test_support::scheduler_test_context();
        let (device, raw, _driver) = test_device();
        for flags in [O_RDWR, O_RDWR | O_NONBLOCK, O_WRONLY] {
            // No DRM file-private state: an empty read must not consult the
            // event queue, even when the descriptor is blocking.
            let file = UpstreamDrmFile::new(device.clone(), false, flags);
            let expected = if flags == O_WRONLY {
                Err(AxError::BadFileDescriptor)
            } else {
                Ok(0)
            };
            let mut bytes = [];
            let mut dst = &mut bytes[..];
            assert_eq!(file.read(&mut dst), expected);
            for nowait in [false, true] {
                let status = OfdIoStatus::new(flags).with_rwf_nowait(nowait);
                assert_eq!(file.read_with_operation_status(status, &mut dst), expected);
            }
        }
        drop(device);
        // SAFETY: all wrappers are dropped and this is the fixture's sole
        // remaining device reference; the driver table is still live.
        unsafe { drm_dev_put(raw) };
    }

    #[test]
    fn lease_file_is_independent_and_setup_precedes_infallible_fd_publication() {
        use tk_drm_upstream::drm_drv_upstream::{drm_dev_register, drm_dev_unregister};

        use crate::file::{FdTable, reserve_fd_in};

        struct Setup {
            table: Arc<FdTable>,
            fd: c_int,
            private: *mut DrmFile,
            calls: usize,
        }
        unsafe extern "C" fn setup(private: *mut DrmFile, ctx: *mut c_void) {
            // SAFETY: this test passes its live Setup record synchronously.
            let state = unsafe { &mut *ctx.cast::<Setup>() };
            assert!(state.table.get_description(state.fd).is_err());
            state.private = private;
            state.calls += 1;
        }

        let _context = crate::test_support::scheduler_test_context();
        // Reserve an earlier primary minor, so the lessee must use the
        // lessor's exact minor rather than the legacy card0 inode shim.
        let (earlier, earlier_raw, _earlier_driver) = test_device();
        let (device, raw, _driver) = test_device();
        // SAFETY: both device and driver table remain live throughout the test.
        assert_eq!(unsafe { drm_dev_register(raw, 0) }, 0);
        let lessor = test_open_file(device.clone(), false);
        let table = Arc::new(FdTable::new().unwrap());
        let reservation = reserve_fd_in(table.clone(), 4, true).unwrap();
        let fd = reservation.fd();
        let mut state = Setup {
            table: table.clone(),
            fd,
            private: ptr::null_mut(),
            calls: 0,
        };
        // SAFETY: setup and its context remain valid for the synchronous call;
        // the lessor is a live DRM file and the reservation is uniquely owned.
        assert_eq!(
            unsafe {
                publish_lessee(
                    &lessor,
                    O_NONBLOCK | O_CLOEXEC,
                    reservation,
                    setup,
                    (&mut state as *mut Setup).cast(),
                )
            },
            Ok(fd)
        );
        assert_eq!(state.calls, 1);
        let description = table.get_description(fd).unwrap();
        assert!(table.get_cloexec(fd).unwrap());
        assert_eq!(
            description.status_flags() & (O_ACCMODE | O_NONBLOCK),
            O_RDWR | O_NONBLOCK
        );
        let lessee = description
            .file_handle()
            .downcast::<UpstreamDrmFile>()
            .unwrap();
        // SAFETY: both open file descriptions retain their private DRM state.
        unsafe {
            assert_ne!((*lessor.raw()).private_data, (*lessee.raw()).private_data);
            assert_eq!(state.private.cast::<c_void>(), (*lessee.raw()).private_data);
            assert_eq!(
                (*state.private).minor,
                (*(*lessor.raw()).private_data.cast::<DrmFile>()).minor
            );
        }
        drop(lessee);
        drop(description);
        drop(table.close(fd).unwrap());

        // Failed opens must release their reservation without running setup.
        // SAFETY: the live lessor and fixture still pin the device.
        unsafe { tk_drm_upstream::drm_drv_upstream::drm_dev_unplug(raw) };
        let reservation = reserve_fd_in(table.clone(), 4, false).unwrap();
        // SAFETY: same valid callback/context as above; unplug forces failure
        // before the setup callback can transfer any ownership.
        assert_eq!(
            unsafe {
                publish_lessee(
                    &lessor,
                    0,
                    reservation,
                    setup,
                    (&mut state as *mut Setup).cast(),
                )
            },
            Err(LinuxError::ENODEV.into())
        );
        assert_eq!(state.calls, 1);
        assert!(table.get_description(fd).is_err());
        drop(lessor);
        drop(device);
        // SAFETY: all descriptors are closed and the fixture owns the final
        // registered-device reference; its driver remains live.
        unsafe {
            drm_dev_unregister(raw);
            drm_dev_put(raw);
        }
        drop(earlier);
        // SAFETY: the unused fixture's owner is gone; release its base reference.
        unsafe { drm_dev_put(earlier_raw) };
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

    #[test]
    fn unplug_wakes_drm_pollers_even_without_read_interest() {
        let _context = crate::test_support::scheduler_test_context();
        for events in [IoEvents::empty(), IoEvents::WRITABLE, IoEvents::READABLE] {
            let (device, raw, _driver) = test_device();
            let file = test_open_file(device.clone(), false);
            let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
            let waker = Waker::from(counter.clone());
            let mut context = Context::from_waker(&waker);
            let registration = file.register(&mut context, events).unwrap();
            assert!(!file.poll().intersects(IoEvents::ALWAYS));
            // SAFETY: the fixture retains the registered file and device for
            // the whole unplug/wakeup operation.
            unsafe { tk_drm_upstream::drm_drv_upstream::drm_dev_unplug(raw) };
            assert!(counter.0.load(Ordering::Relaxed) > 0);
            assert!(file.poll().contains(IoEvents::ERROR | IoEvents::HANGUP));
            drop(registration);
            drop(file);
            drop(device);
            // SAFETY: all open files/wrappers are gone and the driver remains
            // live while the fixture's final reference is released.
            unsafe { drm_dev_put(raw) };
        }
    }

    #[test]
    fn closing_primary_master_restores_console_in_text_mode() {
        let _context = crate::test_support::scheduler_test_context();
        let (surface, _console, _mode) = console_fixture();
        surface.set_master(false).unwrap();

        let (device, raw, _driver) = test_device();
        let file = test_open_file(device.clone(), false);
        let private = unsafe { (*file.raw()).private_data.cast::<DrmFile>() };
        assert!(unsafe { drm_is_current_master(private) });

        drop(file);
        assert!(surface.is_master());
        assert_eq!(surface.restore_text_calls(), 1);
        drop(device);
        unsafe { drm_dev_put(raw) };
    }

    #[test]
    fn drop_master_ioctl_restores_console_and_close_does_not_repeat_it() {
        let _context = crate::test_support::scheduler_test_context();
        let (surface, _console, _mode) = console_fixture();
        surface.set_master(false).unwrap();

        let (device, raw, _driver) = test_device();
        let file = test_open_file(device.clone(), false);
        let private = unsafe { (*file.raw()).private_data.cast::<DrmFile>() };
        assert!(unsafe { drm_is_current_master(private) });

        assert_eq!(file.dispatch_ioctl(DRM_IOCTL_DROP_MASTER, 0), Ok(0));
        assert!(!unsafe { drm_is_current_master(private) });
        assert!(surface.is_master());
        assert_eq!(surface.restore_text_calls(), 1);

        drop(file);
        assert_eq!(surface.restore_text_calls(), 1);
        drop(device);
        unsafe { drm_dev_put(raw) };
    }

    #[test]
    fn nonmaster_render_and_graphics_vt_never_reclaim_scanout() {
        let _context = crate::test_support::scheduler_test_context();
        let (surface, _console, _mode) = console_fixture();
        surface.set_master(false).unwrap();
        let (device, raw, _driver) = test_device();

        let master = test_open_file(device.clone(), false);
        let nonmaster = test_open_file(device.clone(), false);
        let render = test_open_file(device.clone(), true);
        let nonmaster_private = unsafe { (*nonmaster.raw()).private_data.cast::<DrmFile>() };
        assert!(!unsafe { drm_is_current_master(nonmaster_private) });
        drop(nonmaster);
        assert!(!surface.is_master());
        assert_eq!(surface.restore_text_calls(), 0);
        drop(render);
        assert!(!surface.is_master());
        assert_eq!(surface.restore_text_calls(), 0);

        // The screen belongs to a graphics VT now. Neither a final primary
        // close nor an explicit DROP_MASTER may steal it back for fbcon.
        let _graphics = VtModeGuard::set_graphics(true);
        drop(master);
        assert!(!surface.is_master());
        assert_eq!(surface.restore_text_calls(), 0);

        let master = test_open_file(device.clone(), false);
        assert_eq!(master.dispatch_ioctl(DRM_IOCTL_DROP_MASTER, 0), Ok(0));
        assert!(!surface.is_master());
        assert_eq!(surface.restore_text_calls(), 0);
        drop(master);
        assert!(!surface.is_master());
        assert_eq!(surface.restore_text_calls(), 0);

        drop(device);
        unsafe { drm_dev_put(raw) };
    }
}
