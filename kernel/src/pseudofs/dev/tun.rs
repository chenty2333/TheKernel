//! `/dev/net/tun` control and L3 TUN file endpoint.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    any::Any,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, NodeFlags, VfsError, VfsResult};
use axio::prelude::*;
use axnet::{TapHandle, TunHandle};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use linux_raw_sys::{
    general::CAP_NET_ADMIN,
    if_tun::{IFF_MULTI_QUEUE, IFF_NO_PI, IFF_TAP, IFF_TUN, IFF_VNET_HDR},
    ioctl::{TUNSETIFF, TUNSETOFFLOAD, TUNSETPERSIST, TUNSETVNETHDRSZ},
};
use spin::Mutex;

use crate::{
    file::{FileLike, IoDst, IoSrc, IoctlContext, Kstat, OfdIoStatus, anon_inode_stat},
    pseudofs::{DeviceOpen, DeviceOps},
    readiness::block_on_poll_io,
    task::NetworkNamespace,
};

const IFREQ_BYTES: usize = 40;
const IFNAMSIZ: usize = 16;
static NEXT_TUN_NAME: AtomicU32 = AtomicU32::new(0);

/// Per-open `/dev/net/tun` file.  An attachment is OFD-owned and remains
/// bound to the namespace selected at `TUNSETIFF`, even if another thread
/// later changes its current network namespace.
pub(crate) struct TunFile {
    attachment: Mutex<Option<TunAttachment>>,
    nonblocking: AtomicBool,
}

struct TunAttachment {
    handle: VirtualHandle,
    net_ns: Arc<NetworkNamespace>,
    ifindex: u32,
    no_pi: bool,
    tap: bool,
    multi_queue: bool,
    vnet_hdr_size: usize,
}

enum VirtualHandle {
    Tun(Arc<TunHandle>),
    Tap(Arc<TapHandle>),
}
impl VirtualHandle {
    fn read(&self, dst: &mut [u8]) -> AxResult<usize> {
        match self {
            Self::Tun(handle) => handle.try_read_packet(dst),
            Self::Tap(handle) => handle.try_read_frame(dst),
        }
    }
    fn write(&self, src: &[u8]) -> AxResult {
        match self {
            Self::Tun(handle) => handle.try_write_packet(src),
            Self::Tap(handle) => handle.try_write_frame(src),
        }
    }
    fn has_egress(&self) -> bool {
        match self {
            Self::Tun(handle) => handle.has_egress_packet(),
            Self::Tap(handle) => handle.has_egress_frame(),
        }
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        match self {
            Self::Tun(handle) => PollRegistration::single(handle.egress_ready(), context.waker()),
            Self::Tap(handle) => PollRegistration::single(handle.egress_ready(), context.waker()),
        }
    }
}

impl TunFile {
    fn attached(&self) -> AxResult<(VirtualHandle, Arc<NetworkNamespace>, bool, bool, usize)> {
        self.attachment
            .lock()
            .as_ref()
            .map(|attachment| {
                (
                    match &attachment.handle {
                        VirtualHandle::Tun(handle) => VirtualHandle::Tun(handle.clone()),
                        VirtualHandle::Tap(handle) => VirtualHandle::Tap(handle.clone()),
                    },
                    attachment.net_ns.clone(),
                    attachment.no_pi,
                    attachment.tap,
                    attachment.vnet_hdr_size,
                )
            })
            .ok_or(AxError::BadFileDescriptor)
    }

