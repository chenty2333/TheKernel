//! Wrapper functions for assembly instructions.

use memory_addr::{PhysAddr, VirtAddr};
use riscv::register::{satp, sstatus, stvec};

/// Allows the current CPU to respond to interrupts.
#[inline]
pub fn enable_irqs() {
    unsafe { sstatus::set_sie() }
}

/// Makes the current CPU to ignore interrupts.
#[inline]
pub fn disable_irqs() {
    unsafe { sstatus::clear_sie() }
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn irqs_enabled() -> bool {
    sstatus::read().sie()
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn wait_for_irqs() {
    riscv::asm::wfi()
}

/// Halt the current CPU.
#[inline]
pub fn halt() {
    disable_irqs();
    riscv::asm::wfi() // should never return
}

/// Reads the current page table root register for user space (`satp`).
///
/// RISC-V does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_kernel_page_table`].
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_user_page_table() -> PhysAddr {
    pa!(satp::read().ppn() << 12)
}

/// Reads the current page table root register for kernel space (`satp`).
///
/// RISC-V does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_user_page_table`].
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_kernel_page_table() -> PhysAddr {
    read_user_page_table()
}

/// Reads the ASID currently installed in `satp`.
#[cfg(feature = "asid-fast-switch")]
#[inline]
pub fn read_current_asid() -> usize {
    satp::read().asid()
}

/// Writes the register to update the current page table root for user space
/// (`satp`).
///
/// RISC-V does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_kernel_page_table`].
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root_paddr: PhysAddr) {
    unsafe { satp::set(satp::Mode::Sv39, 0, root_paddr.as_usize() >> 12) };
}

/// Writes a user page-table root and its hardware ASID without flushing.
///
/// # Safety
///
/// The caller must ensure that `asid` identifies `root_paddr` under the active
/// allocator generation and must issue any required TLB invalidation.
#[cfg(feature = "asid-fast-switch")]
#[inline]
pub unsafe fn write_user_page_table_with_asid(root_paddr: PhysAddr, asid: usize) {
    unsafe { satp::set(satp::Mode::Sv39, asid, root_paddr.as_usize() >> 12) };
}

/// Probes the implemented WARL width of the `satp.ASID` field.
#[cfg(feature = "asid-fast-switch")]
pub fn probe_asid_width() -> usize {
    let old = satp::read();
    let mode = old.mode();
    let ppn = old.ppn();
    let old_asid = old.asid();

    unsafe { satp::set(mode, u16::MAX as usize, ppn) };
    flush_tlb_all();
    let implemented = satp::read().asid() & u16::MAX as usize;
    unsafe { satp::set(mode, old_asid, ppn) };
    flush_tlb_all();
    // The privileged architecture defines ASIDLEN as a contiguous set of
    // implemented low-order satp.ASID bits, so counting read-back ones yields
    // ASIDLEN after this WARL probe.
    implemented.count_ones() as usize
}

/// Flushes all virtual addresses for all ASIDs on the current hart.
#[inline]
pub fn flush_tlb_all() {
    unsafe { core::arch::asm!("sfence.vma x0, x0", options(nostack)) }
}

/// Flushes one virtual address for one ASID on the current hart.
#[inline]
pub fn flush_tlb_addr_asid(vaddr: VirtAddr, asid: usize) {
    unsafe {
        core::arch::asm!(
            "sfence.vma {addr}, {asid}",
            addr = in(reg) vaddr.as_usize(),
            asid = in(reg) asid,
            options(nostack)
        )
    }
}

/// Writes the register to update the current page table root for user space
/// (`satp`).
///
/// RISC-V does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_user_page_table`].
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root_paddr: PhysAddr) {
    unsafe { write_user_page_table(root_paddr) };
}

/// Flushes the TLB.
///
/// If `vaddr` is [`None`], flushes the entire TLB. Otherwise, flushes the TLB
/// entry that maps the given virtual address.
#[inline]
pub fn flush_tlb(vaddr: Option<VirtAddr>) {
    if let Some(vaddr) = vaddr {
        #[cfg(feature = "asid-fast-switch")]
        let asid = read_current_asid();
        #[cfg(not(feature = "asid-fast-switch"))]
        let asid = 0;
        flush_tlb_addr_asid(vaddr, asid)
    } else {
        flush_tlb_all();
    }
}

/// Synchronizes instruction fetches with earlier writes to executable memory.
#[inline]
pub fn flush_icache_all() {
    // Zifencei specifies that cross-hart instruction publication requires the
    // writer to make its data stores globally visible before requesting
    // remote FENCE.I execution. The higher-level maintenance broker publishes
    // that request only after this function returns.
    unsafe { core::arch::asm!("fence rw, rw", options(nostack)) };
    riscv::asm::fence_i();
}

/// Writes the Supervisor Trap Vector Base Address register (`stvec`).
///
/// # Safety
///
/// This function is unsafe as it changes the exception handling behavior of the
/// current CPU.
#[inline]
pub unsafe fn write_trap_vector_base(stvec: usize) {
    let mut reg = stvec::read();
    reg.set_address(stvec);
    reg.set_trap_mode(stvec::TrapMode::Direct);
    unsafe { stvec::write(reg) }
}

/// Reads the thread pointer of the current CPU (`tp`).
///
/// It is used to implement TLS (Thread Local Storage).
#[inline]
pub fn read_thread_pointer() -> usize {
    let tp;
    unsafe { core::arch::asm!("mv {}, tp", out(reg) tp) };
    tp
}

/// Writes the thread pointer of the current CPU (`tp`).
///
/// It is used to implement TLS (Thread Local Storage).
///
/// # Safety
///
/// This function is unsafe as it changes the CPU states.
#[inline]
pub unsafe fn write_thread_pointer(tp: usize) {
    unsafe { core::arch::asm!("mv tp, {}", in(reg) tp) }
}

#[cfg(feature = "uspace")]
core::arch::global_asm!(include_asm_macros!(), include_str!("user_copy.S"));

#[cfg(feature = "uspace")]
unsafe extern "C" {
    /// Copies data from source to destination, where addresses may be in user
    /// space. Equivalent to memcpy.
    ///
    /// # Safety
    /// This function is unsafe because it performs raw memory operations.
    ///
    /// # Returns
    /// Returns the number of bytes not copied. This means 0 indicates success,
    /// while a value > 0 indicates failure.
    pub fn user_copy(dst: *mut u8, src: *const u8, size: usize) -> usize;
}
