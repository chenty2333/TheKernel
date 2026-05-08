use alloc::{borrow::Cow, sync::Arc};
use core::{ops::Deref, sync::atomic::{AtomicU64, Ordering}, task::Context};

use axerrno::AxResult;
use axpoll::{IoEvents, Pollable};

use super::{
    types::{FileLike, IoDst, IoSrc, Kstat},
    flock, lease,
};

static FILE_DESCRIPTION_ID: AtomicU64 = AtomicU64::new(1);

pub struct FileDescription {
    pub inner: Arc<dyn FileLike>,
    flock_owner: u64,
}

impl FileDescription {
    pub(in crate::file) fn new(inner: Arc<dyn FileLike>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            flock_owner: FILE_DESCRIPTION_ID.fetch_add(1, Ordering::Relaxed),
        })
    }

    pub fn flock_owner(&self) -> u64 {
        self.flock_owner
    }
}

impl Drop for FileDescription {
    fn drop(&mut self) {
        flock::release_owner(self.flock_owner);
        lease::release_owner(self.flock_owner);
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

#[derive(Clone)]
pub struct FileDescriptor {
    pub description: Arc<FileDescription>,
    pub cloexec: bool,
}
