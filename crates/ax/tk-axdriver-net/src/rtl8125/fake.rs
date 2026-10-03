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
        }
    }
}
impl Bus for FakeBus {
    fn read(&mut self, offset: usize, _width: Width) -> u32 {
        *self.registers.get(&offset).unwrap_or(&0)
    }
    fn write(&mut self, offset: usize, width: Width, value: u32) {
        self.writes.push((offset, width, value));
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
    super::bringup::reset(&mut bus).unwrap();
    super::bringup::program(&mut bus, 0x1234_5678_9000, 0x4321_9876_5000);
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
    assert!(super::bringup::reset(&mut bus).is_err());
}
