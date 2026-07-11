use alloc::string::String;
use core::{any::Any, cmp::min};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FileBackend, FileFlags};
use axfs_ng_vfs::{DeviceId, NodeFlags, VfsResult};
use axsync::Mutex;
use lazy_static::lazy_static;
use linux_raw_sys::{
    ioctl::{
        BLKGETSIZE, BLKGETSIZE64, BLKRAGET, BLKRASET, BLKROGET, BLKROSET, BLKRRPART, BLKSSZGET,
    },
    loop_device::{
        LO_FLAGS_DIRECT_IO, LO_FLAGS_READ_ONLY, LOOP_CHANGE_FD, LOOP_CLR_FD, LOOP_CONFIGURE,
        LOOP_CTL_ADD, LOOP_CTL_GET_FREE, LOOP_CTL_REMOVE, LOOP_GET_STATUS, LOOP_GET_STATUS64,
        LOOP_SET_BLOCK_SIZE, LOOP_SET_CAPACITY, LOOP_SET_DIRECT_IO, LOOP_SET_FD, LOOP_SET_STATUS,
        LOOP_SET_STATUS64, loop_config, loop_info, loop_info64,
    },
};
use memory_addr::PAGE_SIZE_4K;
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    file::{File, FileLike, get_file_like, try_path_into_owned},
    pseudofs::DeviceOps,
};

const LOOP_COUNT: usize = 16;
const SECTOR_SIZE: u64 = 512;
const DEFAULT_BLOCK_SIZE: u32 = 512;

const FLAG_READ_ONLY: u32 = LO_FLAGS_READ_ONLY as u32;
const FLAG_DIRECT_IO: u32 = LO_FLAGS_DIRECT_IO as u32;
const STATUS_ACCEPTED_FLAGS: u32 = FLAG_READ_ONLY | FLAG_DIRECT_IO;
const CONFIGURE_SETTABLE_FLAGS: u32 = FLAG_READ_ONLY | FLAG_DIRECT_IO;

lazy_static! {
    static ref LOOP_STATES: [Mutex<LoopState>; LOOP_COUNT] =
        core::array::from_fn(|_| Mutex::new(LoopState::default()));
}

#[derive(Clone)]
struct LoopBacking {
    file: FileBackend,
    path: String,
    writable: bool,
}

#[derive(Clone, Default)]
pub(crate) struct LoopSnapshot {
    pub backing_file: String,
    pub size_sectors: u64,
    pub read_only: bool,
    pub direct_io: bool,
    pub sizelimit: u64,
    pub block_size: u32,
}

struct LoopState {
    visible: bool,
    backing: Option<LoopBacking>,
    flags: u32,
    offset: u64,
    sizelimit: u64,
    size_sectors: u64,
    block_size: u32,
}

impl Default for LoopState {
    fn default() -> Self {
        Self {
            visible: true,
            backing: None,
            flags: 0,
            offset: 0,
            sizelimit: 0,
            size_sectors: 0,
            block_size: DEFAULT_BLOCK_SIZE,
        }
    }
}

impl LoopState {
    fn is_bound(&self) -> bool {
        self.backing.is_some()
    }

    fn is_visible(&self) -> bool {
        self.visible
    }

    fn size_bytes(&self) -> u64 {
        self.size_sectors.saturating_mul(SECTOR_SIZE)
    }

    fn read_only(&self) -> bool {
        self.flags & FLAG_READ_ONLY != 0
    }

    fn snapshot(&self) -> LoopSnapshot {
        if !self.visible {
            return LoopSnapshot::default();
        }

        LoopSnapshot {
            backing_file: self
                .backing
                .as_ref()
                .map_or_else(String::new, |backing| backing.path.clone()),
            size_sectors: self.size_sectors,
            read_only: self.read_only(),
            direct_io: self.flags & FLAG_DIRECT_IO != 0,
            sizelimit: self.sizelimit,
            block_size: self.block_size,
        }
    }

