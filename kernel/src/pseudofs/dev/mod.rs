//! Special devices

mod dri;
#[cfg(feature = "input")]
pub(crate) mod event;
mod fb;
pub(crate) mod fuse;
pub(crate) mod r#loop;
#[cfg(feature = "memtrack")]
mod memtrack;
mod rtc;
pub mod tty;
pub(crate) mod tun;

use alloc::{
    borrow::Cow,
    format,
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::{
    any::Any,
    sync::atomic::{AtomicU64, Ordering},
};

use axdriver::{SharedBlockDevice, prelude::DevError};
use axerrno::AxError;
use axfs::BlockDeviceInfo;
use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DeviceId, DirEntry, FileNode, Filesystem, FsName,
    FsNameBuf, Location, MetadataUpdate, NamedCreateOptions, NodeFlags, NodeOps, NodePermission,
    NodeType, Reference, UnlinkRequest, VfsError, VfsResult,
};
use axpoll::Pollable;
use axsync::Mutex;
use hashbrown::HashMap;
use linux_raw_sys::{
    general::CAP_SYS_ADMIN,
    ioctl::{
        BLKGETSIZE, BLKGETSIZE64, BLKRAGET, BLKRASET, BLKROGET, BLKROSET, BLKSSZGET, RNDGETENTCNT,
    },
};
use crate::{
    file::IoctlContext,
    mm::map_usercopy_error,
    mounts,
    pseudofs::{
        ChildNames, Device, DeviceMmap, DeviceOps, DirMaker, DirMapping, NodeOpsMux, SimpleDir,
        SimpleDirOps, SimpleFile, SimpleFs, try_boxed_names,
    },
    task::Cred,
};

const LOOP_NODE_MODE: u16 = 0o600;
const VT_NODE_MODE: u16 = 0o620;
const FB_NODE_MODE: u16 = 0o660;

/// The devfs namespace is mostly static, but Linux daemons must be able to
/// create their own pathname sockets (notably `/dev/log`). Keep those runtime
/// sockets in the same devfs directory so userspace owns their endpoint and
/// can unlink it when the daemon exits.
struct DevRoot {
    fs: Arc<SimpleFs>,
    static_entries: DirMapping,
    sockets: Mutex<HashMap<FsNameBuf, Arc<SimpleFile>>>,
    namespace_epoch: AtomicU64,
}

impl DevRoot {
    fn new(fs: Arc<SimpleFs>) -> Self {
        Self {
            fs,
            static_entries: DirMapping::new(),
            sockets: Mutex::new(HashMap::new()),
            namespace_epoch: AtomicU64::new(0),
        }
    }

    fn add(&mut self, name: impl AsRef<[u8]>, ops: impl Into<NodeOpsMux>) {
        self.static_entries.add(name, ops);
    }

    fn try_owned_name(name: &FsName) -> VfsResult<FsNameBuf> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(name.as_bytes().len())
            .map_err(|_| VfsError::NoMemory)?;
        bytes.extend_from_slice(name.as_bytes());
        FsNameBuf::from_vec(bytes)
    }

    fn entry_from_ops(
        parent: &DirEntry,
        name: &FsName,
        ops: NodeOpsMux,
    ) -> VfsResult<DirEntry> {
        let reference = Reference::try_new(Some(parent.clone()), name)?;
        match ops {
            NodeOpsMux::Dir(maker) => Ok(DirEntry::new_dir(
                |this| axfs_ng_vfs::DirNode::new(maker(this)),
                reference,
            )),
            NodeOpsMux::File(ops) => {
                let node_type = ops.metadata()?.node_type;
                DirEntry::try_new_file(FileNode::new(ops), node_type, reference)
            }
        }
    }
}

