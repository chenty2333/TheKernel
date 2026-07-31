/*
 * axplat.x template for the Raspberry Pi Zero 2W (BCM2710A1, BCM2837 die).
 *
 * The GPU firmware loads the kernel image to physical 0x0008_0000 and starts
 * the ARM at EL2. The kernel is linked at the high virtual address
 * 0xffff_0000_0008_0000 (phys-virt-offset 0xffff_0000_0000_0000); all sections
 * are given LMA = VMA - AX_LINKER_LOAD_OFFSET so the flat binary firmware
 * loads lands at the physical addresses the boot page table maps.
 *
 * This file is emitted as `axplat.x` into OUT_DIR by build.rs and is
 * INCLUDE'd at the top of axruntime's linker script.
 */

OUTPUT_ARCH(aarch64)
ENTRY(_start)

/* Kernel image virtual load address (VM_LOAD_ADDRESS), used by axruntime. */
PROVIDE(VM_LOAD_ADDRESS = 0xffff_0000_0008_0000);
/* Kernel image physical load address (firmware load point). */
PROVIDE(PHYS_LOAD_ADDRESS = 0x0008_0000);
/* LMA = VMA - AX_LINKER_LOAD_OFFSET. */
AX_LINKER_LOAD_OFFSET = 0xffff_0000_0000_0000;

/* Maximum CPU count booted by this platform (CPU 0 only for now). */
__SMP = {{SMP}};

PROVIDE(PAGE_SIZE = 0x1000);
PROVIDE(STACK_SIZE = 0x40000);

SECTIONS {
    . = VM_LOAD_ADDRESS;
    _stext = .;

    .text.boot : AT(ADDR(.text.boot) - AX_LINKER_LOAD_OFFSET) ALIGN(PAGE_SIZE) {
        KEEP(*(.text.boot))
    }

    .text : AT(ADDR(.text) - AX_LINKER_LOAD_OFFSET) ALIGN(PAGE_SIZE) {
        *(.text .text.*)
        *(.init.text)
        *(.fixup)
        *(.gnu.warning)
    }
    . = ALIGN(PAGE_SIZE);
    _etext = .;

    .rodata : AT(ADDR(.rodata) - AX_LINKER_LOAD_OFFSET) ALIGN(PAGE_SIZE) {
        *(.rodata)
        *(.rodata.*)
        *(.data.rel.ro*)
    }
    . = ALIGN(PAGE_SIZE);
    _erodata = .;

    .data : AT(ADDR(.data) - AX_LINKER_LOAD_OFFSET) ALIGN(16) {
        *(.data .data.*)
        *(.init.data)
        *(.sdata .sdata.*)
        *(.scommon)
    }
    _edata = .;

    /* Per-CPU runtime areas: template section consumed by ax-percpu. The
     * runtime areas themselves live in RAM reserved by the platform, not in
     * the kernel image; this section only carries the per-CPU template. */
    .percpu.init : ALIGN(8) {
        __PERCPU_INIT_START = .;
        KEEP(*(.percpu.init))
        __PERCPU_INIT_END = .;
    }

    .percpu.align : ALIGN(8) {
        __PERCPU_ALIGN_START = .;
        KEEP(*(.percpu.align))
        __PERCPU_ALIGN_END = .;
        __PERCPU_TEMPLATE_ALIGN_START = .;
        __PERCPU_TEMPLATE_ALIGN_END =
            . + MAX(64, ALIGNOF(.percpu.template));
    }

    .percpu.template : {
        KEEP(*(.percpu.template.header))
        *(SORT_BY_NAME(.percpu.template.storage*))
        KEEP(*(.percpu.template.end))
    }
    ASSERT(__CPU_LOCAL_AREA_PREFIX == ADDR(.percpu.template),
           "CPU area prefix must be at template offset zero")
    ASSERT(__CPU_LOCAL_TEMPLATE_END + 1 ==
               ADDR(.percpu.template) + SIZEOF(.percpu.template),
           "CPU area end sentinel must follow every template object")

    .sbss : AT(ADDR(.sbss) - AX_LINKER_LOAD_OFFSET) {
        *(.dynsbss)
        *(.sbss)
        *(.scommon)
    }

    __bss_start = .;
    .bss : AT(ADDR(.bss) - AX_LINKER_LOAD_OFFSET) ALIGN(PAGE_SIZE) {
        *(.dynbss)
        *(.init.bss)
        *(.bss .bss.*)
        *(.bss.stack)
        *(COMMON)

        /* Boot CPU stack. */
        . = ALIGN(16);
        __cpu0_stack = .;
        . += STACK_SIZE;
        __cpu0_stack_top = .;
    }
    . = ALIGN(PAGE_SIZE);
    __bss_stop = .;
    _end = .;
    __kernel_code_end = .;
}
