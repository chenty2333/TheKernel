//! Native Linux x86_64 ALSA hardware PCM/control ABI, shared by HDA and VirtIO.
//! One bounded RW-interleaved playback substream; no mmap/capture/mixer claims.
use super::*;

const BUFFER_FRAMES: u64 = (PERIOD * PERIODS / FRAME) as u64;
const PARAMS: usize = 608;
const PCM_INFO: u32 = 0x81204101;
const REFINE: u32 = 0xc2604110;
const HW_PARAMS: u32 = 0xc2604111;
const SW_PARAMS: u32 = 0xc0884113;
const STATUS: u32 = 0x80984120;
const STATUS_EXT: u32 = 0xc0984124;
const SYNC_PTR: u32 = 0xc0884123;
const WRITE_FRAMES: u32 = 0x40184150;

fn get32(b: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(b[offset..offset + 4].try_into().unwrap())
}
fn get64(b: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(b[offset..offset + 8].try_into().unwrap())
}
fn put32(b: &mut [u8], offset: usize, value: u32) {
    b[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}
fn put64(b: &mut [u8], offset: usize, value: u64) {
    b[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
}
fn input<const N: usize>(c: &IoctlContext, arg: usize) -> AxResult<[u8; N]> {
    let mut b = [0; N];
    c.user_memory()
        .read_into(arg as *const u8, &mut b)
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(b)
}
fn output(c: &IoctlContext, arg: usize, b: &[u8]) -> AxResult<usize> {
    c.user_memory()
        .write_bytes(arg, b)
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(0)
}
fn timestamp(b: &mut [u8], offset: usize) {
    let now = axhal::time::monotonic_time_nanos();
    put64(b, offset, now / 1_000_000_000);
    put64(b, offset + 8, now % 1_000_000_000);
}
fn pcm_info() -> [u8; 288] {
    let mut b = [0; 288];
    b[16..24].copy_from_slice(b"tk-audio");
    b[80..100].copy_from_slice(b"TheKernel analog PCM");
    put32(&mut b, 200, 1); // subdevices_count
    put32(&mut b, 204, u32::from(!CLAIMED.load(Ordering::Acquire)));
    b
}

// Intersect every advertised parameter, including open interval endpoints.
// Original byte-oriented implementation: only ABI layout/constants from UAPI.
fn refine(b: &mut [u8; PARAMS]) -> AxResult<()> {
    for (index, bit) in [3u32, 2, 0].into_iter().enumerate() {
        let offset = 4 + index * 32;
        if get32(b, offset) & (1 << bit) == 0 {
            return Err(AxError::InvalidInput);
        }
        b[offset..offset + 32].fill(0);
        put32(b, offset, 1 << bit);
    }
    // SAMPLE_BITS, FRAME_BITS, CHANNELS, RATE, PERIOD_TIME, PERIOD_SIZE,
    // PERIOD_BYTES, PERIODS, BUFFER_TIME, BUFFER_SIZE, BUFFER_BYTES, TICK_TIME.
    let values = [
        (16, 16),
        (32, 32),
        (2, 2),
        (48000, 48000),
        (21333, 21334),
        (1024, 1024),
        (4096, 4096),
        (4, 4),
        (85333, 85334),
        (4096, 4096),
        (16384, 16384),
        (0, 0),
    ];
    for (index, (low, high)) in values.into_iter().enumerate() {
        let offset = 260 + index * 12;
        let flags = get32(b, offset + 8);
        let min = get32(b, offset)
            .checked_add(flags & 1)
            .ok_or(AxError::InvalidInput)?;
        let max = get32(b, offset + 4)
            .checked_sub((flags >> 1) & 1)
            .ok_or(AxError::InvalidInput)?;
        let min = min.max(low);
        let max = max.min(high);
        if flags & 8 != 0 || min > max {
            return Err(AxError::InvalidInput);
        }
        put32(b, offset, min);
        put32(b, offset + 4, max);
        put32(b, offset + 8, 4); // integer, closed, nonempty
    }
    put32(b, 512, 0); // rmask fully processed
    put32(b, 516, 0x000f_ff07); // cmask: masks and all intervals
    put32(b, 520, 0x0001_0150); // INTERLEAVED | BLOCK_TRANSFER | BATCH
    put32(b, 524, 16);
    put32(b, 528, 48000);
    put32(b, 532, 1);
    put64(b, 536, 0);
    Ok(())
}

struct PcmState {
    phase: u32,
    staged: Vec<u8>,
    appl: u64,
    boundary: u64,
    avail_min: u64,
    start_threshold: u64,
}
impl PcmState {
    fn new() -> AxResult<Self> {
        let mut staged = Vec::new();
        staged
            .try_reserve_exact(PERIOD * PERIODS)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            phase: 0,
            staged,
            appl: 0,
            boundary: BUFFER_FRAMES * 2,
            avail_min: 1024,
            start_threshold: BUFFER_FRAMES,
        })
    }
    fn delay(&self, p: &Playback) -> u64 {
        ((p.state.lock().delay_bytes() + self.staged.len()) / FRAME) as u64
    }
    fn hw(&self, p: &Playback) -> u64 {
        self.appl.saturating_sub(self.delay(p)) % self.boundary
    }
    fn start(&mut self, p: &Playback) -> AxResult<()> {
        if self.phase != 2 {
            return Err(AxError::InvalidInput);
        }
        let mut sent = 0;
        while sent < self.staged.len() {
            sent += p.write_bytes(&self.staged[sent..])?;
        }
        self.staged.clear();
        self.phase = 3;
        Ok(())
    }
}
struct PcmFile {
    playback: Arc<Playback>,
    pcm: Mutex<PcmState>,
    nonblocking: AtomicBool,
    location: Location,
}
impl PcmFile {
    fn stop(&self) -> AxResult<()> {
        let mut pcm = self.pcm.lock();
        let mut s = self.playback.state.lock();
        if s.closing {
            return Err(AxError::Io);
        }
        if s.failed {
            // DROP is the explicit recovery point for a failed substream. Do
            // not discard tokens or staged audio unless the backend proves
            // that its DMA ownership has retired.
            if let Err(error) = s.abort_generation_with(|tokens| {
                axdriver::sound::abort(tokens).map_err(driver_error)
            }) {
                self.playback.ready.wake();
                return Err(error);
            }
        } else if (s.prepared || !s.tokens.is_empty())
            && let Err(error) = axdriver::sound::abort(&s.tokens)
        {
            s.failed = true;
            self.playback.ready.wake();
            return Err(driver_error(error));
        } else {
            s.tokens.clear();
            s.partial.clear();
            s.prepared = false;
        }
        pcm.staged.clear();
        pcm.appl = 0;
        pcm.phase = 1;
        self.playback.ready.wake();
        Ok(())
    }
    fn drain(&self) -> AxResult<()> {
        {
            let mut pcm = self.pcm.lock();
            if pcm.phase == 2 {
                pcm.start(&self.playback)?;
            }
            if pcm.phase != 3 && pcm.phase != 5 {
                return Err(AxError::InvalidInput);
            }
            pcm.phase = 5;
            self.playback.state.lock().submit_partial()?;
        }
        loop {
            let s = self.playback.state.lock();
            if s.failed {
                return Err(AxError::Io);
            }
            if s.tokens.is_empty() {
                break;
            }
            drop(s);
            if self.nonblocking() {
                return Err(AxError::WouldBlock);
            }
            axtask::sleep(Duration::from_millis(1))?;
        }
        // Backend release waits its audible DMA tail before STOP (HDA).
        let mut pcm = self.pcm.lock();
        self.playback
            .state
            .lock()
            .reset_with(|| axdriver::sound::release().map_err(driver_error))?;
        pcm.phase = 1;
        Ok(())
    }
    fn transfer(&self, bytes: &[u8]) -> AxResult<usize> {
        let mut pcm = self.pcm.lock();
        if self.playback.state.lock().failed {
            return Err(AxError::Io);
        }
        let n = match pcm.phase {
            2 => {
                let n = bytes.len().min(PERIOD * PERIODS - pcm.staged.len());
                if n == 0 {
                    return Err(AxError::WouldBlock);
                }
                pcm.staged.extend_from_slice(&bytes[..n]);
                n
            }
            3 => self.playback.write_bytes(bytes)?,
            _ => return Err(AxError::InvalidInput),
        };
        pcm.appl = pcm
            .appl
            .checked_add((n / FRAME) as u64)
            .ok_or(AxError::InvalidInput)?;
        if pcm.phase == 2 && pcm.staged.len() as u64 / FRAME as u64 >= pcm.start_threshold {
            pcm.start(&self.playback)?;
        }
        Ok(n)
    }
}
impl Drop for PcmFile {
    fn drop(&mut self) {
        self.playback.close();
    }
}
impl FileLike for PcmFile {
    fn uring_cmd_manifest(&self) -> &'static [crate::file::UringCmdManifest] {
        &[]
    }
    fn final_close(&self) {
        self.playback.close();
    }
    fn stat(&self) -> AxResult<Kstat> {
        crate::file::fs::location_to_kstat(&self.location)
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"/dev/snd/pcmC0D0p",
        )))
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
    fn ioctl(&self, c: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            0x80044100 => write_int(c, arg, 0x020012),
            PCM_INFO => output(c, arg, &pcm_info()),
            0x40044102..=0x40044104 => {
                let v = read_int(c, arg)?;
                if cmd == 0x40044103 && !(0..=2).contains(&v) {
                    return Err(AxError::InvalidInput);
                }
                Ok(0)
            }
            REFINE | HW_PARAMS => {
                let mut b = input::<PARAMS>(c, arg)?;
                refine(&mut b)?;
                if cmd == HW_PARAMS {
                    if self.pcm.lock().phase > 1 {
                        return Err(AxError::ResourceBusy);
                    }
                    self.stop()?;
                }
                output(c, arg, &b)
            }
            SW_PARAMS => {
                let mut b = input::<136>(c, arg)?;
                let boundary = get64(&b, 64);
                let avail_min = get64(&b, 16);
                if boundary < BUFFER_FRAMES * 2
                    || !boundary.is_multiple_of(BUFFER_FRAMES)
                    || avail_min == 0
                    || avail_min > BUFFER_FRAMES
                    || get32(&b, 0) > 1
                    || get64(&b, 48) != 0
                    || get64(&b, 56) != 0
                {
                    return Err(AxError::InvalidInput);
                }
                let mut pcm = self.pcm.lock();
                if pcm.phase == 0 {
                    return Err(AxError::InvalidInput);
                }
                pcm.boundary = boundary;
                pcm.avail_min = avail_min;
                pcm.start_threshold = get64(&b, 32);
                put32(&mut b, 8, 0); // obsolete sleep_min
                output(c, arg, &b)
            }
            0x4112 => {
                self.stop()?;
                self.pcm.lock().phase = 0;
                Ok(0)
            }
            0x4140 => {
                // PREPARE resets the same bounded substream
                if self.pcm.lock().phase == 0 {
                    return Err(AxError::InvalidInput);
                }
                self.stop()?;
                self.pcm.lock().phase = 2;
                Ok(0)
            }
            0x4142 => {
                self.pcm.lock().start(&self.playback)?;
                Ok(0)
            }
            0x4143 => {
                self.stop()?;
                Ok(0)
            }
            0x4144 => {
                self.drain()?;
                Ok(0)
            }
            0x4141 | 0x4122 => Ok(0),
            0x80084121 => {
                let delay = self.pcm.lock().delay(&self.playback);
                output(c, arg, &delay.to_ne_bytes())
            }
            SYNC_PTR => {
                let mut b = input::<136>(c, arg)?;
                let flags = get32(&b, 0);
                if flags & !7 != 0 {
                    return Err(AxError::InvalidInput);
                }
                let mut pcm = self.pcm.lock();
                if flags & 4 == 0 {
                    let min = get64(&b, 80);
                    if min == 0 || min > BUFFER_FRAMES {
                        return Err(AxError::InvalidInput);
                    }
                    pcm.avail_min = min;
                }
                // RW transfers own appl_ptr. Never trust a forged DMA pointer.
                if flags & 2 == 0 && get64(&b, 72) != pcm.appl % pcm.boundary {
                    return Err(AxError::InvalidInput);
                }
                b[8..72].fill(0);
                put32(&mut b, 8, pcm.phase);
                put64(&mut b, 16, pcm.hw(&self.playback));
                timestamp(&mut b, 24);
                put64(&mut b, 72, pcm.appl % pcm.boundary);
                put64(&mut b, 80, pcm.avail_min);
                output(c, arg, &b)
            }
            STATUS | STATUS_EXT => {
                let pcm = self.pcm.lock();
                let mut b = [0; 152];
                put32(&mut b, 0, pcm.phase);
                timestamp(&mut b, 24);
                put64(&mut b, 40, pcm.appl % pcm.boundary);
                put64(&mut b, 48, pcm.hw(&self.playback));
                let delay = pcm.delay(&self.playback).min(BUFFER_FRAMES);
                put64(&mut b, 56, delay);
                put64(&mut b, 64, BUFFER_FRAMES - delay);
                put64(&mut b, 72, BUFFER_FRAMES);
                output(c, arg, &b)
            }
            WRITE_FRAMES => {
                let mut b = input::<24>(c, arg)?;
                let frames = get64(&b, 16).min((PERIOD / FRAME) as u64) as usize;
                let mut data = [0; PERIOD];
                c.user_memory()
                    .read_into(get64(&b, 8) as *const u8, &mut data[..frames * FRAME])
                    .map_err(crate::mm::map_usercopy_error)?;
                let n = if frames == 0 {
                    0
                } else {
                    block_on_poll_io(self, IoEvents::WRITABLE, self.nonblocking(), || {
                        self.transfer(&data[..frames * FRAME])
                    })?
                };
                put64(&mut b, 0, (n / FRAME) as u64);
                output(c, arg, &b)
            }
            _ => Err(AxError::NotATty),
        }
    }
}
impl Pollable for PcmFile {
    fn poll(&self) -> IoEvents {
        let pcm = self.pcm.lock();
        if pcm.phase == 2 {
            if PERIOD * PERIODS - pcm.staged.len() >= pcm.avail_min as usize * FRAME {
                IoEvents::WRITABLE
            } else {
                IoEvents::empty()
            }
        } else {
            let s = self.playback.state.lock();
            if s.failed {
                IoEvents::ERROR
            } else if s.free_bytes() / FRAME >= pcm.avail_min as usize {
                IoEvents::WRITABLE
            } else {
                IoEvents::empty()
            }
        }
    }
    fn register<'a>(
        &'a self,
        cx: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.playback.register(cx, events)
    }
}

