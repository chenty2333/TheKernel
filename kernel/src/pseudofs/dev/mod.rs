//! Special devices

#[cfg(feature = "input")]
mod event;
mod fb;
#[cfg(feature = "dev-log")]
mod log;
pub(crate) mod r#loop;
#[cfg(feature = "memtrack")]
mod memtrack;
mod rtc;
pub mod tty;

use alloc::{format, string::String, sync::Arc};
use core::any::Any;

use axdriver::{
    SharedBlockDevice,
    prelude::{BlockDriverOps, DevError},
};
use axerrno::AxError;
use axfs::BlockDeviceInfo;
use axfs_ng_vfs::{DeviceId, Filesystem, NodeFlags, NodePermission, NodeType, VfsResult};
use axtask::current;
use linux_raw_sys::{
    general::CAP_SYS_ADMIN,
    ioctl::{
        BLKGETSIZE, BLKGETSIZE64, BLKRAGET, BLKRASET, BLKROGET, BLKROSET, BLKSSZGET, RNDGETENTCNT,
    },
};
#[cfg(feature = "dev-log")]
pub use log::bind_dev_log;
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    mounts,
    pseudofs::{Device, DeviceMmap, DeviceOps, DirMaker, DirMapping, SimpleDir, SimpleFs},
    task::AsThread,
};

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

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            RNDGETENTCNT => {
                (arg as *mut i32).vm_write(crate::random::entropy_bits())?;
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

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            BLKGETSIZE => {
                let sectors = self.info.byte_len() / 512;
                (arg as *mut u32).vm_write(sectors as u32)?;
            }
            BLKGETSIZE64 => {
                (arg as *mut u64).vm_write(self.info.byte_len())?;
            }
            BLKSSZGET => {
                (arg as *mut u32).vm_write(self.info.block_size as u32)?;
            }
            BLKROGET => {
                (arg as *mut u32).vm_write(self.read_only() as u32)?;
            }
            BLKROSET => {
                if !current()
                    .as_thread()
                    .proc_data
                    .has_effective_capability(CAP_SYS_ADMIN)
                {
                    return Err(AxError::PermissionDenied);
                }
                let ro = (arg as *const u32).vm_read()?;
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
    let mut root = DirMapping::new();
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
    root.add(
        "rtc0",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            rtc::RTC0_DEVICE_ID,
            Arc::new(rtc::Rtc),
        ),
    );
    if axdisplay::has_display() {
        root.add(
            "fb0",
            Device::new(
                fs.clone(),
                NodeType::CharacterDevice,
                DeviceId::new(29, 0),
                Arc::new(fb::FrameBuffer::new()),
            ),
        );
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
            tty::N_TTY.clone(),
        ),
    );

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
    #[cfg(feature = "dev-log")]
    root.add(
        "log",
        crate::pseudofs::SimpleFile::new(fs.clone(), NodeType::Socket, || Ok(b"")),
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
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(10, 237),
            Arc::new(r#loop::LoopControl),
        ),
    );
    for i in 0..16 {
        let dev_id = DeviceId::new(7, i);
        root.add(
            format!("loop{i}"),
            Device::new(
                fs.clone(),
                NodeType::BlockDevice,
                dev_id,
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
        SimpleDir::new_maker(fs.clone(), Arc::new(event::input_devices(fs.clone()))),
    );

    SimpleDir::new_maker(fs, Arc::new(root))
}
