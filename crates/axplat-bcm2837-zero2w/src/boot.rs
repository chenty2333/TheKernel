//! Boot-time entry point for the Raspberry Pi Zero 2W.
//!
//! The GPU firmware loads the kernel to physical `0x0008_0000` and starts the
//! ARM cores at EL2. The primary core runs [`_start`], which:
//!
//! 1. sets up a boot stack;
//! 2. drops to EL1 (`ax_cpu::init::switch_to_el1`);
//! 3. enables FP/SIMD;
//! 4. installs a flat early page table (identity mapping plus the
//!    `0xffff_0000_0000_0000` linear mapping, both covering the first 1 GB);
//! 5. enables the MMU and jumps to the kernel's high virtual address;
//! 6. installs the per-CPU runtime areas and calls the kernel entry point.
//!
//! The early page table is built by hand (2 MB block descriptors): the
//! physical RAM and peripheral window both lie in the first 1 GB, and because
//! `0xffff_0000_0000_0000` shares its L0/L1 indices with the low address
//! space, a single table serves both mappings.

#[cfg(not(feature = "legacy"))]
use ax_memory_addr::{PhysAddr, pa};
#[cfg(feature = "legacy")]
use axplat_old::mem::pa;

#[cfg(not(feature = "legacy"))]
use crate::config::KERNEL_ASPACE_BASE;
use crate::config::{
    BOOT_STACK_SIZE, MAX_CPU_NUM, PERIPHERAL_BASE, PERIPHERAL_SIZE, PHYS_VIRT_OFFSET,
};

#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

#[repr(align(4096))]
struct AlignedPageTable([u64; 512]);

#[unsafe(link_section = ".data")]
static mut BOOT_PT_L0: AlignedPageTable = AlignedPageTable([0; 512]);
#[unsafe(link_section = ".data")]
static mut BOOT_PT_L1: AlignedPageTable = AlignedPageTable([0; 512]);
#[unsafe(link_section = ".data")]
static mut BOOT_PT_L2: AlignedPageTable = AlignedPageTable([0; 512]);

const fn block_descriptor(paddr: usize, attr_index: u64, executable: bool) -> u64 {
    let mut desc: u64 = (paddr as u64 & 0x000f_ffff_f000) | 0b11; // valid + block
    desc |= attr_index << 2;
    desc |= 0b11 << 8; // AP[2:1] = EL1 RW
    desc |= 0b11 << 9; // inner shareable
    desc |= 1 << 10; // AF
    if !executable {
        desc |= 1 << 54; // UXN
    }
    desc
}

const fn table_descriptor(next_paddr: usize) -> u64 {
    (next_paddr as u64 & 0x000f_ffff_f000) | 0b11 // valid + table
}

/// Builds the flat early page table.
///
/// L2 entries are 2 MB blocks: `[0, PERIPHERAL_BASE)` is normal memory
/// (MAIR attr 1, executable) and `[PERIPHERAL_BASE, EARLY_MAP_SIZE)` is
/// device memory (MAIR attr 0, non-executable). L0/L1 route both the low
/// address space and the `0xffff_0000_0000_0000` linear mapping through the
/// same tables.
unsafe fn init_boot_page_table() {
    let l2 = pa!(&raw mut BOOT_PT_L2 as usize);
    let l1 = pa!(&raw mut BOOT_PT_L1 as usize);

    let normal_blocks = PERIPHERAL_BASE / 0x20_0000;
    let peripheral_blocks = PERIPHERAL_SIZE / 0x20_0000;

    // SAFETY: this runs on the boot CPU before any other core is online and
    // before the tables are referenced by the MMU.
    unsafe {
        for i in 0..normal_blocks {
            BOOT_PT_L2.0[i] = block_descriptor(i * 0x20_0000, 1, true);
        }
        for i in 0..peripheral_blocks {
            BOOT_PT_L2.0[normal_blocks + i] =
                block_descriptor(PERIPHERAL_BASE + i * 0x20_0000, 0, false);
        }
        BOOT_PT_L1.0[0] = table_descriptor(l2.as_usize());
        BOOT_PT_L0.0[0] = table_descriptor(l1.as_usize());
    }
    // Both the low address space and the high linear mapping reach L0[0]:
    // `0xffff_0000_0000_0000` has zeroes in VA[47:39], so no extra entries
    // are needed for the high mapping.
}

