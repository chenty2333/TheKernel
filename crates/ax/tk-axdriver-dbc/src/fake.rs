use std::vec::Vec;

use crate::{
    Bus, Error,
    bringup::{Driver, State},
    desc::*,
    probe,
    regs::*,
};
const CAP: usize = 0x100;
struct Fake {
    regs: Vec<u32>,
    dma: Vec<u8>,
    writes: Vec<(usize, u32)>,
    base: u64,
    enable: bool,
}
impl Fake {
    fn new() -> Self {
        let mut f = Self {
            regs: std::vec![0;256],
            dma: std::vec![0;DMA_BYTES],
            writes: Vec::new(),
            base: 0x10000,
            enable: true,
        };
        f.regs[0] = 0x40;
        f.regs[4] = (CAP as u32 / 4) << 16 | 1;
        f.regs[CAP / 4] = 10;
        f
    }
    fn running(&mut self) {
        self.regs[(CAP + CONTROL) / 4] |= RUN;
        self.regs[(CAP + PORT) / 4] = 3;
    }
    fn completion(
        &mut self,
        event: usize,
        pointer: u64,
        length: usize,
        ep: u32,
        code: u32,
        cycle: bool,
    ) {
        self.dma_write64(EVENT + event * 16, pointer);
        self.dma_write32(EVENT + event * 16 + 8, code << 24 | length as u32);
        self.dma_write32(
            EVENT + event * 16 + 12,
            TRANSFER_EVENT << 10 | ep << 16 | u32::from(cycle),
        );
    }
}
impl Bus for Fake {
    fn mmio_bytes(&self) -> usize {
        self.regs.len() * 4
    }
    fn read32(&self, o: usize) -> u32 {
        self.regs[o / 4]
    }
    fn write32(&mut self, o: usize, v: u32) {
        self.writes.push((o, v));
        if o == CAP + CONTROL {
            self.regs[o / 4] = (v & !RUN_CHANGE) & if self.enable { u32::MAX } else { !ENABLE };
        } else if o == CAP + PORT {
            self.regs[o / 4] = (self.regs[o / 4] & !((v & PORT_CHANGES) | 2)) | (v & 2);
        } else {
            self.regs[o / 4] = v;
        }
    }
    fn dma_read32(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.dma[o..o + 4].try_into().unwrap())
    }
    fn dma_write32(&mut self, o: usize, v: u32) {
        self.dma[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn dma_read8(&self, o: usize) -> u8 {
        self.dma[o]
    }
    fn dma_write8(&mut self, o: usize, v: u8) {
        self.dma[o] = v;
    }
    fn physical(&self) -> u64 {
        self.base
    }
}
#[test]
fn bounded_readonly_probe_rejects_truncated_cnr_busy_and_long_chain() {
    let mut f = Fake::new();
    assert_eq!(probe::find(&f, 1024), Ok(Some(CAP)));
    assert!(f.writes.is_empty());
    assert_eq!(probe::find(&f, CAP + 0x3f), Err(Error::Bounds));
    f.regs[(0x40 + 4) / 4] = 1 << 11;
    assert_eq!(probe::find(&f, 1024), Err(Error::NotReady));
    f.regs[(0x40 + 4) / 4] = 0;
    f.regs[(CAP + CONTROL) / 4] = ENABLE;
    assert_eq!(probe::find(&f, 1024), Err(Error::Busy));
    f.regs[(CAP + CONTROL) / 4] = 0;
    f.regs[CAP / 4] = 1 | 255 << 8;
    assert_eq!(probe::find(&f, 1024), Err(Error::Bounds));
    f.regs[4] = 1;
    assert_eq!(probe::find(&f, 1024), Ok(None));
    assert!(f.writes.is_empty());
}
#[test]
fn contexts_register_order_and_no_configured_doorbells() {
    let mut d = Driver::start(Fake::new(), CAP, true).unwrap();
    let f = d.bus_mut();
    assert_eq!(f.dma_read32(64 + 4), (2 << 3) | (1024 << 16));
    assert_eq!(f.dma_read32(128 + 4), (6 << 3) | (1024 << 16));
    assert_eq!(f.dma_read32(64 + 8), 0x10000 + RX_RING as u32 + 1);
    assert_eq!(f.dma_read32(ERST + 8), 256);
    assert_eq!(f.dma_read8(320), 4);
    assert_eq!(
        f.writes.last(),
        Some(&(CAP + CONTROL, ENABLE | 2 | RUN_CHANGE))
    );
    assert_eq!(d.write(&[7; 9000]), 8192);
    assert_eq!(d.write(&[1]), 0);
    d.poll(0);
    assert!(!d.bus_mut().writes.iter().any(|(o, _)| *o == CAP + DOORBELL));
}
#[test]
fn actual_bidirectional_payload_short_packets_zero_bytes_and_ownership() {
    let mut d = Driver::start(Fake::new(), CAP, true).unwrap();
    d.bus_mut().running();
    d.write(b"hello");
    d.poll(0);
    assert_eq!(&d.bus_mut().dma[TX_BUFFER..TX_BUFFER + 5], b"hello");
    let bells = d
        .bus_mut()
        .writes
        .iter()
        .filter(|(o, _)| *o == CAP + DOORBELL)
        .count();
    d.poll(1);
    assert_eq!(
        d.bus_mut()
            .writes
            .iter()
            .filter(|(o, _)| *o == CAP + DOORBELL)
            .count(),
        bells
    );
    d.bus_mut().dma[RX_BUFFER..RX_BUFFER + 3].copy_from_slice(&[0, 65, 255]);
    d.bus_mut()
        .completion(0, 0x10000 + RX_RING as u64, 1021, 2, 13, true);
    d.bus_mut()
        .completion(1, 0x10000 + TX_RING as u64, 0, 3, 1, true);
    d.poll(2);
    assert_eq!(d.stats().tx_bytes, 5);
    assert_eq!(d.stats().rx_bytes, 3);
    let mut bytes = [0; 2];
    assert_eq!(d.read(&mut bytes), 2);
    assert_eq!(bytes, [0, 65]);
    assert_eq!(d.read(&mut bytes), 1);
    assert_eq!(bytes[0], 255);
    d.poll(3);
    assert_eq!(d.bus_mut().dma_read32(RX_RING + 16 + 12) & 0xfc00, NORMAL);
}
#[test]
fn event_and_transfer_cycles_wrap_without_reusing_pending_dma() {
    let mut d = Driver::start(Fake::new(), CAP, true).unwrap();
    d.bus_mut().running();
    for n in 0..520 {
        assert_eq!(d.write(&[n as u8]), 1);
        d.poll(n);
        let slot = (n as usize) % 255;
        assert_eq!(
            d.bus_mut().dma_read32(TX_RING + slot * 16 + 12) & 1,
            u32::from(n / 255 % 2 == 0)
        );
        d.bus_mut().completion(
            n as usize % 256,
            0x10000 + TX_RING as u64 + slot as u64 * 16,
            0,
            3,
            1,
            n / 256 % 2 == 0,
        );
        d.poll(n);
    }
    assert_eq!(d.stats().tx_bytes, 520);
    assert_eq!(d.stats().events, 520);
}
#[test]
fn malformed_events_halt_disconnect_and_timeouts_fail_closed_no_doorbells() {
    for mode in 0..5 {
        let mut d = Driver::start(Fake::new(), CAP, true).unwrap();
        d.bus_mut().running();
        d.poll(0);
        match mode {
            0 => d.bus_mut().completion(0, 1234, 0, 2, 1, true),
            1 => d
                .bus_mut()
                .completion(0, 0x10000 + RX_RING as u64, 1025, 2, 1, true),
            2 => d
                .bus_mut()
                .completion(0, 0x10000 + RX_RING as u64, 0, 2, 4, true),
            3 => d.bus_mut().regs[(CAP + CONTROL) / 4] |= HALT,
            _ => d.bus_mut().regs[(CAP + CONTROL) / 4] &= !RUN,
        }
        d.poll(1);
        assert!(matches!(d.state(), State::Failed(_)));
        assert_eq!(d.bus_mut().regs[(CAP + CONTROL) / 4] & ENABLE, 0);
        let writes = d.bus_mut().writes.len();
        d.poll(2);
        assert_eq!(d.write(b"x"), 0);
        assert_eq!(d.bus_mut().writes.len(), writes);
    }
    let mut d = Driver::start(Fake::new(), CAP, true).unwrap();
    d.bus_mut().regs[(CAP + PORT) / 4] = 1;
    d.poll(3);
    d.poll(30003);
    assert_eq!(d.state(), State::Failed(Error::EnumerationTimeout));
    let mut f = Fake::new();
    f.enable = false;
    assert!(matches!(
        Driver::start(f, CAP, true),
        Err(Error::EnableTimeout)
    ));
    let mut f = Fake::new();
    f.base = 0x1_0000_0000;
    assert!(matches!(Driver::start(f, CAP, false), Err(Error::Address)));
}

#[test]
fn port_ack_preserves_device_side_rw_enable_without_reset() {
    let mut d = Driver::start(Fake::new(), CAP, true).unwrap();
    d.bus_mut().running();
    d.bus_mut().regs[(CAP + PORT) / 4] |= PORT_CHANGES;
    d.poll(0);
    assert_eq!(d.bus_mut().regs[(CAP + PORT) / 4], 3);
    assert!(d.bus_mut().writes.contains(&(CAP + PORT, PORT_CHANGES | 2)));
}

#[test]
fn malformed_alignment_floating_and_oversized_bar_claim_do_not_read_outside() {
    let mut f = Fake::new();
    f.regs[0] = 0xff;
    assert_eq!(probe::find(&f, usize::MAX), Err(Error::Bounds));
    f.regs.fill(u32::MAX);
    assert_eq!(probe::find(&f, usize::MAX), Err(Error::Bounds));
    assert!(matches!(
        Driver::start(Fake::new(), CAP + 1, true),
        Err(Error::Bounds)
    ));
    assert!(matches!(
        Driver::start(Fake::new(), 1024, true),
        Err(Error::Bounds)
    ));
}

#[test]
fn full_admission_queue_still_sends_a_short_in_packet_for_host_read_latency() {
    let mut d = Driver::start(Fake::new(), CAP, true).unwrap();
    assert_eq!(d.write(&[7; 8192]), 8192);
    d.bus_mut().running();
    d.poll(0);
    assert_eq!(d.bus_mut().dma_read32(TX_RING + 8), 1023);
    assert!(
        d.bus_mut().dma[TX_BUFFER..TX_BUFFER + 1023]
            .iter()
            .all(|b| *b == 7)
    );
}
