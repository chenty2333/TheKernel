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
    pending: VecDeque<(u16, usize)>,
    retired: VecDeque<u16>,
    tail: usize,
    position: usize,
    running: bool,
    prepared: bool,
    live: bool,
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
        c.bus.write(regs::CORBCTL, 1, 2);
        let present = c.bus.read(regs::STATESTS, 2) as u16;
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
    pub fn prepare(&mut self, period: u32, periods: u32) -> DevResult {
        if !self.live || period as usize != PERIOD || periods as usize != PERIODS {
            return Err(DevError::InvalidParam);
        }
        if !self.pending.is_empty() {
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
        if !self.live || !self.prepared || bytes.len() != PERIOD {
            return Err(DevError::InvalidParam);
        }
        if self.pending.len() == PERIODS {
            return Err(DevError::Again);
        }
        if self.pending.is_empty() && self.running {
            let retired = core::mem::take(&mut self.retired);
            self.prepare(PERIOD as u32, PERIODS as u32)?;
            self.retired = retired;
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
            self.bus.write(self.stream, 1, 2);
            self.running = true;
            self.last_poll = self.bus.now_ns();
        }
        Ok(token)
    }
    pub fn complete(&mut self) -> DevResult<Option<u16>> {
        if !self.live {
            return Err(DevError::Io);
        }
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
        Ok(self.retired.pop_front())
    }
    fn stop(&mut self) -> DevResult {
        self.bus.write(self.stream, 1, 0);
        self.running = false;
        wait(&mut self.bus, self.stream, 1, 2, 0)
    }
    pub fn release(&mut self) -> DevResult {
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
}
impl<H: Hal, B: Bus> Verbs for Controller<H, B> {
    fn verb(&mut self, codec: u8, node: u8, operation: u16, payload: u16) -> DevResult<u32> {
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
impl<H: Hal, B: Bus> Drop for Controller<H, B> {
    fn drop(&mut self) {
        let stopped = self.stop().is_ok();
        self.bus.write(regs::CORBCTL, 1, 0);
        self.bus.write(regs::RIRBCTL, 1, 0);
        self.bus.write(regs::GCTL, 4, 0);
        if stopped && wait(&mut self.bus, regs::GCTL, 4, 1, 0).is_ok() {
            self.corb.safe = true;
            self.rirb.safe = true;
            self.bdl.safe = true;
            self.audio.safe = true;
        }
    }
}
