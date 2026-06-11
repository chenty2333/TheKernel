pub(crate) mod af_alg;
#[cfg(feature = "bpf")]
pub mod bpf;
pub mod epoll;
pub mod event;
pub(crate) mod executable;
pub mod fanotify;
pub mod flock;
mod fs;
pub(crate) mod inode_flags;
pub mod inotify;
pub mod io_uring;
pub(crate) mod lease;
pub(crate) mod memfd;
mod net;
pub(crate) mod packet;
pub(crate) mod permission;
mod pidfd;
pub(crate) mod pipe;
pub mod signalfd;
pub mod timerfd;
pub mod userfaultfd;

mod desc;
mod fd_table;
mod stdio;
mod types;

// Re-exports from split sub-modules — keep the old `crate::file::*` paths unchanged.
pub(crate) use self::fs::{
    allowed_write_len, check_resize_limit, has_tmpfile_state, install_tmpfile_state,
};
pub use self::{
    af_alg::AfAlgSocket,
    desc::*,
    fd_table::*,
    fs::{Directory, File, ResolveAtResult, is_path_only_fd, resolve_at, with_fs, with_path_fs},
    net::Socket,
    packet::PacketSocket,
    pidfd::PidFd,
    pipe::Pipe,
    stdio::add_stdio,
    types::*,
};
