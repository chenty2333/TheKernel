//! Bounded OSS playback endpoint for the existing VirtIO sound device.
//! PulseAudio owns mixing/conversion; the kernel accepts stereo S16LE at 48 kHz.

use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{
    any::Any,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, NodeFlags, VfsError, VfsResult};
use axio::prelude::*;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use spin::Mutex;

use crate::{
    file::{FileLike, IoSrc, IoctlContext, Kstat},
    pseudofs::{DeviceOpen, DeviceOps},
    readiness::block_on_poll_io,
};

const PERIOD: usize = 4096;
const PERIODS: usize = 4;
const FRAME: usize = 4;
const AFMT_S16_LE: i32 = 0x10;
// Linux x86_64 OSS UAPI; each scalar ioctl transfers a 32-bit int.
const RESET: u32 = 0x5000;
const SYNC: u32 = 0x5001;
const SPEED: u32 = 0xc0045002;
const STEREO: u32 = 0xc0045003;
const GETBLKSIZE: u32 = 0xc0045004;
const SETFMT: u32 = 0xc0045005;
const CHANNELS: u32 = 0xc0045006;
const POST: u32 = 0x5008;
const SETFRAGMENT: u32 = 0xc004500a;
const GETFMTS: u32 = 0x8004500b;
const GETOSPACE: u32 = 0x8010500c;
const GETCAPS: u32 = 0x8004500f;
const GETODELAY: u32 = 0x80045017;
static CLAIMED: AtomicBool = AtomicBool::new(false);

struct State {
    partial: Vec<u8>,
    tokens: Vec<u16>,
    prepared: bool,
    closing: bool,
    failed: bool,
}

impl State {
    fn free_bytes(&self) -> usize {
        (PERIODS - self.tokens.len()) * PERIOD - self.partial.len()
    }
    fn delay_bytes(&self) -> usize {
        self.tokens.len() * PERIOD + self.partial.len()
    }

    fn reset_with(&mut self, release: impl FnOnce() -> AxResult<()>) -> AxResult<()> {
        if self.failed || self.closing {
            return Err(AxError::Io);
        }
        if !self.tokens.is_empty() {
            return Err(AxError::ResourceBusy);
        }
        self.partial.clear();
        if self.prepared {
            if let Err(error) = release() {
                // STOP may have succeeded before RELEASE failed. The old
                // prepared state can no longer admit another playback write.
                self.failed = true;
                return Err(error);
            }
            self.prepared = false;
        }
        Ok(())
    }

    fn submit_partial(&mut self) -> AxResult<()> {
        if self.failed {
            return Err(AxError::Io);
        }
        if self.partial.is_empty() {
            return Ok(());
        }
        if self.tokens.len() == PERIODS {
            return Err(AxError::WouldBlock);
        }
        self.partial.resize(PERIOD, 0);
        let token = axdriver::sound::submit(&self.partial).map_err(driver_error)?;
        self.tokens.push(token);
        self.partial.clear();
        Ok(())
    }
}

fn driver_error(error: axdriver::prelude::DevError) -> AxError {
    match error {
        axdriver::prelude::DevError::Again => AxError::WouldBlock,
        axdriver::prelude::DevError::InvalidParam => AxError::InvalidInput,
        _ => AxError::Io,
    }
}

struct Playback {
    state: Mutex<State>,
    ready: PollSet,
}