    fn loop_size_for(backing: &FileBackend, offset: u64, sizelimit: u64) -> VfsResult<u64> {
        let file_size = backing.location().len()?;
        let mut bytes = file_size.saturating_sub(offset);
        if sizelimit != 0 {
            bytes = min(bytes, sizelimit);
        }
        Ok(bytes / SECTOR_SIZE)
    }

    fn recompute_size(&mut self) -> VfsResult<()> {
        self.size_sectors = self.backing.as_ref().map_or(Ok(0), |backing| {
            Self::loop_size_for(&backing.file, self.offset, self.sizelimit)
        })?;
        Ok(())
    }

    fn direct_io(&self) -> bool {
        self.flags & FLAG_DIRECT_IO != 0
    }

    fn bind(&mut self, mut backing: LoopBacking, read_only: bool) -> VfsResult<()> {
        if !self.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        if self.is_bound() {
            return Err(AxError::ResourceBusy);
        }
        backing.file = backing.file.with_direct_io(false);
        let size_sectors = Self::loop_size_for(&backing.file, 0, 0)?;
        self.backing = Some(backing);
        self.flags = if read_only { FLAG_READ_ONLY } else { 0 };
        self.offset = 0;
        self.sizelimit = 0;
        self.size_sectors = size_sectors;
        self.block_size = DEFAULT_BLOCK_SIZE;
        Ok(())
    }

    fn configure(
        &mut self,
        mut backing: LoopBacking,
        backing_read_only: bool,
        info: loop_info64,
        block_size: u32,
    ) -> VfsResult<()> {
        if !self.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        if self.is_bound() {
            return Err(AxError::ResourceBusy);
        }
        validate_flags(info.lo_flags, CONFIGURE_SETTABLE_FLAGS)?;
        if block_size != 0 {
            validate_block_size(block_size)?;
        }
        let block_size = if block_size == 0 {
            DEFAULT_BLOCK_SIZE
        } else {
            block_size
        };
        let direct_io = info.lo_flags & FLAG_DIRECT_IO != 0;
        if direct_io && !info.lo_offset.is_multiple_of(block_size as u64) {
            return Err(AxError::InvalidInput);
        }
        backing.file = backing.file.with_direct_io(direct_io);
        let size_sectors = Self::loop_size_for(&backing.file, info.lo_offset, info.lo_sizelimit)?;

        let mut flags = info.lo_flags & CONFIGURE_SETTABLE_FLAGS;
        if backing_read_only {
            flags |= FLAG_READ_ONLY;
        }
        self.backing = Some(backing);
        self.flags = flags;
        self.offset = info.lo_offset;
        self.sizelimit = info.lo_sizelimit;
        self.size_sectors = size_sectors;
        self.block_size = block_size;
        Ok(())
    }

    fn clear(&mut self) -> VfsResult<()> {
        if !self.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        if !self.is_bound() {
            return Err(AxError::from(LinuxError::ENXIO));
        }
        *self = Self::default();
        Ok(())
    }

    fn set_info64(&mut self, info: loop_info64) -> VfsResult<()> {
        if !self.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        if !self.is_bound() {
            return Err(AxError::from(LinuxError::ENXIO));
        }
        validate_flags(info.lo_flags, STATUS_ACCEPTED_FLAGS)?;
        if self.direct_io() && !info.lo_offset.is_multiple_of(self.block_size as u64) {
            return Err(AxError::InvalidInput);
        }
        let size_sectors = Self::loop_size_for(
            &self.backing.as_ref().unwrap().file,
            info.lo_offset,
            info.lo_sizelimit,
        )?;
        self.offset = info.lo_offset;
        self.sizelimit = info.lo_sizelimit;
        self.size_sectors = size_sectors;
        Ok(())
    }

    fn set_info(&mut self, info: loop_info) -> VfsResult<()> {
        let mut info64 = self.get_info64(0, DeviceId::new(0, 0))?;
        info64.lo_offset = info.lo_offset.max(0) as u64;
        info64.lo_flags = info.lo_flags.max(0) as u32;
        self.set_info64(info64)
    }

