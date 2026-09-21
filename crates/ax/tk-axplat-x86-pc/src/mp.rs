//! Multi-processor booting.

use axplat::{
    mem::{PAGE_SIZE_4K, PhysAddr, pa},
    time::{Duration, busy_wait},
};

const START_PAGE_IDX: u8 = 6;
const START_PAGE_PADDR: PhysAddr = pa!(START_PAGE_IDX as usize * PAGE_SIZE_4K);

core::arch::global_asm!(
    include_str!("ap_start.S"),
    start_page_paddr = const START_PAGE_PADDR.as_usize(),
);

unsafe fn setup_startup_page(stack_top: PhysAddr) {
    unsafe extern "C" {
        fn ap_entry32();
        fn ap_start();
        fn ap_end();
    }
    const U64_PER_PAGE: usize = PAGE_SIZE_4K / 8;

    let start_page_ptr = axplat::mem::phys_to_virt(START_PAGE_PADDR).as_mut_ptr() as *mut u64;
    let start_page = unsafe { core::slice::from_raw_parts_mut(start_page_ptr, U64_PER_PAGE) };
    unsafe {
        core::ptr::copy_nonoverlapping(
            ap_start as *const u64,
            start_page_ptr,
            (ap_end as *const () as usize - ap_start as *const () as usize) / 8,
        );
    }
    start_page[U64_PER_PAGE - 2] = stack_top.as_usize() as u64; // stack_top
    start_page[U64_PER_PAGE - 1] = ap_entry32 as *const () as usize as _; // entry
}

/// How many times the APIC is asked whether it consumed the last ICR write.
///
/// Linux polls 1000 times with a 100 us pause and calls a still-busy ICR a
/// message that never left (`apic_mem_wait_icr_idle_timeout`,
/// arch/x86/kernel/apic/ipi.c:116-127).
const ICR_POLL_LIMIT: usize = 1_000;

/// Waits for the local APIC to consume the message just written to the ICR.
///
/// A stuck ICR is only reported, never retried or turned into a boot failure:
/// an AP that was never addressed cannot be taken back offline here, because
/// the kernel has no reduced-fleet mode -- `axhal::cpu_num()` is fixed once the
/// topology is known, and every CPU in it is expected to reach its rendezvous.
/// What a silent failure becomes instead is the runtime's bring-up wait, which
/// says which CPU never reported in.
fn wait_for_icr_consumed(message: &str, logical_cpu_id: usize, apic_id: u32) {
    for _ in 0..ICR_POLL_LIMIT {
        if !super::apic::ipi_pending() {
            return;
        }
        busy_wait(Duration::from_micros(100));
    }
    error!("CPU {logical_cpu_id} (APIC {apic_id:#x}): local APIC never took the {message} message");
}

/// Starts the given logical secondary CPU with its boot stack.
pub fn start_secondary_cpu(logical_cpu_id: usize, stack_top: PhysAddr) {
    unsafe { setup_startup_page(stack_top) };

    let apic_id = super::cpu::apic_id_for_logical(logical_cpu_id).unwrap_or_else(|| {
        panic!("logical CPU {logical_cpu_id} has no APIC identity in the MADT topology")
    });
    let apic_destination = super::apic::raw_apic_id(apic_id).unwrap_or_else(|| {
        panic!(
            "logical CPU {logical_cpu_id} APIC ID {apic_id:#x} cannot be addressed in xAPIC mode"
        )
    });
    let lapic = super::apic::local_apic();

    // INIT-SIPI-SIPI Sequence
    // Ref: Intel SDM Vol 3C, Section 8.4.4, MP Initialization Example
    super::apic::clear_error_status();
    unsafe { lapic.send_init_ipi(apic_destination) };
    wait_for_icr_consumed("INIT", logical_cpu_id, apic_id);
    busy_wait(Duration::from_millis(10)); // 10ms
    unsafe { lapic.send_sipi(START_PAGE_IDX, apic_destination) };
    wait_for_icr_consumed("first STARTUP", logical_cpu_id, apic_id);
    busy_wait(Duration::from_micros(200)); // 200us
    unsafe { lapic.send_sipi(START_PAGE_IDX, apic_destination) };
    wait_for_icr_consumed("second STARTUP", logical_cpu_id, apic_id);

    let status = super::apic::error_status();
    if status != 0 {
        error!("CPU {logical_cpu_id} (APIC {apic_id:#x}): local APIC error status {status:#x} on startup");
    }
}