/// Installs the per-CPU runtime areas (ax-percpu layout) for the primary
/// core: initializes the whole layout (one area per CPU) and binds CPU 0.
///
/// The platform reserves `CPU_LOCAL_RESERVE_PADDR` for the CPU-local runtime
/// areas; the region is excluded from allocation by [`crate::mem`].
#[cfg(not(feature = "legacy"))]
fn install_cpu_local_primary() {
    let template_size = cpu_local::cpu_area_template_size()
        .expect("per-CPU template must be linked into the kernel image");
    // Keep the region page-aligned for simplicity; the layout validation only
    // requires prefix alignment (64 bytes) plus template object alignment.
    // The reserve size is a fixed platform constant (see CPU_LOCAL_RESERVE_SIZE);
    // template growth beyond it fails the layout validation at boot.
    debug_assert!(template_size <= CPU_LOCAL_RESERVE_SIZE);
    let area_stride = CPU_LOCAL_RESERVE_SIZE;
    let area_count = core::num::NonZeroU32::new(MAX_CPU_NUM as u32).expect("nonzero");
    let runtime_base = KERNEL_ASPACE_BASE + CPU_LOCAL_RESERVE_PADDR;
    let region = ax_percpu::PerCpuRegion::new(
        core::ptr::NonNull::new(runtime_base as *mut u8).expect("nonzero region base"),
        area_stride,
        area_count,
    );
    // SAFETY: the region names exclusively reserved, writable RAM that stays
    // mapped for the kernel lifetime; no CPU accesses it before this call.
    let layout = unsafe { ax_percpu::initialize_layout(region) }
        .expect("per-CPU layout initialization must succeed");
    let _ = layout; // layout is published globally; areas are bound per CPU.
    install_cpu_local_cpu(0);
}

/// Binds the current CPU to its per-CPU runtime area. Called on the primary
/// core after `initialize_layout` and on every secondary core at its entry.
#[cfg(not(feature = "legacy"))]
fn install_cpu_local_cpu(cpu_id: usize) {
    let area = ax_percpu::area(cpu_local::CpuIndex::from_u32(cpu_id as u32).expect("CPU in range"))
        .expect("CPU area must exist")
        .cpu_area()
        .expect("CPU area must be addressable");
    // SAFETY: boot-time core, IRQs masked, no scheduler running.
    unsafe { cpu_local::install_cpu_area(area) }.expect("CPU-local area must install");
}

/// Physical address of the reserved CPU-local runtime area.
pub const CPU_LOCAL_RESERVE_PADDR: usize = 0x0100_0000;
/// Size of the reserved CPU-local runtime area (per-CPU stride * CPU count).
pub const CPU_LOCAL_RESERVE_SIZE: usize = 0x1_0000 * MAX_CPU_NUM;

/// Secondary-core boot parameters: per-CPU stack top (physical address),
/// written by `PowerIf::cpu_boot` on the primary core and read by the
/// secondary-core entry assembly (via the identity mapping, so the
/// PC-relative address equals the physical address).
#[cfg(feature = "smp")]
static mut SMP_PARAMS: [usize; MAX_CPU_NUM] = [0; MAX_CPU_NUM];

/// Stores the secondary stack top (called from `cpu_boot`).
#[cfg(feature = "smp")]
pub(crate) fn secondary_stack_store(cpu_id: usize, stack_top_paddr: usize) {
    // SAFETY: written before the secondary core is released from reset.
    unsafe { SMP_PARAMS[cpu_id] = stack_top_paddr };
}

/// Physical address of the secondary-core entry point.
#[cfg(feature = "smp")]
pub(crate) fn secondary_entry_paddr() -> usize {
    let entry: *const () = _start_secondary as *const ();
    entry as usize - PHYS_VIRT_OFFSET
}

// Architecture helpers selected by the interface feature. Both ax-cpu (new
// tgoskits interface) and axcpu (legacy crates.io interface) provide the same
// boot-time functions; the wrappers keep the assembly independent of the
// selected crate.
#[cfg(not(feature = "legacy"))]
unsafe extern "C" fn plat_switch_to_el1() {
    unsafe { ax_cpu::init::switch_to_el1() }
}
#[cfg(not(feature = "legacy"))]
unsafe extern "C" fn plat_init_mmu(root_paddr: usize) {
    unsafe { ax_cpu::init::init_mmu(PhysAddr::from(root_paddr)) }
}
#[cfg(not(feature = "legacy"))]
unsafe extern "C" fn plat_enable_fp() {
    ax_cpu::asm::enable_fp();
}

