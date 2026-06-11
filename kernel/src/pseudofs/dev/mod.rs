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
use core::{
    any::Any,
    sync::atomic::{AtomicU32, Ordering},
};

use axerrno::AxError;
use axfs::BlockDeviceInfo;
use axfs_ng_vfs::{DeviceId, Filesystem, NodeFlags, NodeType, VfsResult};
use axsync::Mutex;
use linux_raw_sys::ioctl::{
    BLKGETSIZE, BLKGETSIZE64, BLKRAGET, BLKRASET, BLKROGET, BLKROSET, BLKSSZGET, RNDGETENTCNT,
};
#[cfg(feature = "dev-log")]
pub use log::bind_dev_log;
use rand::{Rng, SeedableRng, rngs::SmallRng};
use starry_vm::{VmMutPtr, VmPtr};

use crate::pseudofs::{Device, DeviceMmap, DeviceOps, DirMaker, DirMapping, SimpleDir, SimpleFs};

const RANDOM_SEED: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
pub(crate) const RANDOM_ENTROPY_BITS: i32 = 256;

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

struct Random {
    rng: Mutex<SmallRng>,
}

impl Random {
    pub fn new() -> Self {
        Self {
            rng: Mutex::new(SmallRng::from_seed(*RANDOM_SEED)),
        }
    }
}

impl DeviceOps for Random {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.rng.lock().fill_bytes(buf);
        Ok(buf.len())
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            RNDGETENTCNT => {
                (arg as *mut i32).vm_write(RANDOM_ENTROPY_BITS)?;
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
    ra: AtomicU32,
}

impl BlockDevice {
    fn new(name: String, info: BlockDeviceInfo) -> Self {
        Self {
            name,
            info,
            ra: AtomicU32::new(512),
        }
    }

    fn read_only(&self) -> bool {
        axfs::block_device_is_read_only(&self.name).unwrap_or(false)
    }
}

impl DeviceOps for BlockDevice {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() || offset >= self.info.byte_len() {
            return Ok(0);
        }
        Err(AxError::InvalidInput)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if self.read_only() {
            return Err(AxError::ReadOnlyFilesystem);
        }
        Err(AxError::InvalidInput)
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
                let ro = (arg as *const u32).vm_read()?;
                if ro != 0 && ro != 1 {
                    return Err(AxError::InvalidInput);
                }
                axfs::set_block_device_read_only(&self.name, ro != 0)
                    .map_err(|_| AxError::NoSuchDevice)?;
            }
            BLKRAGET => {
                (arg as *mut usize).vm_write(self.ra.load(Ordering::Relaxed) as usize)?;
            }
            BLKRASET => {
                self.ra.store(arg as u32, Ordering::Relaxed);
            }
            _ => return Err(AxError::NotATty),
        }
        Ok(0)
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

struct CpuDmaLatency;

impl DeviceOps for CpuDmaLatency {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
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
            Arc::new(Random::new()),
        ),
    );
    root.add(
        "urandom",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 9),
            Arc::new(Random::new()),
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

    root.add(
        "cpu_dma_latency",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(10, 1024),
            Arc::new(CpuDmaLatency),
        ),
    );

    // This is mounted to a tmpfs in `new_procfs`
    root.add(
        "shm",
        SimpleDir::new_maker(fs.clone(), Arc::new(DirMapping::new())),
    );

    // Loop devices
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
        let part_dev_id = DeviceId::new(7, 256 + i);
        root.add(
            format!("loop{i}p1"),
            Device::new(
                fs.clone(),
                NodeType::BlockDevice,
                part_dev_id,
                Arc::new(r#loop::LoopDevice::new(i, part_dev_id)),
            ),
        );
    }

    for (index, name) in axfs::block_device_names().into_iter().enumerate() {
        let Some(info) = axfs::block_device_info(&name) else {
            continue;
        };
        root.add(
            name.clone(),
            Device::new(
                fs.clone(),
                NodeType::BlockDevice,
                DeviceId::new(8, 16 + index as u32),
                Arc::new(BlockDevice::new(name, info)),
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
