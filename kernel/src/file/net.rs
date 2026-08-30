use alloc::{borrow::Cow, sync::Arc};
use core::{ffi::c_int, ops::Deref, task::Context};

use axerrno::{AxError, AxResult};
use axnet::{
    RecvOptions, SendOptions, Socket as SocketInner, SocketOps, SocketTransferDirection,
    options::{Configurable, SetSocketOption},
};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use linux_raw_sys::{
    general::O_PATH,
    ioctl::{FIONREAD, TIOCINQ},
};

use super::{File, FileHandle, FileLike, IoctlContext, Kstat, PseudoInode, try_pseudo_inode_path};
#[cfg(feature = "bpf")]
use crate::bpf::{prog::BpfProgram, vm::BpfVm};
use crate::{
    file::{IoDst, IoSrc, get_file_like, get_typed_file, packet::socket_ifreq_ioctl},
    task::{
        Cred, NetworkNamespace,
        security::{AbstractUnixSocketLabelReservation, LandlockDomain},
    },
};

#[cfg(feature = "bpf")]
struct AttachedSocketFilter {
    prog: Arc<BpfProgram>,
}

#[cfg(feature = "bpf")]
impl axnet::SocketFilter for AttachedSocketFilter {
    fn filter(&self, data: &mut [u8]) -> AxResult<usize> {
        let mut vm = BpfVm::with_aux_budget(
            &self.prog.insns,
            &self.prog.decoded_insns,
            &self.prog.maps,
            u64::MAX,
        );
        let ret = vm.execute(data)? as usize;
        Ok(ret.min(data.len()))
    }
}

pub struct Socket {
    pub inner: SocketInner,
    net_ns: Arc<NetworkNamespace>,
    inode: PseudoInode,
    abstract_landlock_label: Mutex<Option<AbstractUnixSocketLabelReservation>>,
    creator_security: Option<(Arc<Cred>, LandlockDomain)>,
}

impl Deref for Socket {
    type Target = SocketInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Socket {
    pub(crate) fn new(inner: SocketInner, net_ns: Arc<NetworkNamespace>) -> Self {
        Self {
            inner,
            net_ns,
            inode: PseudoInode::socket(),
            abstract_landlock_label: Mutex::new(None),
            creator_security: None,
        }
    }

    pub(crate) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        &self.net_ns
    }

    pub(crate) fn install_abstract_landlock_label(
        &self,
        label: AbstractUnixSocketLabelReservation,
    ) {
        *self.abstract_landlock_label.lock() = Some(label);
    }

    /// Freeze the task security state at socket creation.  A later holder of
    /// the fd must not be able to relabel an abstract endpoint on bind.
    pub(crate) fn capture_creator_security(&mut self, cred: Arc<Cred>, domain: LandlockDomain) {
        self.creator_security = Some((cred, domain));
    }

    pub(crate) fn creator_landlock_domain(&self) -> AxResult<LandlockDomain> {
        self.creator_security
            .as_ref()
            .map(|(_, domain)| domain.clone())
            .ok_or(AxError::BadState)
    }

    pub(crate) fn read_with_nonblocking(
        &self,
        dst: &mut IoDst,
        nonblocking: bool,
    ) -> AxResult<usize> {
        self.recv(
            dst,
            RecvOptions {
                nonblocking_override: Some(nonblocking),
                ..RecvOptions::default()
            },
        )
    }

    pub(crate) fn write_with_nonblocking(
        &self,
        src: &mut IoSrc,
        nonblocking: bool,
    ) -> AxResult<usize> {
        self.send(
            src,
            SendOptions {
                nonblocking_override: Some(nonblocking),
                ..SendOptions::default()
            },
        )
    }

    pub(crate) fn retry_transfer<T>(
        &self,
        direction: SocketTransferDirection,
        effective_nonblocking: bool,
        attempt: impl FnMut() -> AxResult<T>,
    ) -> AxResult<T> {
        self.inner
            .retry_transfer(direction, effective_nonblocking, attempt)
    }

