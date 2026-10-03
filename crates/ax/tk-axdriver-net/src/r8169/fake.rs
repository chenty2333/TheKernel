use core::ptr::NonNull;
use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    collections::BTreeMap,
};

use super::{
    Hal,
    regs::{self as r, Bus, Width},
};
std::thread_local! {
    static COUNTS: core::cell::Cell<(usize,usize)> = const { core::cell::Cell::new((0,0)) };
    static FAIL_AT: core::cell::Cell<usize> = const { core::cell::Cell::new(usize::MAX) };
}
pub struct FakeHal;
impl FakeHal {
    pub fn counts() -> (usize, usize) {
        COUNTS.get()
    }
    pub fn fail_after(successes: usize) {
        FAIL_AT.set(COUNTS.get().0 + successes);
    }
}
impl Hal for FakeHal {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)> {
        let (allocated, freed) = COUNTS.get();
        if FAIL_AT.get() == allocated {
            FAIL_AT.set(usize::MAX);
            return None;
        }
        COUNTS.set((allocated + 1, freed));
        let pointer = NonNull::new(unsafe {
            alloc_zeroed(Layout::from_size_align(pages * 4096, 4096).ok()?)
        })?;
        Some((pointer.as_ptr() as u64, pointer))
    }
    unsafe fn deallocate(_address: u64, pointer: NonNull<u8>, pages: usize) {
        let (allocated, freed) = COUNTS.get();
        COUNTS.set((allocated, freed + 1));
        unsafe {
            dealloc(
                pointer.as_ptr(),
                Layout::from_size_align(pages * 4096, 4096).unwrap(),
            );
        }
    }
}
pub struct FakeBus {
    registers: BTreeMap<usize, u32>,
    pub writes: std::vec::Vec<(usize, Width, u32)>,
    pub stuck_reset: bool,
    pub msi: bool,
    pub stuck_indirect: bool,
    indirect: BTreeMap<(usize, u32), u32>,
}
impl FakeBus {
    pub fn new() -> Self {
        let mut registers = BTreeMap::new();
        registers.insert(r::TX_CONFIG, 0x64100000);
        for (i, value) in [2, 3, 4, 5, 6, 7].iter().enumerate() {
            registers.insert(i, *value);
        }
        Self {
            registers,
            writes: std::vec::Vec::new(),
            stuck_reset: false,
            msi: false,
            stuck_indirect: false,
            indirect: BTreeMap::new(),
        }
    }
    pub fn h8168() -> Self {
        let mut bus = Self::new();
        bus.registers.insert(r::TX_CONFIG, 0x54100000);
        bus
    }
}
impl Bus for FakeBus {
    fn interrupts_available(&self) -> bool {
        self.msi
    }
    fn read(&mut self, offset: usize, _width: Width) -> u32 {
        *self.registers.get(&offset).unwrap_or(&0)
    }
    fn write(&mut self, offset: usize, width: Width, value: u32) {
        self.writes.push((offset, width, value));
        if [0x74, 0x80, 0xb0, 0xb8].contains(&offset) {
            if self.stuck_indirect && offset != 0xb0 {
                self.registers.insert(offset, value);
                return;
            }
            let key = match offset {
                0x74 => value & 0xfff,
                0x80 => (value >> 16) & 31,
                _ => (value >> 15) & 0xffff,
            };
            let write = value & (1 << 31) != 0;
            let data = if offset == 0x74 {
                *self.registers.get(&0x70).unwrap_or(&0)
            } else {
                value & 0xffff
            };
            if write {
                self.indirect.insert((offset, key), data);
            }
            let data = *self.indirect.get(&(offset, key)).unwrap_or(&0);
            if offset == 0x74 {
                self.registers.insert(0x70, data);
            }
            self.registers.insert(
                offset,
                if write {
                    0
                } else {
                    (if offset == 0xb0 { 0 } else { 1 << 31 }) | data
                },
            );
            return;
        }
        self.registers.insert(
            offset,
            if offset == r::COMMAND && value == r::RESET && !self.stuck_reset {
                0
            } else {
                value
            },
        );
    }
    fn delay_us(&mut self, _micros: u32) {}
}
#[test]
fn reset_is_bounded_and_dma_addresses_precede_enable() {
    let mut bus = FakeBus::new();
    super::bringup::reset(&mut bus, super::ids::Chip::Rtl8125B).unwrap();
    super::bringup::program(
        &mut bus,
        super::ids::Chip::Rtl8125B,
        0x1234_5678_9000,
        0x4321_9876_5000,
    )
    .unwrap();
    let enable = bus
        .writes
        .iter()
        .position(|x| *x == (r::COMMAND, Width::Byte, r::RX_TX_ENABLE))
        .unwrap();
    let tx_high = bus.writes.iter().position(|x| x.0 == r::TX_HIGH).unwrap();
    let tx_low = bus.writes.iter().position(|x| x.0 == r::TX_LOW).unwrap();
    assert!(tx_high < tx_low && tx_low < enable);
    assert!(
        bus.writes
            .iter()
            .filter(|x| x.0 == r::IRQ_MASK)
            .all(|x| x.2 == 0)
    );
    bus.stuck_reset = true;
    assert!(super::bringup::reset(&mut bus, super::ids::Chip::Rtl8125B).is_err());
}

