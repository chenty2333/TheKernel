use alloc::{borrow::Cow, sync::Arc};
use core::{
    ffi::c_int,
    ops::Deref,
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
};

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
use crate::bpf::{helpers::BpfExecution, prog::BpfProgram};
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
        // Network struct_ops consumers observe the actual packet buffer
        // before the socket-filter return value trims it.
        crate::bpf::run_struct_ops(data);
        let execution =
            BpfExecution::new(data, &self.prog.maps, u64::MAX).with_streams(&self.prog.streams);
        let stats = crate::bpf::prog::BpfStatsRunGuard::begin();
        let result = execution.execute(&self.prog.mechanism);
        self.prog.account_run(&stats);
        let ret = result?.0 as usize;
        Ok(ret.min(data.len()))
    }
}

pub struct Socket {
    pub inner: SocketInner,
    net_ns: Arc<NetworkNamespace>,
    inode: PseudoInode,
    abstract_landlock_label: Mutex<Option<AbstractUnixSocketLabelReservation>>,
    creator_security: Mutex<Option<(Arc<Cred>, LandlockDomain)>>,
    diag_registration: Option<Arc<super::netlink::SocketDiagRegistration>>,
    // The Linux socket ABI exposes these three creation-time facts through
    // SOL_SOCKET.  They are not derivable from an unbound raw-IP transport
    // (in particular DCCP has no endpoint address before bind/connect), so
    // retain the exact values that were admitted at socket(2).
    inet_identity: Option<InetSocketIdentity>,
    /// Serializes filter publication with BPF link update/detach.  The
    /// generation makes a stale link close unable to remove a later
    /// SO_ATTACH_BPF or link replacement.
    bpf_filter_lock: Mutex<()>,
    bpf_filter_generation: AtomicU64,
}

