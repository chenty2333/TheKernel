use x86_64::{
    PrivilegeLevel,
    instructions::tables::load_tss,
    registers::segmentation::{CS, Segment, SegmentSelector},
    structures::tss::TaskStateSegment,
};
use core::arch::asm;

#[percpu::def_percpu]
#[unsafe(no_mangle)]
static TSS: TaskStateSegment = TaskStateSegment::new();

#[repr(C, align(16))]
struct CpuGdt { entries: [u64; 9] }
impl CpuGdt { const fn new() -> Self { Self { entries: [0; 9] } } }

#[percpu::def_percpu]
static GDT: CpuGdt = CpuGdt::new();

/// Kernel code segment for 64-bit mode.
pub const KCODE64: SegmentSelector = SegmentSelector::new(1, PrivilegeLevel::Ring0);
/// Kernel data segment.
pub const KDATA: SegmentSelector = SegmentSelector::new(2, PrivilegeLevel::Ring0);
/// User data segment.
pub const UDATA: SegmentSelector = SegmentSelector::new(3, PrivilegeLevel::Ring3);
/// User code segment for 64-bit mode.
pub const UCODE64: SegmentSelector = SegmentSelector::new(4, PrivilegeLevel::Ring3);
/// Reserved two-entry, per-CPU LDT system descriptor.
pub const LDT: SegmentSelector = SegmentSelector::new(7, PrivilegeLevel::Ring0);

#[repr(C, packed)]
struct GdtPointer { limit: u16, base: u64 }

fn load_gdt(gdt: &CpuGdt) {
    let pointer = GdtPointer { limit: (core::mem::size_of::<CpuGdt>() - 1) as u16, base: gdt.entries.as_ptr() as u64 };
    unsafe { asm!("lgdt [{}]", in(reg) &pointer, options(nostack, preserves_flags)); }
}

/// Initializes the per-CPU TSS and GDT structures and loads them into the
/// current CPU.
pub(super) fn init() {
    let gdt = unsafe { GDT.current_ref_mut_raw() };
    gdt.entries[1] = 0x00af9b000000ffff;
    gdt.entries[2] = 0x00cf93000000ffff;
    gdt.entries[3] = 0x00cff3000000ffff;
    gdt.entries[4] = 0x00affb000000ffff;
    let base = unsafe { TSS.current_ref_raw() } as *const _ as u64;
    let limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u64;
    gdt.entries[5] = (limit & 0xffff) | ((base & 0xffffff) << 16)
        | (9 << 40) | (1 << 47) | (((limit >> 16) & 0xf) << 48)
        | (((base >> 24) & 0xff) << 56);
    gdt.entries[6] = base >> 32;
    let tss = SegmentSelector::new(5, PrivilegeLevel::Ring0);
    load_gdt(gdt);
    unsafe {
        CS::set_reg(KCODE64);
        load_tss(tss);
    }
}

/// Replaces this CPU's LDT system descriptor and LDTR.  IRQs and preemption
/// must be disabled; the caller retains `base` until all active CPUs reload.
pub unsafe fn load_ldt(base: *const u8, bytes: usize) {
    debug_assert!(!crate::asm::irqs_enabled());
    let gdt = unsafe { GDT.current_ref_mut_raw() };
    if bytes == 0 {
        gdt.entries[7] = 0; gdt.entries[8] = 0;
        let selector: u16 = 0;
        unsafe { asm!("lldt {0:x}", in(reg) selector, options(nostack, preserves_flags)); }
        return;
    }
    let base = base as u64;
    let limit = (bytes - 1) as u64;
    gdt.entries[7] = (limit & 0xffff) | ((base & 0xffffff) << 16)
        | (2 << 40) | (1 << 47) | (((limit >> 16) & 0xf) << 48)
        | (((base >> 24) & 0xff) << 56);
    gdt.entries[8] = base >> 32;
    let selector = LDT.0;
    unsafe { asm!("lldt {0:x}", in(reg) selector, options(nostack, preserves_flags)); }
}