struct ControlFile {
    location: Location,
    ready: PollSet,
}
impl FileLike for ControlFile {
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"/dev/snd/controlC0",
        )))
    }
    fn set_nonblocking(&self, _: bool) -> AxResult {
        Ok(())
    }

    fn uring_cmd_manifest(&self) -> &'static [crate::file::UringCmdManifest] {
        &[]
    }
    fn stat(&self) -> AxResult<Kstat> {
        crate::file::fs::location_to_kstat(&self.location)
    }
    fn ioctl(&self, c: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            0x80045500 => write_int(c, arg, 0x020009),
            0x81785501 => {
                let mut b = [0; 376];
                b[8..15].copy_from_slice(b"TKAudio");
                b[24..32].copy_from_slice(b"tk-audio");
                b[40..55].copy_from_slice(b"TheKernel audio");
                b[72..90].copy_from_slice(b"TheKernel playback");
                output(c, arg, &b)
            }
            0xc0045530 => write_int(c, arg, if read_int(c, arg)? < 0 { 0 } else { -1 }),
            0x40045532 => {
                if read_int(c, arg)? > 0 {
                    return Err(AxError::NoSuchDevice);
                }
                Ok(0)
            }
            0xc1205531 => {
                let b = input::<288>(c, arg)?;
                if get32(&b, 0) != 0 || get32(&b, 4) != 0 || get32(&b, 8) != 0 {
                    return Err(AxError::NoSuchDevice);
                }
                output(c, arg, &pcm_info())
            }
            0xc0505510 => {
                let mut b = input::<80>(c, arg)?;
                put32(&mut b, 8, 0);
                put32(&mut b, 12, 0);
                output(c, arg, &b)
            }
            0xc0045516 => {
                let v = read_int(c, arg)?;
                write_int(c, arg, i32::from(v > 0))
            }
            _ => Err(AxError::NotATty),
        }
    }
}
impl Pollable for ControlFile {
    fn register<'a>(
        &'a self,
        cx: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::single(&self.ready, cx.waker())
    }

    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }
}

