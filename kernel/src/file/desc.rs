use alloc::{borrow::Cow, sync::Arc};
use core::{ops::Deref, sync::atomic::{AtomicU64, Ordering}, task::Context};

use axerrno::AxResult;
use axpoll::{IoEvents, Pollable};

use super::{
    types::{FileLike, IoDst, IoSrc, Kstat},
    flock, lease,
};

static FILE_DESCRIPTION_ID: AtomicU64 = AtomicU64::new(1);

/// Cached type tag used by `sys_read`/`sys_write` to skip vtable dispatch on
/// the most common fd types (regular file, pipe, socket). Falls back to
/// `Arc<dyn FileLike>` for everything else.
pub enum FileFast {
    Regular(Arc<super::fs::File>),
    Pipe(Arc<super::pipe::Pipe>),
    Socket(Arc<super::net::Socket>),
    Other,
}

pub struct FileDescription {
    pub inner: Arc<dyn FileLike>,
    pub(super) fast: FileFast,
    flock_owner: u64,
}

impl FileDescription {
    pub(in crate::file) fn new(inner: Arc<dyn FileLike>) -> Arc<Self> {
        use downcast_rs::DowncastSync;

        let fast = {
            // Each downcast_arc call consumes the Arc, so clone first.
            // Only one branch succeeds; the others return the original Arc
            // which is then dropped.
            let tmp = inner.clone();
            if let Ok(pipe) = tmp.downcast_arc::<super::pipe::Pipe>() {
                FileFast::Pipe(pipe)
            } else if let Ok(file) = inner.clone().downcast_arc::<super::fs::File>() {
                FileFast::Regular(file)
            } else if let Ok(sock) = inner.clone().downcast_arc::<super::net::Socket>() {
                FileFast::Socket(sock)
            } else {
                FileFast::Other
            }
        };
        Arc::new(Self {
            inner,
            fast,
            flock_owner: FILE_DESCRIPTION_ID.fetch_add(1, Ordering::Relaxed),
        })
    }

    pub fn flock_owner(&self) -> u64 {
        self.flock_owner
    }

    /// Fast-path read that dispatches through the cached `FileFast` tag
    /// instead of the `dyn FileLike` vtable.
    pub(crate) fn fast_read(
        &self,
        dst: &mut (impl axio::Write + axio::IoBufMut),
    ) -> AxResult<usize> {
        match &self.fast {
            FileFast::Pipe(pipe) => pipe.read_fast(dst),
            FileFast::Regular(file) => file.read_fast(dst),
            FileFast::Socket(sock) => sock.read_fast(dst),
            FileFast::Other => self.inner.read(dst),
        }
    }

    /// Returns true if the fast tag indicates a regular file (used by
    /// `sys_writev` for `check_readable` pre-validation).
    pub(crate) fn is_regular_fast(&self) -> bool {
        matches!(&self.fast, FileFast::Regular(_))
    }

    /// Fast-path write that dispatches through the cached `FileFast` tag
    /// instead of the `dyn FileLike` vtable.
    pub(crate) fn fast_write(
        &self,
        src: &mut (impl axio::Read + axio::IoBuf),
    ) -> AxResult<usize> {
        match &self.fast {
            FileFast::Pipe(pipe) => pipe.write_fast(src),
            FileFast::Regular(file) => file.write_fast(src),
            FileFast::Socket(sock) => sock.write_fast(src),
            FileFast::Other => self.inner.write(src),
        }
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
