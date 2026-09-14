/*
 * multiboot.h -- Multiboot v1 (spec 0.6.96) definitions used by the
 * Phase 2a "hello" kernel.
 *
 * This header deliberately depends on nothing: no <stdint.h>, no <stddef.h>,
 * no other C library header.  The kernel is built with -nostdinc, so if a
 * libc header ever sneaks in here the build fails instead of silently
 * acquiring a host runtime dependency.
 *
 * Contract implemented here (Multiboot Specification v0.6.96, section 3.2):
 *
 *   - The kernel image carries a 12-byte Multiboot header, 4-byte aligned,
 *     inside the first 8192 bytes of the file.
 *   - magic    = 0x1BADB002
 *     flags    = MULTIBOOT_HEADER_FLAGS
 *     checksum = -(magic + flags)   (mod 2^32)
 *   - A boot loader that accepts the image jumps to the ELF entry point with
 *     EAX = 0x2BADB002 and EBX = physical address of the Multiboot info
 *     structure, in 32-bit protected mode, paging off, interrupts disabled.
 *
 * QEMU implements exactly that: hw/i386/multiboot.c validates the header and
 * hw/i386/../pc-bios/optionrom/multiboot_dma.S does
 *     movl $0x2badb002, %eax ; jmp *%ecx
 * with EBX loaded from FW_CFG_INITRD_ADDR (0x9500).
 */

#ifndef NESTED_HELLO_MULTIBOOT_H
#define NESTED_HELLO_MULTIBOOT_H

/* ------------------------------------------------------------------ types */
/*
 * Fixed-width types, spelled out here instead of pulling in <stdint.h>.
 * The kernel targets x86_64 System V only; these are the widths the ABI
 * guarantees for the underlying compiler.
 */
typedef unsigned char      mb_u8;
typedef unsigned short     mb_u16;
typedef unsigned int       mb_u32;
typedef unsigned long long mb_u64;

/* --------------------------------------------------------- header (v1) */

/* Value the boot loader puts in EAX. */
#define MULTIBOOT_BOOTLOADER_MAGIC 0x2BADB002u

/* Value the kernel puts at the start of its Multiboot header. */
#define MULTIBOOT_HEADER_MAGIC     0x1BADB002u

/*
 * Requested features:
 *   bit 0 (0x1)  align boot modules on page boundaries
 *   bit 1 (0x2)  provide mem_lower / mem_upper in the info structure
 *
 * Bit 16 (0x10000, the "a.out kludge" / explicit load addresses) is left
 * clear on purpose: with it clear, QEMU takes the ELF path in
 * load_multiboot() and derives the load address and entry point from the
 * ELF program headers.  Setting it would make QEMU treat the file as a flat
 * binary and read load_addr/entry_addr out of the header instead.
 *
 * Bit 2 (0x4, video mode request) must stay clear: QEMU answers
 * "multiboot knows VBE. we don't" and would then ignore the request anyway.
 */
#define MULTIBOOT_HEADER_FLAGS     0x00000003u

/*
 * The 32-bit sum magic + flags + checksum must be zero, so the checksum is
 * the two's-complement negation of the other two fields.
 */
#define MULTIBOOT_HEADER_CHECKSUM \
    ((mb_u32)(0u - (MULTIBOOT_HEADER_MAGIC + MULTIBOOT_HEADER_FLAGS)))

/* ------------------------------------------------------- info (v1) */

/* Bits of multiboot_info.flags that this kernel inspects. */
#define MULTIBOOT_INFO_MEMORY      0x00000001u  /* mem_lower/mem_upper valid */
#define MULTIBOOT_INFO_BOOTDEV     0x00000002u
#define MULTIBOOT_INFO_CMDLINE     0x00000004u
#define MULTIBOOT_INFO_MODS        0x00000008u
#define MULTIBOOT_INFO_MMAP        0x00000040u  /* mmap_* valid            */
#define MULTIBOOT_INFO_BOOTLOADER  0x00000200u  /* boot_loader_name valid  */

/*
 * Multiboot v1 information structure.  Offsets are fixed by the
 * specification; this struct is the canonical layout and is never written
 * by the kernel, only read.
 */
struct multiboot_info {
    mb_u32 flags;               /*  0: which of the fields below are valid */
    mb_u32 mem_lower;           /*  4: KiB of conventional memory          */
    mb_u32 mem_upper;           /*  8: KiB of memory above 1 MiB           */
    mb_u32 boot_device;         /* 12                                      */
    mb_u32 cmdline;             /* 16: physical address of cmdline string  */
    mb_u32 mods_count;          /* 20                                      */
    mb_u32 mods_addr;           /* 24                                      */
    mb_u32 syms[4];             /* 28: union a.out/ELF symbol table info   */
    mb_u32 mmap_length;         /* 44: bytes of memory map                 */
    mb_u32 mmap_addr;           /* 48: physical address of memory map      */
    mb_u32 drives_length;       /* 52                                      */
    mb_u32 drives_addr;         /* 56                                      */
    mb_u32 config_table;        /* 60                                      */
    mb_u32 boot_loader_name;    /* 64: physical address of loader name     */
    mb_u32 apm_table;           /* 68                                      */
    mb_u32 vbe_control_info;    /* 72                                      */
    mb_u32 vbe_mode_info;       /* 76                                      */
    mb_u16 vbe_mode;            /* 80                                      */
    mb_u16 vbe_interface_seg;   /* 82                                      */
    mb_u16 vbe_interface_off;   /* 84                                      */
    mb_u16 vbe_interface_len;   /* 86                                      */
};

/* Size of the structure above, as QEMU builds it (MBI_SIZE == 88). */
#define MULTIBOOT_INFO_SIZE 88u

#endif /* NESTED_HELLO_MULTIBOOT_H */
