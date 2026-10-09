//! Linux i2c-dev character device adapter.
use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{FsPathBuf, Location, NodeFlags, VfsError, VfsResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::Mutex;
use tk_i2c::{IicError, IicMessage, i2cdev::*};

use crate::{
    file::{FileLike, IoDst, IoSrc, IoctlContext, Kstat},
    pseudofs::{DeviceOpen, DeviceOps},
};

pub const DEVICE_MAJOR: u32 = 89;
const MAX_RDWR_TOTAL: usize = 64 * 1024;
const I2C_M_TEN: u16 = 0x0010;

#[derive(Clone, Copy, Default)]
struct Config {
    address: Option<u8>,
    retries: u32,
    timeout: u32,
    pec: bool,
}

pub struct I2cDevice {
    pub bus: usize,
}

struct I2cFile {
    location: Location,
    bus: usize,
    config: Mutex<Config>,
    nonblocking: AtomicBool,
}

fn read_u32(cx: &IoctlContext, address: usize) -> AxResult<u32> {
    let mut bytes = [0; 4];
    cx.user_memory()
        .read_into(address as *const u8, &mut bytes)
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(u32::from_ne_bytes(bytes))
}

fn write_u32(cx: &IoctlContext, address: usize, value: u32) -> AxResult<usize> {
    cx.user_memory()
        .write_bytes(address, &value.to_ne_bytes())
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(0)
}

fn write_u64(cx: &IoctlContext, address: usize, value: u64) -> AxResult<usize> {
    cx.user_memory()
        .write_bytes(address, &value.to_ne_bytes())
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(0)
}

fn read_user(cx: &IoctlContext, address: u64, buffer: &mut [u8]) -> AxResult<()> {
    let address = usize::try_from(address).map_err(|_| AxError::InvalidInput)?;
    cx.user_memory()
        .read_into(address as *const u8, buffer)
        .map_err(crate::mm::map_usercopy_error)
}
fn write_user(cx: &IoctlContext, address: u64, buffer: &[u8]) -> AxResult<()> {
    let address = usize::try_from(address).map_err(|_| AxError::InvalidInput)?;
    cx.user_memory()
        .write_bytes(address, buffer)
        .map_err(crate::mm::map_usercopy_error)
}

fn iic_error(error: IicError) -> AxError {
    match error {
        IicError::HardwareUnavailable => AxError::NoSuchDevice,
        IicError::NotSupported => AxError::OperationNotSupported,
        IicError::Invalid => AxError::InvalidInput,
        IicError::BusBusy => AxError::ResourceBusy,
        IicError::Timeout => AxError::TimedOut,
        _ => AxError::Io,
    }
}

impl I2cFile {
    fn rdwr(&self, cx: &IoctlContext, arg: usize) -> AxResult<usize> {
        let mut header = [0u8; 16];
        cx.user_memory()
            .read_into(arg as *const u8, &mut header)
            .map_err(crate::mm::map_usercopy_error)?;
        let descriptors = u64::from_ne_bytes(header[..8].try_into().unwrap());
        let count = u32::from_ne_bytes(header[8..12].try_into().unwrap()) as usize;
        if count == 0 || count > I2C_RDWR_IOCTL_MAX_MSGS {
            return Err(AxError::InvalidInput);
        }
        let descriptor_bytes = count
            .checked_mul(core::mem::size_of::<I2cMsg>())
            .ok_or(AxError::InvalidInput)?;
        let mut raw = Vec::new();
        raw.try_reserve_exact(descriptor_bytes)
            .map_err(|_| AxError::NoMemory)?;
        raw.resize(descriptor_bytes, 0);
        read_user(cx, descriptors, &mut raw)?;
        let mut buffers: Vec<(I2cMsg, Vec<u8>)> = Vec::new();
        buffers
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        let mut total = 0usize;
        for bytes in raw.chunks_exact(16) {
            let message = I2cMsg {
                address: u16::from_ne_bytes(bytes[0..2].try_into().unwrap()),
                flags: u16::from_ne_bytes(bytes[2..4].try_into().unwrap()),
                length: u16::from_ne_bytes(bytes[4..6].try_into().unwrap()),
                buffer: u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            };
            let len = usize::from(message.length);
            total = total.checked_add(len).ok_or(AxError::InvalidInput)?;
            if total > MAX_RDWR_TOTAL || message.address > 0x7f {
                return Err(AxError::InvalidInput);
            }
            if len == 0
                || message.flags & I2C_M_TEN != 0
                || message.flags & !(I2C_M_RD | I2C_M_NOSTART | I2C_M_STOP) != 0
            {
                return Err(AxError::OperationNotSupported);
            }
            let mut data = Vec::new();
            data.try_reserve_exact(len).map_err(|_| AxError::NoMemory)?;
            data.resize(len, 0);
            if message.flags & I2C_M_RD == 0 {
                read_user(cx, message.buffer, &mut data)?;
            }
            buffers.push((message, data));
        }
        let mut messages: Vec<IicMessage<'_>> = Vec::new();
        messages
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        let buffer_count = buffers.len();
        for (i, (descriptor, buffer)) in buffers.iter_mut().enumerate() {
            let mut flags = if descriptor.flags & I2C_M_RD != 0 {
                tk_i2c::ig4::IIC_M_RD
            } else {
                0
            };
            if descriptor.flags & I2C_M_NOSTART != 0 {
                flags |= tk_i2c::ig4::IIC_M_NOSTART;
            }
            if i + 1 < buffer_count && descriptor.flags & I2C_M_STOP == 0 {
                flags |= tk_i2c::ig4::IIC_M_NOSTOP;
            }
            messages.push(IicMessage {
                slave: descriptor.address << 1,
                flags,
                buf: buffer,
            });
        }
        axdriver::i2c::transfer(self.bus, &mut messages).map_err(iic_error)?;
        drop(messages);
        for (descriptor, buffer) in &buffers {
            if descriptor.flags & I2C_M_RD != 0 {
                write_user(cx, descriptor.buffer, buffer)?;
            }
        }
        Ok(count)
    }

    fn smbus(&self, cx: &IoctlContext, arg: usize) -> AxResult<usize> {
        let mut raw = [0u8; 16];
        cx.user_memory()
            .read_into(arg as *const u8, &mut raw)
            .map_err(crate::mm::map_usercopy_error)?;
        let read = raw[0] != 0;
        let command = raw[1];
        let size = u32::from_ne_bytes(raw[4..8].try_into().unwrap());
        let data_ptr = u64::from_ne_bytes(raw[8..16].try_into().unwrap());
        let config = *self.config.lock();
        let address = config.address.ok_or(AxError::InvalidInput)?;
        if config.pec { return Err(AxError::OperationNotSupported); }
        if size == I2C_SMBUS_QUICK {
            return Err(AxError::OperationNotSupported);
        }
        let mut union = [0u8; 34];
        if !read {
            read_user(cx, data_ptr, &mut union)?;
        }
        let mut first = Vec::new();
        let mut second = Vec::new();
        let mut flags1 = 0;
        let flags2 = tk_i2c::ig4::IIC_M_RD;
        match size {
            I2C_SMBUS_BYTE if read => second.resize(1, 0),
            I2C_SMBUS_BYTE if !read => {
                first.push(command);
            }
            I2C_SMBUS_BYTE_DATA => {
                first.push(command);
                if read {
                    second.resize(1, 0);
                } else {
                    first.push(union[0]);
                }
            }
            I2C_SMBUS_WORD_DATA => {
                first.push(command);
                if read {
                    second.resize(2, 0);
                } else {
                    first.extend_from_slice(&union[..2]);
                }
            }
            I2C_SMBUS_I2C_BLOCK_DATA if read => {
                let length = usize::from(union[0]);
                if length == 0 || length > I2C_SMBUS_BLOCK_MAX {
                    return Err(AxError::InvalidInput);
                }
                first.push(command);
                second.resize(length, 0);
            }
            I2C_SMBUS_I2C_BLOCK_DATA => {
                let length = usize::from(union[0]);
                if length == 0 || length > I2C_SMBUS_BLOCK_MAX {
                    return Err(AxError::InvalidInput);
                }
                first.push(command);
                first.push(length as u8);
                first.extend_from_slice(&union[1..1 + length]);
                flags1 = 0;
            }
            _ => return Err(AxError::OperationNotSupported),
        }
        let mut messages = Vec::new();
        messages
            .try_reserve_exact(if second.is_empty() { 1 } else { 2 })
            .map_err(|_| AxError::NoMemory)?;
        if !first.is_empty() {
            if !second.is_empty() {
                flags1 |= tk_i2c::ig4::IIC_M_NOSTOP;
            }
            messages.push(IicMessage {
                slave: u16::from(address) << 1,
                flags: flags1,
                buf: &mut first,
            });
        }
        if !second.is_empty() {
            messages.push(IicMessage {
                slave: u16::from(address) << 1,
                flags: flags2,
                buf: &mut second,
            });
        }
        if messages.is_empty() {
            return Err(AxError::OperationNotSupported);
        }
        axdriver::i2c::transfer(self.bus, &mut messages).map_err(iic_error)?;
        drop(messages);
        if read {
            match size {
                I2C_SMBUS_BYTE => union[0] = second[0],
                I2C_SMBUS_BYTE_DATA => union[0] = second[0],
                I2C_SMBUS_WORD_DATA => union[..2].copy_from_slice(&second),
                I2C_SMBUS_I2C_BLOCK_DATA => {
                    union[0] = second.len() as u8;
                    union[1..1 + second.len()].copy_from_slice(&second);
                }
                _ => {}
            }
            write_user(cx, data_ptr, &union)?;
        }
        Ok(0)
    }

    fn handle_ioctl(&self, cx: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            I2C_FUNCS => write_u64(cx, arg, functions()),
            I2C_RDWR => self.rdwr(cx, arg),
            I2C_SMBUS => self.smbus(cx, arg),
            I2C_SLAVE | I2C_SLAVE_FORCE => {
                if cmd == I2C_SLAVE_FORCE
                    && !cx
                        .caller_cred()
                        .has_effective_capability(linux_raw_sys::general::CAP_SYS_RAWIO)
                {
                    return Err(AxError::PermissionDenied);
                }
                let value = read_u32(cx, arg)?;
                if value > 0x7f {
                    return Err(AxError::InvalidInput);
                }
                self.config.lock().address = Some(value as u8);
                Ok(0)
            }
            I2C_TENBIT => {
                if read_u32(cx, arg)? == 0 {
                    Ok(0)
                } else {
                    Err(AxError::OperationNotSupported)
                }
            }
            I2C_RETRIES => {
                self.config.lock().retries = read_u32(cx, arg)?;
                Ok(0)
            }
            I2C_TIMEOUT => {
                self.config.lock().timeout = read_u32(cx, arg)?;
                Ok(0)
            }
            I2C_PEC => {
                let value = read_u32(cx, arg)?;
                if value == 0 {
                    self.config.lock().pec = false;
                    Ok(0)
                } else {
                    Err(AxError::OperationNotSupported)
                }
            }
            _ => Err(AxError::NotATty),
        }
    }
}

