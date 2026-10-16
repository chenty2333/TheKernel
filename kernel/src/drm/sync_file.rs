//! Descriptors for LinuxKPI sync_file files.
//!
//! `tk-linux-kapi::sync_file` owns the Linux `struct sync_file` and its file
//! operations. This module installs such a file in the kernel's fd table: poll
//! and ioctl forward to the kapi functions, and the descriptor's final close
//! drops the file reference, whose last `fput()` runs `sync_file_release`.

use alloc::{borrow::Cow, boxed::Box};
use core::{ffi::c_void, ptr, task::Context};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::FsPath;
use axpoll::{
    IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable,
    PreparedPollRegistration,
};
use tk_linux_kapi::{
    shmem::{File, fput_file},
    sync_file::{sync_file_ioctl, sync_file_poll, sync_file_wait_queue},
    types::{ListHead, WaitQueueEntry},
    wait::{add_wait_queue, remove_wait_queue},
};

use crate::file::{FileLike, IoctlContext, Kstat, anon_inode_stat};

/// `EPOLLIN`, the only event Linux's `sync_file_poll()` reports.
const EPOLLIN: u32 = 0x001;

/// Wakeup state of one sync_file descriptor. It is boxed because the wait
/// queue entry names it by address, and that address must survive moves of
/// the descriptor.
struct SyncFileWakeup {
    entry: WaitQueueEntry,
    waiters: PollSet,
}

/// A sync_file installed in the fd table. Owns one reference to the LinuxKPI
/// file, dropped in `Drop`.
pub(crate) struct SyncFileFd {
    file: *mut File,
    wakeup: Box<SyncFileWakeup>,
}

// SAFETY: the LinuxKPI file is reached only through its refcounted get/put API
// and the sync_file accessors, which the descriptor serialises with the wait
// queue lock; the wakeup state is `Sync` apart from the raw entry pointers,
// which only the wait queue touches.
unsafe impl Send for SyncFileFd {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for SyncFileFd {}

impl SyncFileFd {
    /// Takes ownership of one reference to `file`, which must be a sync_file
    /// file. On failure that reference is dropped.
    pub(crate) fn try_new(file: *mut File) -> AxResult<Self> {
        let mut wakeup = match Box::try_new(SyncFileWakeup {
            entry: WaitQueueEntry {
                flags: 0,
                private: ptr::null_mut(),
                func: Some(sync_file_wake),
                entry: ListHead {
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                },
            },
            waiters: PollSet::new(),
        }) {
            Ok(wakeup) => wakeup,
            Err(_) => {
                // SAFETY: this function owns the reference it was given.
                unsafe { fput_file(file) };
                return Err(AxError::NoMemory);
            }
        };
        let state: *mut SyncFileWakeup = &mut *wakeup;
        wakeup.entry.private = state.cast::<c_void>();
        // SAFETY: `file` is live and owned here; the wait queue is the one
        // `sync_file_create` initialised. The entry is removed in `Drop` before
        // the reference is dropped.
        unsafe {
            add_wait_queue(&mut *sync_file_wait_queue(file), &mut wakeup.entry);
        }
        Ok(Self { file, wakeup })
    }

    /// The LinuxKPI file this descriptor holds, without taking a reference.
    pub(crate) fn linux_file(&self) -> *mut File {
        self.file
    }
}

/// Linux wait-queue callback: a fence callback woke the sync_file's wait
/// queue, so every poller registered on this descriptor is woken.
unsafe extern "C" fn sync_file_wake(
    entry: *mut WaitQueueEntry,
    _mode: u32,
    _sync: i32,
    _key: *mut c_void,
) -> i32 {
    // SAFETY: `private` is set in `try_new` to the live boxed wakeup state,
    // which outlives the entry because `Drop` unlinks the entry first.
    let state = unsafe { (*entry).private }.cast::<SyncFileWakeup>();
    unsafe { (*state).waiters.wake() };
    1
}

impl Drop for SyncFileFd {
    fn drop(&mut self) {
        // SAFETY: the reference held by `self` keeps the wait queue live.
        unsafe { remove_wait_queue(&mut *sync_file_wait_queue(self.file), &mut self.wakeup.entry) };
        // SAFETY: this descriptor owns one reference, dropped exactly once here.
        unsafe { fput_file(self.file) };
    }
}

impl Pollable for SyncFileFd {
    fn poll(&self) -> IoEvents {
        // SAFETY: `file` is live while `self` is.
        let events = unsafe { sync_file_poll(self.file, ptr::null_mut()) };
        if events & EPOLLIN != 0 {
            IoEvents::READABLE
        } else {
            IoEvents::empty()
        }
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        // Linux arms the fence callback on the first poll; registering a poller
        // does the same so the wait queue is woken when the fence signals.
        // SAFETY: `file` is live while `self` is.
        unsafe { sync_file_poll(self.file, ptr::null_mut()) };
        let readable = events.intersects(IoEvents::READABLE);
        let mut prepared = PreparedPollRegistration::try_new(if readable { 1 } else { 0 })?;
        if readable {
            prepared.arm(&self.wakeup.waiters, context.waker())?;
        }
        prepared.commit()
    }
}

impl FileLike for SyncFileFd {
    fn path(&self) -> AxResult<Cow<'_, FsPath>> {
        Ok(Cow::Borrowed(FsPath::new(b"anon_inode:[sync_file]")))
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult<()> {
        Ok(())
    }

    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn ioctl(&self, _context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        // SAFETY: `file` is live while `self` is; `arg` is a user address for
        // the commands that take one, validated by the kapi copy helpers.
        let ret = unsafe { sync_file_ioctl(self.file, cmd, arg) };
        if ret < 0 {
            let errno = i32::try_from(-ret).unwrap_or(LinuxError::EINVAL.code());
            return Err(LinuxError::try_from(errno).unwrap_or(LinuxError::EINVAL).into());
        }
        Ok(ret as usize)
    }
}