    fn get_info(&self, number: u32, dev_id: DeviceId) -> VfsResult<loop_info> {
        if !self.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        if !self.is_bound() {
            return Err(AxError::from(LinuxError::ENXIO));
        }
        let mut res: loop_info = unsafe { core::mem::zeroed() };
        res.lo_number = number as _;
        res.lo_rdevice = dev_id.0 as _;
        res.lo_offset = self.offset.min(i32::MAX as u64) as _;
        res.lo_flags = self.flags as _;
        if let Some(backing) = self.backing.as_ref() {
            copy_cstr_to_c_char(&backing.path, &mut res.lo_name);
        }
        Ok(res)
    }

    fn get_info64(&self, number: u32, dev_id: DeviceId) -> VfsResult<loop_info64> {
        if !self.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        if !self.is_bound() {
            return Err(AxError::from(LinuxError::ENXIO));
        }
        let mut res: loop_info64 = unsafe { core::mem::zeroed() };
        res.lo_number = number;
        res.lo_rdevice = dev_id.0;
        res.lo_offset = self.offset;
        res.lo_sizelimit = self.sizelimit;
        res.lo_flags = self.flags;
        if let Some(backing) = self.backing.as_ref() {
            copy_cstr_to_u8(&backing.path, &mut res.lo_file_name);
        }
        Ok(res)
    }

    fn change_fd(&mut self, mut backing: LoopBacking) -> VfsResult<()> {
        if !self.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        if !self.is_bound() {
            return Err(AxError::from(LinuxError::ENXIO));
        }
        if !self.read_only() {
            return Err(AxError::InvalidInput);
        }
        let new_size = Self::loop_size_for(&backing.file, self.offset, self.sizelimit)?;
        if new_size != self.size_sectors {
            return Err(AxError::InvalidInput);
        }
        backing.file = backing.file.with_direct_io(self.direct_io());
        self.backing = Some(backing);
        Ok(())
    }

    fn set_direct_io(&mut self, enabled: bool) -> VfsResult<()> {
        if !self.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        if !self.is_bound() {
            return Err(AxError::from(LinuxError::ENXIO));
        }
        if enabled && !self.offset.is_multiple_of(self.block_size as u64) {
            return Err(AxError::InvalidInput);
        }
        if enabled == self.direct_io() {
            return Ok(());
        }
        let backing = self.backing.as_mut().unwrap();
        backing.file = backing.file.with_direct_io(enabled);
        if enabled {
            self.flags |= FLAG_DIRECT_IO;
        } else {
            self.flags &= !FLAG_DIRECT_IO;
        }
        Ok(())
    }
}

fn loop_control_get_free() -> VfsResult<usize> {
    for number in 0..LOOP_COUNT {
        let state = LOOP_STATES[number].lock();
        if state.is_visible() && !state.is_bound() {
            return Ok(number);
        }
    }

    Err(AxError::from(LinuxError::ENOSPC))
}

fn loop_control_add(number: usize) -> VfsResult<usize> {
    if number >= LOOP_COUNT {
        return Err(AxError::from(LinuxError::ENOSPC));
    }

    let mut state = LOOP_STATES[number].lock();
    if state.is_visible() {
        return Err(AxError::from(LinuxError::EEXIST));
    }

    state.visible = true;
    Ok(number)
}

fn loop_control_remove(number: usize) -> VfsResult<()> {
    if number >= LOOP_COUNT {
        return Err(AxError::from(LinuxError::ENODEV));
    }
    Err(AxError::OperationNotSupported)
}

/// /dev/loop-control controller.
pub struct LoopControl;

impl DeviceOps for LoopControl {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(AxError::InvalidInput)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            LOOP_CTL_GET_FREE => loop_control_get_free(),
            LOOP_CTL_ADD => loop_control_add(arg),
            LOOP_CTL_REMOVE => {
                if arg > i32::MAX as usize {
                    return Err(AxError::InvalidInput);
                }
                loop_control_remove(arg)?;
                Ok(0)
            }
            _ => Err(AxError::from(LinuxError::ENOSYS)),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

/// /dev/loopX devices
pub struct LoopDevice {
    number: u32,
    dev_id: DeviceId,
}

impl LoopDevice {
    pub(crate) fn new(number: u32, dev_id: DeviceId) -> Self {
        Self { number, dev_id }
    }

