pub(crate) mod af_alg;
#[cfg(feature = "bpf")]
pub mod bpf;
pub(crate) mod dnotify;
pub mod epoll;
pub mod event;
pub(crate) mod executable;
pub mod fanotify;
pub mod flock;
mod fs;
pub(crate) mod inode_flags;
pub mod inotify;
pub(crate) mod lease;
pub(crate) mod memfd;
pub(crate) mod namespace_mutation;
mod net;
pub(crate) mod netlink;
pub(crate) mod packet;
pub(crate) mod permission;
mod pidfd;
pub(crate) mod pipe;
pub(crate) mod privilege_metadata;
pub mod signalfd;
pub mod timerfd;
pub(crate) mod unix_socket;
pub(crate) mod xattr_provider;

mod desc;
mod fd_table;
mod metadata;
mod socket;
mod stdio;
mod types;

// Re-exports from split sub-modules — keep the old `crate::file::*` paths unchanged.
pub use self::{
    af_alg::AfAlgSocket,
    desc::*,
    fd_table::*,
    fs::{
        Directory, File, ResolveAtResult, resolve_at, resolve_at_with_credentials, with_fs,
        with_path_fs,
    },
    net::Socket,
    netlink::NetlinkSocket,
    pidfd::PidFd,
    pipe::Pipe,
    stdio::add_stdio,
    types::*,
};
pub(crate) use self::{
    fs::{
        allowed_write_len, check_resize_limit, resolve_at_with_security, validate_pathname,
        validate_symlink_target,
    },
    metadata::{PseudoInode, anon_inode_stat},
    socket::{PinnedSocketDescription, SocketBackendKind},
};
