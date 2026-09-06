use core::arch::asm;

use x86_64::{
    addr::VirtAddr,
    instructions::tables::load_tss,
    registers::segmentation::{Segment, SegmentSelector, CS},
    structures::tss::TaskStateSegment,
    PrivilegeLevel,
};

/// The x86 TSS IST slot dedicated to non-maskable interrupts.
pub(super) const NMI_IST_INDEX: u16 = 0;
const NMI_STACK_BYTES: usize = 16 * 1024;

#[repr(C, align(16))]
struct NmiStack([u8; NMI_STACK_BYTES]);

impl NmiStack {
    const fn new() -> Self {
        Self([0; NMI_STACK_BYTES])
    }
}

/// Number of architecturally addressable I/O ports.
pub(super) const IO_BITMAP_BYTES: usize = 65_536 / 8;
const IO_BITMAP_TERMINATOR_BYTES: usize = 1;

/// Keep the permission map contiguous with the TSS: the CPU interprets
/// `iomap_base` as an offset from the TSS descriptor base.
#[repr(C, align(16))]
struct TssWithIoBitmap {
    tss: TaskStateSegment,
    bitmap: [u8; IO_BITMAP_BYTES + IO_BITMAP_TERMINATOR_BYTES],
}

impl TssWithIoBitmap {
    const fn new() -> Self {
        Self {
            tss: TaskStateSegment::new(),
            // A one bit denies the corresponding port. The required byte
            // after the 65536-port map is also all ones.
            bitmap: [0xff; IO_BITMAP_BYTES + IO_BITMAP_TERMINATOR_BYTES],
        }
    }

    const fn invalid_iomap_base() -> u16 {
        // The GDT limit includes `bitmap`; this is exactly one byte beyond
        // its final terminator and makes every CPL3 port access fault.
        (core::mem::size_of::<TaskStateSegment>() + IO_BITMAP_BYTES + IO_BITMAP_TERMINATOR_BYTES)
            as u16
    }
}

#[percpu::def_percpu]
#[unsafe(no_mangle)]
static TSS: TssWithIoBitmap = TssWithIoBitmap::new();

// An IST belongs to a CPU, exactly like the TSS.  It is deliberately separate
// from task kernel stacks, so an overflow PMI/NMI cannot consume an arbitrary
// interrupted task's stack. Intel NMI blocking prevents another architectural
// NMI from re-entering this stack before IRET; #MC has its own architectural
// vector and is not installed on this IST.
#[percpu::def_percpu]
static NMI_STACK: NmiStack = NmiStack::new();

#[repr(C, align(16))]
struct CpuGdt {
    entries: [u64; 9],
}
impl CpuGdt {
    const fn new() -> Self {
        Self { entries: [0; 9] }
    }
}

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
struct GdtPointer {
    limit: u16,
    base: u64,
}

fn load_gdt(gdt: &CpuGdt) {
    let pointer = GdtPointer {
        limit: (core::mem::size_of::<CpuGdt>() - 1) as u16,
        base: gdt.entries.as_ptr() as u64,
    };
    unsafe {
        asm!("lgdt [{}]", in(reg) &pointer, options(nostack, preserves_flags));
    }
}

fn ldt_data_selector_is_usable(base: *const u8, bytes: usize, selector: u16) -> bool {
    let index = (selector as usize) >> 3;
    if index >= bytes / core::mem::size_of::<u64>() {
        return false;
    }
    let descriptor = unsafe { core::ptr::read_unaligned(base.cast::<u64>().add(index)) };
    let ty = (descriptor >> 40) & 0xf;
    // LDT entries emitted by modify_ldt are ring-3 code/data descriptors.
    // DS/ES accept data descriptors and readable code descriptors only.
    descriptor & (1 << 47) != 0
        && descriptor & (1 << 44) != 0
        && (descriptor >> 45) & 3 == 3
        && (ty & 8 == 0 || ty & 2 != 0)
}

