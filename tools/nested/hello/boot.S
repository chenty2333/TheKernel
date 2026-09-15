/*
 * boot.S -- Phase 2a hello kernel, 32-bit Multiboot v1 entry stub.
 *
 * Assembled with `as --32` and linked with `ld -m elf_i386`, producing an
 * ELF32/EM_386 executable: that is the format QEMU's -kernel Multiboot
 * loader accepts.  hw/i386/multiboot.c rejects an EM_X86_64 image outright
 * ("Cannot load x86-64 image, give a 32bit one"), then reloads the file
 * through load_elf(..., I386_ELF_MACHINE, ...).
 *
 * The artifact is therefore split in two halves:
 *
 *   boot.S   (this file, ELF32)  -- Multiboot header, 32-bit protected-mode
 *                                   bootstrap, page tables, long-mode entry.
 *   hello.c  (ELF64 payload)     -- the actual x86_64 kernel.
 *
 * The 64-bit payload is linked separately at KERNEL64_BASE, flattened to a
 * raw binary, and embedded here via .incbin in the .kernel64 section.  The
 * result is one contiguous ELF32 image, which QEMU requires:
 * load_multiboot() copies `elf_high - elf_low` bytes as a single blob
 * starting at the lowest load address.
 *
 * Entry contract on the first instruction (_start):
 *   EAX = 0x2BADB002  (MULTIBOOT_BOOTLOADER_MAGIC)
 *   EBX = physical address of the Multiboot info structure
 *   CS  = 32-bit flat code segment, DS/ES/FS/GS/SS = 32-bit flat data
 *   CR0.PE = 1, CR0.PG = 0 (protected mode, paging off), IF = 0
 *
 * The magic is *not* trusted here; it is passed through to kmain(), which
 * verifies it and reports the result on COM1.
 *
 * NOTE: this file is assembled by `as` directly, with no C preprocessor, so
 * constants use GAS .equ rather than #define ('#' starts a comment in GAS).
 */

/* ------------------------------------------------------------------ GDT */
/*
 * Our own GDT, installed before the switch to long mode.  Selectors:
 *   0x00  null
 *   0x08  32-bit code, base 0, limit 4 GiB, G=1 D=1 L=0
 *   0x10  32-bit data, base 0, limit 4 GiB, G=1 D=1 L=0
 *   0x18  64-bit code, base 0, limit 4 GiB, G=1 D=0 L=1   <-- long mode
 *   0x20  64-bit data, base 0, limit 4 GiB, G=1 D=1 L=0
 *
 * In long mode the base/limit of DS/ES/SS are ignored, so a flat data
 * descriptor is all that is needed.
 */
        .equ    SEL_CODE32, 0x08
        .equ    SEL_DATA32, 0x10
        .equ    SEL_CODE64, 0x18
        .equ    SEL_DATA64, 0x20

/* ------------------------------------------- page table geometry (64-bit) */
/*
 * Long mode uses 4-level PAE paging.  We identity-map the first 1 GiB with
 * 2 MiB pages, which covers everything the kernel touches: the image is
 * loaded at 1 MiB and the Multiboot info structure sits at 0x9500.
 *
 *   PML4[0] -> PDPT
 *   PDPT[0] -> PD
 *   PD[i]   = i * 2 MiB | P | RW | PS      (i = 0..511 -> 1 GiB)
 *
 * 2 MiB pages need no PSE bit in long mode; PDE.PS (bit 7) is sufficient.
 */
        .equ    PTE_P,       0x001      /* present                          */
        .equ    PTE_RW,      0x002      /* writable                         */
        .equ    PTE_PS,      0x080      /* 2 MiB page                       */
        .equ    PD_ENTRIES,  512        /* 512 * 2 MiB = 1 GiB              */
        .equ    PAGE_2MIB,   0x200000
        .equ    PT_BYTES,    12288      /* PML4 + PDPT + PD, one 4 KiB each */

/* Control-register and MSR bits used below. */
        .equ    MSR_EFER,    0xC0000080
        .equ    EFER_LME,    0x00000100 /* long mode enable                 */
        .equ    CR0_PE,      0x00000001
        .equ    CR0_PG,      0x80000000
        .equ    CR4_PAE,     0x00000020

/* ====================================================================== */
/* Section 1: the Multiboot v1 header.                                     */
/*                                                                         */
/* linker.ld places .multiboot first, so these 12 bytes land within the    */
/* first few hundred bytes of the file.  QEMU scans the first             */
/* 8192 - 48 bytes on 4-byte boundaries looking for the magic, then        */
/* validates that magic + flags + checksum == 0.                           */
/* ====================================================================== */
        .section .multiboot, "a", @progbits
        .align  4
        .long   0x1BADB002                  /* magic                        */
        .long   0x00000003                  /* flags: page-align + mem info */
        .long   -(0x1BADB002 + 0x00000003)  /* checksum                     */

/* ====================================================================== */
/* Section 2: 32-bit protected-mode bootstrap.                             */
/* ====================================================================== */
        .section .text.boot, "ax", @progbits
        .code32
        .globl  _start
        .type   _start, @function