impl SimpleDirOps for DevRoot {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let mut names = Vec::new();
        for name in self.static_entries.child_names()? {
            names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            names.push(Self::try_owned_name(name.as_ref())?);
        }
        for name in self.sockets.lock().keys() {
            names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            names.push(Self::try_owned_name(name.as_ref())?);
        }
        try_boxed_names(names.into_iter().map(Cow::Owned))
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        match self.static_entries.lookup_child(name) {
            Ok(ops) => Ok(ops),
            Err(VfsError::NotFound) => self
                .sockets
                .lock()
                .get(name)
                .cloned()
                .map(|socket| NodeOpsMux::File(socket))
                .ok_or(VfsError::NotFound),
            Err(error) => Err(error),
        }
    }

    fn is_cacheable(&self) -> bool {
        true
    }

    fn namespace_epoch(&self) -> u64 {
        self.namespace_epoch.load(Ordering::Acquire)
    }

    fn supports_named_create(&self, node_type: NodeType) -> bool {
        node_type == NodeType::Socket
    }

    fn create_named(
        &self,
        parent: Option<DirEntry>,
        name: &FsName,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        let parent = parent.ok_or(VfsError::NotFound)?;
        let mut sockets = self.sockets.lock();
        if let Ok(ops) = self.static_entries.lookup_child(name) {
            return match disposition {
                CreateDisposition::OpenOrCreate => Ok(CreateOutcome {
                    entry: Self::entry_from_ops(&parent, name, ops)?,
                    created: false,
                }),
                CreateDisposition::Exclusive => Err(VfsError::AlreadyExists),
            };
        }
        if let Some(socket) = sockets.get(name) {
            return match disposition {
                CreateDisposition::OpenOrCreate => Ok(CreateOutcome {
                    entry: Self::entry_from_ops(
                        &parent,
                        name,
                        NodeOpsMux::File(socket.clone()),
                    )?,
                    created: false,
                }),
                CreateDisposition::Exclusive => Err(VfsError::AlreadyExists),
            };
        }
        if options.node_type != NodeType::Socket {
            return Err(VfsError::OperationNotSupported);
        }
        // SimpleFile has no xattr provider, so reject prepared ACL state
        // rather than publishing a socket with silently incomplete metadata.
        if options.initial_attributes.project_inherit
            || options.initial_attributes.access_acl.is_some()
            || options.initial_attributes.default_acl.is_some()
        {
            return Err(VfsError::OperationNotSupported);
        }

        let owned_name = Self::try_owned_name(name)?;
        sockets.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        let socket = SimpleFile::try_new_with_permission(
            self.fs.clone(),
            NodeType::Socket,
            options.permission,
            || Ok(b""),
        )?;
        socket.update_metadata(MetadataUpdate {
            owner: options.owner,
            project_id: options.initial_attributes.project_id,
            ..Default::default()
        })?;
        let entry = Self::entry_from_ops(
            &parent,
            name,
            NodeOpsMux::File(socket.clone()),
        )?;
        options.install_initial_data(&entry)?;
        self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        sockets.insert(owned_name, socket);
        Ok(CreateOutcome {
            entry,
            created: true,
        })
    }

    fn supports_unlink(&self) -> bool {
        true
    }

    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        if request.is_dir {
            return Err(VfsError::NotADirectory);
        }
        let mut sockets = self.sockets.lock();
        let Some(socket) = sockets.get(request.name) else {
            return Err(if self.static_entries.lookup_child(request.name).is_ok() {
                VfsError::OperationNotPermitted
            } else {
                VfsError::NotFound
            });
        };
        if request
            .expected
            .is_some_and(|expected| expected.object_key() != socket.object_key())
        {
            return Err(VfsError::NotFound);
        }
        self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        sockets.remove(request.name);
        Ok(())
    }
}

/// Devfs-facing VT endpoint.
///
/// Virtual terminals share the normal TTY stream semantics even though their
/// switching ioctls are implemented independently from the serial console.
/// Keep the VFS flags here with the node publication, rather than changing
/// the VT state machine for a devfs-specific concern.
struct VtNode(tty::VtDevice);

impl VtNode {
    fn new(number: u16) -> Self {
        Self(tty::VtDevice::new(number))
    }
}

