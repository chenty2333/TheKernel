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
use linux_raw_sys::general::S_IFCHR;

use crate::{
    drm::{DrmDevice, DrmFile},
    file::{FileLike, FileMmapRequest, IoDst, IoSrc, IoctlContext, Kstat, PreparedFileMmap},
    pseudofs::{
        DeviceOpen, DeviceOps,
        device_registry::{
            DeviceAttribute, DeviceHandle, DeviceIdentity, DeviceRegistration, DeviceReservation,
            MAX_DEVICES, global_device_registry,
        },
    },
    readiness::block_on_poll_io,
};

pub(crate) const DRM_PRIMARY_DEVICE_ID: DeviceId = DeviceId::new(226, 0);
pub(crate) const DRM_PRIMARY_NODE_MODE: u16 = 0o600;
pub(crate) const DRM_RENDER_DEVICE_ID: DeviceId = DeviceId::new(226, 128);
// udev grants the conventional render group after userspace starts; before
// that, do not expose a world-writable GPU node.
pub(crate) const DRM_RENDER_NODE_MODE: u16 = 0o600;

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

fn attribute(name: &str, value: &'static str) -> DeviceAttribute {
    DeviceAttribute::try_new(name.into(), move || Ok(value)).expect("static DRM sysfs attribute")
}

fn mode_attribute(mode: crate::drm::Mode) -> DeviceAttribute {
    DeviceAttribute::try_new("modes".into(), move || {
        Ok(alloc::format!("{}x{}\n", mode.width, mode.height))
    })
    .expect("static DRM sysfs mode attribute")
}

fn publish_sysfs(device: &DrmDevice) -> VfsResult<Vec<DeviceHandle<'static, MAX_DEVICES>>> {
    let card = DeviceRegistration::try_new(
        DeviceIdentity::new(
            "virtio0".into(),
            "drm".into(),
            "card0".into(),
            DRM_PRIMARY_DEVICE_ID,
        )?
        .with_devname("dri/card0".into())?,
        "drm_minor".into(),
        Vec::new(),
        None,
    )?;
    let connector = DeviceRegistration::try_new(
        DeviceIdentity::without_dev("virtio0".into(), "drm".into(), "card0-Virtual-1".into())?
            .child_of("virtio0".into(), "card0".into())?,
        "drm_connector".into(),
        vec![
            attribute("status", "connected\n"),
            mode_attribute(device.preferred_mode()),
        ],
        None,
    )?;
    let render = if device.has_render() {
        Some(DeviceRegistration::try_new(
            DeviceIdentity::new(
                "virtio0".into(),
                "drm".into(),
                "renderD128".into(),
                DRM_RENDER_DEVICE_ID,
            )?
            .with_devname("dri/renderD128".into())?,
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
        return Ok(vec![card_handle, connector_handle]);
    };
    let render_reservation = global_device_registry().reserve(render.identity().clone())?;
    Ok(DeviceReservation::publish_many([
        (card_reservation, card),
        (connector_reservation, connector),
        (render_reservation, render),
    ])?
    .into_iter()
    .collect())
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
}

impl DrmFileAdapter {
    fn new(file: DrmFile, render: bool) -> Self {
        Self {
            file,
            nonblocking: AtomicBool::new(false),
            render,
        }
    }
}

impl DeviceOps for DrmPrimary {
    fn open_description(&self, _location: &Location, _flags: u32) -> VfsResult<Option<DeviceOpen>> {
        let file: Arc<dyn FileLike> =
            Arc::try_new(DrmFileAdapter::new(self.device.open_primary(), false))
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
    fn open_description(&self, _: &Location, _: u32) -> VfsResult<Option<DeviceOpen>> {
        let file: Arc<dyn FileLike> = Arc::try_new(DrmFileAdapter::new(
            self.device.open_render().map_err(AxError::from)?,
            true,
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
        block_on_poll_io(self, IoEvents::READABLE, self.nonblocking(), || {
            let result = self.file.read_events(dst);
            match result {
                Ok(0) => Err(AxError::WouldBlock),
                other => other,
            }
        })
    }

    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
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

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok(if self.render {
            "/dev/dri/renderD128"
        } else {
            "/dev/dri/card0"
        }
        .into())
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
        fn create_dumb(&self, _: DumbRequest, _: u32, _: u64) -> DrmResult<Arc<dyn GemBacking>> {
            Ok(Arc::new(Backing))
        }

        fn present(&self, _: Scanout) -> DrmResult<()> {
            Ok(())
        }
    }

    #[test]
    fn primary_node_registers_linux_identity_and_opens_per_ofd() {
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

        let first = DrmFileAdapter::new(primary.device.open_primary(), false);
        let second = DrmFileAdapter::new(primary.device.open_primary(), false);
        assert_ne!(first.file.id(), second.file.id());
    }
}