_start:
        cli                                 /* no IDT installed yet         */
        cld

        /*
         * Save what the boot loader gave us before touching any register it
         * might also have used.  EAX (magic) and EBX (info pointer) are the
         * entire Multiboot entry contract.
         */
        movl    %eax, mb_magic
        movl    %ebx, mb_info

        /*
         * Install our own GDT immediately.  The Multiboot spec guarantees
         * the loader's segments have base 0 and limit 4 GiB (though not
         * particular selector values), so a DS-relative `lgdt` is safe
         * before anything else is set up.
         */
        lgdt    gdt_desc
        ljmp    $SEL_CODE32, $gdt_loaded

gdt_loaded:
        /* All data segments flat, from our own GDT. */
        movw    $SEL_DATA32, %ax
        movw    %ax, %ds
        movw    %ax, %es
        movw    %ax, %ss
        movw    %ax, %fs
        movw    %ax, %gs

        /* Our own stack: the boot loader's stack is not specified at all. */
        movl    $boot_stack_top, %esp
        xorl    %ebp, %ebp

        /* ---- build the identity page tables ---------------------------- */
        /*
         * .bootdata is PROGBITS, so these buffers arrive zeroed as part of
         * the image QEMU copies into RAM.  Clear them explicitly anyway, so
         * the stub does not silently depend on loader zero-fill behaviour.
         *
         * NOTE: long-mode page-table entries are 8 bytes wide (they hold a
         * 52-bit physical address plus the NX bit), unlike the 4-byte
         * entries of 32-bit non-PAE paging.  Every store below therefore
         * writes a low dword followed by an explicit zero high dword, and
         * the page-directory loop uses an 8-byte stride.
         */
        movl    $boot_pt, %edi
        xorl    %eax, %eax
        movl    $(PT_BYTES / 4), %ecx
        rep stosl

        /* PML4[0] = PDPT | P | RW */
        movl    $boot_pdpt, %eax
        orl     $(PTE_P | PTE_RW), %eax
        movl    %eax, boot_pml4
        movl    $0, boot_pml4 + 4

        /* PDPT[0] = PD | P | RW   (PDPTE.PS stays clear: 2 MiB pages, not 1 GiB) */
        movl    $boot_pd, %eax
        orl     $(PTE_P | PTE_RW), %eax
        movl    %eax, boot_pdpt
        movl    $0, boot_pdpt + 4

        /*
         * PD[i] = phys | P | RW | PS : 2 MiB pages, identity mapped.
         *
         * %eax:%ebx is the 64-bit physical base.  The 1 GiB map never leaves
         * the low 4 GiB, so %ebx stays zero, but the carry is propagated
         * anyway so the loop is correct if PD_ENTRIES is ever raised.
         */
        movl    $boot_pd, %edi
        xorl    %ecx, %ecx                  /* page index                   */
        xorl    %eax, %eax                  /* physical base, bits 31:0     */
        xorl    %ebx, %ebx                  /* physical base, bits 63:32    */
1:
        movl    %eax, %edx
        orl     $(PTE_P | PTE_RW | PTE_PS), %edx
        movl    %edx, (%edi, %ecx, 8)       /* low dword of PDE[i]          */
        movl    %ebx, 4(%edi, %ecx, 8)      /* high dword: address, no NX   */
        addl    $PAGE_2MIB, %eax
        adcl    $0, %ebx
        incl    %ecx
        cmpl    $PD_ENTRIES, %ecx
        jb      1b

        /* ---- enter long mode ------------------------------------------- */
        /* 1. CR4.PAE = 1 : PAE paging is mandatory in long mode. */
        movl    %cr4, %eax
        orl     $(CR4_PAE), %eax
        movl    %eax, %cr4

        /* 2. CR3 = physical address of the PML4. */
        movl    $boot_pml4, %eax
        movl    %eax, %cr3

        /* 3. EFER.LME = 1 : request long mode (armed until CR0.PG = 1). */
        movl    $MSR_EFER, %ecx
        rdmsr
        orl     $(EFER_LME), %eax
        wrmsr

        /*
         * 4. CR0.PG = 1 with EFER.LME already set activates long mode.  The
         *    current CS still has L = 0, so the CPU is in 32-bit
         *    compatibility mode for exactly one more instruction -- the far
         *    jump below.  Paging is on and the identity map covers this
         *    code, so the fetch after this store still works.
         */
        movl    %cr0, %eax
        orl     $(CR0_PG | CR0_PE), %eax
        movl    %eax, %cr0

        /*
         * Restore the Multiboot contract registers for the 64-bit payload.
         *
         * EDI and EBX do NOT survive the page-table setup above: the page
         * directory loop uses EDI as its destination pointer and EBX as the
         * high half of the physical base.  Reloading from the .bootdata
         * slots here, while still in 32-bit mode, is what carries the
         * original EAX/EBX across the mode switch.
         *
         * (Loading them after the far jump would need a 64-bit absolute
         * memory operand, which `as --32` cannot encode: it would emit
         * BFD_RELOC_X86_64_32S, a relocation the ELF32 container has no
         * room for.  32-bit absolute displacements are plain R_386_32.)
         */
        movl    mb_magic, %edi          /* arg 1: MULTIBOOT_BOOTLOADER_MAGIC */
        movl    mb_info, %esi           /* arg 2: physical address of the MBI */

        /* 5. Far jump into the 64-bit code segment (GDT selector 0x18). */
        ljmp    $SEL_CODE64, $long_mode_entry
        .size   _start, . - _start

