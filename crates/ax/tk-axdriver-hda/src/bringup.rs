//! CORB/RIRB controller and one bounded analog playback DMA stream.
use alloc::collections::VecDeque;
use core::{
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{Ordering, fence},
};

use axdriver_base::{DevError, DevResult};

use crate::{
    Hal,
    codec::{self, Route, Verbs},
    desc::{BufferDescriptor, FORMAT, PERIOD, PERIODS, verb},
    eld::Eld,
    regs::{self, Bus},
};
struct Dma<H: Hal> {
    address: u64,
    pointer: NonNull<u8>,
    pages: usize,
    safe: bool,
    hal: PhantomData<H>,
}
// SAFETY: allocations are owned by a controller whose mutation is serialized.
unsafe impl<H: Hal> Send for Dma<H> {}
// SAFETY: shared access never exposes mutable DMA memory.
unsafe impl<H: Hal> Sync for Dma<H> {}
impl<H: Hal> Dma<H> {
    fn new(pages: usize) -> DevResult<Self> {
        let (address, pointer) = H::allocate(pages).ok_or(DevError::NoMemory)?;
        if address == 0 || address & 4095 != 0 || pointer.as_ptr() as usize & 4095 != 0 {
            // SAFETY: allocation has not been published to hardware.
            unsafe {
                H::release(address, pointer, pages);
            }
            return Err(DevError::InvalidParam);
        }
        // SAFETY: owned writable allocation covers pages * 4096 bytes.
        unsafe {
            pointer.as_ptr().write_bytes(0, pages * 4096);
        }
        Ok(Self {
            address,
            pointer,
            pages,
            safe: true,
            hal: PhantomData,
        })
    }
}
impl<H: Hal> Drop for Dma<H> {
    fn drop(&mut self) {
        if self.safe {
            // SAFETY: device reset/stop has proved DMA retirement, or never published.
            unsafe {
                H::release(self.address, self.pointer, self.pages);
            }
        }
    }
}
pub struct Controller<H: Hal, B: Bus> {
    bus: B,
    corb: Dma<H>,
    rirb: Dma<H>,
    bdl: Dma<H>,
    audio: Dma<H>,
    wp: u16,
    rp: u16,
    stream: usize,
    route: Option<Route>,
    present: u16,
    hdmi_route: Option<Route>,
    hdmi_eld: Option<Eld>,
    route_invalidated: bool,
    pending: VecDeque<(u16, usize)>,
    retired: VecDeque<u16>,
    tail: usize,
    position: usize,
    running: bool,
    prepared: bool,
    live: bool,
    shutdown: bool,
    sequence: u16,
    last_poll: u64,
}
impl<H: Hal, B: Bus> Controller<H, B> {
    pub fn new(mut bus: B) -> DevResult<Self> {
        let gcap = bus.read(regs::GCAP, 2);
        if gcap >> 12 == 0 || gcap & 1 == 0 {
            return Err(DevError::Unsupported);
        }
        bus.write(regs::GCTL, 4, 0);
        wait(&mut bus, regs::GCTL, 4, 1, 0)?;
        bus.delay_us(100);
        bus.write(regs::GCTL, 4, 1);
        wait(&mut bus, regs::GCTL, 4, 1, 1)?;
        bus.delay_us(521);
        let mut c = Self {
            bus,
            corb: Dma::new(1)?,
            rirb: Dma::new(1)?,
            bdl: Dma::new(1)?,
            audio: Dma::new(PERIODS)?,
            wp: 0,
            rp: 0,
            stream: 0x80 + ((gcap >> 8) & 15) as usize * 0x20,
            route: None,
            pending: VecDeque::new(),
            retired: VecDeque::new(),
            tail: 0,
            position: 0,
            running: false,
            prepared: false,
            live: true,
            shutdown: false,
            present: 0,
            hdmi_route: None,
            hdmi_eld: None,
            route_invalidated: false,
            sequence: 0,
            last_poll: 0,
        };
        c.bus.write(regs::CORBCTL, 1, 0);
        c.bus.write(regs::RIRBCTL, 1, 0);
        wait(&mut c.bus, regs::CORBCTL, 1, 2, 0)?;
        wait(&mut c.bus, regs::RIRBCTL, 1, 2, 0)?;
        if c.bus.read(regs::CORBSIZE, 1) & 64 == 0 || c.bus.read(regs::RIRBSIZE, 1) & 64 == 0 {
            return Err(DevError::Unsupported);
        }
        c.bus.write(regs::CORBSIZE, 1, 2);
        c.bus.write(regs::RIRBSIZE, 1, 2);
        c.address(regs::CORB, c.corb.address);
        c.address(regs::RIRB, c.rirb.address);
        c.bus.write(regs::CORBWP, 2, 0);
        c.bus.write(regs::CORBRP, 2, 0x8000);
        // ICH controllers may self-clear the reset; both outcomes retire RP to zero.
        let reset = c.bus.read(regs::CORBRP, 2);
        if reset & 0x8000 != 0 {
            wait(&mut c.bus, regs::CORBRP, 2, 0x8000, 0x8000)?;
            c.bus.write(regs::CORBRP, 2, 0);
        }
        wait(&mut c.bus, regs::CORBRP, 2, 0xffff, 0)?;
        c.bus.write(regs::RIRBWP, 2, 0x8000);
        c.bus.write(regs::RINTCNT, 2, 1);
        c.bus.write(regs::RIRBSTS, 1, 5);
        c.corb.safe = false;
        c.rirb.safe = false;
        // Enable response-status generation, but keep global interrupt delivery off.
        // QEMU throttles CORB at RINTCNT until that status is acknowledged.
        c.bus.write(0x20, 4, 0);
        c.bus.write(regs::RIRBCTL, 1, 3);
        // Do not issue codec verbs until the response DMA engine has accepted
        // its enable bits. CORB may otherwise run without a consumer for the
        // responses it generates.
        wait(&mut c.bus, regs::RIRBCTL, 1, 3, 3)?;
        c.bus.write(regs::CORBCTL, 1, 2);
        // Likewise, a posted/ignored write must not be mistaken for an
        // operational command ring.
        wait(&mut c.bus, regs::CORBCTL, 1, 2, 2)?;
        let present = c.bus.read(regs::STATESTS, 2) as u16;
        c.present = present;
        let route = codec::enumerate(&mut c, present)?;
        let converter = route.path.last().ok_or(DevError::Unsupported)?;
        if converter.caps & 1 == 0 {
            return Err(DevError::Unsupported);
        }
        let format_node = if converter.caps & 16 != 0 {
            converter.node
        } else {
            route.function
        };
        let pcm = c.verb(route.codec, format_node, 0xf00, 0xa)?;
        if pcm & (1 << 6) == 0
            || pcm & (1 << 17) == 0
            || c.verb(route.codec, format_node, 0xf00, 0xb)? & 1 == 0
        {
            return Err(DevError::Unsupported);
        }
        codec::configure(&mut c, &route)?;
        c.route = Some(route);
        Ok(c)
    }
    fn address(&mut self, offset: usize, address: u64) {
        self.bus.write(offset, 4, address as u32);
        self.bus.write(offset + 4, 4, (address >> 32) as u32);
    }
    pub fn route(&self) -> &Route {
        self.route.as_ref().unwrap()
    }

