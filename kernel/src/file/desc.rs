use alloc::{borrow::Cow, sync::Arc};
use core::{
    ops::Deref,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    task::Context,
};

use axerrno::AxResult;
use axpoll::{IoEvents, Pollable};
use axtask::{current, current_may_uninit};
use spin::Mutex;
use starry_process::Pid;

use super::{
    executable::{self, ExecutableKey},
    fanotify::FanotifyFile,
    flock, lease,
    types::{FileLike, IoDst, IoSrc, Kstat},
};
use crate::task::AsThread;

static FILE_DESCRIPTION_ID: AtomicU64 = AtomicU64::new(1);

scope_local::scope_local! {
    pub static FILE_WRITE_CREDENTIALS: Option<OpenCredentials> = None;
}

#[derive(Clone, Copy, Debug)]
pub struct OpenCredentials {
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub cgroup_ns_id: u64,
}

impl OpenCredentials {
    pub fn current() -> Self {
        let Some(task) = current_may_uninit() else {
            return Self::root();
        };
        let Some(thread) = task.try_as_thread() else {
            return Self::root();
        };
        let proc_data = &thread.proc_data;
        Self {
            uid: proc_data.uid(),
            euid: proc_data.euid(),
            suid: proc_data.suid(),
            fsuid: proc_data.fsuid(),
            cgroup_ns_id: proc_data.cgroup_ns_id(),
        }
    }

    const fn root() -> Self {
        Self {
            uid: 0,
            euid: 0,
            suid: 0,
            fsuid: 0,
            cgroup_ns_id: 0,
        }
    }
}

pub fn current_file_write_credentials() -> Option<OpenCredentials> {
    *FILE_WRITE_CREDENTIALS
}

#[derive(Clone, Copy)]
pub enum AsyncIoOwner {
    Tid(Pid),
    Pid(Pid),
    Pgrp(Pid),
}

#[derive(Clone, Copy)]
pub struct AsyncIoState {
    pub owner: AsyncIoOwner,
    pub signal: u8,
}

impl Default for AsyncIoState {
    fn default() -> Self {
        Self {
            owner: AsyncIoOwner::Pid(0),
            signal: 0,
        }
    }
}

pub struct FileDescription {
    pub inner: Arc<dyn FileLike>,
    open_credentials: OpenCredentials,
    flock_owner: u64,
    status_flags: AtomicU32,
    write_open_key: Option<ExecutableKey>,
    async_io: Mutex<AsyncIoState>,
}

impl FileDescription {
    pub(in crate::file) fn new(inner: Arc<dyn FileLike>) -> Arc<Self> {
        Self::new_with_flags(inner, 0)
    }

    pub(in crate::file) fn new_with_flags(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
    ) -> Arc<Self> {
        Self::new_inner(inner, status_flags, None)
    }

    pub(in crate::file) fn new_with_write_open_key(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
        write_open_key: Option<ExecutableKey>,
    ) -> Arc<Self> {
        Self::new_inner(inner, status_flags, write_open_key)
    }

    fn new_inner(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
        write_open_key: Option<ExecutableKey>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            open_credentials: OpenCredentials::current(),
            flock_owner: FILE_DESCRIPTION_ID.fetch_add(1, Ordering::Relaxed),
            status_flags: AtomicU32::new(status_flags),
            write_open_key,
            async_io: Mutex::new(AsyncIoState::default()),
        })
    }

    pub fn flock_owner(&self) -> u64 {
        self.flock_owner
    }

    pub fn open_credentials(&self) -> OpenCredentials {
        self.open_credentials
    }

    pub fn status_flags(&self) -> u32 {
        self.status_flags.load(Ordering::Relaxed)
    }

    pub fn set_status_flags(&self, flags: u32) {
        self.status_flags.store(flags, Ordering::Relaxed);
    }

    pub fn async_io_state(&self) -> AsyncIoState {
        *self.async_io.lock()
    }

    pub fn set_async_io_owner(&self, owner: AsyncIoOwner) {
        self.async_io.lock().owner = owner;
    }

    pub fn set_async_io_signal(&self, signal: u8) {
        self.async_io.lock().signal = signal;
    }
}

impl Drop for FileDescription {
    fn drop(&mut self) {
        if let Some(fanotify) = self.inner.downcast_ref::<FanotifyFile>() {
            fanotify.release();
        }
        flock::release_owner(self.flock_owner);
        flock::release_ofd_owner(self.flock_owner);
        lease::release_owner(self.flock_owner);
        executable::release_write_open(self.write_open_key);
    }
}

impl FileLike for FileDescription {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.inner.read(dst)
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.inner.write(src)
    }

    fn stat(&self) -> AxResult<Kstat> {
        self.inner.stat()
    }

    fn path(&self) -> Cow<'_, str> {
        self.inner.path()
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        self.inner.ioctl(cmd, arg)
    }

    fn nonblocking(&self) -> bool {
        self.inner.nonblocking()
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.inner.set_nonblocking(nonblocking)
    }
}

impl Pollable for FileDescription {
    fn poll(&self) -> IoEvents {
        self.inner.poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.inner.register(context, events);
    }
}

pub struct FileHandle<T: ?Sized> {
    pub(in crate::file) description: Arc<FileDescription>,
    pub(in crate::file) file: Arc<T>,
}

impl<T: ?Sized> Clone for FileHandle<T> {
    fn clone(&self) -> Self {
        Self {
            description: self.description.clone(),
            file: self.file.clone(),
        }
    }
}

impl<T: ?Sized> Deref for FileHandle<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.file.as_ref()
    }
}

impl<T: ?Sized> AsRef<T> for FileHandle<T> {
    fn as_ref(&self) -> &T {
        self.file.as_ref()
    }
}

impl<T: ?Sized> FileHandle<T> {
    pub fn status_flags(&self) -> u32 {
        self.description.status_flags()
    }

    pub fn with_write_credentials<R>(&self, f: impl FnOnce() -> R) -> R {
        let credentials = self.description.open_credentials();
        let previous = current().as_thread().with_mut_scope(|scope| {
            let mut slot = FILE_WRITE_CREDENTIALS.scope_mut(scope);
            let previous = *slot;
            *slot = Some(credentials);
            previous
        });
        let result = f();
        current().as_thread().with_mut_scope(|scope| {
            *FILE_WRITE_CREDENTIALS.scope_mut(scope) = previous;
        });
        result
    }
}

#[derive(Clone)]
pub struct FileDescriptor {
    pub description: Arc<FileDescription>,
    pub cloexec: bool,
}