impl DeviceOps for VtNode {
    fn open_description(
        &self,
        location: &Location,
        flags: u32,
    ) -> VfsResult<Option<crate::pseudofs::DeviceOpen>> {
        self.0.open_description(location, flags)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.0.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.0.write_at(buf, offset)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.0.ioctl(context, cmd, arg)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_pollable(&self) -> Option<&dyn Pollable> {
        self.0.as_pollable()
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
            | NodeFlags::NO_SEEK
    }
}

fn require_blkroset_admin(credential: &Cred) -> VfsResult<()> {
    if credential.has_effective_capability(CAP_SYS_ADMIN) {
        Ok(())
    } else {
        Err(AxError::PermissionDenied)
    }
}

/// Authorizes a mutation of global loop-device state.
///
/// Loop bindings are not scoped to the descriptor that issued the ioctl: they
/// publish a backing file and change the device subsequently seen by every
/// opener.  Consequently node DAC alone is insufficient (an already-open
/// descriptor can be inherited or passed across a credential transition).
pub(super) fn require_loop_admin(credential: &Cred) -> VfsResult<()> {
    if credential.has_effective_capability(CAP_SYS_ADMIN) {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

pub(crate) fn new_devfs() -> Filesystem {
    SimpleFs::new_with("devfs".into(), 0x01021994, builder)
}

struct Null;

impl DeviceOps for Null {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

struct Zero;

impl DeviceOps for Zero {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn mmap(&self) -> DeviceMmap {
        DeviceMmap::Anonymous
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

struct Random;

impl DeviceOps for Random {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        crate::random::fill_secure(buf)?;
        Ok(buf.len())
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::OperationNotSupported)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            RNDGETENTCNT => {
                context
                    .user_memory()
                    .write_bytes(arg, &crate::random::entropy_bits().to_ne_bytes())
                    .map_err(map_usercopy_error)?;
                Ok(0)
            }
            _ => Err(AxError::NotATty),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

struct Full;

impl DeviceOps for Full {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::StorageFull)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

struct BlockDevice {
    name: String,
    info: BlockDeviceInfo,
    device: SharedBlockDevice,
}

impl BlockDevice {
    fn new(name: String, info: BlockDeviceInfo, device: SharedBlockDevice) -> Self {
        Self { name, info, device }
    }

    fn read_only(&self) -> bool {
        axfs::block_device_is_read_only(&self.name).unwrap_or(false)
    }

    fn map_error(err: DevError) -> AxError {
        match err {
            DevError::AlreadyExists => AxError::AlreadyExists,
            DevError::Again => AxError::WouldBlock,
            DevError::BadState => AxError::BadState,
            DevError::InvalidParam => AxError::InvalidInput,
            DevError::Io => AxError::Io,
            DevError::NoMemory => AxError::NoMemory,
            DevError::ResourceBusy => AxError::ResourceBusy,
            DevError::Unsupported => AxError::OperationNotSupported,
        }
    }
}

impl DeviceOps for BlockDevice {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() || offset >= self.info.byte_len() {
            return Ok(0);
        }
        self.device.read_at(offset, buf).map_err(Self::map_error)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.read_only() {
            return Err(AxError::ReadOnlyFilesystem);
        }
        if offset >= self.info.byte_len() {
            return Err(AxError::StorageFull);
        }
        self.device.write_at(offset, buf).map_err(Self::map_error)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            BLKGETSIZE => {
                let sectors = self.info.byte_len() / 512;
                context
                    .user_memory()
                    .write_bytes(arg, &(sectors as u32).to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            BLKGETSIZE64 => {
                context
                    .user_memory()
                    .write_bytes(arg, &self.info.byte_len().to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            BLKSSZGET => {
                context
                    .user_memory()
                    .write_bytes(arg, &(self.info.block_size as u32).to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            BLKROGET => {
                context
                    .user_memory()
                    .write_bytes(arg, &(self.read_only() as u32).to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            BLKROSET => {
                require_blkroset_admin(context.caller_cred())?;
                let ro = context
                    .user_memory()
                    .read_value(arg as *const u32)
                    .map_err(map_usercopy_error)?;
                if ro != 0 && ro != 1 {
                    return Err(AxError::InvalidInput);
                }
                axfs::set_block_device_read_only(&self.name, ro != 0).map_err(|err| match err {
                    axfs::OpenBlockDeviceError::NotFound => AxError::NoSuchDevice,
                    axfs::OpenBlockDeviceError::Busy => AxError::ResourceBusy,
                })?;
            }
            BLKRAGET | BLKRASET => return Err(AxError::OperationNotSupported),
            _ => return Err(AxError::NotATty),
        }
        Ok(0)
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        self.device.lock().flush().map_err(Self::map_error)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn len(&self) -> VfsResult<u64> {
        Ok(self.info.byte_len())
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

fn builder(fs: Arc<SimpleFs>) -> DirMaker {
    let mut root = DevRoot::new(fs.clone());
    root.add(
        "null",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 3),
            Arc::new(Null),
        ),
    );
    root.add(
        "zero",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 5),
            Arc::new(Zero),
        ),
    );
    root.add(
        "full",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 7),
            Arc::new(Full),
        ),
    );
    root.add(
        "random",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 8),
            Arc::new(Random),
        ),
    );
    root.add(
        "urandom",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 9),
            Arc::new(Random),
        ),
    );
    // The FUSE transport is an OFD-owned character device: each daemon open
    // creates one independent connection which is later selected by fsopen's
    // `fd=` configuration.  It must not share state across daemon instances.
    root.add(
        "fuse",
        Device::new_with_permissions(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(10, 229),
            NodePermission::from_bits_truncate(0o666),
            Arc::new(fuse::FuseDevice),
        ),
    );
    let mut net = DirMapping::new();
    net.add(
        "tun",
        Device::new_with_permissions(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(10, 200),
            NodePermission::from_bits_truncate(0o666),
            Arc::new(tun::TunDevice),
        ),
    );
    root.add("net", SimpleDir::new_maker(fs.clone(), Arc::new(net)));
    root.add(
        "rtc0",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            rtc::RTC0_DEVICE_ID,
            Arc::new(rtc::Rtc),
        ),
    );
    // DRM owns VirtIO-GPU scanout before devfs publication. `/dev/fb0` is an
    // emulation client of that primary device, never a competing raw display.
    if crate::drm::primary_device().is_some() {
        match fb::FrameBuffer::try_new() {
            Ok(framebuffer) => root.add(
                "fb0",
                Device::new_with_permissions(
                    fs.clone(),
                    NodeType::CharacterDevice,
                    fb::FB_DEVICE_ID,
                    NodePermission::from_bits_truncate(FB_NODE_MODE),
                    Arc::new(framebuffer),
                ),
            ),
            Err(error) => error!("Failed to initialize framebuffer device: {error}"),
        }
    }

    if let Some(device) = crate::drm::primary_device() {
        let mut dri = DirMapping::new();
        dri.add("card0", dri::primary_node(fs.clone(), device.clone()));
        if let Some(render) = dri::render_node(fs.clone(), device) {
            dri.add("renderD128", render);
        }
        root.add("dri", SimpleDir::new_maker(fs.clone(), Arc::new(dri)));
    }

    root.add(
        "tty",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 0),
            Arc::new(tty::CurrentTty),
        ),
    );
    root.add(
        "console",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 1),
            Arc::new(VtNode::new(0)),
        ),
    );

