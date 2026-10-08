//! Linux AF_BLUETOOTH HCI socket shell for device discovery and monitor setup.
//! Protocol framing follows the Bluetooth Core and Linux UAPI; no GPL net stack
//! implementation is copied here.
use alloc::borrow::Cow;
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, Pollable};
use bytemuck::{Pod, Zeroable};
use linux_raw_sys::net::{SOCK_RAW, sockaddr};

use super::{FileLike, IoDst, IoSrc, IoctlContext, Kstat, PseudoInode, try_pseudo_inode_path};
use crate::mm::UserMemoryCapability;

pub const AF_BLUETOOTH: u32 = 31;
pub const BTPROTO_HCI: u32 = 1;
const HCIDEVUP: u32 = 0x4004_48c9;
const HCIDEVDOWN: u32 = 0x4004_48ca;
const HCIGETDEVLIST: u32 = 0x8004_48d2;
const HCIGETDEVINFO: u32 = 0x8004_48d3;
const HCI_DEV_NONE: u16 = 0xffff;
const HCI_CHANNEL_MONITOR: u16 = 2;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct SockaddrHci {
    family: u16,
    device: u16,
    channel: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_channel_binds_without_hardware_but_device_channels_fail() {
        let socket = HciSocket::new();
        assert!(
            socket
                .bind(SockaddrHci {
                    family: AF_BLUETOOTH as u16,
                    device: HCI_DEV_NONE,
                    channel: HCI_CHANNEL_MONITOR
                })
                .is_ok()
        );
        assert_eq!(
            socket.bind(SockaddrHci {
                family: AF_BLUETOOTH as u16,
                device: 0,
                channel: 0
            }),
            Err(LinuxError::ENODEV.into())
        );
        assert_eq!(
            HciSocket::validate_socket_type(SOCK_RAW as u32, BTPROTO_HCI),
            Ok(())
        );
    }
}

pub struct HciSocket {
    inode: PseudoInode,
    nonblocking: AtomicBool,
    bound: AtomicBool,
}

impl HciSocket {
    pub(crate) fn new() -> Self {
        Self {
            inode: PseudoInode::socket(),
            nonblocking: AtomicBool::new(false),
            bound: AtomicBool::new(false),
        }
    }

    pub(crate) fn validate_socket_type(ty: u32, protocol: u32) -> AxResult<()> {
        if ty != SOCK_RAW as u32 {
            return Err(LinuxError::ESOCKTNOSUPPORT.into());
        }
        if protocol != BTPROTO_HCI {
            return Err(LinuxError::EPROTONOSUPPORT.into());
        }
        Ok(())
    }

    pub(crate) fn bind(&self, address: SockaddrHci) -> AxResult<()> {
        if address.family as u32 != AF_BLUETOOTH {
            return Err(AxError::InvalidInput);
        }
        // HCI monitor is global and remains bindable without an adapter. The
        // HCI_DEV_NONE spelling is accepted as on Linux; raw/user need a device.
        if address.channel == HCI_CHANNEL_MONITOR {
            self.bound.store(true, Ordering::Release);
            return Ok(());
        }
        if address.device == HCI_DEV_NONE {
            return Err(AxError::InvalidInput);
        }
        Err(LinuxError::ENODEV.into())
    }

    pub(crate) fn read_from_user(
        &self,
        capability: &UserMemoryCapability,
        address: *const sockaddr,
        len: usize,
    ) -> AxResult<SockaddrHci> {
        if len < core::mem::size_of::<SockaddrHci>() {
            return Err(AxError::InvalidInput);
        }
        capability
            .read_value(address.cast())
            .map_err(crate::mm::map_usercopy_error)
    }
}

impl Pollable for HciSocket {
    fn poll(&self) -> IoEvents {
        IoEvents::WRITABLE
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

impl FileLike for HciSocket {
    fn read(&self, _dst: &mut IoDst) -> AxResult<usize> {
        Err(if self.nonblocking.load(Ordering::Acquire) {
            LinuxError::EAGAIN.into()
        } else {
            LinuxError::ENODEV.into()
        })
    }
    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        Err(LinuxError::ENODEV.into())
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            HCIGETDEVLIST => {
                context
                    .user_memory()
                    .write_value(arg as *mut u16, 0)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            HCIDEVUP | HCIDEVDOWN | HCIGETDEVINFO => Err(LinuxError::ENODEV.into()),
            _ => Err(AxError::NotATty),
        }
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        try_pseudo_inode_path("socket", self.inode.inode())
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
}

impl Default for HciSocket {
    fn default() -> Self {
        Self::new()
    }
}