impl FileLike for I2cFile {
    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.nonblocking.store(nonblocking, Ordering::Release);
        Ok(())
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn stat(&self) -> AxResult<Kstat> {
        crate::file::fs::location_to_kstat(&self.location)
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Owned(FsPathBuf::from_vec(
            alloc::format!("/dev/i2c-{}", self.bus).into_bytes(),
        )))
    }
    fn vfs_location(&self) -> Option<&Location> {
        Some(&self.location)
    }
    fn ioctl(&self, cx: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        self.handle_ioctl(cx, cmd, arg)
    }
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let address = self.config.lock().address.ok_or(AxError::InvalidInput)?;
        let length = dst.remaining_mut().min(u16::MAX as usize);
        if length == 0 { return Ok(0); }
        let mut data = Vec::new();
        data.try_reserve_exact(length).map_err(|_| AxError::NoMemory)?;
        data.resize(length, 0);
        let mut messages = [IicMessage { slave: u16::from(address) << 1, flags: tk_i2c::ig4::IIC_M_RD, buf: &mut data }];
        axdriver::i2c::transfer(self.bus, &mut messages).map_err(iic_error)?;
        dst.write(&data)
    }
    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let address = self.config.lock().address.ok_or(AxError::InvalidInput)?;
        let length = src.remaining().min(u16::MAX as usize);
        if length == 0 { return Ok(0); }
        let mut data = Vec::new();
        data.try_reserve_exact(length).map_err(|_| AxError::NoMemory)?;
        data.resize(length, 0);
        src.read_exact(&mut data)?;
        let mut messages = [IicMessage { slave: u16::from(address) << 1, flags: 0, buf: &mut data }];
        axdriver::i2c::transfer(self.bus, &mut messages).map_err(iic_error)?;
        Ok(length)
    }
}

impl Pollable for I2cFile {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
    fn register<'a>(
        &'a self,
        _: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}

impl DeviceOps for I2cDevice {
    fn open_description(&self, location: &Location, _flags: u32) -> VfsResult<Option<DeviceOpen>> {
        if self.bus >= axdriver::i2c::bus_count() {
            return Err(VfsError::NoSuchDevice);
        }
        let file = Arc::try_new(I2cFile {
            location: location.clone(),
            bus: self.bus,
            config: Mutex::new(Config::default()),
            nonblocking: AtomicBool::new(false),
        })
        .map_err(|_| VfsError::NoMemory)?;
        Ok(Some(DeviceOpen::new(file, None)))
    }
    fn read_at(&self, _: &mut [u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM | NodeFlags::NO_SEEK
    }
}
