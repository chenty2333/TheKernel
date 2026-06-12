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

use alloc::{borrow::Cow, boxed::Box, format, string::String, sync::Arc};
use core::{
    any::Any,
    sync::atomic::{AtomicU32, Ordering},
};

use axerrno::AxError;
use axfs::BlockDeviceInfo;
use axfs_ng_vfs::{DeviceId, Filesystem, NodeFlags, NodeType, VfsError, VfsResult};
use axsync::Mutex;
use linux_raw_sys::ioctl::{
    BLKGETSIZE, BLKGETSIZE64, BLKRAGET, BLKRASET, BLKROGET, BLKROSET, BLKSSZGET, RNDGETENTCNT,
    TUNGETFEATURES,
};
#[cfg(feature = "dev-log")]
pub use log::bind_dev_log;
use rand::{Rng, SeedableRng, rngs::SmallRng};
use starry_vm::{VmMutPtr, VmPtr};

use crate::pseudofs::{
    Device, DeviceMmap, DeviceOps, DirMaker, DirMapping, NodeOpsMux, SimpleDir, SimpleDirOps,
    SimpleFs,
};

const RANDOM_SEED: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
pub(crate) const RANDOM_ENTROPY_BITS: i32 = 256;
const IFF_TUN: u32 = 0x0001;
const IFF_TAP: u32 = 0x0002;
const IFF_NAPI: u32 = 0x0010;
const IFF_NAPI_FRAGS: u32 = 0x0020;
const IFF_NO_CARRIER: u32 = 0x0040;
const IFF_MULTI_QUEUE: u32 = 0x0100;
const IFF_NO_PI: u32 = 0x1000;
const IFF_ONE_QUEUE: u32 = 0x2000;
const IFF_VNET_HDR: u32 = 0x4000;

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

struct LoopPartitionDevices {
    fs: Arc<SimpleFs>,
}

impl LoopPartitionDevices {
    fn parse_name(name: &str) -> Option<(u32, u32)> {
        let rest = name.strip_prefix("loop")?;
        let (number, partition) = rest.split_once('p')?;
        Some((number.parse().ok()?, partition.parse().ok()?))
    }
}

impl SimpleDirOps for LoopPartitionDevices {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new((0..16).flat_map(|number| {
            (1..=r#loop::partition_count(number))
                .map(move |partition| Cow::Owned(format!("loop{number}p{partition}")))
        }))
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let (number, partition) = Self::parse_name(name).ok_or(VfsError::NotFound)?;
        if number >= 16 || !r#loop::partition_visible(number, partition) {
            return Err(VfsError::NotFound);
        }

        let dev_id = DeviceId::new(7, 256 * partition + number);
        Ok(NodeOpsMux::File(Device::new(
            self.fs.clone(),
            NodeType::BlockDevice,
            dev_id,
            Arc::new(r#loop::LoopDevice::new(number, dev_id)),
        )))
    }

    fn is_cacheable(&self) -> bool {
        false
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

struct Tun;

impl Tun {
    const FEATURES: u32 = IFF_TUN
        | IFF_TAP
        | IFF_NO_CARRIER
        | IFF_NO_PI
        | IFF_ONE_QUEUE
        | IFF_VNET_HDR
        | IFF_MULTI_QUEUE
        | IFF_NAPI
        | IFF_NAPI_FRAGS;
}

impl DeviceOps for Tun {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            TUNGETFEATURES => {
                (arg as *mut u32).vm_write(Self::FEATURES)?;
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

    root.add("net", {
        let mut net = DirMapping::new();
        net.add(
            "tun",
            Device::new(
                fs.clone(),
                NodeType::CharacterDevice,
                DeviceId::new(10, 200),
                Arc::new(Tun),
            ),
        );
        SimpleDir::new_maker(fs.clone(), Arc::new(net))
    });

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

    let root = root.chain(LoopPartitionDevices { fs: fs.clone() });
    SimpleDir::new_maker(fs, Arc::new(root))
}
