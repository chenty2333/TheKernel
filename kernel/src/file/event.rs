use alloc::{borrow::Cow, sync::Arc};
use core::{
    ffi::{c_int, c_void},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, PollSet, Pollable};
use tk_linux_fd::{EVENTFD_COUNTER_MAX, EventFdPlan, EventFdSnapshot};

use crate::{
    file::{FileLike, IoDst, IoSrc, Kstat, anon_inode_stat, get_typed_file},
    readiness::block_on_poll_io,
};

pub struct EventFd {
    count: AtomicU64,
    semaphore: bool,
    non_blocking: AtomicBool,

    poll_rx: PollSet,
    poll_tx: PollSet,
}

impl EventFd {
    pub fn new(initval: u64, semaphore: bool) -> Arc<Self> {
        Arc::new(Self {
            count: AtomicU64::new(initval),
            semaphore,
            non_blocking: AtomicBool::new(false),

            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
        })
    }

    pub fn signal(&self, value: u64) -> AxResult {
        self.plan_write(value)?;
        self.poll_rx.wake();
        Ok(())
    }

    fn snapshot(&self, count: u64) -> EventFdSnapshot {
        // All mutations go through the ABI plans, so the atomic value always
        // remains a representable eventfd counter.
        EventFdSnapshot::new(count, self.semaphore).expect("eventfd counter invariant")
    }

    fn plan_write(&self, value: u64) -> AxResult {
        if value == u64::MAX {
            return Err(AxError::InvalidInput);
        }
        self.count
            .try_update(Ordering::Release, Ordering::Acquire, |count| {
                match self.snapshot(count).plan_write(value) {
                    Ok(EventFdPlan::Write { after, .. }) => Some(after.counter()),
                    Ok(_) | Err(_) => None,
                }
            })
            .map_err(|_| AxError::WouldBlock)?;
        Ok(())
    }

    /// Performs one eventfd read with a request-local nonblocking override.
    ///
    /// `RWF_NOWAIT` must not change the open-file description's O_NONBLOCK
    /// state: only this attempt observes `nonblocking`.
    pub(crate) fn read_with_nonblocking(
        &self,
        dst: &mut IoDst,
        nonblocking: bool,
    ) -> axio::Result<usize> {
        if dst.remaining_mut() < size_of::<u64>() {
            return Err(AxError::InvalidInput);
        }

        block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            let mut value = 0;
            let result = self
                .count
                .try_update(Ordering::Release, Ordering::Acquire, |count| {
                    match self.snapshot(count).plan_read() {
                        Ok(EventFdPlan::Read {
                            value: next, after, ..
                        }) => {
                            value = next;
                            Some(after.counter())
                        }
                        Ok(_) | Err(_) => None,
                    }
                });
            match result {
                Ok(_) => {
                    dst.write(&value.to_ne_bytes())?;
                    self.poll_tx.wake();
                    Ok(size_of::<u64>())
                }
                Err(_) => Err(AxError::WouldBlock),
            }
        })
    }

    /// Performs one eventfd write with a request-local nonblocking override.
    pub(crate) fn write_with_nonblocking(
        &self,
        src: &mut IoSrc,
        nonblocking: bool,
    ) -> axio::Result<usize> {
        // Linux eventfd_write accepts exactly one 64-bit counter value.
        if src.remaining() != size_of::<u64>() {
            return Err(AxError::InvalidInput);
        }

        let mut value = [0; size_of::<u64>()];
        src.read(&mut value)?;
        let value = u64::from_ne_bytes(value);
        block_on_poll_io(self, IoEvents::WRITABLE, nonblocking, || {
            match self.plan_write(value) {
                Ok(()) => {
                    self.poll_rx.wake();
                    Ok(size_of::<u64>())
                }
                Err(error) => Err(error),
            }
        })
    }
}

/// LinuxKPI `eventfd_ctx_fdget()`: acquire one opaque reference to an existing
/// VFS eventfd. Error pointers carry the normal Linux errno.
#[unsafe(no_mangle)]
pub extern "C" fn thekernel_eventfd_ctx_fdget(fd: c_int) -> *mut c_void {
    match get_typed_file::<EventFd>(fd) {
        Ok(file) => Arc::into_raw(file.clone_file_arc()).cast_mut().cast(),
        Err(error) => {
            let code = LinuxError::from(error).code();
            (-(code as isize)) as *mut c_void
        }
    }
}

