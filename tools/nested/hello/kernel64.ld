/*
 * kernel64.ld -- stage-2 layout for the x86_64 payload.
 *
 * The payload is linked as a normal ELF64 at KERNEL64_BASE, then flattened
 * with `objcopy -O binary` and embedded into the ELF32 stage-1 image by
 * .incbin in boot.S.  Only the flat bytes survive, so this script must
 * produce exactly one contiguous, fully initialised image:
 *
 *   - .text.boot64 (entry64.S) is pinned first, because boot.S jumps to the
 *     bare address KERNEL64_BASE with no ELF entry lookup.  build.sh asserts
 *     that _start64 == KERNEL64_BASE.
 *   - No SHT_NOBITS section may exist.  `objcopy -O binary` drops NOBITS
 *     contents, so a .bss here would silently become a hole in the payload.
 *     hello.c uses no zero-initialised globals and is compiled with
 *     -fno-zero-initialized-in-bss; build.sh re-checks the linked ELF for
 *     NOBITS sections and fails the build if one appears.
 */

ENTRY(_start64)

SECTIONS
{
    . = KERNEL64_BASE;

    .payload ALIGN(16) : {
        KEEP(*(.text.boot64))
        *(.text .text.*)
        *(.rodata .rodata.*)
        *(.data .data.*)
        *(.got .got.plt)
    }
    __payload_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note.gnu.build-id)
        *(.note.gnu.property)
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
