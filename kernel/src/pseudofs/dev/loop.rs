use alloc::vec::Vec;
use core::{
    any::Any,
    cmp::min,
    mem::{MaybeUninit, align_of, offset_of, size_of, size_of_val},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FileBackend, FileFlags};
use axfs_ng_vfs::{DeviceId, FsPathBuf, NodeFlags, VfsResult};
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

use crate::{
    file::{File, FileLike, IoctlContext, try_path_into_owned},
    mm::{UserMemoryCapability, map_usercopy_error},
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

struct LoopBacking {
    file: FileBackend,
    path: FsPathBuf,
    writable: bool,
}

#[derive(Clone, Default)]
pub(crate) struct LoopSnapshot {
    pub backing_file: Vec<u8>,
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

#[derive(Clone, Copy)]
enum LoopBlockOutput {
    U32(u32),
    U64(u64),
}

fn snapshot_block_output(state: &LoopState, cmd: u32) -> VfsResult<LoopBlockOutput> {
    if !state.is_visible() {
        return Err(AxError::NoSuchDevice);
    }
    match cmd {
        BLKGETSIZE | BLKGETSIZE64 => {
            if !state.is_bound() {
                return Err(AxError::from(LinuxError::ENXIO));
            }
            if cmd == BLKGETSIZE {
                Ok(LoopBlockOutput::U32(state.size_sectors as u32))
            } else {
                Ok(LoopBlockOutput::U64(state.size_bytes()))
            }
        }
        BLKSSZGET => Ok(LoopBlockOutput::U32(state.block_size)),
        BLKROGET => Ok(LoopBlockOutput::U32(state.read_only() as u32)),
        _ => unreachable!(),
    }
}

fn copy_block_output(
    user_memory: &UserMemoryCapability,
    address: usize,
    output: LoopBlockOutput,
) -> AxResult {
    match output {
        LoopBlockOutput::U32(value) => user_memory
            .write_bytes(address, &value.to_ne_bytes())
            .map_err(map_usercopy_error),
        LoopBlockOutput::U64(value) => user_memory
            .write_bytes(address, &value.to_ne_bytes())
            .map_err(map_usercopy_error),
    }
}

const _: () = {
    assert!(align_of::<loop_info>() == 8);
    assert!(size_of::<loop_info>() == 168 || size_of::<loop_info>() == 160);
    assert!(offset_of!(loop_info, lo_number) == 0);
    assert!(offset_of!(loop_info, lo_name) > offset_of!(loop_info, lo_flags));
    assert!(size_of::<loop_info64>() == 232);
    assert!(align_of::<loop_info64>() == 8);
    assert!(offset_of!(loop_info64, lo_file_name) == 56);
    assert!(offset_of!(loop_info64, lo_init) == 216);
    assert!(offset_of!(loop_config, info) == 8);
    assert!(size_of::<loop_config>() == 304);
};

fn read_user_bytes<const N: usize>(context: &IoctlContext, address: usize) -> AxResult<[u8; N]> {
    let mut bytes = [MaybeUninit::<u8>::uninit(); N];
    context
        .user_memory()
        .read_bytes(address, &mut bytes)
        .map_err(map_usercopy_error)?;
    Ok(core::array::from_fn(|index| {
        // SAFETY: read_bytes initializes every byte before returning.
        unsafe { bytes[index].assume_init() }
    }))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(bytes[offset..][..4].try_into().unwrap())
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_ne_bytes(bytes[offset..][..4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(bytes[offset..][..8].try_into().unwrap())
}

fn read_old_ulong(bytes: &[u8], offset: usize) -> u64 {
    match size_of::<core::ffi::c_ulong>() {
        4 => u64::from(read_u32(bytes, offset)),
        8 => read_u64(bytes, offset),
        _ => unreachable!(),
    }
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..][..4].copy_from_slice(&value.to_ne_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..][..8].copy_from_slice(&value.to_ne_bytes());
}

fn put_old_dev(bytes: &mut [u8], offset: usize, value: u64, width: usize) {
    match width {
        4 => put_u32(bytes, offset, value as u32),
        8 => put_u64(bytes, offset, value),
        _ => unreachable!(),
    }
}

fn loop_info_from_user_bytes(bytes: [u8; size_of::<loop_info>()]) -> loop_info {
    let mut lo_name = [0i8; 64];
    let lo_name_len = lo_name.len();
    for (dst, src) in lo_name.iter_mut().zip(
        bytes[offset_of!(loop_info, lo_name)..][..lo_name_len]
            .iter()
            .copied(),
    ) {
        *dst = src as i8;
    }
    let mut lo_encrypt_key = [0u8; 32];
    let lo_encrypt_key_len = lo_encrypt_key.len();
    lo_encrypt_key
        .copy_from_slice(&bytes[offset_of!(loop_info, lo_encrypt_key)..][..lo_encrypt_key_len]);
    let mut lo_init = [0u64; 2];
    for (index, value) in lo_init.iter_mut().enumerate() {
        *value = read_old_ulong(
            &bytes,
            offset_of!(loop_info, lo_init) + index * size_of::<core::ffi::c_ulong>(),
        );
    }
    loop_info {
        lo_number: read_i32(&bytes, offset_of!(loop_info, lo_number)),
        lo_device: read_old_ulong(&bytes, offset_of!(loop_info, lo_device)) as _,
        lo_inode: read_old_ulong(&bytes, offset_of!(loop_info, lo_inode)) as _,
        lo_rdevice: read_old_ulong(&bytes, offset_of!(loop_info, lo_rdevice)) as _,
        lo_offset: read_i32(&bytes, offset_of!(loop_info, lo_offset)),
        lo_encrypt_type: read_i32(&bytes, offset_of!(loop_info, lo_encrypt_type)),
        lo_encrypt_key_size: read_i32(&bytes, offset_of!(loop_info, lo_encrypt_key_size)),
        lo_flags: read_i32(&bytes, offset_of!(loop_info, lo_flags)),
        lo_name,
        lo_encrypt_key,
        lo_init: lo_init.map(|value| value as _),
        // The ABI reserves these bytes; accept but do not retain them.
        reserved: [0; 4],
    }
}

fn loop_info64_from_user_bytes(bytes: [u8; size_of::<loop_info64>()]) -> loop_info64 {
    let mut lo_file_name = [0u8; 64];
    let lo_file_name_len = lo_file_name.len();
    lo_file_name
        .copy_from_slice(&bytes[offset_of!(loop_info64, lo_file_name)..][..lo_file_name_len]);
    let mut lo_crypt_name = [0u8; 64];
    let lo_crypt_name_len = lo_crypt_name.len();
    lo_crypt_name
        .copy_from_slice(&bytes[offset_of!(loop_info64, lo_crypt_name)..][..lo_crypt_name_len]);
    let mut lo_encrypt_key = [0u8; 32];
    let lo_encrypt_key_len = lo_encrypt_key.len();
    lo_encrypt_key
        .copy_from_slice(&bytes[offset_of!(loop_info64, lo_encrypt_key)..][..lo_encrypt_key_len]);
    let mut lo_init = [0u64; 2];
    for (index, value) in lo_init.iter_mut().enumerate() {
        *value = read_u64(
            &bytes,
            offset_of!(loop_info64, lo_init) + index * size_of::<u64>(),
        );
    }
    loop_info64 {
        lo_device: read_u64(&bytes, offset_of!(loop_info64, lo_device)),
        lo_inode: read_u64(&bytes, offset_of!(loop_info64, lo_inode)),
        lo_rdevice: read_u64(&bytes, offset_of!(loop_info64, lo_rdevice)),
        lo_offset: read_u64(&bytes, offset_of!(loop_info64, lo_offset)),
        lo_sizelimit: read_u64(&bytes, offset_of!(loop_info64, lo_sizelimit)),
        lo_number: read_u32(&bytes, offset_of!(loop_info64, lo_number)),
        lo_encrypt_type: read_u32(&bytes, offset_of!(loop_info64, lo_encrypt_type)),
        lo_encrypt_key_size: read_u32(&bytes, offset_of!(loop_info64, lo_encrypt_key_size)),
        lo_flags: read_u32(&bytes, offset_of!(loop_info64, lo_flags)),
        lo_file_name,
        lo_crypt_name,
        lo_encrypt_key,
        lo_init,
    }
}

fn loop_config_from_user_bytes(bytes: [u8; size_of::<loop_config>()]) -> loop_config {
    let info_start = offset_of!(loop_config, info);
    let info_end = info_start + size_of::<loop_info64>();
    loop_config {
        fd: read_u32(&bytes, offset_of!(loop_config, fd)),
        block_size: read_u32(&bytes, offset_of!(loop_config, block_size)),
        info: loop_info64_from_user_bytes(bytes[info_start..info_end].try_into().unwrap()),
        __reserved: [0; 8],
    }
}

fn loop_info_to_user_bytes(value: loop_info) -> [u8; size_of::<loop_info>()] {
    let mut bytes = [0u8; size_of::<loop_info>()];
    put_u32(
        &mut bytes,
        offset_of!(loop_info, lo_number),
        value.lo_number as u32,
    );
    put_old_dev(
        &mut bytes,
        offset_of!(loop_info, lo_device),
        value.lo_device,
        size_of_val(&value.lo_device),
    );
    put_old_dev(
        &mut bytes,
        offset_of!(loop_info, lo_inode),
        value.lo_inode,
        size_of_val(&value.lo_inode),
    );
    put_old_dev(
        &mut bytes,
        offset_of!(loop_info, lo_rdevice),
        value.lo_rdevice,
        size_of_val(&value.lo_rdevice),
    );
    put_u32(
        &mut bytes,
        offset_of!(loop_info, lo_offset),
        value.lo_offset as u32,
    );
    put_u32(
        &mut bytes,
        offset_of!(loop_info, lo_encrypt_type),
        value.lo_encrypt_type as u32,
    );
    put_u32(
        &mut bytes,
        offset_of!(loop_info, lo_encrypt_key_size),
        value.lo_encrypt_key_size as u32,
    );
    put_u32(
        &mut bytes,
        offset_of!(loop_info, lo_flags),
        value.lo_flags as u32,
    );
    for (index, field) in value.lo_name.into_iter().enumerate() {
        bytes[offset_of!(loop_info, lo_name) + index] = field as u8;
    }
    bytes[offset_of!(loop_info, lo_encrypt_key)..]
        .iter_mut()
        .zip(value.lo_encrypt_key)
        .for_each(|(dst, src)| *dst = src);
    for (index, field) in value.lo_init.into_iter().enumerate() {
        let width = size_of_val(&field);
        put_old_dev(
            &mut bytes,
            offset_of!(loop_info, lo_init) + index * width,
            field,
            width,
        );
    }
    // `reserved` is an ABI hole, so it remains explicitly zeroed.
    bytes
}

fn loop_info64_to_user_bytes(value: loop_info64) -> [u8; size_of::<loop_info64>()] {
    let mut bytes = [0u8; size_of::<loop_info64>()];
    put_u64(
        &mut bytes,
        offset_of!(loop_info64, lo_device),
        value.lo_device,
    );
    put_u64(
        &mut bytes,
        offset_of!(loop_info64, lo_inode),
        value.lo_inode,
    );
    put_u64(
        &mut bytes,
        offset_of!(loop_info64, lo_rdevice),
        value.lo_rdevice,
    );
    put_u64(
        &mut bytes,
        offset_of!(loop_info64, lo_offset),
        value.lo_offset,
    );
    put_u64(
        &mut bytes,
        offset_of!(loop_info64, lo_sizelimit),
        value.lo_sizelimit,
    );
    put_u32(
        &mut bytes,
        offset_of!(loop_info64, lo_number),
        value.lo_number,
    );
    put_u32(
        &mut bytes,
        offset_of!(loop_info64, lo_encrypt_type),
        value.lo_encrypt_type,
    );
    put_u32(
        &mut bytes,
        offset_of!(loop_info64, lo_encrypt_key_size),
        value.lo_encrypt_key_size,
    );
    put_u32(
        &mut bytes,
        offset_of!(loop_info64, lo_flags),
        value.lo_flags,
    );
    bytes[offset_of!(loop_info64, lo_file_name)..]
        .iter_mut()
        .zip(value.lo_file_name)
        .for_each(|(dst, src)| *dst = src);
    bytes[offset_of!(loop_info64, lo_crypt_name)..]
        .iter_mut()
        .zip(value.lo_crypt_name)
        .for_each(|(dst, src)| *dst = src);
    bytes[offset_of!(loop_info64, lo_encrypt_key)..]
        .iter_mut()
        .zip(value.lo_encrypt_key)
        .for_each(|(dst, src)| *dst = src);
    for (index, field) in value.lo_init.into_iter().enumerate() {
        put_u64(
            &mut bytes,
            offset_of!(loop_info64, lo_init) + index * size_of::<u64>(),
            field,
        );
    }
    bytes
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
                .map_or_else(Vec::new, |backing| backing.path.as_bytes().to_vec()),
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
            copy_cstr_to_c_char(backing.path.as_bytes(), &mut res.lo_name);
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
            copy_cstr_to_u8(backing.path.as_bytes(), &mut res.lo_file_name);
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

/// Identifies operations that can change a loop device globally rather than
/// merely report its current state.  Keep this classification adjacent to the
/// dispatch so new ioctls cannot accidentally inherit node-mode-only access.
const fn loop_ioctl_requires_admin(cmd: u32) -> bool {
    matches!(
        cmd,
        LOOP_SET_FD
            | LOOP_CONFIGURE
            | LOOP_CHANGE_FD
            | LOOP_CLR_FD
            | LOOP_SET_STATUS
            | LOOP_SET_STATUS64
            | LOOP_SET_BLOCK_SIZE
            | LOOP_SET_CAPACITY
            | LOOP_SET_DIRECT_IO
            | BLKROSET
    )
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

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            LOOP_CTL_GET_FREE => loop_control_get_free(),
            LOOP_CTL_ADD => {
                super::require_loop_admin(context.caller_cred())?;
                loop_control_add(arg)
            }
            LOOP_CTL_REMOVE => {
                super::require_loop_admin(context.caller_cred())?;
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
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
            | NodeFlags::NO_SEEK
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

    fn read_backing_fd(context: &IoctlContext, fd: i32) -> AxResult<(LoopBacking, bool)> {
        if fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        let f = context.get_file_like(fd)?;
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
            .location()
            .entry()
            .as_file()?
            .write_at(&buf[..limit], state.offset + offset)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> VfsResult<usize> {
        // DeviceOps is shared by every descriptor for this device and does
        // not receive the descriptor's open mode.  Never treat a successful
        // open as authority to alter its globally visible backing or state.
        // The context is captured at syscall dispatch, so this also avoids
        // resolving a potentially different credential or fd table later.
        if loop_ioctl_requires_admin(cmd) {
            super::require_loop_admin(context.caller_cred())?;
        }
        match cmd {
            LOOP_SET_FD => {
                let (backing, read_only) = Self::read_backing_fd(context, arg as i32)?;
                self.state().lock().bind(backing, read_only)?;
            }
            LOOP_CLR_FD => {
                self.state().lock().clear()?;
            }
            LOOP_GET_STATUS => {
                let info = self.state().lock().get_info(self.number, self.dev_id)?;
                let bytes = loop_info_to_user_bytes(info);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(map_usercopy_error)?;
            }
            LOOP_GET_STATUS64 => {
                let info = self.state().lock().get_info64(self.number, self.dev_id)?;
                let bytes = loop_info64_to_user_bytes(info);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(map_usercopy_error)?;
            }
            LOOP_SET_STATUS => {
                let info = loop_info_from_user_bytes(read_user_bytes(context, arg)?);
                self.state().lock().set_info(info)?;
            }
            LOOP_SET_STATUS64 => {
                let info = loop_info64_from_user_bytes(read_user_bytes(context, arg)?);
                self.state().lock().set_info64(info)?;
            }
            LOOP_CHANGE_FD => {
                let (backing, _) = Self::read_backing_fd(context, arg as i32)?;
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
                let config = loop_config_from_user_bytes(read_user_bytes(context, arg)?);
                let fd = i32::try_from(config.fd).map_err(|_| AxError::BadFileDescriptor)?;
                let (backing, backing_read_only) = Self::read_backing_fd(context, fd)?;
                self.state().lock().configure(
                    backing,
                    backing_read_only,
                    config.info,
                    config.block_size,
                )?;
            }
            // TODO: the following should apply to any block devices
            BLKGETSIZE | BLKGETSIZE64 | BLKSSZGET | BLKROGET => {
                let output = {
                    let state = self.state().lock();
                    snapshot_block_output(&state, cmd)?
                };
                copy_block_output(context.user_memory(), arg, output)?;
            }
            BLKROSET => {
                let ro = context
                    .user_memory()
                    .read_value(arg as *const u32)
                    .map_err(map_usercopy_error)?;
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

fn copy_cstr_to_u8(src: &[u8], dst: &mut [u8]) {
    if dst.is_empty() {
        return;
    }
    let len = src.len().min(dst.len() - 1);
    dst[..len].copy_from_slice(&src[..len]);
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

fn copy_cstr_to_c_char<T: LoopChar>(src: &[u8], dst: &mut [T]) {
    if dst.is_empty() {
        return;
    }
    let len = src.len().min(dst.len() - 1);
    for (target, byte) in dst[..len].iter_mut().zip(src.iter().copied()) {
        *target = T::from_byte(byte);
    }
    dst[len] = T::from_byte(0);
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::*;

    #[test]
    fn scalar_outputs_are_snapshotted_before_usercopy() {
        let state = Mutex::new(LoopState::default());
        let output = {
            let state = state.lock();
            snapshot_block_output(&state, BLKSSZGET).unwrap()
        };
        assert!(matches!(output, LoopBlockOutput::U32(DEFAULT_BLOCK_SIZE)));

        let state = LoopState {
            flags: FLAG_READ_ONLY,
            ..LoopState::default()
        };
        assert!(matches!(
            snapshot_block_output(&state, BLKROGET),
            Ok(LoopBlockOutput::U32(1))
        ));
        assert!(snapshot_block_output(&state, BLKGETSIZE).is_err());
    }

    #[test]
    fn invisible_loop_rejects_every_scalar_output() {
        let state = LoopState {
            visible: false,
            ..LoopState::default()
        };
        for cmd in [BLKGETSIZE, BLKGETSIZE64, BLKSSZGET, BLKROGET] {
            assert!(snapshot_block_output(&state, cmd).is_err());
        }
    }

    #[test]
    fn loop_info_codec_zeroes_reserved_bytes() {
        let decoded = loop_info_from_user_bytes([0xa5; size_of::<loop_info>()]);
        let encoded = loop_info_to_user_bytes(decoded);
        let reserved = offset_of!(loop_info, reserved);

        assert!(
            encoded[reserved..reserved + 4]
                .iter()
                .all(|&byte| byte == 0)
        );
    }

    #[test]
    fn scalar_copyout_fault_leaves_state_unchanged() {
        let _test_context = crate::test_support::scheduler_test_context();
        let device = LoopDevice::new(0, DeviceId::new(7, 0));
        let block_size = device.state().lock().block_size;
        let output = {
            let state = device.state().lock();
            snapshot_block_output(&state, BLKSSZGET).unwrap()
        };
        let user_memory = UserMemoryCapability::new(Arc::new(Mutex::new(
            crate::mm::new_user_aspace_empty().unwrap(),
        )));

        assert!(copy_block_output(&user_memory, usize::MAX, output).is_err());
        assert_eq!(device.state().lock().block_size, block_size);
    }

    #[test]
    fn global_loop_mutations_are_explicitly_classified() {
        for command in [
            LOOP_SET_FD,
            LOOP_CONFIGURE,
            LOOP_CHANGE_FD,
            LOOP_CLR_FD,
            LOOP_SET_STATUS,
            LOOP_SET_STATUS64,
            LOOP_SET_BLOCK_SIZE,
            LOOP_SET_CAPACITY,
            LOOP_SET_DIRECT_IO,
            BLKROSET,
        ] {
            assert!(loop_ioctl_requires_admin(command));
        }
        for command in [
            LOOP_GET_STATUS,
            LOOP_GET_STATUS64,
            BLKGETSIZE,
            BLKGETSIZE64,
            BLKSSZGET,
            BLKROGET,
        ] {
            assert!(!loop_ioctl_requires_admin(command));
        }
    }
}
