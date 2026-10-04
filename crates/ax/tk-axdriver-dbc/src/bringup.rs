use core::sync::atomic::{Ordering, fence};

use crate::{Bus, Error, desc::*, ids, regs::*};
const QUEUE: usize = 8192;
const HANDSHAKE: usize = 1000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    Running,
    Failed(Error),
}
#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub events: u64,
}
struct Ring {
    base: usize,
    next: usize,
    cycle: bool,
    pending: Option<(u64, usize)>,
}
impl Ring {
    fn new(base: usize) -> Self {
        Self {
            base,
            next: 0,
            cycle: true,
            pending: None,
        }
    }
    fn submit(
        &mut self,
        bus: &mut impl Bus,
        buffer: usize,
        length: usize,
        direction: u32,
        cap: usize,
    ) {
        let at = self.base + self.next * 16;
        // Payload before cycle, then doorbell. One outstanding TD prevents
        // overwriting a hardware-owned slot/buffer even across ring wrap.
        bus.dma_write64(at, bus.physical() + buffer as u64);
        bus.dma_write32(at + 8, length as u32);
        if self.next == TRBS - 2 {
            bus.dma_write32(
                self.base + (TRBS - 1) * 16 + 12,
                LINK | 2 | u32::from(self.cycle),
            );
        }
        fence(Ordering::Release);
        bus.dma_write32(
            at + 12,
            NORMAL | (1 << 5) | (1 << 2) | u32::from(self.cycle),
        );
        self.pending = Some((bus.physical() + at as u64, length));
        self.next += 1;
        if self.next == TRBS - 1 {
            self.next = 0;
            self.cycle = !self.cycle;
        }
        fence(Ordering::Release);
        bus.write32(cap + DOORBELL, direction << 8);
    }
}
/// Owns the allocation for its entire lifetime. Never free DMA while enabled;
/// the platform deliberately retains it until reboot, even after failure.
pub struct Driver<B: Bus> {
    bus: B,
    cap: usize,
    state: State,
    connected_since: Option<u64>,
    tx: Ring,
    rx: Ring,
    event: usize,
    event_cycle: bool,
    queue: [u8; QUEUE],
    head: usize,
    count: usize,
    input: [u8; PACKET],
    input_at: usize,
    input_len: usize,
    stats: Stats,
}
impl<B: Bus> Driver<B> {
    pub fn start(mut bus: B, cap: usize, supports_64: bool) -> Result<Self, Error> {
        if !cap.is_multiple_of(4)
            || cap < 0x20
            || cap
                .checked_add(0x40)
                .is_none_or(|end| end > bus.mmio_bytes())
        {
            return Err(Error::Bounds);
        }
        let base = bus.physical();
        if !base.is_multiple_of(65536)
            || base
                .checked_add(DMA_BYTES as u64 - 1)
                .is_none_or(|end| !supports_64 && end > u32::MAX as u64)
        {
            return Err(Error::Address);
        }
        let control = bus.read32(cap + CONTROL);
        if control & ENABLE != 0 {
            return Err(Error::Busy);
        }
        initialize(&mut bus, (control >> 16) & 0xff);
        bus.write32(cap + ERST_SIZE, 1);
        bus.write64(cap + ERST_BASE, base + ERST as u64);
        bus.write64(cap + ERDP, base + EVENT as u64);
        bus.write64(cap + CONTEXT, base);
        bus.write32(
            cap + INFO1,
            (ids::VENDOR as u32) << 16 | ids::PROTOCOL as u32,
        );
        bus.write32(cap + INFO2, 0x0010_0000 | ids::PRODUCT as u32);
        fence(Ordering::Release);
        // No RMW of mixed RO/RW1S/RW1C control fields.
        bus.write32(cap + CONTROL, ENABLE | 2 | RUN_CHANGE);
        let enabled = (0..HANDSHAKE).any(|_| bus.read32(cap + CONTROL) & ENABLE != 0);
        if !enabled {
            bus.write32(cap + CONTROL, 0);
            return Err(Error::EnableTimeout);
        }
        Ok(Self {
            bus,
            cap,
            state: State::Waiting,
            connected_since: None,
            tx: Ring::new(TX_RING),
            rx: Ring::new(RX_RING),
            event: 0,
            event_cycle: true,
            queue: [0; QUEUE],
            head: 0,
            count: 0,
            input: [0; PACKET],
            input_at: 0,
            input_len: 0,
            stats: Stats::default(),
        })
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn stats(&self) -> Stats {
        self.stats
    }
    pub fn write(&mut self, bytes: &[u8]) -> usize {
        if matches!(self.state, State::Failed(_)) {
            return 0;
        }
        let n = bytes.len().min(QUEUE - self.count);
        for (i, &byte) in bytes[..n].iter().enumerate() {
            self.queue[(self.head + self.count + i) % QUEUE] = byte;
        }
        self.count += n;
        n
    }
    pub fn input_ready(&self) -> bool {
        self.input_at < self.input_len
    }
    pub fn read(&mut self, bytes: &mut [u8]) -> usize {
        let n = bytes.len().min(self.input_len - self.input_at);
        bytes[..n].copy_from_slice(&self.input[self.input_at..self.input_at + n]);
        self.input_at += n;
        if self.input_at == self.input_len {
            self.input_at = 0;
            self.input_len = 0;
        }
        n
    }
    fn fail(&mut self, error: Error) {
        self.state = State::Failed(error);
        // DCE clear terminates DMA; never free/reuse even if readback stalls.
        self.bus.write32(self.cap + CONTROL, 0);
        for _ in 0..HANDSHAKE {
            if self.bus.read32(self.cap + CONTROL) & ENABLE == 0 {
                break;
            }
        }
    }
    fn event(&mut self, pointer: u64, status: u32, control: u32) -> Result<(), Error> {
        let endpoint = (control >> 16) & 0x1f;
        let ring = match endpoint {
            2 => &mut self.rx,
            3 => &mut self.tx,
            _ => return Err(Error::MalformedEvent),
        };
        let (expected, length) = ring.pending.ok_or(Error::MalformedEvent)?;
        let remaining = (status & 0xff_ffff) as usize;
        if pointer != expected || remaining > length {
            return Err(Error::MalformedEvent);
        }
        if !matches!(status >> 24, 1 | 13) {
            return Err(Error::Transfer);
        }
        ring.pending = None;
        let actual = length - remaining;
        if endpoint == 2 {
            if self.input_len != 0 || actual > PACKET {
                return Err(Error::MalformedEvent);
            }
            for i in 0..actual {
                self.input[i] = self.bus.dma_read8(RX_BUFFER + i);
            }
            self.input_len = actual;
            self.input_at = 0;
            self.stats.rx_bytes += actual as u64;
        } else {
            self.stats.tx_bytes += actual as u64;
        }
        Ok(())
    }
    pub fn poll(&mut self, now_ms: u64) {
        if matches!(self.state, State::Failed(_)) {
            return;
        }
        let control = self.bus.read32(self.cap + CONTROL);
        if control & ENABLE == 0
            || control & HALT != 0
            || (self.state == State::Running && control & (RUN | RUN_CHANGE) != RUN)
        {
            self.fail(Error::Disconnected);
            return;
        }
        let port = self.bus.read32(self.cap + PORT);
        // Acknowledge change bits while preserving RW PED (unlike host
        // PORTSC, writing zero here really disables the debug port).
        if port & PORT_CHANGES != 0 {
            self.bus.write32(self.cap + PORT, port & (PORT_CHANGES | 2));
        }
        if control & RUN != 0 {
            if control & RUN_CHANGE != 0 {
                self.fail(Error::Disconnected);
                return;
            }
            self.state = State::Running;
        } else if port & 1 != 0 {
            let since = *self.connected_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= 30_000 {
                self.fail(Error::EnumerationTimeout);
                return;
            }
        } else {
            self.connected_since = None;
        }
        // Work budget independent of a malicious or broken producer.
        for _ in 0..TRBS {
            let at = EVENT + self.event * 16;
            let ctrl = self.bus.dma_read32(at + 12);
            if ctrl & 1 != u32::from(self.event_cycle) {
                break;
            }
            fence(Ordering::Acquire);
            let pointer =
                self.bus.dma_read32(at) as u64 | (self.bus.dma_read32(at + 4) as u64) << 32;
            let status = self.bus.dma_read32(at + 8);
            let result = match (ctrl >> 10) & 0x3f {
                TRANSFER_EVENT => self.event(pointer, status, ctrl),
                PORT_EVENT => Ok(()),
                _ => Err(Error::MalformedEvent),
            };
            if let Err(error) = result {
                self.fail(error);
                return;
            }
            self.stats.events += 1;
            self.event += 1;
            if self.event == TRBS {
                self.event = 0;
                self.event_cycle = !self.event_cycle;
            }
        }
        fence(Ordering::Release);
        self.bus.write64(
            self.cap + ERDP,
            self.bus.physical() + EVENT as u64 + self.event as u64 * 16,
        );
        if self.state != State::Running {
            return;
        }
        if self.rx.pending.is_none() && self.input_len == 0 {
            self.rx
                .submit(&mut self.bus, RX_BUFFER, PACKET, 0, self.cap);
        }
        if self.tx.pending.is_none() && self.count > 0 {
            // End each IN TD with a short packet so a host bulk reader
            // does not wait for another console write to complete its URB.
            let n = self.count.min(PACKET - 1);
            for i in 0..n {
                self.bus
                    .dma_write8(TX_BUFFER + i, self.queue[(self.head + i) % QUEUE]);
            }
            self.head = (self.head + n) % QUEUE;
            self.count -= n;
            self.tx.submit(&mut self.bus, TX_BUFFER, n, 1, self.cap);
        }
    }
    #[cfg(test)]
    pub(crate) fn bus_mut(&mut self) -> &mut B {
        &mut self.bus
    }
}
