use alloc::{borrow::Cow, format, sync::Arc, vec::Vec};
use core::{ffi::c_int, ops::Deref, task::Context};

use axerrno::{AxError, AxResult};
use axnet::{
    RecvOptions, SendOptions, Socket as SocketInner, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption},
};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::general::S_IFSOCK;
use spin::Mutex;

use super::{File, FileHandle, FileLike, Kstat};
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

#[derive(Default)]
struct SocketCompatState {
    tcp_tls_ulp: bool,
}

pub struct Socket {
    pub inner: SocketInner,
    compat: Mutex<SocketCompatState>,
}

impl Deref for Socket {
    type Target = SocketInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Socket {
    pub fn new(inner: SocketInner) -> Self {
        Self {
            inner,
            compat: Mutex::new(SocketCompatState::default()),
        }
    }

    pub fn set_bpf_filter(&self, prog: Option<Arc<BpfProgram>>) -> AxResult<()> {
        let filter = prog
            .map(|prog| Arc::new(AttachedSocketFilter { prog }) as Arc<dyn axnet::SocketFilter>);
        self.inner.set_filter(filter)
    }

    pub fn is_tcp(&self) -> bool {
        matches!(&self.inner, SocketInner::Tcp(_))
    }

    pub fn set_tcp_tls_ulp(&self) {
        self.compat.lock().tcp_tls_ulp = true;
    }

    pub fn has_tcp_tls_ulp(&self) -> bool {
        self.compat.lock().tcp_tls_ulp
    }

    pub fn listen(&self) -> AxResult<()> {
        if self.has_tcp_tls_ulp() {
            return Err(AxError::InvalidInput);
        }
        self.inner.listen()
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
        // TODO(mivik): implement stat for sockets
        Ok(Kstat {
            mode: S_IFSOCK | 0o777u32, // rwxrwxrwx
            blksize: 4096,
            ..Default::default()
        })
    }

    fn nonblocking(&self) -> bool {
        let mut result = false;
        self.get_option(GetSocketOption::NonBlocking(&mut result))
            .unwrap();
        result
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult<()> {
        self.inner
            .set_option(SetSocketOption::NonBlocking(&nonblocking))
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        socket_ifreq_ioctl(cmd, arg)
    }

    fn path(&self) -> Cow<'_, str> {
        format!("socket:[{}]", self as *const _ as usize).into()
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