/* ====================================================================== */
/* Section 3: 64-bit trampoline.                                           */
/*                                                                         */
/* Still part of the ELF32 image, just executing with CS.L = 1.  It loads  */
/* the data segments, installs the 64-bit stack, and tail-jumps into the   */
/* separately linked x86_64 payload.  EDI/ESI carry the Multiboot magic    */
/* and info pointer straight through to kmain().                           */
/* ====================================================================== */
        .code64
        .globl  long_mode_entry
        .type   long_mode_entry, @function
long_mode_entry:
        movw    $SEL_DATA64, %ax
        movw    %ax, %ds
        movw    %ax, %es
        movw    %ax, %ss
        movw    %ax, %fs
        movw    %ax, %gs

        /* EDI/ESI still hold the Multiboot magic and info pointer. */

        /*
         * `movl` into %esp zero-extends into %rsp, and boot_stack_top is
         * 16-byte aligned.  The SysV AMD64 ABI wants RSP % 16 == 8 on entry
         * to a function (i.e. RSP % 16 == 0 immediately before the call),
         * which is exactly what the payload's `call kmain` will observe.
         */
        movl    $boot_stack_top, %esp
        xorl    %ebp, %ebp

        /*
         * Tail-jump to the first byte of the embedded x86_64 payload.
         * KERNEL64_BASE is defined once in build.sh and passed to both the
         * assembler and the stage-1 linker script, so the value used here
         * and the address .kernel64 is placed at cannot drift apart.
         */
        movl    $KERNEL64_BASE, %eax
        jmp     *%rax
        .size   long_mode_entry, . - long_mode_entry

/* ====================================================================== */
/* Section 4: GDT and the LGDT operand.                                    */
/* ====================================================================== */
        .section .rodata32, "a", @progbits
        .align  8
        .globl  gdt
gdt:
        .quad   0x0000000000000000      /* 0x00 null                        */
        .quad   0x00CF9A000000FFFF      /* 0x08 32-bit code, base 0, 4 GiB  */
        .quad   0x00CF92000000FFFF      /* 0x10 32-bit data, base 0, 4 GiB  */
        .quad   0x00AF9A000000FFFF      /* 0x18 64-bit code, L=1, D=0       */
        .quad   0x00CF92000000FFFF      /* 0x20 64-bit data                 */
gdt_end:
gdt_desc:
        .word   gdt_end - gdt - 1       /* limit                            */
        .long   gdt                     /* 32-bit linear base               */

/* ====================================================================== */
/* Section 5: boot-time storage.                                           */
/*                                                                         */
/* PROGBITS on purpose.  QEMU's load_multiboot() copies the image as one   */
/* contiguous blob of `elf_high - elf_low` bytes and re-registers it via   */
/* fw_cfg, so every byte the kernel touches must be part of the file.  A   */
/* SHT_NOBITS (.bss) tail would be honest ELF but harder to reason about;  */
/* ~28 KiB of explicit zeros is cheap.                                     */
/* ====================================================================== */
        .section .bootdata, "aw", @progbits
        .align  8
        .globl  mb_magic
mb_magic:
        .long   0
        .globl  mb_info
mb_info:
        .long   0

        /*
         * One 12 KiB block holding PML4, PDPT and PD back to back, so the
         * single `rep stosl` above clears exactly the right range.
         */
        .align  4096
        .globl  boot_pt
boot_pt:
        .space  PT_BYTES, 0
        .equ    boot_pml4, boot_pt
        .equ    boot_pdpt, boot_pt + 4096
        .equ    boot_pd,   boot_pt + 8192

        /* 16 KiB stack shared by the 32-bit stub and the 64-bit kernel. */
        .align  16
boot_stack:
        .space  16384, 0
        .globl  boot_stack_top
boot_stack_top:

/* ====================================================================== */
/* Section 6: the embedded x86_64 payload.                                 */
/*                                                                         */
/* payload64.bin is entry64.S + hello.c linked as ELF64 at KERNEL64_BASE    */
/* and flattened with `objcopy -O binary`.  Its first byte must land at     */
/* exactly KERNEL64_BASE: load_multiboot() copies the image as one          */
/* contiguous blob, so a gap here would be copied as whatever the buffer    */
/* happened to hold, and boot.S jumps to the bare address KERNEL64_BASE     */
/* rather than looking up an ELF entry point.                               */
/* ====================================================================== */
        .section .kernel64, "a", @progbits
        .incbin "payload64.bin"

/* The image is freestanding code; do not ask for an executable stack. */
        .section .note.GNU-stack, "", @progbits
