/*
 * linker.ld -- stage-1 image layout.
 *
 * Produces the file that QEMU's `-kernel` Multiboot v1 loader consumes:
 * ELF32, little-endian, EM_386, non-relocatable, with the Multiboot header
 * inside the first 8192 bytes of the file.
 *
 * The image is a single contiguous run of bytes, because QEMU does not treat
 * it as a normal ELF after validation:
 *
 *   hw/i386/multiboot.c
 *       mh_load_addr    = elf_low;
 *       mb_kernel_size  = elf_high - elf_low;
 *       rom_copy(mbs.mb_buf, mh_load_addr, mb_kernel_size);
 *   ...
 *   pc-bios/optionrom/multiboot_dma.S
 *       read_fw_blob_dma(FW_CFG_KERNEL)   -- one flat copy to elf_low
 *
 * So every byte from the lowest to the highest load address is copied to
 * physical memory verbatim, and any gap between sections is copied as
 * whatever happened to be in the buffer.  .bootdata is PROGBITS for exactly
 * this reason: the page tables and boot stack are real bytes in the file
 * rather than an SHT_NOBITS tail.
 *
 * Layout (LOAD_BASE and KERNEL64_BASE come from build.sh via --defsym):
 *
 *   0x100000  .boot       Multiboot header, 32-bit stub, GDT
 *   0x101000  .kernel64   the embedded x86_64 payload (flat binary)
 *   ...       .bootdata   page tables, boot stack, saved magic/mbi
 */

ENTRY(_start)

SECTIONS
{
    . = LOAD_BASE;

    /*
     * .multiboot must be first: QEMU scans the raw file's first 8192 bytes
     * for the magic, so the header has to precede everything else.  That
     * also means _start is LOAD_BASE + 12, not LOAD_BASE; build.sh asserts
     * __boot_start == LOAD_BASE and that _start lies inside the stub slot.
     */
    .boot ALIGN(16) : {
        __boot_start = .;
        KEEP(*(.multiboot))
        *(.text.boot)
        *(.rodata32)
    }
    __boot_end = .;

    /*
     * The 32-bit stub must fit in its 4 KiB slot before the payload.
     * ASSERT is evaluated while the linker script runs, so an oversized stub
     * fails the build rather than silently overlapping the payload.
     */
    ASSERT(__boot_end <= KERNEL64_BASE,
           "32-bit boot stub overflows its 4 KiB slot before KERNEL64_BASE")

    /*
     * The x86_64 payload, linked separately at KERNEL64_BASE and embedded by
     * .incbin in boot.S.  Placing it at exactly KERNEL64_BASE keeps the
     * image contiguous and matches the address boot.S jumps to.
     */
    . = KERNEL64_BASE;
    .kernel64 ALIGN(16) : {
        __kernel64_start = .;
        KEEP(*(.kernel64))
        __kernel64_end = .;
    }

    /* Page tables, boot stack, saved Multiboot registers (all PROGBITS). */
    .bootdata ALIGN(16) : {
        *(.bootdata)
    }
    __image_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note.gnu.build-id)
        *(.note.gnu.property)
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
