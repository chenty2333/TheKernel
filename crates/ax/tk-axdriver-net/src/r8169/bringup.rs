//! A polling, single-queue MAC path which preserves the firmware's PHY setup.
//! Facts: Linux r8169_main.c:2673-2679 (reset), 2790-2815 (DMA geometry),
//! 3866-3886 (queue and descriptor format), 4043-4061 (coalescing),
//! 4131-4136 (enable), 4554 (8125 doorbell). This is not Linux's PHY/workaround
//! initialization and must not be presented as a cold-boot equivalent.
use super::regs::{
    self as r, Bus,
    Width::{Byte, Dword, Word},
};
use crate::{DevError, DevResult};

pub fn reset(bus: &mut impl Bus, chip: super::ids::Chip) -> DevResult {
    let (mask, _, width) = chip.irq();
    bus.write(mask, width, 0);
    bus.write(r::COMMAND, Byte, r::RESET);
    for _ in 0..1000 {
        if bus.read(r::COMMAND, Byte) & r::RESET == 0 {
            return Ok(());
        }
        bus.delay_us(10);
    }
    Err(DevError::Io)
}
fn legacy_descriptors(bus: &mut impl Bus) {
    // MAC OCP encoding: Linux r8169_main.c:1141-1165. Clear only the
    // new-descriptor bit, preserving every unrelated firmware field.
    let command = 0xeb58u32 << 15;
    bus.write(r::OCP_DATA, Dword, command);
    let value = bus.read(r::OCP_DATA, Dword) & 0xffff;
    bus.write(r::OCP_DATA, Dword, (1 << 31) | command | (value & !1));
}
fn program_8125(bus: &mut impl Bus, tx: u64, rx: u64) {
    bus.write(r::CFG_LOCK, Byte, 0xc0);
    bus.write(r::INT_CFG, Byte, 0);
    bus.write(r::INT_CFG1, Word, 0);
    for offset in (0xa00..0xa80).step_by(4) {
        bus.write(offset, Dword, 0);
    }
    bus.write(r::RSS, Dword, 0);
    bus.write(r::QUEUES, Word, 0);
    legacy_descriptors(bus);
    let cplus = bus.read(r::CPLUS, Word);
    bus.write(r::CPLUS, Word, cplus & !(1 << 5)); // no receive checksum offload
    bus.write(r::RX_MAX, Word, super::desc::BUFFER as u32);
    bus.write(r::TX_HIGH, Dword, (tx >> 32) as u32);
    bus.write(r::TX_LOW, Dword, tx as u32);
    bus.write(r::RX_HIGH, Dword, (rx >> 32) as u32);
    bus.write(r::RX_LOW, Dword, rx as u32);
    let misc = bus.read(r::MISC, Dword);
    bus.write(r::MISC, Dword, misc & !(1 << 19)); // release RXDV gate
    bus.write(r::CFG_LOCK, Byte, 0);
    bus.write(r::COMMAND, Byte, r::RX_TX_ENABLE);
    // RTL8125B fetch=8, unrestricted PCI burst, pause slot, own+multicast+broadcast.
    bus.write(r::RX_CONFIG, Dword, (8 << 27) | (7 << 8) | (1 << 11) | 0x0e);
    bus.write(0x08, Dword, u32::MAX);
    bus.write(0x0c, Dword, u32::MAX);
    bus.write(r::TX_CONFIG, Dword, (3 << 24) | (7 << 8) | (1 << 7));
    bus.write(r::IRQ_STATUS, Dword, u32::MAX);
    // Posted-write flush; no interrupt source is ever unmasked.
    let _ = bus.read(r::COMMAND, Byte);
}

/// Family dispatch occurs before any chip-specific indirect register access.
pub fn program(bus: &mut impl Bus, chip: super::ids::Chip, tx: u64, rx: u64) -> DevResult {
    match chip {
        super::ids::Chip::Rtl8125B => program_8125(bus, tx, rx),
        super::ids::Chip::Rtl8168H => super::h8168::program(bus, tx, rx)?,
    }
    if bus.interrupts_available() {
        let (mask, _, width) = chip.irq();
        bus.write(mask, width, 0x002f); // RX/TX success/error and link change
    }
    Ok(())
}