    fn set_iff(&self, context: &IoctlContext, arg: usize) -> AxResult {
        if !context
            .caller_cred()
            .has_effective_capability(CAP_NET_ADMIN)
        {
            return Err(AxError::PermissionDenied);
        }
        // An OFD owns precisely one queue. Keep this lock across allocation,
        // copyout, and publication: a concurrent ioctl cannot create an
        // invisible competing device, and a failed copyout is rolled back.
        let mut attachment = self.attachment.lock();
        if attachment.is_some() {
            return Err(AxError::ResourceBusy);
        }
        let mut raw_ifreq = [MaybeUninit::<u8>::uninit(); IFREQ_BYTES];
        context
            .user_memory()
            .read_bytes(arg, &mut raw_ifreq)
            .map_err(crate::mm::map_usercopy_error)?;
        // `read_bytes` completed the exact fixed-width ABI object above.
        let mut ifreq = raw_ifreq.map(|byte| unsafe { byte.assume_init() });
        let flags = u16::from_ne_bytes([ifreq[IFNAMSIZ], ifreq[IFNAMSIZ + 1]]) as u32;
        if flags & (IFF_TUN | IFF_TAP) == 0
            || flags & (IFF_TUN | IFF_TAP) == (IFF_TUN | IFF_TAP)
            || flags & !((IFF_TUN | IFF_TAP | IFF_NO_PI | IFF_MULTI_QUEUE | IFF_VNET_HDR) as u32)
                != 0
        {
            return Err(AxError::InvalidInput);
        }
        let name_end = ifreq[..IFNAMSIZ]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(IFNAMSIZ);
        let name = if name_end == 0 {
            alloc::format!("tun{}", NEXT_TUN_NAME.fetch_add(1, Ordering::Relaxed))
        } else {
            core::str::from_utf8(&ifreq[..name_end])
                .map_err(|_| AxError::InvalidInput)?
                .into()
        };
        let net_ns = context.caller_process().net_ns();
        let tap = flags & IFF_TAP != 0;
        let multi_queue = flags & IFF_MULTI_QUEUE != 0;
        let (ifindex, handle, attached_existing) = if multi_queue && name_end != 0 {
            if tap {
                match net_ns.stack().attach_tap(&name) {
                    Ok((index, handle)) => (index, VirtualHandle::Tap(handle), true),
                    Err(AxError::NoSuchDevice) => {
                        let (index, handle) =
                            net_ns.stack().create_tap(name.clone(), multi_queue)?;
                        (index, VirtualHandle::Tap(handle), false)
                    }
                    Err(error) => return Err(error),
                }
            } else {
                match net_ns.stack().attach_tun(&name) {
                    Ok((index, handle)) => (index, VirtualHandle::Tun(handle), true),
                    Err(AxError::NoSuchDevice) => {
                        let (index, handle) =
                            net_ns.stack().create_tun(name.clone(), multi_queue)?;
                        (index, VirtualHandle::Tun(handle), false)
                    }
                    Err(error) => return Err(error),
                }
            }
        } else if name_end != 0 {
            if tap {
                match net_ns.stack().reopen_persistent_tap(&name) {
                    Ok((index, handle)) => (index, VirtualHandle::Tap(handle), true),
                    Err(AxError::NoSuchDevice) => {
                        let (index, handle) =
                            net_ns.stack().create_tap(name.clone(), multi_queue)?;
                        (index, VirtualHandle::Tap(handle), false)
                    }
                    Err(error) => return Err(error),
                }
            } else {
                match net_ns.stack().reopen_persistent_tun(&name) {
                    Ok((index, handle)) => (index, VirtualHandle::Tun(handle), true),
                    Err(AxError::NoSuchDevice) => {
                        let (index, handle) =
                            net_ns.stack().create_tun(name.clone(), multi_queue)?;
                        (index, VirtualHandle::Tun(handle), false)
                    }
                    Err(error) => return Err(error),
                }
            }
        } else if tap {
            let (index, handle) = net_ns.stack().create_tap(name.clone(), multi_queue)?;
            (index, VirtualHandle::Tap(handle), false)
        } else {
            let (index, handle) = net_ns.stack().create_tun(name.clone(), multi_queue)?;
            (index, VirtualHandle::Tun(handle), false)
        };
        ifreq[..IFNAMSIZ].fill(0);
        ifreq[..name.len()].copy_from_slice(name.as_bytes());
        if let Err(error) = context
            .user_memory()
            .write_bytes(arg, &ifreq)
            .map_err(crate::mm::map_usercopy_error)
        {
            // `create_{tun,tap}` publishes only after queue construction; it
            // is nevertheless our responsibility to unpublish it if the ABI
            // copyout fault means this ioctl cannot commit.
            // An already-existing multiqueue device is not ours to roll
            // back; only a newly created interface is unpublished here.
            if attached_existing {
                let _ = net_ns.stack().detach_tun_queue(ifindex, tap);
            } else {
                let _ = net_ns.stack().remove_device(ifindex);
            }
            return Err(error);
        }
        *attachment = Some(TunAttachment {
            handle,
            net_ns,
            ifindex,
            no_pi: flags & IFF_NO_PI != 0,
            tap,
            multi_queue,
            vnet_hdr_size: if flags & IFF_VNET_HDR != 0 { 10 } else { 0 },
        });
        Ok(())
    }