impl Playback {
    fn new() -> AxResult<Arc<Self>> {
        let mut partial = Vec::new();
        partial
            .try_reserve_exact(PERIOD)
            .map_err(|_| AxError::NoMemory)?;
        let mut tokens = Vec::new();
        tokens
            .try_reserve_exact(PERIODS)
            .map_err(|_| AxError::NoMemory)?;
        Arc::try_new(Self {
            state: Mutex::new(State {
                partial,
                tokens,
                prepared: false,
                closing: false,
                failed: false,
            }),
            ready: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn close(&self) {
        self.state.lock().closing = true;
        self.ready.wake();
    }

    fn write_bytes(&self, bytes: &[u8]) -> AxResult<usize> {
        let mut s = self.state.lock();
        if s.failed || s.closing {
            return Err(AxError::Io);
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        if !bytes.len().is_multiple_of(FRAME) {
            return Err(AxError::InvalidInput);
        }
        if s.tokens.len() == PERIODS {
            return Err(AxError::WouldBlock);
        }
        if !s.prepared {
            if let Err(error) = axdriver::sound::prepare(PERIOD as _, PERIODS as _) {
                s.failed = true;
                return Err(driver_error(error));
            }
            s.prepared = true;
        }
        let count = bytes.len().min(PERIOD - s.partial.len());
        s.partial.extend_from_slice(&bytes[..count]);
        if s.partial.len() == PERIOD {
            if let Err(error) = s.submit_partial() {
                s.failed = true;
                return Err(error);
            }
        }
        Ok(count)
    }

    // One low-frequency polling worker owns DMA retirement and final close.
    // It never holds the state lock while sleeping; no audio-period spin wait.
    fn run(self: Arc<Self>) {
        let mut stalled = 0;
        loop {
            let mut done = false;
            let wake;
            {
                let mut s = self.state.lock();
                let previous = s.tokens.len();
                if !s.failed {
                    loop {
                        match axdriver::sound::complete() {
                            Ok(Some(token)) => {
                                if let Some(index) = s.tokens.iter().position(|t| *t == token) {
                                    s.tokens.remove(index);
                                } else {
                                    s.failed = true;
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(_) => {
                                s.failed = true;
                                break;
                            }
                        }
                    }
                }
                if s.closing && !s.failed {
                    if s.submit_partial().is_err() {
                        s.failed = true;
                    }
                    if s.tokens.is_empty() {
                        if !s.prepared || axdriver::sound::release().is_ok() {
                            CLAIMED.store(false, Ordering::Release);
                        }
                        done = true;
                    }
                }
                if s.tokens.is_empty() || s.tokens.len() < previous {
                    stalled = 0;
                } else {
                    stalled += 1;
                }
                if stalled >= 2000 {
                    s.failed = true;
                }
                // On hardware failure retain the global driver and its DMA buffers.
                // Do not let a new opener reuse an uncertain device generation.
                if s.failed && s.closing {
                    done = true;
                }
                wake = s.tokens.len() != previous || s.failed || done;
            }
            if wake {
                self.ready.wake();
            }
            if done {
                break;
            }
            if axtask::sleep(Duration::from_millis(1)).is_err() {
                self.state.lock().failed = true;
                self.ready.wake();
                break;
            }
        }
    }
}

impl Pollable for Playback {
    fn poll(&self) -> IoEvents {
        let s = self.state.lock();
        if s.failed {
            IoEvents::ERROR
        } else if s.closing {
            IoEvents::HANGUP
        } else if s.free_bytes() >= PERIOD {
            IoEvents::WRITABLE
        } else {
            IoEvents::empty()
        }
    }
    fn register<'a>(
        &'a self,
        cx: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::single(&self.ready, cx.waker())
    }
}

struct DspFile {
    playback: Arc<Playback>,
    nonblocking: AtomicBool,
    location: Location,
}

impl Drop for DspFile {
    fn drop(&mut self) {
        self.playback.close();
    }
}

fn read_int(context: &IoctlContext, arg: usize) -> AxResult<i32> {
    let mut bytes = [MaybeUninit::uninit(); 4];
    context
        .user_memory()
        .read_bytes(arg, &mut bytes)
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(i32::from_ne_bytes(
        bytes.map(|b| unsafe { b.assume_init() }),
    ))
}
fn write_int(context: &IoctlContext, arg: usize, value: i32) -> AxResult<usize> {
    context
        .user_memory()
        .write_bytes(arg, &value.to_ne_bytes())
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(0)
}

impl FileLike for DspFile {
    fn uring_cmd_manifest(&self) -> &'static [crate::file::UringCmdManifest] {
        &[]
    }
    fn final_close(&self) {
        self.playback.close();
    }
    fn write(&self, source: &mut IoSrc) -> AxResult<usize> {
        if !source.remaining().is_multiple_of(FRAME) {
            return Err(AxError::InvalidInput);
        }
        let count = source.remaining().min(PERIOD);
        let mut bytes = [0u8; PERIOD];
        // IoSrc is the operation-local cursor; the syscall reports only the
        // accepted count. Keep this scratch across readiness retries so an
        // EAGAIN never causes another read from the already advanced cursor.
        // User faults therefore occur without the playback spin lock held.
        source.read_exact(&mut bytes[..count])?;
        block_on_poll_io(
            self.playback.as_ref(),
            IoEvents::WRITABLE,
            self.nonblocking(),
            || self.playback.write_bytes(&bytes[..count]),
        )
    }
    fn stat(&self) -> AxResult<Kstat> {
        crate::file::fs::location_to_kstat(&self.location)
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(b"/dev/dsp")))
    }
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        {
            let state = self.playback.state.lock();
            if state.failed || state.closing {
                return Err(AxError::Io);
            }
        }
        match cmd {
            GETCAPS => write_int(context, arg, 0), // no mmap, capture, duplex or trigger
            GETFMTS | SETFMT => {
                if cmd == SETFMT {
                    let _ = read_int(context, arg)?;
                }
                write_int(context, arg, AFMT_S16_LE)
            }
            SPEED | CHANNELS | STEREO => {
                let _ = read_int(context, arg)?;
                write_int(
                    context,
                    arg,
                    match cmd {
                        SPEED => 48000,
                        CHANNELS => 2,
                        _ => 1,
                    },
                )
            }
            SETFRAGMENT => {
                let _ = read_int(context, arg)?;
                if self.playback.state.lock().prepared {
                    return Err(AxError::ResourceBusy);
                }
                write_int(context, arg, ((PERIODS as i32) << 16) | 12)
            }
            GETBLKSIZE => write_int(context, arg, PERIOD as _),
            GETODELAY => {
                let delay = self.playback.state.lock().delay_bytes();
                write_int(context, arg, delay as _)
            }
            GETOSPACE => {
                let bytes = self.playback.state.lock().free_bytes();
                let values = [bytes / PERIOD, PERIODS, PERIOD, bytes];
                let mut output = [0u8; 16];
                for (chunk, value) in output.chunks_exact_mut(4).zip(values) {
                    chunk.copy_from_slice(&(value as i32).to_ne_bytes());
                }
                context
                    .user_memory()
                    .write_bytes(arg, &output)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            POST | SYNC => {
                self.playback.state.lock().submit_partial()?;
                if cmd == SYNC {
                    loop {
                        {
                            let s = self.playback.state.lock();
                            if s.failed {
                                return Err(AxError::Io);
                            }
                            if s.tokens.is_empty() {
                                break;
                            }
                        }
                        axtask::sleep(Duration::from_millis(1))?;
                    }
                }
                Ok(0)
            }
            RESET => {
                let result = self.playback.state.lock().reset_with(|| {
                    axdriver::sound::release().map_err(driver_error)
                });
                if result.is_err() {
                    self.playback.ready.wake();
                }
                result?;
                Ok(0)
            }
            _ => Err(AxError::NotATty),
        }
    }
}

impl Pollable for DspFile {
    fn poll(&self) -> IoEvents {
        self.playback.poll()
    }
    fn register<'a>(
        &'a self,
        cx: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.playback.register(cx, events)
    }
}

pub(crate) struct Dsp;
impl DeviceOps for Dsp {
    fn open_description(&self, location: &Location, flags: u32) -> VfsResult<Option<DeviceOpen>> {
        if flags & linux_raw_sys::general::O_ACCMODE != linux_raw_sys::general::O_WRONLY {
            return Err(VfsError::PermissionDenied);
        }
        if !axdriver::sound::available() {
            return Err(VfsError::NoSuchDevice);
        }
        if CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(VfsError::ResourceBusy);
        }
        let result: AxResult<Option<DeviceOpen>> = (|| {
            let playback = Playback::new()?;
            let file = Arc::try_new(DspFile {
                playback: playback.clone(),
                location: location.clone(),
                nonblocking: AtomicBool::new(flags & linux_raw_sys::general::O_NONBLOCK != 0),
            })
            .map_err(|_| AxError::NoMemory)?;
            axtask::spawn_with_name(move || playback.run(), "sound-playback".into())?;
            let file: Arc<dyn FileLike> = file;
            Ok(Some(DeviceOpen::new(file, None)))
        })();
        if result.is_err() {
            CLAIMED.store(false, Ordering::Release);
        }
        result.map_err(VfsError::from)
    }
    fn read_at(&self, _: &mut [u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_SEEK
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_failure_cannot_reuse_a_stopped_stream() {
        let playback = Playback::new().unwrap();
        {
            let mut state = playback.state.lock();
            state.prepared = true;
            state.tokens.push(1);
            assert_eq!(
                state.reset_with(|| panic!("live DMA must be retained")),
                Err(AxError::ResourceBusy)
            );
            assert!(!state.failed);
            state.tokens.clear();
            assert_eq!(state.reset_with(|| Err(AxError::Io)), Err(AxError::Io));
        }
        assert_eq!(playback.write_bytes(&[0; FRAME]), Err(AxError::Io));
        assert_eq!(playback.poll(), IoEvents::ERROR);
    }

    #[test]
    fn reset_releases_a_drained_stream_before_reprepare() {
        let playback = Playback::new().unwrap();
        let mut state = playback.state.lock();
        state.prepared = true;
        state.partial.extend_from_slice(&[0; FRAME]);
        state.reset_with(|| Ok(())).unwrap();
        assert!(!state.prepared);
        assert!(state.partial.is_empty());
        state
            .reset_with(|| panic!("unprepared stream must not be released again"))
            .unwrap();
    }
    #[test]
    fn incomplete_frames_and_failed_streams_never_reach_the_device() {
        let playback = Playback::new().unwrap();
        let incomplete = &[1u8, 2, 3][..];
        assert_eq!(playback.write_bytes(incomplete), Err(AxError::InvalidInput));
        assert_eq!(incomplete.len(), 3);
        assert!(!playback.state.lock().prepared);
        playback.state.lock().failed = true;
        let frame = &[0u8; FRAME][..];
        assert_eq!(playback.write_bytes(frame), Err(AxError::Io));
        assert_eq!(frame.len(), FRAME);
        assert_eq!(playback.poll(), IoEvents::ERROR);
    }

    #[test]
    fn poll_backpressure_and_close_follow_period_ownership() {
        let playback = Playback::new().unwrap();
        assert_eq!(playback.poll(), IoEvents::WRITABLE);
        playback.state.lock().tokens.extend([1, 2, 3, 4]);
        assert!(playback.poll().is_empty());
        playback.state.lock().tokens.remove(0);
        assert_eq!(playback.poll(), IoEvents::WRITABLE);
        playback.close();
        assert_eq!(playback.poll(), IoEvents::HANGUP);
    }

    #[test]
    fn capacity_accounts_for_dma_and_partial_frames() {
        let mut s = State {
            partial: Vec::new(),
            tokens: Vec::new(),
            prepared: false,
            closing: false,
            failed: false,
        };
        assert_eq!(s.free_bytes(), 16384);
        s.tokens.extend([1, 2, 3]);
        s.partial.extend([0; 128]);
        assert_eq!(s.free_bytes(), PERIOD - 128);
        assert_eq!(s.delay_bytes(), 3 * PERIOD + 128);
        s.partial.clear();
        s.tokens.push(4);
        assert_eq!(s.free_bytes(), 0);
        s.tokens.remove(0);
        assert_eq!(s.free_bytes(), PERIOD);
    }
}