    /// Publish/unpublish ELD for one already active DDI HDMI route.
    ///
    /// ADL-P's display-codec pin topology is confirmed by the enumerated widget
    /// capabilities and route before any converter or pin verb is changed.
    /// Changing the endpoint stops the owned stream first; if a submitted period
    /// was retired by that transition, the extant playback owner sees one I/O
    /// error rather than waiting forever for a token that can no longer arrive.
    pub fn set_display_eld(&mut self, port: u8, bytes: Option<&[u8]>) -> DevResult {
        if !self.live || self.shutdown {
            return Err(DevError::Io);
        }
        let pin = display_pin_for_port(port)?;
        let Some(bytes) = bytes else {
            if self
                .hdmi_route
                .as_ref()
                .is_some_and(|route| route.path.first().is_some_and(|w| w.node == pin))
            {
                return self.restore_analog_route();
            }
            return Ok(());
        };
        let eld = Eld::parse(bytes)?;
        if self.hdmi_route.as_ref().is_some_and(|active| {
            active.path.first().map(|w| w.node) == Some(pin) && self.hdmi_eld.as_ref() == Some(&eld)
        }) {
            return Ok(());
        }
        self.stop_for_route_change()?;
        let route = match codec::enumerate_hdmi(self, self.present, pin) {
            Ok(route) => route,
            Err(error) => {
                if self.hdmi_route.is_some() {
                    self.restore_analog_route()?;
                }
                return Err(error);
            }
        };
        let converter = route.path.last().ok_or(DevError::Unsupported)?;
        let format_node = if converter.caps & 16 != 0 {
            converter.node
        } else {
            route.function
        };
        let pcm = self.verb(route.codec, format_node, 0xf00, 0xa)?;
        if pcm & (1 << 6) == 0
            || pcm & (1 << 17) == 0
            || self.verb(route.codec, format_node, 0xf00, 0xb)? & 1 == 0
        {
            if self.hdmi_route.is_some() {
                self.restore_analog_route()?;
            }
            return Err(DevError::Unsupported);
        }
        if let Some(previous) = self.hdmi_route.clone() {
            if codec::disable_hdmi(self, &previous).is_err() {
                self.live = false;
                return Err(DevError::Io);
            }
        } else if let Some(analog) = self.route.clone()
            && codec::disable_analog(self, &analog).is_err()
        {
            self.live = false;
            return Err(DevError::Io);
        }
        if codec::configure_hdmi(self, &route).is_err() {
            let restored = self
                .route
                .clone()
                .is_some_and(|analog| codec::configure(self, &analog).is_ok());
            if !restored {
                self.live = false;
            }
            return Err(DevError::Io);
        }
        self.hdmi_route = Some(route);
        self.hdmi_eld = Some(eld);
        Ok(())
    }

