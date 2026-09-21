use lazyinit::LazyInit;
use x86_64::{
    addr::VirtAddr,
    structures::idt::{Entry, InterruptDescriptorTable},
};

const NUM_INT: usize = 256;

/// Vectors that run on their own interrupt stack instead of the interrupted
/// context's stack, paired with the TSS slot that holds it.
///
/// Linux does the same for DF, NMI, DB and MCE
/// (`tss->x86_tss.ist[IST_INDEX_*]`, arch/x86/kernel/cpu/common.c:2375-2378);
/// a kernel stack overflow is a double fault, so without an IST for #DF the
/// report cannot be written anywhere and the CPU triple-faults.  The list and
/// the slot numbers must match the `.equ IST_VECTOR_*` values in `trap.S`,
/// which routes these vectors through the frame-relocation stub, and
/// `gdt::init`, which fills the slots before this table is loaded.
const IST_VECTORS: [(u8, u16); 3] = [
    (2, super::gdt::NMI_IST_INDEX),
    (8, super::gdt::DOUBLE_FAULT_IST_INDEX),
    (18, super::gdt::MACHINE_CHECK_IST_INDEX),
];

static IDT: LazyInit<InterruptDescriptorTable> = LazyInit::new();

/// Initializes the global IDT and loads it into the current CPU.
pub(super) fn init() {
    IDT.call_once(|| {
        unsafe extern "C" {
            #[link_name = "trap_handler_table"]
            static ENTRIES: [VirtAddr; NUM_INT];
        }
        let mut table = InterruptDescriptorTable::new();
        let entries = unsafe {
            core::mem::transmute::<&mut InterruptDescriptorTable, &mut [Entry<()>; NUM_INT]>(
                &mut table,
            )
        };
        for i in 0..NUM_INT {
            let opt = unsafe { entries[i].set_handler_addr(ENTRIES[i]) };
            if i == 0x3 || i == 0x80 {
                // enable user space breakpoints and legacy int 0x80 syscall
                opt.set_privilege_level(x86_64::PrivilegeLevel::Ring3);
            }
            if let Some((_, ist_index)) = IST_VECTORS.iter().find(|(v, _)| *v as usize == i) {
                // A PMI delivered as an APIC NMI, a machine check, or the
                // double fault itself must not borrow an arbitrary interrupted
                // task's stack. `gdt::init` populates every slot named here
                // before this IDT is loaded on that CPU.
                // SAFETY: each `IST_VECTORS` slot index is below the TSS
                // interrupt stack table length asserted in `gdt.rs`.
                unsafe { opt.set_stack_index(*ist_index) };
            }
        }

        table
    });
    IDT.load();
}