    fn state(&self) -> &'static Mutex<LoopState> {
        &LOOP_STATES[self.number as usize]
    }

    fn read_backing_fd(fd: i32) -> AxResult<(LoopBacking, bool)> {
        if fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        let f = get_file_like(fd)?;
        let Some(file) = f.downcast_ref::<File>() else {
            return Err(AxError::InvalidInput);
        };
        let flags = file.inner().flags();
        if !flags.contains(FileFlags::READ) {
            return Err(AxError::BadFileDescriptor);
        }
        let writable = flags.contains(FileFlags::WRITE);
        let backing = LoopBacking {
            file: file.inner().backend()?.clone(),
            path: try_path_into_owned(file.path()?)?,
            writable,
        };
        Ok((backing, !writable))
    }
}

pub(crate) fn snapshot(number: u32) -> LoopSnapshot {
    if (number as usize) >= LOOP_COUNT {
        return LoopSnapshot::default();
    }
    LOOP_STATES[number as usize].lock().snapshot()
}

impl DeviceOps for LoopDevice {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let state = self.state().lock();
        if !state.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        let backing = state
            .backing
            .as_ref()
            .ok_or(AxError::OperationNotPermitted)?;
        if buf.is_empty() || offset >= state.size_bytes() {
            return Ok(0);
        }
        let limit = min(buf.len() as u64, state.size_bytes() - offset) as usize;
        backing
            .file
            .read_at_slice(&mut buf[..limit], state.offset + offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let state = self.state().lock();
        if !state.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        if state.read_only() {
            return Err(AxError::ReadOnlyFilesystem);
        }
        let backing = state
            .backing
            .as_ref()
            .ok_or(AxError::OperationNotPermitted)?;
        if buf.is_empty() || offset >= state.size_bytes() {
            return Ok(0);
        }
        let limit = min(buf.len() as u64, state.size_bytes() - offset) as usize;
        backing
            .file
            .write_at_slice(&buf[..limit], state.offset + offset)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            LOOP_SET_FD => {
                let (backing, read_only) = Self::read_backing_fd(arg as i32)?;
                self.state().lock().bind(backing, read_only)?;
            }
            LOOP_CLR_FD => {
                self.state().lock().clear()?;
            }
            LOOP_GET_STATUS => {
                let info = self.state().lock().get_info(self.number, self.dev_id)?;
                (arg as *mut loop_info).vm_write(info)?;
            }
            LOOP_GET_STATUS64 => {
                let info = self.state().lock().get_info64(self.number, self.dev_id)?;
                (arg as *mut loop_info64).vm_write(info)?;
            }
            LOOP_SET_STATUS => {
                let info = unsafe { (arg as *const loop_info).vm_read_uninit()?.assume_init() };
                self.state().lock().set_info(info)?;
            }
            LOOP_SET_STATUS64 => {
                let info = unsafe { (arg as *const loop_info64).vm_read_uninit()?.assume_init() };
                self.state().lock().set_info64(info)?;
            }
            LOOP_CHANGE_FD => {
                let (backing, _) = Self::read_backing_fd(arg as i32)?;
                self.state().lock().change_fd(backing)?;
            }
            LOOP_SET_CAPACITY => {
                let mut state = self.state().lock();
                if !state.is_visible() {
                    return Err(AxError::NoSuchDevice);
                }
                if !state.is_bound() {
                    return Err(AxError::from(LinuxError::ENXIO));
                }
                state.recompute_size()?;
            }
            LOOP_SET_DIRECT_IO => {
                self.state().lock().set_direct_io(arg != 0)?;
            }
            LOOP_SET_BLOCK_SIZE => {
                let block_size = u32::try_from(arg).map_err(|_| AxError::InvalidInput)?;
                let mut state = self.state().lock();
                if !state.is_visible() {
                    return Err(AxError::NoSuchDevice);
                }
                if !state.is_bound() {
                    return Err(AxError::from(LinuxError::ENXIO));
                }
                validate_block_size(block_size)?;
                if state.direct_io() && !state.offset.is_multiple_of(block_size as u64) {
                    return Err(AxError::InvalidInput);
                }
                state.block_size = block_size;
            }
            LOOP_CONFIGURE => {
                let config = unsafe { (arg as *const loop_config).vm_read_uninit()?.assume_init() };
                let fd = i32::try_from(config.fd).map_err(|_| AxError::BadFileDescriptor)?;
                let (backing, backing_read_only) = Self::read_backing_fd(fd)?;
                self.state().lock().configure(
                    backing,
                    backing_read_only,
                    config.info,
                    config.block_size,
                )?;
            }
            // TODO: the following should apply to any block devices
            BLKGETSIZE | BLKGETSIZE64 => {
                let state = self.state().lock();
                if !state.is_visible() {
                    return Err(AxError::NoSuchDevice);
                }
                if !state.is_bound() {
                    return Err(AxError::from(LinuxError::ENXIO));
                }
                if cmd == BLKGETSIZE {
                    (arg as *mut u32).vm_write(state.size_sectors as _)?;
                } else {
                    (arg as *mut u64).vm_write(state.size_bytes())?;
                }
            }
            BLKSSZGET => {
                let state = self.state().lock();
                if !state.is_visible() {
                    return Err(AxError::NoSuchDevice);
                }
                (arg as *mut u32).vm_write(state.block_size)?;
            }
            BLKROGET => {
                let state = self.state().lock();
                if !state.is_visible() {
                    return Err(AxError::NoSuchDevice);
                }
                (arg as *mut u32).vm_write(state.read_only() as u32)?;
            }
            BLKROSET => {
                let ro = (arg as *const u32).vm_read()?;
                if ro != 0 && ro != 1 {
                    return Err(AxError::InvalidInput);
                }
                let mut state = self.state().lock();
                if !state.is_visible() {
                    return Err(AxError::NoSuchDevice);
                }
                if ro == 0 {
                    if state
                        .backing
                        .as_ref()
                        .is_some_and(|backing| !backing.writable)
                    {
                        return Err(AxError::ReadOnlyFilesystem);
                    }
                    state.flags &= !FLAG_READ_ONLY;
                } else {
                    state.flags |= FLAG_READ_ONLY;
                }
            }
            BLKRRPART | BLKRAGET | BLKRASET => return Err(AxError::OperationNotSupported),
            _ => {
                warn!("unknown ioctl for loop device: {cmd}");
                return Err(AxError::NotATty);
            }
        }
        Ok(0)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn len(&self) -> VfsResult<u64> {
        let state = self.state().lock();
        if !state.is_visible() {
            return Err(AxError::NoSuchDevice);
        }
        Ok(state.size_bytes())
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

fn validate_block_size(block_size: u32) -> VfsResult<()> {
    if !(DEFAULT_BLOCK_SIZE..=PAGE_SIZE_4K as u32).contains(&block_size)
        || !block_size.is_power_of_two()
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn validate_flags(flags: u32, allowed: u32) -> VfsResult<()> {
    if flags & !allowed != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn copy_cstr_to_u8(src: &str, dst: &mut [u8]) {
    if dst.is_empty() {
        return;
    }
    let bytes = src.as_bytes();
    let len = bytes.len().min(dst.len() - 1);
    dst[..len].copy_from_slice(&bytes[..len]);
    dst[len] = 0;
}

trait LoopChar: Copy {
    fn from_byte(byte: u8) -> Self;
}

impl LoopChar for u8 {
    fn from_byte(byte: u8) -> Self {
        byte
    }
}

impl LoopChar for i8 {
    fn from_byte(byte: u8) -> Self {
        byte as i8
    }
}

fn copy_cstr_to_c_char<T: LoopChar>(src: &str, dst: &mut [T]) {
    if dst.is_empty() {
        return;
    }
    let bytes = src.as_bytes();
    let len = bytes.len().min(dst.len() - 1);
    for (target, byte) in dst[..len].iter_mut().zip(bytes.iter().copied()) {
        *target = T::from_byte(byte);
    }
    dst[len] = T::from_byte(0);
}