/// LinuxKPI `eventfd_ctx_get()`: acquire one additional reference.
///
/// # Safety
/// `ctx` must be a live pointer returned by `thekernel_eventfd_ctx_fdget` or
/// this function, with one outstanding owned reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thekernel_eventfd_ctx_get(ctx: *mut c_void) -> *mut c_void {
    if ctx.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: the caller guarantees that `ctx` still owns a live Arc reference.
    unsafe { Arc::increment_strong_count(ctx.cast::<EventFd>()) };
    ctx
}

/// Signal the existing VFS eventfd and wake its normal poll/read waiters.
///
/// # Safety
/// `ctx` must be a live pointer returned by the eventfd context helpers and
/// held for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thekernel_eventfd_signal(ctx: *mut c_void, count: u64) -> c_int {
    if ctx.is_null() {
        return -LinuxError::EINVAL.code();
    }
    // SAFETY: the caller owns a live eventfd context reference for this call.
    match unsafe { &*ctx.cast::<EventFd>() }.signal(count) {
        Ok(()) => 0,
        Err(error) => -LinuxError::from(error).code(),
    }
}

/// Drop one eventfd context reference.
///
/// # Safety
/// `ctx` must be a non-null pointer with one outstanding reference returned
/// by one of the eventfd context acquisition helpers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thekernel_eventfd_ctx_put(ctx: *mut c_void) {
    if !ctx.is_null() {
        // SAFETY: the caller transfers exactly one previously acquired Arc ref.
        drop(unsafe { Arc::from_raw(ctx.cast::<EventFd>()) });
    }
}

impl FileLike for EventFd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn read(&self, dst: &mut IoDst) -> axio::Result<usize> {
        self.read_with_nonblocking(dst, self.nonblocking())
    }

    fn write(&self, src: &mut IoSrc) -> axio::Result<usize> {
        self.write_with_nonblocking(src, self.nonblocking())
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, non_blocking: bool) -> axio::Result {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[eventfd]",
        )))
    }
}

