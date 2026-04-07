use alloc::{borrow::Cow, format, sync::Arc, vec::Vec};
use core::{ffi::c_int, ops::Deref, task::Context};

use axerrno::{AxError, AxResult};
use axnet::{
    RecvOptions, SendOptions, Socket as SocketInner, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption},
};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::general::S_IFSOCK;

use super::{File, FileHandle, FileLike, Kstat};
use crate::{
    bpf::{prog::BpfProgram, vm::BpfVm},
    file::{IoDst, IoSrc, get_file_like, get_typed_file},
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

pub struct Socket(pub SocketInner);

impl Deref for Socket {
    type Target = SocketInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Socket {
    pub fn set_bpf_filter(&self, prog: Option<Arc<BpfProgram>>) -> AxResult<()> {
        let filter = prog.map(|prog| Arc::new(AttachedSocketFilter { prog }) as Arc<dyn axnet::SocketFilter>);
        self.0.set_filter(filter)
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
        self.0
            .set_option(SetSocketOption::NonBlocking(&nonblocking))
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
        self.0.poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.0.register(context, events);
    }
}
