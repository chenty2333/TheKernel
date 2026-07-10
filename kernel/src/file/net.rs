use alloc::{borrow::Cow, format, sync::Arc, vec::Vec};
use core::{ffi::c_int, ops::Deref, task::Context};

use axerrno::{AxError, AxResult};
use axnet::{
    NetStack, RecvOptions, SendOptions, Socket as SocketInner, SocketOps,
    options::{Configurable, SetSocketOption},
};
use axpoll::{IoEvents, Pollable};

use super::{File, FileHandle, FileLike, Kstat, PseudoInode};
use crate::{
    bpf::{prog::BpfProgram, vm::BpfVm},
    file::{IoDst, IoSrc, get_file_like, get_typed_file, packet::socket_ifreq_ioctl},
};

struct AttachedSocketFilter {
    prog: Arc<BpfProgram>,
}

impl axnet::SocketFilter for AttachedSocketFilter {
    fn filter(&self, data: &mut Vec<u8>) -> AxResult<usize> {
        let mut vm = BpfVm::with_aux_budget(
            &self.prog.insns,
            &self.prog.decoded_insns,
            &self.prog.maps,
            u64::MAX,
        );
        let ret = vm.execute(data.as_mut_slice())? as usize;
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
            .map(|prog| Arc::new(AttachedSocketFilter { prog }) as Arc<dyn axnet::SocketFilter>);
        self.inner.set_filter(filter)
    }

    pub fn listen(&self, backlog: usize) -> AxResult<()> {
        self.inner.listen(backlog)
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