pub(crate) struct AlsaDevice {
    pub(crate) pcm: bool,
}
impl DeviceOps for AlsaDevice {
    fn open_description(&self, location: &Location, flags: u32) -> VfsResult<Option<DeviceOpen>> {
        if !self.pcm {
            let file = Arc::try_new(ControlFile {
                ready: PollSet::new(),
                location: location.clone(),
            })
            .map_err(|_| VfsError::NoMemory)?;
            return Ok(Some(DeviceOpen::new(file, None)));
        }
        if flags & linux_raw_sys::general::O_ACCMODE != linux_raw_sys::general::O_RDWR {
            return Err(VfsError::PermissionDenied);
        }
        if CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(VfsError::ResourceBusy);
        }
        let result: AxResult<Option<DeviceOpen>> = (|| {
            let playback = Playback::new()?;
            let file = Arc::try_new(PcmFile {
                playback: playback.clone(),
                pcm: Mutex::new(PcmState::new()?),
                nonblocking: AtomicBool::new(flags & linux_raw_sys::general::O_NONBLOCK != 0),
                location: location.clone(),
            })
            .map_err(|_| AxError::NoMemory)?;
            axtask::spawn_with_name(move || playback.run(), "alsa-playback".into())?;
            Ok(Some(DeviceOpen::new(file, None)))
        })();
        if result.is_err() {
            CLAIMED.store(false, Ordering::Release);
        }
        result
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
    fn read_at(&self, _: &mut [u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }
    fn write_at(&self, _: &[u8], _: u64) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn any() -> [u8; PARAMS] {
        let mut b = [0; PARAMS];
        b[4..100].fill(255);
        for i in 0..12 {
            put32(&mut b, 264 + i * 12, u32::MAX);
        }
        b
    }
    #[test]
    fn native_alsa_refines_only_actual_hardware_capabilities() {
        let mut b = any();
        refine(&mut b).unwrap();
        assert_eq!(get32(&b, 4), 8);
        assert_eq!(get32(&b, 36), 4);
        assert_eq!(get32(&b, 296), 48000);
        assert_eq!(get32(&b, 320), 1024);
        assert_eq!(get32(&b, 520) & 3, 0); // no MMAP/MMAP_VALID
        refine(&mut b).unwrap();
    }
    #[test]
    fn native_alsa_rejects_empty_or_unsupported_intersections() {
        let mut b = any();
        put32(&mut b, 36, 1 << 10);
        assert!(refine(&mut b).is_err());
        let mut b = any();
        put32(&mut b, 296, 44100);
        put32(&mut b, 300, 44100);
        assert!(refine(&mut b).is_err());
        let mut b = any();
        put32(&mut b, 284, 2);
        put32(&mut b, 288, 2);
        put32(&mut b, 292, 1);
        assert!(refine(&mut b).is_err());
    }
    #[test]
    fn native_alsa_prestart_pointers_and_buffer_are_bounded() {
        let p = Playback::new().unwrap();
        let mut pcm = PcmState::new().unwrap();
        pcm.phase = 2;
        pcm.appl = 1024;
        pcm.staged.extend_from_slice(&[7; PERIOD]);
        assert_eq!(pcm.delay(&p), 1024);
        assert_eq!(pcm.hw(&p), 0);
        pcm.staged.clear();
        p.state.lock().tokens.push(8);
        assert_eq!(pcm.hw(&p), 0);
        p.state.lock().tokens.clear();
        assert_eq!(pcm.hw(&p), 1024);
        pcm.appl = pcm.boundary + 1024;
        assert_eq!(pcm.hw(&p), 1024);
    }
    #[test]
    fn native_alsa_control_info_layout_is_zero_reserved() {
        let b = pcm_info();
        assert_eq!(get32(&b, 200), 1);
        assert!(b[224..].iter().all(|v| *v == 0));
    }
}
