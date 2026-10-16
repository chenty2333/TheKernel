//! DRM primary-node devfs adapter.
//!
//! The device node is shared, but [`DrmPrimary::open_description`] creates a
//! fresh [`DrmFileAdapter`] for every open file description.  All mutable DRM
//! state consequently remains scoped to the OFD rather than `/dev/dri/card0`.

use alloc::{borrow::Cow, sync::Arc, vec, vec::Vec};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{DeviceId, Location, NodeFlags, VfsResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use linux_raw_sys::general::{O_ACCMODE, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, S_IFCHR};

use super::upstream_dri::{
    DRM_PRIMARY_NODE_MODE, DRM_RENDER_NODE_MODE, attach_drm_minor, attribute, mode_attribute,
    pci_registrations, platform_registration, publish_pci_parent,
};

pub(crate) const DRM_PRIMARY_DEVICE_ID: DeviceId = DeviceId::new(226, 0);
pub(crate) const DRM_RENDER_DEVICE_ID: DeviceId = DeviceId::new(226, 128);
use crate::{
    drm::{DrmDevice, DrmFile},
    file::{
        FileLike, FileMmapRequest, IoDst, IoSrc, IoctlContext, Kstat, OfdIoStatus, PreparedFileMmap,
    },
    pseudofs::{
        DeviceOpen, DeviceOps,
        device_registry::{
            DeviceAttribute, DeviceHandle, DeviceIdentity, DeviceRegistration, DeviceReservation,
            MAX_DEVICES, global_device_registry,
        },
    },
    readiness::block_on_poll_io,
};

/// Shared primary-node factory.  It owns no file-open state.
pub(crate) struct DrmPrimary {
    device: Arc<DrmDevice>,
    _registry_handles: Vec<DeviceHandle<'static, MAX_DEVICES>>,
}

impl DrmPrimary {
    pub(crate) fn new(device: Arc<DrmDevice>) -> Self {
        Self {
            _registry_handles: publish_sysfs(&device).unwrap_or_else(|error| {
                error!("DRM sysfs publication failed: {error}");
                Vec::new()
            }),
            device,
        }
    }
}

fn publish_sysfs(device: &DrmDevice) -> VfsResult<Vec<DeviceHandle<'static, MAX_DEVICES>>> {
    let mut parents = Vec::new();
    let transport = if let Some(identity) = device.adapter.pci_identity() {
        Some(publish_pci_parent(identity, &mut parents)?)
    } else if let Some(name) = device.adapter.platform_name() {
        let platform = platform_registration(name)?;
        let reservation = global_device_registry().reserve(platform.identity().clone())?;
        parents.push(reservation.publish(platform)?);
        Some(alloc::format!("platform/{name}"))
    } else {
        None
    };
    // GT-only operation keeps simpledrm KMS, but ANV/Iris must discover the
    // render engine as its actual PCI function, not a platform framebuffer.
    let render_transport = if device.has_intel_gt() && device.adapter.pci_identity().is_none() {
        let identity =
            crate::drm::intel::gt_pci_identity().ok_or(axfs_ng_vfs::VfsError::NotFound)?;
        Some(publish_pci_parent(identity, &mut parents)?)
    } else {
        transport.clone()
    };
    let attach = |identity| attach_drm_minor(identity, &transport);
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
    let connector_parent = transport
        .as_ref()
        .map_or_else(|| "virtio0".into(), |path| alloc::format!("{path}/drm"));
    let connector = DeviceRegistration::try_new(
        DeviceIdentity::without_dev("virtio0".into(), "drm".into(), "card0-Virtual-1".into())?
            .child_of_path(connector_parent, "card0".into())?,
        "drm_connector".into(),
        vec![
            attribute("status", "connected\n"),
            mode_attribute(device.preferred_mode().width, device.preferred_mode().height),
        ],
        None,
    )?;
    let render = if device.has_render() {
        Some(DeviceRegistration::try_new(
            attach_drm_minor(
                DeviceIdentity::new(
                    "virtio0".into(),
                    "drm".into(),
                    "renderD128".into(),
                    DRM_RENDER_DEVICE_ID,
                )?
                .with_devname("dri/renderD128".into())?,
                &render_transport,
            )?,
            "drm_minor".into(),
            Vec::new(),
            None,
        )?)
    } else {
        None
    };
    let card_reservation = global_device_registry().reserve(card.identity().clone())?;
    let connector_reservation = global_device_registry().reserve(connector.identity().clone())?;
    let Some(render) = render else {
        let (card_handle, connector_handle) = DeviceReservation::publish_pair(
            card_reservation,
            card,
            connector_reservation,
            connector,
        )?;
        parents.extend([card_handle, connector_handle]);
        return Ok(parents);
    };
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

pub(crate) fn primary_node(
    fs: Arc<crate::pseudofs::SimpleFs>,
    device: Arc<DrmDevice>,
) -> Arc<crate::pseudofs::Device> {
    crate::pseudofs::Device::new_with_permissions(
        fs,
        axfs_ng_vfs::NodeType::CharacterDevice,
        DRM_PRIMARY_DEVICE_ID,
        axfs_ng_vfs::NodePermission::from_bits_truncate(DRM_PRIMARY_NODE_MODE),
        Arc::new(DrmPrimary::new(device)),
    )
}

/// `/dev/dri/renderD128` exists only for a GPU which negotiated legacy VIRGL.
pub(crate) fn render_node(
    fs: Arc<crate::pseudofs::SimpleFs>,
    device: Arc<DrmDevice>,
) -> Option<Arc<crate::pseudofs::Device>> {
    device.has_render().then(|| {
        crate::pseudofs::Device::new_with_permissions(
            fs,
            axfs_ng_vfs::NodeType::CharacterDevice,
            DRM_RENDER_DEVICE_ID,
            axfs_ng_vfs::NodePermission::from_bits_truncate(DRM_RENDER_NODE_MODE),
            Arc::new(DrmRender { device }),
        )
    })
}

struct DrmRender {
    device: Arc<DrmDevice>,
}

/// Per-open-file-description VFS transport for DRM.
struct DrmFileAdapter {
    file: DrmFile,
    nonblocking: AtomicBool,
    render: bool,
    access_mode: u32,
}

impl DrmFileAdapter {
    fn new(file: DrmFile, render: bool, flags: u32) -> Self {
        file.set_open_flags(flags);
        Self {
            file,
            nonblocking: AtomicBool::new(flags & O_NONBLOCK != 0),
            render,
            access_mode: flags & O_ACCMODE,
        }
    }

    fn allows_read(&self) -> bool {
        matches!(self.access_mode, O_RDONLY | O_RDWR)
    }

    fn allows_write(&self) -> bool {
        matches!(self.access_mode, O_WRONLY | O_RDWR)
    }
}

impl DeviceOps for DrmPrimary {
    fn open_description(&self, location: &Location, flags: u32) -> VfsResult<Option<DeviceOpen>> {
        crate::pseudofs::dev::tty::remember_primary_node(location)?;
        let file: Arc<dyn FileLike> = Arc::try_new(DrmFileAdapter::new(
            self.device.open_primary(),
            false,
            flags,
        ))
        .map_err(|_| AxError::NoMemory)?;
        Ok(Some(DeviceOpen::new(file, None)))
    }

    // Ordinary VFS-backed files must never reach these node-global methods:
    // `open_description` above always replaces the file with DrmFileAdapter.
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM | NodeFlags::NO_SEEK
    }
}