    fn set_persist(&self, context: &IoctlContext, arg: usize) -> AxResult {
        if !context
            .caller_cred()
            .has_effective_capability(CAP_NET_ADMIN)
        {
            return Err(AxError::PermissionDenied);
        }
        let value = context
            .user_memory()
            .read_value(arg as *const i32)
            .map_err(crate::mm::map_usercopy_error)?;
        let attachment = self.attachment.lock();
        let attachment = attachment.as_ref().ok_or(AxError::BadFileDescriptor)?;
        attachment
            .net_ns
            .stack()
            .set_tun_persist(attachment.ifindex, attachment.tap, value != 0)
    }

    fn set_vnet_hdr_size(&self, context: &IoctlContext, arg: usize) -> AxResult {
        let size = context
            .user_memory()
            .read_value(arg as *const i32)
            .map_err(crate::mm::map_usercopy_error)?;
        if size != 10 {
            return Err(AxError::InvalidInput);
        }
        let mut attachment = self.attachment.lock();
        let attachment = attachment.as_mut().ok_or(AxError::BadFileDescriptor)?;
        if attachment.vnet_hdr_size == 0 {
            return Err(AxError::InvalidInput);
        }
        attachment.vnet_hdr_size = size as usize;
        Ok(())
    }

    fn set_offload(&self, context: &IoctlContext, arg: usize) -> AxResult {
        let flags = context
            .user_memory()
            .read_value(arg as *const u32)
            .map_err(crate::mm::map_usercopy_error)?;
        // The router transmits fully formed packets.  It has no GSO/CSUM
        // completion engine, so only the exact no-offload setting is real.
        if flags == 0 {
            Ok(())
        } else {
            Err(AxError::OperationNotSupported)
        }
    }

    /// Reads one TUN/TAP frame with a request-local nonblocking override.
    /// RWF_NOWAIT uses this without changing the shared OFD state.
    pub(crate) fn read_with_nonblocking(
        &self,
        dst: &mut IoDst,
        nonblocking: bool,
    ) -> AxResult<usize> {
        if dst.remaining_mut() == 0 {
            return Ok(0);
        }
        let (handle, net_ns, no_pi, tap, vnet_hdr_size) = self.attached()?;
        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            let header_size = usize::from(!no_pi) * 4 + vnet_hdr_size;
            if dst.remaining_mut() < header_size {
                return Err(AxError::OutOfRange);
            }
            let mut packet = Vec::new();
            let capacity = dst.remaining_mut() - header_size;
            packet
                .try_reserve_exact(capacity)
                .map_err(|_| AxError::NoMemory)?;
            packet.resize(capacity, 0);
            let len = handle.read(&mut packet)?;
            // Router egress has already traversed LOCAL_OUT/POSTROUTING
            // before the device queues this frame.  Replaying the hook here
            // would run a netfilter BPF program twice and make NAT state
            // diverge from the packet actually placed on the wire.
            let _ = net_ns;
            if !no_pi {
                let protocol: u16 = if tap {
                    if len < 14 {
                        return Err(AxError::InvalidInput);
                    }
                    u16::from_be_bytes([packet[12], packet[13]])
                } else {
                    match packet.first().map(|first| first >> 4) {
                        Some(4) => 0x0800,
                        Some(6) => 0x86dd,
                        _ => return Err(AxError::InvalidInput),
                    }
                };
                let pi = [0, 0, (protocol >> 8) as u8, protocol as u8];
                dst.write(&pi)?;
            }
            if vnet_hdr_size != 0 {
                dst.write(&[0; 10])?;
            }
            let copied = dst.write(&packet[..len])?;
            Ok(copied + header_size)
        })
    }
}

impl Drop for TunFile {
    fn drop(&mut self) {
        if let Some(attachment) = self.attachment.get_mut().take() {
            let _ = match &attachment.handle {
                VirtualHandle::Tun(handle) => attachment
                    .net_ns
                    .stack()
                    .detach_tun_queue_handle(attachment.ifindex, handle),
                VirtualHandle::Tap(handle) => attachment
                    .net_ns
                    .stack()
                    .detach_tap_queue_handle(attachment.ifindex, handle),
            };
        }
    }
}

