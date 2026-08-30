use crate::task::AsThread;

struct TaskIoAccountingIf;

#[crate_interface::impl_interface]
impl axfs::TaskIoAccounting for TaskIoAccountingIf {
    fn account_read(bytes: usize) {
        if let Some(task) = axtask::current_may_uninit()
            && let Some(thread) = task.try_as_thread()
        {
            thread.account_backing_read(bytes);
        }
    }

    fn account_write(bytes: usize) {
        if let Some(task) = axtask::current_may_uninit()
            && let Some(thread) = task.try_as_thread()
        {
            thread.account_backing_write(bytes);
        }
    }
}

pub(crate) mod af_alg;
#[cfg(feature = "bpf")]
pub mod bpf;
pub(crate) mod dnotify;
pub mod epoll;
pub mod event;
pub(crate) mod executable;
pub mod fanotify;
mod fiemap;
pub mod flock;
mod fs;
pub(crate) mod inode_flags;
pub mod inotify;
pub(crate) mod io_uring;
pub(crate) mod lease;
pub(crate) mod memfd;
pub(crate) mod secretmem;
pub(crate) mod namespace_mutation;
mod net;
pub(crate) mod netlink;
pub(crate) mod packet;
pub(crate) mod packet_socket;
pub(crate) mod permission;
mod pidfd;
pub(crate) mod pipe;
pub(crate) mod privilege_metadata;
pub mod signalfd;
pub mod timerfd;
pub(crate) mod unix_socket;
pub(crate) mod userfaultfd;
pub(crate) mod xattr_provider;

mod desc;
mod fd_table;
mod fs_types;
mod metadata;
mod socket;
mod stdio;
mod types;

// Re-exports from split sub-modules — keep the old `crate::file::*` paths unchanged.
pub use self::{
    af_alg::AfAlgSocket,
    desc::*,
    fd_table::*,
    fs::{Directory, File, ResolveAtResult, resolve_at, with_path_fs},
    net::Socket,
    netlink::NetlinkSocket,
    pidfd::PidFd,
    pipe::Pipe,
    stdio::add_stdio,
    types::*,
};
pub(crate) use self::{
    pipe::NamedPipe,
    fs::{
        allowed_write_len, check_resize_limit, resolve_at_with_security,
        resolve_at_with_synthetic_credentials, validate_pathname, validate_symlink_target,
    },
    fs_types::filesystem_type_catalog,
    metadata::{PseudoInode, anon_inode_stat},
    packet_socket::PacketSocket,
    io_uring::IoUring,
    userfaultfd::UserfaultFile,
    secretmem::SecretMemFile,
    socket::{
        AcceptedSocketSecurityRef, BareAcceptedSocketSecurityRef, PACKET_SOCKADDR_STORAGE_LEN,
        PacketSockaddrSnapshot, PendingSocketSecurityRef, PinnedSocketDescription,
        PreparedSocketAddress, PreparedSocketMessage, SocketBackendKind, SocketSecurityRef,
        UnixEndpointSecurityRef,
    },
};
