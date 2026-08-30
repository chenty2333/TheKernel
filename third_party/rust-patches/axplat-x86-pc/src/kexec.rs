//! Terminal x86_64 kexec platform operations.

use x86_64::instructions::port::Port;

/// The owned Multiboot handoff is the only safe source of firmware records at
/// kexec time; the original bootloader block may already have been recycled.
pub fn boot_memory_regions() -> &'static [(usize, usize)] {
    crate::mem::ram_regions()
}

pub fn boot_rsdp() -> Option<&'static [u8; 36]> {
    crate::boot_info::get().rsdp().map(|rsdp| rsdp.bytes())
}

#[cfg(target_os = "none")]
core::arch::global_asm!(include_str!("kexec_transition.S"));

#[cfg(target_os = "none")]
unsafe extern "C" {
    fn axplat_kexec_transition(cr3: usize, stack_top: usize, bootparams: usize, entry: usize) -> !;
    static axplat_kexec_transition_start: u8;
    static axplat_kexec_transition_end: u8;
    fn axplat_kexec_transition32_enter(
        cr3: usize,
        stack_top: usize,
        bootparams: usize,
        entry: usize,
        stub: usize,
    ) -> !;
    static axplat_kexec_transition32_enter_start: u8;
    static axplat_kexec_transition32_enter_end: u8;
    static axplat_kexec_transition32_start: u8;
    static axplat_kexec_transition32_end: u8;
}

/// Enters an identity-mapped replacement image. All arguments must be low
/// aliases reachable after loading `cr3`; this function never returns.
#[cfg(target_os = "none")]
pub unsafe fn transition(cr3: usize, stack_top: usize, bootparams: usize, entry: usize) -> ! {
    unsafe { axplat_kexec_transition(cr3, stack_top, bootparams, entry) }
}

/// Direct-map virtual range occupied by the assembly reached from
/// [`transition`] immediately after the CR3 switch.
#[cfg(target_os = "none")]
pub fn transition_assembly_range() -> (usize, usize) {
    unsafe {
        let start = core::ptr::addr_of!(axplat_kexec_transition_start);
        let end = core::ptr::addr_of!(axplat_kexec_transition_end);
        (start as usize, end.offset_from(start) as usize)
    }
}

/// The self-contained 64-to-32 bit bzImage handoff blob.
#[cfg(target_os = "none")]
pub fn transition32_blob() -> &'static [u8] {
    unsafe {
        let start = core::ptr::addr_of!(axplat_kexec_transition32_start);
        let end = core::ptr::addr_of!(axplat_kexec_transition32_end);
        core::slice::from_raw_parts(start, end.offset_from(start) as usize)
    }
}

/// Enter a copied low-memory handoff blob.  `stub` and all other pointers are
/// physical identity aliases; the blob turns off long mode before jumping to
/// Linux `startup_32`.
#[cfg(target_os = "none")]
pub unsafe fn transition32(
    stub: usize,
    cr3: usize,
    stack_top: usize,
    bootparams: usize,
    entry: usize,
) -> ! {
    unsafe { axplat_kexec_transition32_enter(cr3, stack_top, bootparams, entry, stub) }
}

/// Exact virtual range of the high-half entry shim reached before control
/// transfers to the copied low-memory handoff blob.
#[cfg(target_os = "none")]
pub fn transition32_entry_range() -> (usize, usize) {
    unsafe {
        let start = core::ptr::addr_of!(axplat_kexec_transition32_enter_start);
        let end = core::ptr::addr_of!(axplat_kexec_transition32_enter_end);
        (start as usize, end.offset_from(start) as usize)
    }
}

/// Disables PCI bus mastering on every conventional PCI BDF and reads the
/// command register back before the kexec image may overwrite RAM.
pub fn fence_pci_bus_mastering() {
    for bus in 0u16..=255 {
        for device in 0u16..32 {
            for function in 0u16..8 {
                let address = 0x8000_0000u32
                    | ((bus as u32) << 16)
                    | ((device as u32) << 11)
                    | ((function as u32) << 8)
                    | 4;
                let mut cfg_addr = Port::<u32>::new(0xcf8);
                let mut cfg_data = Port::<u32>::new(0xcfc);
                unsafe { cfg_addr.write(address) };
                let value = unsafe { cfg_data.read() };
                if value == u32::MAX {
                    continue;
                }
                let command = (value & 0xffff) as u16;
                if command & (1 << 2) != 0 {
                    // PCI Status occupies the upper half of this dword and
                    // carries W1C bits.  Writing only Command preserves it.
                    let mut cfg_command = Port::<u16>::new(0xcfc);
                    unsafe { cfg_command.write(command & !4) };
                    unsafe { cfg_addr.write(address) };
                    let verified = unsafe { cfg_data.read() };
                    if verified & 4 != 0 {
                        loop {
                            core::hint::spin_loop();
                        }
                    }
                }
            }
        }
    }
}