#[cfg(feature = "legacy")]
unsafe extern "C" fn plat_switch_to_el1() {
    unsafe { axcpu_old::init::switch_to_el1() }
}
#[cfg(feature = "legacy")]
unsafe extern "C" fn plat_init_mmu(root_paddr: usize) {
    unsafe { axcpu_old::init::init_mmu(axplat_old::mem::PhysAddr::from(root_paddr)) }
}
#[cfg(feature = "legacy")]
unsafe extern "C" fn plat_enable_fp() {
    axcpu_old::asm::enable_fp();
}

/// The earliest entry point for the primary CPU, called by the GPU firmware
/// at physical `0x0008_0000` with the CPU in EL2.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.boot")]
unsafe extern "C" fn _start() -> ! {
    // X0 = dtb (unused on this platform; the firmware passes no FDT)
    core::arch::naked_asm!(
        "
        mrs     x19, mpidr_el1
        and     x19, x19, #0xff         // CPU id

        adrp    x8, {boot_stack}        // setup boot stack (low address)
        add     x8, x8, {boot_stack_size}
        mov     sp, x8

        bl      {switch_to_el1}         // EL2 -> EL1
        bl      {enable_fp}
        bl      {init_boot_page_table}
        adrp    x0, {boot_pt}
        bl      {init_mmu}              // enable MMU (x0 = page table paddr)

        mov     x8, {phys_virt_offset}  // jump to the high address space
        add     sp, sp, x8

        mov     x0, x19                 // call_platform_main(cpu_id, 0)
        mov     x1, xzr
        ldr     x8, ={entry}
        blr     x8
        b       .",
        switch_to_el1 = sym plat_switch_to_el1,
        init_mmu = sym plat_init_mmu,
        enable_fp = sym plat_enable_fp,
        init_boot_page_table = sym init_boot_page_table,
        boot_pt = sym BOOT_PT_L0,
        boot_stack = sym BOOT_STACK,
        boot_stack_size = const BOOT_STACK_SIZE,
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        entry = sym bcm2837_main,
    )
}

/// Platform entry before the kernel main: installs CPU-local state, then
/// hands control to the kernel entry point selected by the interface feature.
#[unsafe(no_mangle)]
fn bcm2837_main(cpu_id: usize, arg: usize) -> ! {
    #[cfg(not(feature = "legacy"))]
    {
        install_cpu_local_primary();
    }
    #[cfg(not(feature = "legacy"))]
    {
        ax_plat::call_main(cpu_id, arg)
    }
    #[cfg(feature = "legacy")]
    {
        // The legacy axplat runtime owns its per-CPU state.
        axplat_old::call_main(cpu_id, arg)
    }
}

/// The earliest entry point for the secondary CPUs, released from reset by
/// the primary core through the BCM2836 local mailbox. The firmware starts
/// the core at EL2; the identity mapping (built by the primary core) is
/// already active, so PC-relative access to the boot parameters works at
/// physical addresses.
#[cfg(feature = "smp")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn _start_secondary() -> ! {
    core::arch::naked_asm!(
        "
        mrs     x19, mpidr_el1
        and     x19, x19, #0xff         // CPU id

        adrp    x8, {smp_params}        // stack top from boot params (low addr)
        ldr     x9, [x8, x19, lsl #3]
        mov     sp, x9

        bl      {switch_to_el1}         // EL2 -> EL1
        bl      {enable_fp}
        adrp    x0, {boot_pt}
        bl      {init_mmu}              // enable MMU (shared boot page table)

        mov     x8, {phys_virt_offset}  // jump to the high address space
        add     sp, sp, x8

        mov     x0, x19                 // call_platform_secondary_main(cpu_id)
        ldr     x8, ={entry}
        blr     x8
        b       .",
        switch_to_el1 = sym plat_switch_to_el1,
        init_mmu = sym plat_init_mmu,
        enable_fp = sym plat_enable_fp,
        boot_pt = sym BOOT_PT_L0,
        smp_params = sym SMP_PARAMS,
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        entry = sym bcm2837_secondary_main,
    )
}

/// Platform entry for the secondary cores: bind the CPU-local area and hand
/// control to the kernel's secondary entry point.
#[cfg(feature = "smp")]
#[unsafe(no_mangle)]
fn bcm2837_secondary_main(cpu_id: usize) -> ! {
    #[cfg(not(feature = "legacy"))]
    {
        install_cpu_local_cpu(cpu_id);
    }
    #[cfg(not(feature = "legacy"))]
    {
        ax_plat::call_secondary_main(cpu_id)
    }
    #[cfg(feature = "legacy")]
    {
        // The legacy axplat runtime owns its per-CPU state.
        axplat_old::call_secondary_main(cpu_id)
    }
}
