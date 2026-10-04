use crate::Bus;
pub const DMA_BYTES: usize = 6 * 4096;
pub const EVENT: usize = 4096;
pub const TX_RING: usize = 2 * 4096;
pub const RX_RING: usize = 3 * 4096;
pub const TX_BUFFER: usize = 4 * 4096;
pub const RX_BUFFER: usize = 5 * 4096;
pub const ERST: usize = 256;
pub const TRBS: usize = 256;
pub const PACKET: usize = 1024;
pub const NORMAL: u32 = 1 << 10;
pub const LINK: u32 = 6 << 10;
pub const TRANSFER_EVENT: u32 = 32;
pub const PORT_EVENT: u32 = 34;
fn string(bus: &mut impl Bus, at: usize, text: &str) -> u32 {
    let len = 2 + text.len() * 2;
    bus.dma_write8(at, len as u8);
    bus.dma_write8(at + 1, 3);
    for (i, byte) in text.bytes().enumerate() {
        bus.dma_write8(at + 2 + i * 2, byte);
        bus.dma_write8(at + 3 + i * 2, 0);
    }
    len as u32
}
/// Zeroed, 64KiB-aligned coherent allocation; three 64-byte contexts.
pub fn initialize(bus: &mut impl Bus, burst: u32) {
    let base = bus.physical();
    let strings = [320usize, 384, 448, 512];
    for (i, at) in strings.iter().enumerate() {
        bus.dma_write64(i * 8, base + *at as u64);
    }
    for (i, byte) in [4, 3, 9, 4].into_iter().enumerate() {
        bus.dma_write8(strings[0] + i, byte);
    }
    let a = string(bus, strings[1], "TheKernel");
    let b = string(bus, strings[2], "DbC console");
    let c = string(bus, strings[3], "debug-1");
    bus.dma_write32(32, 4 | a << 8 | b << 16 | c << 24);
    for (context, ring, kind) in [(64, RX_RING, 2u32), (128, TX_RING, 6u32)] {
        bus.dma_write32(context + 4, kind << 3 | burst << 8 | 1024 << 16);
        bus.dma_write64(context + 8, (base + ring as u64) | 1);
        bus.dma_write64(ring + (TRBS - 1) * 16, base + ring as u64);
        bus.dma_write32(ring + (TRBS - 1) * 16 + 12, LINK | 2 | 1);
    }
    bus.dma_write64(ERST, base + EVENT as u64);
    bus.dma_write32(ERST + 8, TRBS as u32);
}