#[test]
fn h8168_never_writes_8125_queue_registers_and_faults_leave_mac_disabled() {
    let mut bus = FakeBus::h8168();
    super::bringup::reset(&mut bus, super::ids::Chip::Rtl8168H).unwrap();
    super::bringup::program(
        &mut bus,
        super::ids::Chip::Rtl8168H,
        0x1_0000_2000,
        0x2_0000_4000,
    )
    .unwrap();
    assert_eq!(bus.writes[0], (0x3c, Width::Word, 0));
    assert!(!bus.writes.iter().any(|(reg, ..)| *reg >= 0x1000));
    assert!(
        !bus.writes
            .iter()
            .any(|(reg, width, _)| *reg == 0x3c && *width == Width::Dword)
    );
    assert!(bus.writes.contains(&(0x3e, Width::Word, 0xffff)));
    let enabled = bus
        .writes
        .iter()
        .position(|x| *x == (r::COMMAND, Width::Byte, r::RX_TX_ENABLE))
        .unwrap();
    for reg in [r::RX_LOW, r::RX_HIGH, r::TX_LOW, r::TX_HIGH] {
        assert!(bus.writes.iter().position(|x| x.0 == reg).unwrap() < enabled);
    }
    let mut bus = FakeBus::h8168();
    bus.stuck_indirect = true;
    assert!(super::bringup::program(&mut bus, super::ids::Chip::Rtl8168H, 0x2000, 0x4000).is_err());
    assert!(
        !bus.writes
            .contains(&(r::COMMAND, Width::Byte, r::RX_TX_ENABLE))
    );
    assert_eq!(bus.writes.last(), Some(&(r::CFG_LOCK, Width::Byte, 0)));
}

#[test]
fn interrupts_only_unmask_after_successful_mac_and_ring_setup() {
    for chip in [super::ids::Chip::Rtl8125B, super::ids::Chip::Rtl8168H] {
        let mut bus = FakeBus::new();
        bus.msi = true;
        super::bringup::reset(&mut bus, chip).unwrap();
        super::bringup::program(&mut bus, chip, 0x2000, 0x4000).unwrap();
        let (mask, _, width) = chip.irq();
        assert_eq!(bus.writes.last(), Some(&(mask, width, 0x2f)));
        let mut bus = FakeBus::new();
        bus.msi = true;
        bus.stuck_reset = true;
        assert!(super::bringup::reset(&mut bus, chip).is_err());
        assert!(!bus.writes.iter().any(|x| x.0 == mask && x.2 != 0));
    }
}