    fn stop_for_route_change(&mut self) -> DevResult {
        let invalidate_playback = self.prepared || self.running || !self.pending.is_empty();
        if invalidate_playback {
            self.abort()?;
            self.route_invalidated = true;
        }
        Ok(())
    }

    fn restore_analog_route(&mut self) -> DevResult {
        self.stop_for_route_change()?;
        let Some(route) = self.hdmi_route.clone() else {
            return Ok(());
        };
        if codec::disable_hdmi(self, &route).is_err() {
            self.live = false;
            return Err(DevError::Io);
        }
        let Some(analog) = self.route.clone() else {
            self.live = false;
            return Err(DevError::BadState);
        };
        if codec::configure(self, &analog).is_err() {
            self.live = false;
            return Err(DevError::Io);
        }
        self.hdmi_route = None;
        self.hdmi_eld = None;
        Ok(())
    }
    pub fn prepare(&mut self, period: u32, periods: u32) -> DevResult {
        if self.shutdown {
            return Err(DevError::BadState);
        }
        if !self.live || period as usize != PERIOD || periods as usize != PERIODS {
            return Err(DevError::InvalidParam);
        }
        if !self.pending.is_empty() || !self.retired.is_empty() {
            return Err(DevError::ResourceBusy);
        }
        self.stop()?;
        self.bus.write(self.stream, 1, 1);
        wait(&mut self.bus, self.stream, 1, 1, 1)?;
        self.bus.write(self.stream, 1, 0);
        wait(&mut self.bus, self.stream, 1, 1, 0)?;
        self.tail = 0;
        self.position = 0;
        self.retired.clear();
        // SAFETY: stopped stream does not own BDL/audio memory.
        unsafe {
            self.audio.pointer.as_ptr().write_bytes(0, PERIOD * PERIODS);
            for index in 0..PERIODS {
                self.bdl
                    .pointer
                    .as_ptr()
                    .cast::<BufferDescriptor>()
                    .add(index)
                    .write(BufferDescriptor {
                        address: self.audio.address + (index * PERIOD) as u64,
                        length: PERIOD as u32,
                        flags: 1,
                    });
            }
        }
        self.address(self.stream + 0x18, self.bdl.address);
        self.bus
            .write(self.stream + 8, 4, (PERIOD * PERIODS) as u32);
        self.bus.write(self.stream + 0xc, 2, (PERIODS - 1) as u32);
        self.bus.write(self.stream + 0x12, 2, u32::from(FORMAT));
        self.bus.write(self.stream + 2, 1, 0x10);
        self.bus.write(self.stream + 3, 1, 0x1c);
        self.prepared = true;
        self.bdl.safe = false;
        self.audio.safe = false;
        Ok(())
    }
    pub fn submit(&mut self, bytes: &[u8]) -> DevResult<u16> {
        // A route change invalidates tokens from the previous stream. Require
        // its one-shot completion error to be observed before accepting data
        // for the replacement route, or that error could be attributed to a
        // newly submitted period.
        if self.route_invalidated {
            return Err(DevError::Io);
        }
        if self.shutdown {
            return Err(DevError::BadState);
        }
        if !self.live || !self.prepared || bytes.len() != PERIOD {
            return Err(DevError::InvalidParam);
        }
        // Observe hardware progress before reusing any ring slot. A period may
        // have completed since the caller's last poll; accept no new data
        // until its completion token has been collected.
        if self.running {
            self.poll_stream_position()?;
            if !self.retired.is_empty() {
                return Err(DevError::Again);
            }
        }
        if self.pending.len() == PERIODS {
            return Err(DevError::Again);
        }
        if self.pending.is_empty() && self.running {
            self.prepare(PERIOD as u32, PERIODS as u32)?;
        }
        let slot = self.tail;
        if self.running && (self.bus.read(self.stream + 4, 4) as usize / PERIOD) % PERIODS == slot {
            return Err(DevError::Again);
        }
        self.sequence = self.sequence.wrapping_add(1);
        let token = self.sequence;
        // SAFETY: this slot is not one of the pending DMA descriptors, and copy is bounded.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.audio.pointer.as_ptr().add(slot * PERIOD),
                PERIOD,
            );
        }
        fence(Ordering::SeqCst);
        self.pending.push_back((token, slot));
        self.tail = (slot + 1) % PERIODS;
        if !self.running {
            // Treat an unacknowledged RUN write as uncertain ownership: the
            // device may still have started DMA, so poison normal operations
            // and keep the slot allocated until teardown proves quiescence.
            self.running = true;
            self.bus.write(self.stream, 1, 2);
            if let Err(error) = wait(&mut self.bus, self.stream, 1, 2, 2) {
                self.live = false;
                return Err(error);
            }
            self.last_poll = self.bus.now_ns();
        }
        Ok(token)
    }
    pub fn complete(&mut self) -> DevResult<Option<u16>> {
        if !self.live || self.shutdown {
            return Err(DevError::Io);
        }
        if self.route_invalidated {
            self.route_invalidated = false;
            return Err(DevError::Io);
        }
        self.poll_stream_position()?;
        Ok(self.retired.pop_front())
    }

    fn poll_stream_position(&mut self) -> DevResult {
        if self.running {
            let now = self.bus.now_ns();
            if now.saturating_sub(self.last_poll) >= 80_000_000 {
                self.live = false;
                self.stop()?;
                return Err(DevError::Io);
            }
            self.last_poll = now;
            if self.bus.read(self.stream + 3, 1) & 0x18 != 0 {
                self.live = false;
                self.stop()?;
                return Err(DevError::Io);
            }
            let position = (self.bus.read(self.stream + 4, 4) as usize / PERIOD) % PERIODS;
            while self.position != position {
                if let Some((token, slot)) = self.pending.front().copied() {
                    if slot != self.position {
                        self.live = false;
                        self.stop()?;
                        return Err(DevError::BadState);
                    }
                    self.pending.pop_front();
                    self.retired.push_back(token);
                    // SAFETY: LPIB progressed beyond this descriptor; zero before the next lap.
                    unsafe {
                        self.audio
                            .pointer
                            .as_ptr()
                            .add(self.position * PERIOD)
                            .write_bytes(0, PERIOD);
                    }
                }
                self.position = (self.position + 1) % PERIODS;
            }
        }
        Ok(())
    }
    fn stop(&mut self) -> DevResult {
        self.bus.write(self.stream, 1, 0);
        if let Err(error) = wait(&mut self.bus, self.stream, 1, 2, 0) {
            // Keep ownership and the running state intact: the device may
            // still be fetching BDL/audio memory. Poison normal operations
            // until teardown can prove a stop or reset.
            self.live = false;
            return Err(error);
        }
        self.running = false;
        Ok(())
    }
    /// Cancel playback only after RUN readback proves that DMA stopped.
    pub fn abort(&mut self) -> DevResult {
        if self.shutdown {
            return Err(DevError::BadState);
        }
        if let Err(error) = self.stop() {
            self.live = false;
            return Err(error);
        }
        self.pending.clear();
        self.retired.clear();
        // SAFETY: STOP readback proved retirement before reusing owned slots.
        unsafe {
            self.audio.pointer.as_ptr().write_bytes(0, PERIOD * PERIODS);
        }
        self.prepared = false;
        // An explicit abort has acknowledged and retired the invalidated
        // stream generation. Route changes call abort first, then set this
        // again to deliver their one-shot interruption to the old owner.
        self.route_invalidated = false;
        Ok(())
    }
    pub fn release(&mut self) -> DevResult {
        if self.shutdown {
            return Err(DevError::BadState);
        }
        if !self.pending.is_empty() {
            return Err(DevError::ResourceBusy);
        }
        // DMA retirement precedes audible FIFO retirement. Keep a silent
        // tail running before stopping the stream, including codec prefetch.
        // Two periods also cover QEMU's output FIFO; GCAP/FIFOS alone does
        // not describe an emulated backend's additional queued samples.
        if self.running {
            let fifo = self.bus.read(self.stream + 0x10, 2).saturating_add(1);
            let bytes = fifo.max((PERIOD * 2) as u32);
            self.bus
                .delay_us((u64::from(bytes) * 1_000_000).div_ceil(192_000) as u32);
        }
        self.stop()?;
        self.prepared = false;
        Ok(())
    }

    /// Permanently stop all HDA DMA engines before allowing their buffers to be
    /// released. Each allocation is retired only after its own engine's stop
    /// readback; Drop retries shutdown but never treats timeout as proof.
    pub fn shutdown(&mut self) -> DevResult {
        if self.shutdown {
            return Ok(());
        }

        let stream_stopped = self.stop().is_ok();
        self.bus.write(regs::CORBCTL, 1, 0);
        self.bus.write(regs::RIRBCTL, 1, 0);
        let corb_stopped = wait(&mut self.bus, regs::CORBCTL, 1, 2, 0).is_ok();
        let rirb_stopped = wait(&mut self.bus, regs::RIRBCTL, 1, 3, 0).is_ok();
        self.bus.write(regs::GCTL, 4, 0);
        let controller_reset = wait(&mut self.bus, regs::GCTL, 4, 1, 0).is_ok();

        // Retire each allocation according to its owning engine's explicit
        // stop readback. A reset failure must not leak buffers whose engine is
        // known stopped, but it must not release memory still reachable by an
        // engine that ignored its stop command.
        if corb_stopped {
            self.corb.safe = true;
        }
        if rirb_stopped {
            self.rirb.safe = true;
        }
        if stream_stopped {
            self.bdl.safe = true;
            self.audio.safe = true;
        }

        self.live = false;
        if !(stream_stopped && corb_stopped && rirb_stopped && controller_reset) {
            return Err(DevError::Io);
        }

        // No token can still refer to a hardware-owned buffer after every
        // engine acknowledged stop and the controller is held in reset.
        self.pending.clear();
        self.retired.clear();
        self.prepared = false;
        self.running = false;
        self.route_invalidated = false;
        self.shutdown = true;
        Ok(())
    }
}
impl<H: Hal, B: Bus> Verbs for Controller<H, B> {
    fn verb(&mut self, codec: u8, node: u8, operation: u16, payload: u16) -> DevResult<u32> {
        if codec >= 15 || node >= 128 || operation > 0xfff {
            return Err(DevError::InvalidParam);
        }
        if !self.live {
            return Err(DevError::BadState);
        }
        self.wp = (self.wp + 1) & 255;
        // SAFETY: 256 4-byte CORB entries fit one page and previous command completed.
        unsafe {
            self.corb
                .pointer
                .as_ptr()
                .cast::<u32>()
                .add(usize::from(self.wp))
                .write_volatile(verb(codec, node, operation, payload));
        }
        fence(Ordering::SeqCst);
        self.bus.write(regs::CORBWP, 2, u32::from(self.wp));
        for _ in 0..25_000 {
            let end = self.bus.read(regs::RIRBWP, 2) as u16 & 255;
            while self.rp != end {
                self.rp = (self.rp + 1) & 255;
                fence(Ordering::SeqCst);
                // SAFETY: RIRB WP published this 8-byte response in our DMA page.
                let response = unsafe {
                    self.rirb
                        .pointer
                        .as_ptr()
                        .cast::<u64>()
                        .add(usize::from(self.rp))
                        .read_volatile()
                };
                let extra = (response >> 32) as u32;
                if extra & 16 == 0 && extra & 15 == u32::from(codec) {
                    self.bus.write(regs::RIRBSTS, 1, 1);
                    return Ok(response as u32);
                }
            }
            if self.bus.read(regs::RIRBSTS, 1) & 4 != 0 {
                self.live = false;
                return Err(DevError::Io);
            }
            self.bus.delay_us(10);
        }
        self.live = false;
        Err(DevError::Io)
    }
}
fn wait(bus: &mut impl Bus, offset: usize, width: usize, mask: u32, value: u32) -> DevResult {
    for _ in 0..10_000 {
        if bus.read(offset, width) & mask == value {
            return Ok(());
        }
        bus.delay_us(10);
    }
    Err(DevError::Io)
}

fn display_pin_for_port(port: u8) -> DevResult<u8> {
    // The ADL-P HDA codec's port map assigns PORT_D/TC1 and PORT_E/TC2 to
    // widget NIDs 0x0a and 0x0b. `enumerate_hdmi` must still confirm the live
    // codec vendor, pin capabilities, default connectivity and digital route.
    match port {
        3 => Ok(0x0a),
        4 => Ok(0x0b),
        _ => Err(DevError::Unsupported),
    }
}
impl<H: Hal, B: Bus> Drop for Controller<H, B> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
