//! Raspberry Pi firmware mailbox (property channel).
//!
//! The GPU firmware manages the clock tree on BCM283x; the HDMI pixel clock
//! (and its "BVB" companion) must be requested through the mailbox property
//! channel. The protocol is documented in the official firmware wiki
//! ("Mailbox property interface"). Request buffers live in normal memory and
//! are cache-maintained by the driver before/after each transfer.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

use kspin::SpinNoIrq;

use crate::regs::*;

fn read_reg(offset: usize) -> u32 {
    unsafe { read_volatile((MBOX_BASE + offset) as *const u32) }
}

fn write_reg(offset: usize, value: u32) {
    unsafe { write_volatile((MBOX_BASE + offset) as *mut u32, value) }
}

/// Waits until the mailbox is writable and submits a request buffer.
fn write_request(addr: usize, channel: u32) {
    while read_reg(MBOX_STATUS) & MBOX_STATUS_FULL != 0 {}
    write_reg(MBOX_WRITE, (addr as u32 & 0xffff_fff0) | channel);
}

/// Waits for the response and returns the buffer address (high 28 bits) of
/// the first reply with the matching channel.
fn read_response(channel: u32) -> usize {
    loop {
        while read_reg(MBOX_STATUS) & MBOX_STATUS_EMPTY != 0 {}
        let value = read_reg(MBOX_READ);
        if value & 0xf == channel {
            return (value & 0xffff_fff0) as usize;
        }
    }
}

// Cache maintenance for the request buffer: clean (writeback) before handing
// to the GPU, invalidate after the GPU returns.
fn cache_line_size() -> usize {
    let ctr: u64;
    unsafe {
        asm!("mrs {0}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags));
    }
    4 << (ctr & 0xf) // log2 of bytes per cache line, minus 2
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

/// Request buffer shared by all mailbox transactions. Must stay within the
/// low 1 GB (GPU bus address space) and be physically contiguous, which a
/// static array satisfies.
const REQ_BUF_WORDS: usize = 32;
static mut REQ_BUF: [u32; REQ_BUF_WORDS] = [0; REQ_BUF_WORDS];
static MBOX_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());

/// Sends a single-tag property-channel request; returns `true` on success.
fn property_call(tag_id: u32, data: &[u32]) -> bool {
    let _guard = MBOX_LOCK.lock();
    let buf_addr = &raw mut REQ_BUF as usize;
    // SAFETY: exclusive access under MBOX_LOCK during this call.
    let req = unsafe { &mut *(&raw mut REQ_BUF as *mut [u32; REQ_BUF_WORDS]) };

    let data_words = data.len();
    let total_words = 2 + 2 + data_words + 1; // header + tag header + data + end
    let total_bytes = total_words * 4;
    req[0] = total_bytes as u32;
    req[1] = 0; // request code
    req[2] = tag_id;
    req[3] = (data_words * 4) as u32; // tag data size
    req[4] = (data_words * 4) as u32; // data size (request)
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

    // Success is reported in the tag's data-size high bit.
    req[4] & (1 << 31) != 0
}

/// Requests the firmware to set a clock rate (Hz).
pub fn set_clock_rate(clock_id: u32, rate_hz: u32) -> bool {
    property_call(TAG_CLOCK_SET_RATE, &[clock_id, rate_hz, 0])
}
