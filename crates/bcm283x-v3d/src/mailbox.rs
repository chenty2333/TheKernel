//! Minimal firmware mailbox property-channel client (clock requests only).
//!
//! The GPU firmware owns the clock tree on BCM283x; the V3D clock is enabled
//! and set through the mailbox `CLOCK_SET_RATE` request rather than by
//! poking CPRMAN directly (the firmware knows the safe PLL configuration).

use core::{
    arch::asm,
    ptr::{read_volatile, write_volatile},
};

use kspin::SpinNoIrq;

const MBOX_BASE: usize = 0x3f00_b880;
const MBOX_READ: usize = 0x00;
const MBOX_STATUS: usize = 0x18;
const MBOX_WRITE: usize = 0x20;

const MBOX_STATUS_EMPTY: u32 = 1 << 30;
const MBOX_STATUS_FULL: u32 = 1 << 31;
const MBOX_CH_PROPERTY: u32 = 8;

const TAG_CLOCK_SET_RATE: u32 = 0x0003_8002;
const TAG_END: u32 = 0;

/// V3D clock id for the firmware clock manager.
pub const CLK_V3D: u32 = 5;

fn read_reg(offset: usize) -> u32 {
    unsafe { read_volatile((MBOX_BASE + offset) as *const u32) }
}

fn write_reg(offset: usize, value: u32) {
    unsafe { write_volatile((MBOX_BASE + offset) as *mut u32, value) }
}

fn write_request(addr: usize, channel: u32) {
    while read_reg(MBOX_STATUS) & MBOX_STATUS_FULL != 0 {}
    write_reg(MBOX_WRITE, (addr as u32 & 0xffff_fff0) | channel);
}

fn read_response(channel: u32) -> usize {
    loop {
        while read_reg(MBOX_STATUS) & MBOX_STATUS_EMPTY != 0 {}
        let value = read_reg(MBOX_READ);
        if value & 0xf == channel {
            return (value & 0xffff_fff0) as usize;
        }
    }
}

fn cache_line_size() -> usize {
    let ctr: u64;
    unsafe {
        asm!("mrs {0}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags));
    }
    4 << (ctr & 0xf)
}

fn clean_range(addr: usize, size: usize) {
    let line = cache_line_size();
    let mut a = addr & !(line - 1);
    let end = addr + size;
    while a < end {
        unsafe {
            asm!("dc cvac, {0}", in(reg) a, options(nostack, preserves_flags));
        }
        a += line;
    }
    unsafe {
        asm!("dsb sy", options(nostack, preserves_flags));
    }
}

fn invalidate_range(addr: usize, size: usize) {
    let line = cache_line_size();
    let mut a = addr & !(line - 1);
    let end = addr + size;
    while a < end {
        unsafe {
            asm!("dc ivac, {0}", in(reg) a, options(nostack, preserves_flags));
        }
        a += line;
    }
    unsafe {
        asm!("dsb sy", options(nostack, preserves_flags));
    }
}

const REQ_BUF_WORDS: usize = 32;
static mut REQ_BUF: [u32; REQ_BUF_WORDS] = [0; REQ_BUF_WORDS];
static MBOX_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());

fn property_call(tag_id: u32, data: &[u32]) -> bool {
    let _guard = MBOX_LOCK.lock();
    let buf_addr = &raw mut REQ_BUF as usize;
    // SAFETY: exclusive access under MBOX_LOCK during this call.
    let req = unsafe { &mut *(&raw mut REQ_BUF as *mut [u32; REQ_BUF_WORDS]) };

    let data_words = data.len();
    let total_words = 2 + 2 + data_words + 1;
    let total_bytes = total_words * 4;
    req[0] = total_bytes as u32;
    req[1] = 0;
    req[2] = tag_id;
    req[3] = (data_words * 4) as u32;
    req[4] = (data_words * 4) as u32;
    req[5..5 + data_words].copy_from_slice(data);
    let end = 2 + 2 + data_words;
    req[end] = TAG_END;
    for word in req.iter_mut().skip(end + 1) {
        *word = 0;
    }

    clean_range(buf_addr, total_bytes);
    write_request(buf_addr, MBOX_CH_PROPERTY);
    if read_response(MBOX_CH_PROPERTY) != buf_addr {
        return false;
    }
    invalidate_range(buf_addr, total_bytes);
    req[4] & (1 << 31) != 0
}

/// Requests the firmware to set a clock rate (Hz).
pub fn set_clock_rate(clock_id: u32, rate_hz: u32) -> bool {
    property_call(TAG_CLOCK_SET_RATE, &[clock_id, rate_hz, 0])
}