#[derive(Clone, Copy)]
pub(crate) struct InetSocketIdentity {
    pub(crate) family: u16,
    pub(crate) socket_type: u8,
    pub(crate) protocol: u8,
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
            creator_security: Mutex::new(None),
            diag_registration: None,
            inet_identity: None,
            bpf_filter_lock: Mutex::new(()),
            bpf_filter_generation: AtomicU64::new(0),
        }
    }

    pub(crate) fn net_namespace(&self) -> &Arc<NetworkNamespace> {
        &self.net_ns
    }

    /// Records this inet OFD in the namespace-local SOCK_DIAG registry.  The
    /// retained entry follows normal file-description sharing and vanishes on
    /// final close, rather than relying on task-local bookkeeping.
    pub(crate) fn register_sock_diag(
        &mut self,
        family: u16,
        socket_type: u8,
        protocol: u8,
    ) -> AxResult<()> {
        self.diag_registration = Some(super::netlink::register_socket_diag(
            &self.net_ns,
            family,
            socket_type,
            protocol,
        )?);
        self.inet_identity = Some(InetSocketIdentity {
            family,
            socket_type,
            protocol,
        });
        Ok(())
    }

    pub(crate) const fn inet_identity(&self) -> Option<InetSocketIdentity> {
        self.inet_identity
    }

    /// Accepted inet sockets retain their listener's immutable ABI identity,
    /// but are a distinct open file description and therefore need their own
    /// SOCK_DIAG lifetime token. Keeping the token on the child makes
    /// close/dup/fork/exec retirement exact.
    pub(crate) fn inherit_inet_identity_from(&mut self, listener: &Self) -> AxResult<()> {
        let Some(identity) = listener.inet_identity else {
            return Ok(());
        };
        self.register_sock_diag(identity.family, identity.socket_type, identity.protocol)
    }

    pub(crate) fn install_abstract_landlock_label(
        &self,
        label: AbstractUnixSocketLabelReservation,
    ) {
        *self.abstract_landlock_label.lock() = Some(label);
    }

    /// Freeze the task security state at socket creation.  A later holder of
    /// the fd must not be able to relabel an abstract endpoint on bind.
    pub(crate) fn capture_creator_security(&self, cred: Arc<Cred>, domain: LandlockDomain) {
        *self.creator_security.lock() = Some((cred, domain));
    }

    /// Older socket creation paths can hand a TCP OFD to fsconfig without a
    /// retained creator snapshot.  Pin the fsconfig caller exactly once so
    /// NFS can still retain that ordinary socket; an existing creator is
    /// never relabelled by a later FD holder.
    pub(crate) fn capture_creator_security_if_absent(
        &self,
        cred: Arc<Cred>,
        domain: LandlockDomain,
    ) {
        let mut creator = self.creator_security.lock();
        if creator.is_none() {
            *creator = Some((cred, domain));
        }
    }

    /// An accepted socket is a new OFD, but it must retain the security
    /// provenance fixed on the listener rather than consulting a later task.
    pub(crate) fn inherit_creator_security_from(&self, listener: &Self) {
        if let Ok((cred, domain)) = listener.creator_security_snapshot() {
            self.capture_creator_security_if_absent(cred, domain);
        }
    }

    pub(crate) fn creator_landlock_domain(&self) -> AxResult<LandlockDomain> {
        self.creator_security
            .lock()
            .as_ref()
            .map(|(_, domain)| domain.clone())
            .ok_or(AxError::BadState)
    }
    pub(crate) fn creator_security_snapshot(&self) -> AxResult<(Arc<Cred>, LandlockDomain)> {
        self.creator_security
            .lock()
            .clone()
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
        .map_err(|error| crate::syscall::map_socket_send_error(&self.inner, error))
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
        let _guard = self.bpf_filter_lock.lock();
        self.install_bpf_filter_locked(prog)
    }

    #[cfg(feature = "bpf")]
    fn install_bpf_filter_locked(&self, prog: Option<Arc<BpfProgram>>) -> AxResult<()> {
        let filter = prog
            .map(|prog| {
                Arc::try_new(AttachedSocketFilter { prog })
                    .map(|filter| filter as Arc<dyn axnet::SocketFilter>)
                    .map_err(|_| AxError::NoMemory)
            })
            .transpose()?;
        self.inner.set_filter(filter)?;
        self.bpf_filter_generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Publishes a BPF link-owned filter and returns its exact ownership
    /// generation.  The link keeps the program alive; this socket only owns
    /// the executable adapter installed in the transport endpoint.
    #[cfg(feature = "bpf")]
    pub(crate) fn attach_bpf_filter_link(&self, program: Arc<BpfProgram>) -> AxResult<u64> {
        let _guard = self.bpf_filter_lock.lock();
        self.install_bpf_filter_locked(Some(program))?;
        Ok(self.bpf_filter_generation.load(Ordering::Acquire))
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn replace_bpf_filter_link_if_current(
        &self,
        generation: u64,
        program: Arc<BpfProgram>,
    ) -> AxResult<u64> {
        let _guard = self.bpf_filter_lock.lock();
        if self.bpf_filter_generation.load(Ordering::Acquire) != generation {
            return Err(AxError::NotFound);
        }
        self.install_bpf_filter_locked(Some(program))?;
        Ok(self.bpf_filter_generation.load(Ordering::Acquire))
    }

    #[cfg(feature = "bpf")]
    pub(crate) fn detach_bpf_filter_link_if_current(&self, generation: u64) {
        let _guard = self.bpf_filter_lock.lock();
        if self.bpf_filter_generation.load(Ordering::Acquire) == generation {
            // A transport allocation failure cannot occur for `None`; keep a
            // best-effort final-close boundary rather than panicking during
            // descriptor teardown.
            let _ = self.install_bpf_filter_locked(None);
        }
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

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
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

impl Drop for Socket {
    fn drop(&mut self) {
        // DCCP has an on-wire close handshake, but its raw-IP backing socket
        // only learns about it through SocketOps::shutdown.  Final OFD close
        // must therefore drive the same terminal transition as shutdown(2),
        // before RawSocket removes its namespace socket-set entry.
        if let SocketInner::Dccp(dccp) = &self.inner {
            let _ = dccp.shutdown(axnet::Shutdown::Both);
        }
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