    // Linux reserves major 4 for virtual consoles: tty0 is an alias for the
    // active console and tty1..tty63 are fixed VT endpoints.
    for number in 0..=63 {
        root.add(
            format!("tty{number}"),
            Device::new_with_permissions(
                fs.clone(),
                NodeType::CharacterDevice,
                DeviceId::new(4, number),
                NodePermission::from_bits_truncate(VT_NODE_MODE),
                Arc::new(VtNode::new(number as u16)),
            ),
        );
    }

    root.add(
        "ptmx",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 2),
            Arc::new(tty::Ptmx(fs.clone())),
        ),
    );
    root.add(
        "pts",
        SimpleDir::new_maker(fs.clone(), Arc::new(tty::PtsDir)),
    );
    #[cfg(feature = "memtrack")]
    root.add(
        "memtrack",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(114, 514),
            Arc::new(memtrack::MemTrack),
        ),
    );

    // This is mounted to a tmpfs in `new_procfs`
    root.add(
        "shm",
        SimpleDir::new_maker(fs.clone(), Arc::new(DirMapping::new())),
    );

    // Loop devices
    root.add(
        "loop-control",
        Device::new_with_permissions(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(10, 237),
            NodePermission::from_bits_truncate(LOOP_NODE_MODE),
            Arc::new(r#loop::LoopControl),
        ),
    );
    for i in 0..16 {
        let dev_id = DeviceId::new(7, i);
        root.add(
            format!("loop{i}"),
            Device::new_with_permissions(
                fs.clone(),
                NodeType::BlockDevice,
                dev_id,
                NodePermission::from_bits_truncate(LOOP_NODE_MODE),
                Arc::new(r#loop::LoopDevice::new(i, dev_id)),
            ),
        );
    }

    if let (Some(info), Ok(device)) = (
        axfs::root_block_device_info(),
        axfs::raw_block_device(axfs::ROOT_BLOCK_DEVICE_NAME),
    ) {
        root.add(
            axfs::ROOT_BLOCK_DEVICE_NAME,
            Device::new_with_permissions(
                fs.clone(),
                NodeType::BlockDevice,
                mounts::ROOT_BLOCK_DEVICE_ID,
                NodePermission::from_bits_truncate(0o600),
                Arc::new(BlockDevice::new(
                    axfs::ROOT_BLOCK_DEVICE_NAME.into(),
                    info,
                    device,
                )),
            ),
        );
    }

    for (index, name) in axfs::block_device_names().into_iter().enumerate() {
        let Some(info) = axfs::block_device_info(&name) else {
            continue;
        };
        let Ok(device) = axfs::raw_block_device(&name) else {
            continue;
        };
        let Some(dev_id) = mounts::extra_block_device_id(index) else {
            continue;
        };
        root.add(
            name.clone(),
            Device::new_with_permissions(
                fs.clone(),
                NodeType::BlockDevice,
                dev_id,
                NodePermission::from_bits_truncate(0o600),
                Arc::new(BlockDevice::new(name, info, device)),
            ),
        );
    }

    // Input devices
    #[cfg(feature = "input")]
    root.add(
        "input",
        SimpleDir::new_maker(fs.clone(), event::input_devices(fs.clone())),
    );

    SimpleDir::new_maker(fs, Arc::new(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Cred, Kgid, Kuid, UserNamespace};
    use axfs_ng_vfs::FsName;

    #[test]
    fn devfs_publishes_linux_virtual_console_nodes() {
        let devfs = new_devfs();
        let root = devfs.root_dir();
        let root = root.as_dir().unwrap();

        assert!(matches!(
            root.lookup(FsName::new(b"log")),
            Err(VfsError::NotFound)
        ));

        for number in 0..=63 {
            let node = root.lookup(FsName::new(format!("tty{number}").as_bytes())).unwrap();
            let metadata = node.metadata().unwrap();
            assert_eq!(metadata.node_type, NodeType::CharacterDevice);
            assert_eq!(metadata.rdev, DeviceId::new(4, number));
            assert_eq!(metadata.mode.bits(), VT_NODE_MODE);
            let expected_flags = NodeFlags::NON_CACHEABLE
                | NodeFlags::STREAM
                | NodeFlags::NO_POSITIONED_READ
                | NodeFlags::NO_POSITIONED_WRITE
                | NodeFlags::NO_SEEK;
            assert_eq!(node.flags().bits(), expected_flags.bits());
        }

        // These existing character devices are separate Linux ABI nodes.
        assert_eq!(
            root.lookup(FsName::new(b"tty")).unwrap().metadata().unwrap().rdev,
            DeviceId::new(5, 0)
        );
        assert_eq!(
            root.lookup(FsName::new(b"console")).unwrap().metadata().unwrap().rdev,
            DeviceId::new(5, 1)
        );
        let console = root.lookup(FsName::new(b"console")).unwrap();
        let console = console.downcast::<Device>().unwrap();
        let console = console.inner().as_any().downcast_ref::<VtNode>().unwrap();
        assert!(console.0.is_active_alias());
    }

    #[test]
    fn devfs_allows_userspace_owned_pathname_sockets() {
        let devfs = new_devfs();
        let root = devfs.root_dir();
        let root = root.as_dir().unwrap();
        let name = FsName::new(b"log");

        let socket = root
            .create(name, NodeType::Socket, NodePermission::from_bits_truncate(0o666))
            .unwrap();
        assert_eq!(socket.metadata().unwrap().node_type, NodeType::Socket);
        assert_eq!(root.lookup(name).unwrap().object_key(), socket.object_key());

        root.unlink(name, false).unwrap();
        assert!(matches!(root.lookup(name), Err(VfsError::NotFound)));
    }

    #[test]
    fn loop_nodes_are_owner_only_by_default() {
        assert_eq!(
            NodePermission::from_bits_truncate(LOOP_NODE_MODE).bits(),
            0o600
        );
    }

    #[test]
    fn framebuffer_node_is_not_world_writable() {
        assert_eq!(
            NodePermission::from_bits_truncate(FB_NODE_MODE).bits(),
            0o660
        );
    }

    #[test]
    fn blkroset_admin_is_confined_to_initial_user_namespace() {
        let initial = UserNamespace::try_new_root().unwrap();
        let initial_root = Cred::try_root(initial.clone()).unwrap();
        assert!(require_blkroset_admin(&initial_root).is_ok());

        let child = initial
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let child_root = Cred::try_with_user_namespace(&initial_root, child).unwrap();
        assert!(matches!(
            require_blkroset_admin(&child_root),
            Err(AxError::PermissionDenied)
        ));
    }

    #[test]
    fn loop_mutation_admin_is_confined_to_initial_user_namespace() {
        let initial = UserNamespace::try_new_root().unwrap();
        let initial_root = Cred::try_root(initial.clone()).unwrap();
        assert!(require_loop_admin(&initial_root).is_ok());

        let child = initial
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let child_root = Cred::try_with_user_namespace(&initial_root, child).unwrap();
        assert!(matches!(
            require_loop_admin(&child_root),
            Err(AxError::OperationNotPermitted)
        ));
    }
}