impl FileLike for TunFile {
    // TUN/TAP exposes ioctl plus packet read/write; it has no Linux URING_CMD
    // provider command ABI to advertise.
    fn uring_cmd_manifest(&self) -> &'static [crate::file::UringCmdManifest] {
        &[]
    }
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.read_with_nonblocking(dst, self.nonblocking.load(Ordering::Acquire))
    }
    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let (handle, net_ns, no_pi, tap, vnet_hdr_size) = self.attached()?;
        let len = src.remaining();
        let header_size = usize::from(!no_pi) * 4 + vnet_hdr_size;
        if len < header_size {
            return Err(AxError::InvalidInput);
        }
        let mut packet = Vec::new();
        packet
            .try_reserve_exact(len)
            .map_err(|_| AxError::NoMemory)?;
        packet.resize(len, 0);
        src.read_exact(&mut packet)?;
        let (pi_protocol, packet) = if no_pi {
            (None, packet.as_slice())
        } else {
            (
                Some(u16::from_be_bytes([packet[2], packet[3]])),
                &packet[4..],
            )
        };
        let packet = if vnet_hdr_size != 0 {
            // We support the base virtio-net header and require no offload
            // request.  Accepting GSO here would silently bypass checksum
            // and segmentation work which this device does not implement.
            if packet[0] != 0 || packet[1] != 0 || packet[2..10].iter().any(|byte| *byte != 0) {
                return Err(AxError::OperationNotSupported);
            }
            &packet[vnet_hdr_size..]
        } else {
            packet
        };
        let data = if no_pi {
            packet
        } else {
            let protocol = pi_protocol.expect("PI protocol is present when IFF_NO_PI is clear");
            let data = packet;
            if tap {
                if data.len() < 14 || protocol != u16::from_be_bytes([data[12], data[13]]) {
                    return Err(AxError::InvalidInput);
                }
            } else if !matches!(
                (protocol, data.first().map(|first| first >> 4)),
                (0x0800, Some(4)) | (0x86dd, Some(6))
            ) {
                return Err(AxError::InvalidInput);
            }
            data
        };
        // TUN/TAP injection is consumed by the namespace router.  It owns
        // fragment reassembly and performs PREROUTING/INPUT exactly once
        // after a complete datagram exists; direct filtering here would let
        // a requested BPF_F_NETFILTER_IP_DEFRAG link observe partial bytes.
        let _ = net_ns;
        handle.write(data)?;
        Ok(len)
    }
    fn write_with_operation_status(
        &self,
        _status: OfdIoStatus,
        src: &mut IoSrc,
    ) -> AxResult<usize> {
        // Injection has no synchronous readiness wait; it either consumes the
        // packet or reports its ordinary immediate validation/resource error.
        self.write(src)
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(b"anon_inode:[tun]")))
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            TUNSETIFF => {
                self.set_iff(context, arg)?;
                Ok(0)
            }
            TUNSETPERSIST => {
                self.set_persist(context, arg)?;
                Ok(0)
            }
            TUNSETVNETHDRSZ => {
                self.set_vnet_hdr_size(context, arg)?;
                Ok(0)
            }
            TUNSETOFFLOAD => {
                self.set_offload(context, arg)?;
                Ok(0)
            }
            _ => Err(AxError::InvalidInput),
        }
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
}

impl Pollable for TunFile {
    fn poll(&self) -> IoEvents {
        let Ok((handle, ..)) = self.attached() else {
            return IoEvents::ERROR;
        };
        let mut events = IoEvents::WRITABLE;
        events.set(IoEvents::READABLE, handle.has_egress());
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if !events.intersects(IoEvents::READABLE | IoEvents::READ_NORMAL) {
            return PollRegistration::empty();
        }
        let attachment = self.attachment.lock();
        let attachment = attachment
            .as_ref()
            .ok_or(PollRegistrationError::InvalidState)?;
        // A successful TUNSETIFF is immutable for this OFD: subsequent
        // TUNSETIFF calls fail while `attachment` is populated, and dropping
        // this file requires exclusive ownership.  The Pollable borrow of
        // `self` therefore keeps the selected handle live for `'a` after the
        // short mutex guard is released, while the registration itself holds
        // the actual queue subscription.
        let handle: &'a VirtualHandle = unsafe { &*core::ptr::from_ref(&attachment.handle) };
        handle.register(context)
    }
}

pub(crate) struct TunDevice;

impl DeviceOps for TunDevice {
    fn open_description(&self, _location: &Location, _flags: u32) -> VfsResult<Option<DeviceOpen>> {
        let file: Arc<dyn FileLike> = Arc::try_new(TunFile {
            attachment: Mutex::new(None),
            nonblocking: AtomicBool::new(false),
        })
        .map_err(|_| VfsError::NoMemory)?;
        Ok(Some(DeviceOpen::new(file, None)))
    }
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_SEEK
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
    }
}
