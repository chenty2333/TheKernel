use alloc::{borrow::Cow, format, sync::Arc};
use core::{ffi::c_int, ops::Deref, task::Context};

use axerrno::{AxError, AxResult};
use axnet::{
    NetStack, RecvOptions, SendOptions, Socket as SocketInner, SocketOps,
    options::{Configurable, SetSocketOption},
};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::general::O_PATH;

use super::{File, FileHandle, FileLike, Kstat, PseudoInode};
use crate::{
    bpf::{prog::BpfProgram, vm::BpfVm},
    file::{IoDst, IoSrc, get_file_like, get_typed_file, packet::socket_ifreq_ioctl},
};

struct AttachedSocketFilter {
    prog: Arc<BpfProgram>,
}

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
    net_stack: Arc<NetStack>,
    inode: PseudoInode,
}

impl Deref for Socket {
    type Target = SocketInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Socket {
    pub fn new(inner: SocketInner, net_stack: Arc<NetStack>) -> Self {
        Self {
            inner,
            net_stack,
            inode: PseudoInode::socket(),
        }
    }

    pub fn net_stack(&self) -> &Arc<NetStack> {
        &self.net_stack
    }

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
        self.recv(dst, RecvOptions::default())
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.send(src, SendOptions::default())
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(self.inode.stat())
    }

    fn nonblocking(&self) -> bool {
        self.inner.nonblocking()
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        self.inner
            .set_option(SetSocketOption::NonBlocking(&nonblocking))
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        socket_ifreq_ioctl(&self.net_stack, cmd, arg)
    }

    fn path(&self) -> Cow<'_, str> {
        format!("socket:[{}]", self.inode.inode()).into()
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

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.inner.register(context, events);
    }
}