unsafe fn refresh_ldt_data_segments(base: *const u8, bytes: usize) {
    macro_rules! refresh {
        ($segment:literal) => {{
            let selector: u16;
            unsafe {
                asm!(
                    concat!("mov {selector:x}, ", $segment),
                    selector = out(reg) selector,
                    options(nostack, preserves_flags)
                )
            };
            if selector & 4 != 0 {
                let selector = ldt_data_selector_is_usable(base, bytes, selector)
                    .then_some(selector)
                    .unwrap_or(0);
                unsafe {
                    asm!(
                        concat!("mov ", $segment, ", {selector:x}"),
                        selector = in(reg) selector,
                        options(nostack, preserves_flags)
                    )
                };
            }
        }};
    }

    refresh!("ds");
    refresh!("es");
}

/// Installs the current task's I/O permissions for the imminent user return.
///
/// The caller must have disabled preemption and interrupts. A TSS belongs to
/// a CPU, not a task, so it must be refreshed at every final return to ring 3.
pub(super) fn install_user_io_bitmap(
    bitmap: Option<&[u8; IO_BITMAP_BYTES]>,
    revoked: Option<&[u8; IO_BITMAP_BYTES]>,
    allow_all: bool,
) {
    let tss = unsafe { TSS.current_ref_mut_raw() };
    match (allow_all, bitmap) {
        (true, _) => {
            tss.bitmap[..IO_BITMAP_BYTES].fill(0);
            tss.tss.iomap_base = core::mem::size_of::<TaskStateSegment>() as u16;
        }
        (false, Some(bitmap)) => {
            tss.bitmap[..IO_BITMAP_BYTES].copy_from_slice(bitmap);
            if let Some(revoked) = revoked {
                for (entry, revoked) in tss.bitmap[..IO_BITMAP_BYTES].iter_mut().zip(revoked) {
                    *entry |= revoked;
                }
            }
            tss.tss.iomap_base = core::mem::size_of::<TaskStateSegment>() as u16;
        }
        (false, None) => {
            tss.tss.iomap_base = TssWithIoBitmap::invalid_iomap_base();
        }
    }
    // Never let a partial copy turn the final byte into an allowed port.
    tss.bitmap[IO_BITMAP_BYTES] = 0xff;
}

/// Initializes the per-CPU TSS and GDT structures and loads them into the
/// current CPU.
pub(super) fn init() {
    let nmi_stack = unsafe { NMI_STACK.current_ref_raw() };
    let gdt = unsafe { GDT.current_ref_mut_raw() };
    gdt.entries[1] = 0x00af9b000000ffff;
    gdt.entries[2] = 0x00cf93000000ffff;
    gdt.entries[3] = 0x00cff3000000ffff;
    gdt.entries[4] = 0x00affb000000ffff;
    let tss_storage = unsafe { TSS.current_ref_mut_raw() };
    let nmi_top = unsafe { nmi_stack.0.as_ptr().add(NMI_STACK_BYTES) } as u64;
    tss_storage.tss.interrupt_stack_table[NMI_IST_INDEX as usize] = VirtAddr::new(nmi_top);
    let base = tss_storage as *const _ as u64;
    let limit =
        (core::mem::size_of::<TaskStateSegment>() + IO_BITMAP_BYTES + IO_BITMAP_TERMINATOR_BYTES
            - 1) as u64;
    gdt.entries[5] = (limit & 0xffff)
        | ((base & 0xffffff) << 16)
        | (9 << 40)
        | (1 << 47)
        | (((limit >> 16) & 0xf) << 48)
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
        gdt.entries[7] = 0;
        gdt.entries[8] = 0;
        let selector: u16 = 0;
        unsafe {
            asm!("lldt {0:x}", in(reg) selector, options(nostack, preserves_flags));
        }
        unsafe { refresh_ldt_data_segments(base, bytes) };
        return;
    }
    let table_base = base;
    let base = base as u64;
    let limit = (bytes - 1) as u64;
    gdt.entries[7] = (limit & 0xffff)
        | ((base & 0xffffff) << 16)
        | (2 << 40)
        | (1 << 47)
        | (((limit >> 16) & 0xf) << 48)
        | (((base >> 24) & 0xff) << 56);
    gdt.entries[8] = base >> 32;
    let selector = LDT.0;
    unsafe {
        asm!("lldt {0:x}", in(reg) selector, options(nostack, preserves_flags));
    }
    unsafe { refresh_ldt_data_segments(table_base, bytes) };
}