impl Pollable for EventFd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let count = self.count.load(Ordering::Acquire);
        events.set(IoEvents::READABLE, count > 0);
        events.set(IoEvents::ERROR, false);
        events.set(IoEvents::WRITABLE, count < EVENTFD_COUNTER_MAX);
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        let read = events.intersects(IoEvents::READABLE | IoEvents::ERROR);
        let write = events.contains(IoEvents::WRITABLE);
        let mut prepared =
            axpoll::PreparedPollRegistration::try_new(read as usize + write as usize)?;
        if read {
            prepared.arm(&self.poll_rx, context.waker())?;
        }
        if write {
            prepared.arm(&self.poll_tx, context.waker())?;
        }
        prepared.commit()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use axio::{IoBuf, IoBufMut, Read, Write};

    use super::*;

    struct SliceSource {
        bytes: Vec<u8>,
        position: usize,
    }

    impl SliceSource {
        fn counter(value: u64) -> Self {
            Self {
                bytes: value.to_ne_bytes().to_vec(),
                position: 0,
            }
        }
    }

    impl Read for SliceSource {
        fn read(&mut self, destination: &mut [u8]) -> axio::Result<usize> {
            let source = &self.bytes[self.position..];
            let copied = source.len().min(destination.len());
            destination[..copied].copy_from_slice(&source[..copied]);
            self.position += copied;
            Ok(copied)
        }
    }

    impl IoBuf for SliceSource {
        fn remaining(&self) -> usize {
            self.bytes.len() - self.position
        }
    }

    struct SliceDestination {
        bytes: Vec<u8>,
        remaining: usize,
    }

    impl SliceDestination {
        fn counter() -> Self {
            Self {
                bytes: Vec::new(),
                remaining: size_of::<u64>(),
            }
        }

        fn value(&self) -> u64 {
            u64::from_ne_bytes(self.bytes.clone().try_into().unwrap())
        }
    }

    impl Write for SliceDestination {
        fn write(&mut self, source: &[u8]) -> axio::Result<usize> {
            self.bytes.extend_from_slice(source);
            self.remaining -= source.len();
            Ok(source.len())
        }

        fn flush(&mut self) -> axio::Result {
            Ok(())
        }
    }

    impl axio::IoBuf for SliceDestination {
        fn remaining(&self) -> usize {
            self.remaining
        }
    }

    impl IoBufMut for SliceDestination {
        fn remaining_mut(&self) -> usize {
            self.remaining
        }
    }

    fn read_counter(event: &EventFd) -> u64 {
        let mut destination = SliceDestination::counter();
        assert_eq!(event.read(&mut destination), Ok(size_of::<u64>()));
        destination.value()
    }

    #[test]
    fn counter_read_write_and_poll_follow_eventfd_contract() {
        let event = EventFd::new(0, false);
        event.set_nonblocking(true).unwrap();
        assert_eq!(event.poll(), IoEvents::WRITABLE);

        let mut two = SliceSource::counter(2);
        let mut three = SliceSource::counter(3);
        assert_eq!(event.write(&mut two), Ok(size_of::<u64>()));
        assert_eq!(event.write(&mut three), Ok(size_of::<u64>()));
        assert_eq!(event.poll(), IoEvents::READABLE | IoEvents::WRITABLE);
        assert_eq!(read_counter(&event), 5);
        assert_eq!(event.poll(), IoEvents::WRITABLE);
    }

    #[test]
    fn semaphore_reads_decrement_one_at_a_time() {
        let event = EventFd::new(3, true);
        event.set_nonblocking(true).unwrap();
        assert_eq!(read_counter(&event), 1);
        assert_eq!(read_counter(&event), 1);
        assert_eq!(read_counter(&event), 1);
        assert_eq!(
            event.read(&mut SliceDestination::counter()),
            Err(AxError::WouldBlock)
        );
    }

    #[test]
    fn invalid_or_noncanonical_writes_preserve_counter() {
        let event = EventFd::new(2, false);
        let mut maximum = SliceSource::counter(u64::MAX);
        assert_eq!(event.write(&mut maximum), Err(AxError::InvalidInput));
        assert_eq!(event.count.load(Ordering::Acquire), 2);

        let mut oversized = SliceSource {
            bytes: vec![0; size_of::<u64>() + 1],
            position: 0,
        };
        assert_eq!(event.write(&mut oversized), Err(AxError::InvalidInput));
        assert_eq!(event.count.load(Ordering::Acquire), 2);
    }

    #[test]
    fn opaque_eventfd_context_refs_share_the_vfs_counter() {
        let event = EventFd::new(0, false);
        let ctx = Arc::into_raw(event.clone()).cast_mut().cast::<c_void>();
        let duplicated = unsafe { thekernel_eventfd_ctx_get(ctx) };
        assert_eq!(duplicated, ctx);
        assert_eq!(unsafe { thekernel_eventfd_signal(ctx, 7) }, 0);
        assert_eq!(event.count.load(Ordering::Acquire), 7);

        unsafe { thekernel_eventfd_ctx_put(duplicated) };
        unsafe { thekernel_eventfd_ctx_put(ctx) };
        assert_eq!(Arc::strong_count(&event), 1);
    }

    #[test]
    fn eventfd_context_from_fd_signals_the_same_ofd() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let table = crate::file::FdTable::new().unwrap();
        assert!(matches!(table.get_description(i32::MAX), Err(AxError::BadFileDescriptor)));

        let event = EventFd::new(0, false);
        let fd = table.add_file_like(event.clone(), true, 64).unwrap();
        let file = table.get_description(fd).unwrap().file_handle().downcast::<EventFd>().unwrap();
        let ctx: *mut c_void = Arc::into_raw(file.clone_file_arc()).cast_mut().cast();
        drop(file);
        assert!(!ctx.is_null());
        assert!((ctx as isize) >= 0);
        assert_eq!(unsafe { thekernel_eventfd_signal(ctx, 5) }, 0);
        assert_eq!(event.count.load(Ordering::Acquire), 5);
        unsafe { thekernel_eventfd_ctx_put(ctx) };
        drop(table.close(fd).unwrap());
    }
}