impl DeviceOps for DrmRender {
    fn open_description(&self, _: &Location, flags: u32) -> VfsResult<Option<DeviceOpen>> {
        let file: Arc<dyn FileLike> = Arc::try_new(DrmFileAdapter::new(
            self.device.open_render().map_err(AxError::from)?,
            true,
            flags,
        ))
        .map_err(|_| AxError::NoMemory)?;
        Ok(Some(DeviceOpen::new(file, None)))
    }
    fn read_at(&self, _: &mut [u8], _: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM | NodeFlags::NO_SEEK
    }
}

impl FileLike for DrmFileAdapter {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        if !self.allows_read() {
            return Err(AxError::BadFileDescriptor);
        }
        block_on_poll_io(self, IoEvents::READABLE, self.nonblocking(), || {
            let result = self.file.read_events(dst);
            match result {
                Ok(0) => Err(AxError::WouldBlock),
                other => other,
            }
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
            || match self.file.read_events(dst) {
                Ok(0) => Err(AxError::WouldBlock),
                other => other,
            },
        )
    }

    fn write_with_operation_status(
        &self,
        _status: OfdIoStatus,
        src: &mut IoSrc,
    ) -> AxResult<usize> {
        self.write(src)
    }

    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        if !self.allows_write() {
            return Err(AxError::BadFileDescriptor);
        }
        Err(AxError::InvalidInput)
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
            blksize: 4096,
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

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        self.file.ioctl(context, cmd, arg)
    }

    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        self.file.prepare_mmap(request)
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        self.nonblocking.store(nonblocking, Ordering::Release);
        self.file.set_nonblocking_flag(nonblocking);
        Ok(())
    }
}