    #[cfg(feature = "bpf")]
    pub fn set_bpf_filter(&self, prog: Option<Arc<BpfProgram>>) -> AxResult<()> {
        let filter = prog
            .map(|prog| {
                Arc::try_new(AttachedSocketFilter { prog })
                    .map(|filter| filter as Arc<dyn axnet::SocketFilter>)
                    .map_err(|_| AxError::NoMemory)
            })
            .transpose()?;
        self.inner.set_filter(filter)
    }

    pub fn listen(&self, backlog: usize) -> AxResult<()> {
        self.inner.listen(backlog)
    }

    /// Borrows a network socket from one already-stabilized open file
    /// description.
    ///
    /// Syscalls that must perform usercopy before reporting `ENOTSOCK` can
    /// retain the numeric fd's OFD at entry and downcast it only after the ABI
    /// copy. This avoids a second fd-table lookup that a `CLONE_FILES` sibling
    /// could redirect through close-and-reuse.
    pub(crate) fn from_file_handle(file: &FileHandle<dyn FileLike>) -> AxResult<&Self> {
        if file.status_flags() & O_PATH != 0 {
            return Err(AxError::BadFileDescriptor);
        }
        if let Some(socket) = file.downcast_ref::<Self>() {
            return Ok(socket);
        }
        if file
            .downcast_ref::<File>()
            .is_some_and(|file| file.inner().is_path())
        {
            return Err(AxError::BadFileDescriptor);
        }
        Err(AxError::NotASocket)
    }
}

impl FileLike for Socket {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.read_with_nonblocking(dst, self.nonblocking())
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.write_with_nonblocking(src, self.nonblocking())
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }

    fn update_timestamps(
        &self,
        atime: Option<axfs_ng_vfs::Timestamp>,
        mtime: Option<axfs_ng_vfs::Timestamp>,
        ctime: axfs_ng_vfs::Timestamp,
    ) -> AxResult<()> {
        self.inode.update_timestamps(atime, mtime, ctime);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.inner.nonblocking()
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        self.inner
            .set_option(SetSocketOption::NonBlocking(&nonblocking))
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        if cmd == FIONREAD || cmd == TIOCINQ {
            return self.inner.recv_pending_len();
        }
        socket_ifreq_ioctl(context, self.net_ns.stack(), cmd, arg)
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        try_pseudo_inode_path("socket", self.inode.inode())
    }

    fn from_fd(fd: c_int) -> AxResult<FileHandle<Self>>
    where
        Self: Sized + 'static,
    {
        match get_typed_file(fd) {
            Ok(file) => Ok(file),
            Err(AxError::InvalidInput) => {
                let file = get_file_like(fd)?;
                if let Some(file) = file.downcast_ref::<File>()
                    && file.inner().is_path()
                {
                    return Err(AxError::BadFileDescriptor);
                }
                Err(AxError::NotASocket)
            }
            Err(err) => Err(err),
        }
    }
}
impl Pollable for Socket {
    fn poll(&self) -> IoEvents {
        self.inner.poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        self.inner.register(context, events)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::Arc;

    use axnet::{
        Socket as SocketInner,
        unix::{DgramTransport, UnixSocket},
    };

    use super::Socket;
    use crate::task::{NetworkNamespace, UserNamespace};

    #[test]
    fn namespace_owner_ordinary_socket_retains_complete_network_namespace() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let net_ns = NetworkNamespace::try_new_loopback_only(user_ns).unwrap();
        let weak = Arc::downgrade(&net_ns);
        let unix = UnixSocket::new(
            DgramTransport::new().unwrap(),
            net_ns.stack().unix_namespace(),
        );
        let socket = Socket::new(SocketInner::Unix(unix), net_ns.clone());

        drop(net_ns);
        assert!(weak.upgrade().is_some());
        drop(socket);
        assert!(weak.upgrade().is_none());
    }
}
