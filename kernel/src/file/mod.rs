pub(crate) mod af_alg;
#[cfg(feature = "bpf")]
pub mod bpf;
pub mod epoll;
pub mod event;
pub mod flock;
mod fs;
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

pub(crate) use self::fs::{allowed_write_len, check_resize_limit};
pub use self::{
    af_alg::AfAlgSocket,
    fs::{Directory, File, ResolveAtResult, resolve_at, with_fs, with_path_fs},
    net::Socket,
    packet::PacketSocket,
    pidfd::PidFd,
    pipe::Pipe,
};

// Re-exports from split sub-modules — keep the old `crate::file::*` paths unchanged.
pub use self::desc::*;
pub use self::fd_table::*;
pub use self::stdio::add_stdio;
pub use self::types::*;