impl Pollable for DrmFileAdapter {
    fn poll(&self) -> IoEvents {
        self.file.poll_events()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.file.register_events(context, events)
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axfs_ng_vfs::NodeOps;

    use super::*;
    use crate::{
        drm::{DisplayAdapter, DrmResult, DumbRequest, GemBacking, Scanout},
        pseudofs::{DirMapping, SimpleDir, SimpleFs},
    };

    struct Backing;
    impl GemBacking for Backing {
        fn shared_pages(&self) -> DrmResult<Arc<crate::file::SharedPages>> {
            panic!("this registration test never creates a GEM object")
        }
    }

    struct Adapter;
    impl DisplayAdapter for Adapter {
        fn create_dumb(
            &self,
            _: DumbRequest,
            _: u32,
            _: u64,
            _allocation_owner: Arc<dyn Send + Sync>,
        ) -> DrmResult<Arc<dyn GemBacking>> {
            Ok(Arc::new(Backing))
        }

        fn present(&self, _: Scanout) -> DrmResult<Arc<crate::drm::fence::Fence>> {
            Ok(crate::drm::fence::Fence::new(true))
        }
    }

    #[test]
    fn firmware_device_has_a_real_platform_identity_for_libdrm() {
        let registration = platform_registration("simple-framebuffer.0").unwrap();
        assert!(
            registration
                .uevent_payload()
                .contains("SUBSYSTEM=platform\n")
        );
        assert!(
            registration
                .uevent_payload()
                .contains("MODALIAS=platform:simple-framebuffer.0\n")
        );
    }

    #[test]
    fn pci_discovery_preserves_nondefault_function_and_libdrm_slot_identity() {
        let identity = axdriver_display::DisplayPciIdentity {
            bus: 2,
            device: 7,
            function: 3,
            vendor_id: 0x1af4,
            device_id: 0x1050,
            subsystem_vendor: 0x1af4,
            subsystem_device: 0x1100,
            revision: 1,
        };
        let [pci, virtio]: [Arc<DeviceRegistration>; 2] = pci_registrations(identity)
            .unwrap()
            .try_into()
            .ok()
            .unwrap();
        assert_eq!(pci.identity().bus, "pci0000:02");
        assert_eq!(pci.identity().name, "0000:02:07.3");
        assert!(
            pci.uevent_payload()
                .contains("PCI_SLOT_NAME=0000:02:07.3\n")
        );
        assert!(pci.uevent_payload().contains("SUBSYSTEM=pci\n"));
        assert_eq!(
            virtio.identity().parent,
            Some(("pci0000:02".into(), "0000:02:07.3".into()))
        );
        assert_eq!(virtio.identity().name, "virtio571");
        assert!(
            virtio
                .uevent_payload()
                .contains("DEVPATH=/devices/pci0000:02/0000:02:07.3/virtio571\nSUBSYSTEM=virtio\n")
        );
    }

    #[test]
    fn native_gt_render_identity_is_pci_not_virtio_or_firmware_platform() {
        let identity = axdriver_display::DisplayPciIdentity {
            bus: 0,
            device: 2,
            function: 0,
            vendor_id: 0x8086,
            device_id: 0x46d0,
            subsystem_vendor: 0x8086,
            subsystem_device: 0x2212,
            revision: 0,
        };
        let registrations = pci_registrations(identity).unwrap();
        assert_eq!(registrations.len(), 1);
        let pci = &registrations[0];
        assert!(pci.uevent_payload().contains("SUBSYSTEM=pci\n"));
        assert!(
            pci.uevent_payload()
                .contains("PCI_SLOT_NAME=0000:00:02.0\n")
        );
        assert!(!pci.uevent_payload().contains("virtio"));
        let render = attach_drm_minor(
            DeviceIdentity::new(
                "virtio0".into(),
                "drm".into(),
                "renderD128".into(),
                DRM_RENDER_DEVICE_ID,
            )
            .unwrap(),
            &Some("pci0000:00/0000:00:02.0".into()),
        )
        .unwrap();
        let render =
            DeviceRegistration::try_new(render, "drm_minor".into(), Vec::new(), None).unwrap();
        assert!(
            render
                .uevent_payload()
                .contains("DEVPATH=/devices/pci0000:00/0000:00:02.0/drm/renderD128\n")
        );
        let card = attach_drm_minor(
            DeviceIdentity::new(
                "virtio0".into(),
                "drm".into(),
                "card0".into(),
                DRM_PRIMARY_DEVICE_ID,
            )
            .unwrap(),
            &Some("platform/simple-framebuffer.0".into()),
        )
        .unwrap();
        let card = DeviceRegistration::try_new(card, "drm_minor".into(), Vec::new(), None).unwrap();
        assert!(
            card.uevent_payload()
                .contains("DEVPATH=/devices/platform/simple-framebuffer.0/drm/card0\n")
        );
    }

    #[test]
    fn primary_node_registers_linux_identity_and_opens_per_ofd() {
        let _context = crate::test_support::scheduler_test_context();
        let holder = Arc::new(axsync::Mutex::new(None));
        let holder_for_root = holder.clone();
        let filesystem = SimpleFs::new_with("dri-test".into(), 0, move |fs| {
            *holder_for_root.lock() = Some(fs.clone());
            SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
        });
        let fs = holder.lock().take().unwrap();
        let primary = DrmPrimary::new(DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4));
        let node = primary_node(fs, primary.device.clone());
        let metadata = node.metadata().unwrap();
        assert_eq!(metadata.rdev, DeviceId::new(226, 0));
        assert_eq!(metadata.mode.bits(), 0o600);
        drop(filesystem);

        let first = DrmFileAdapter::new(primary.device.open_primary(), false, O_RDONLY);
        let second = DrmFileAdapter::new(primary.device.open_primary(), false, O_RDONLY);
        assert_ne!(first.file.id(), second.file.id());
    }

    #[test]
    fn drm_ofd_retains_open_access_mode() {
        let device = DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4);
        let read_only = DrmFileAdapter::new(device.open_primary(), false, O_RDONLY);
        let write_only = DrmFileAdapter::new(device.open_primary(), false, O_WRONLY);
        let read_write = DrmFileAdapter::new(device.open_primary(), false, O_RDWR);
        let mut destination_storage = [0; 1];
        let mut destination = &mut destination_storage[..];
        let mut source = &b"x"[..];

        assert!(read_only.allows_read());
        assert!(!read_only.allows_write());
        assert!(!write_only.allows_read());
        assert!(write_only.allows_write());
        assert!(read_write.allows_read());
        assert!(read_write.allows_write());
        assert_eq!(
            write_only.read(&mut destination),
            Err(AxError::BadFileDescriptor)
        );
        assert_eq!(
            read_only.write(&mut source),
            Err(AxError::BadFileDescriptor)
        );
    }
}
