//! Kernel booting using Multiboot1 and Multiboot2 headers.

#[cfg(not(test))]
use core::arch::global_asm;

#[cfg(not(test))]
use x86_64::registers::{
    control::{Cr0Flags, Cr4Flags},
    model_specific::EferFlags,
};

#[cfg(not(test))]
use crate::config::plat::{BOOT_STACK_SIZE, PHYS_VIRT_OFFSET};

/// Flags set in the ’flags’ member of the Multiboot1 header.
///
/// (bits 1, 16: memory information, address fields in header)
#[cfg(not(test))]
const MULTIBOOT_HEADER_FLAGS: usize = 0x0001_0002;

/// The Multiboot1 header magic field should contain this.
#[cfg(not(test))]
const MULTIBOOT_HEADER_MAGIC: usize = 0x1BADB002;

/// This should be in EAX for a Multiboot1 handoff.
pub(crate) const MULTIBOOT_BOOTLOADER_MAGIC: usize = 0x2BADB002;

/// The Multiboot2 header magic field should contain this.
#[cfg(not(test))]
const MULTIBOOT2_HEADER_MAGIC: u32 = 0xE852_50D6;

/// Multiboot2 architecture identifier for an i386 entry point.
#[cfg(not(test))]
const MULTIBOOT2_HEADER_ARCH: u32 = 0;

/// The Multiboot2 header magic passed in EAX by GRUB.
pub(crate) const MULTIBOOT2_BOOTLOADER_MAGIC: usize = 0x36D7_6289;

// Header (16 bytes), address tag (24 bytes), entry tag (16 bytes including
// alignment padding), and end tag (8 bytes).
#[cfg(not(test))]
const MULTIBOOT2_HEADER_LENGTH: u32 = 16 + 24 + 16 + 8;
#[cfg(not(test))]
const MULTIBOOT2_HEADER_CHECKSUM: u32 = 0u32.wrapping_sub(
    MULTIBOOT2_HEADER_MAGIC
        .wrapping_add(MULTIBOOT2_HEADER_ARCH)
        .wrapping_add(MULTIBOOT2_HEADER_LENGTH),
);

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct EarlyBootRecord {
    pub(crate) magic: usize,
    pub(crate) info_paddr: usize,
}

// rust_entry runs before axruntime clears .bss.  Keep this record in the
// initialized data segment so the raw boot arguments survive that clear.
#[used]
#[unsafe(link_section = ".data.boot")]
static mut EARLY_BOOT_RECORD: EarlyBootRecord = EarlyBootRecord {
    magic: usize::MAX,
    info_paddr: usize::MAX,
};

pub(crate) unsafe fn record_early_args(magic: usize, info_paddr: usize) {
    unsafe {
        core::ptr::addr_of_mut!(EARLY_BOOT_RECORD).write(EarlyBootRecord { magic, info_paddr });
    }
}

pub(crate) fn early_record() -> EarlyBootRecord {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(EARLY_BOOT_RECORD)) }
}

#[cfg(not(test))]
const CR0: u64 = Cr0Flags::PROTECTED_MODE_ENABLE.bits()
    | Cr0Flags::MONITOR_COPROCESSOR.bits()
    | Cr0Flags::NUMERIC_ERROR.bits()
    | Cr0Flags::WRITE_PROTECT.bits()
    | Cr0Flags::PAGING.bits();
#[cfg(not(test))]
const CR4: u64 = Cr4Flags::PHYSICAL_ADDRESS_EXTENSION.bits()
    | Cr4Flags::PAGE_GLOBAL.bits()
    | if cfg!(feature = "fp-simd") {
        Cr4Flags::OSFXSR.bits() | Cr4Flags::OSXMMEXCPT_ENABLE.bits()
    } else {
        0
    };
#[cfg(not(test))]
const EFER: u64 = EferFlags::LONG_MODE_ENABLE.bits() | EferFlags::NO_EXECUTE_ENABLE.bits();

#[cfg(not(test))]
#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

#[cfg(not(test))]
global_asm!(
    include_str!("multiboot.S"),
    mb_magic = const MULTIBOOT_BOOTLOADER_MAGIC,
    mb_hdr_magic = const MULTIBOOT_HEADER_MAGIC,
    mb_hdr_flags = const MULTIBOOT_HEADER_FLAGS,
    mb2_hdr_magic = const MULTIBOOT2_HEADER_MAGIC,
    mb2_hdr_arch = const MULTIBOOT2_HEADER_ARCH,
    mb2_hdr_length = const MULTIBOOT2_HEADER_LENGTH,
    mb2_hdr_checksum = const MULTIBOOT2_HEADER_CHECKSUM,
    entry = sym crate::rust_entry,
    entry_secondary = sym crate::rust_entry_secondary,

    offset = const PHYS_VIRT_OFFSET,
    boot_stack_size = const BOOT_STACK_SIZE,
    boot_stack = sym BOOT_STACK,

    cr0 = const CR0,
    cr4 = const CR4,
    efer_msr = const x86::msr::IA32_EFER,
    efer = const EFER,
);
