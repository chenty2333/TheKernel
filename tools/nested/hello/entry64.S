/*
 * entry64.S -- entry trampoline for the x86_64 payload of the Phase 2a
 * hello kernel.
 *
 * This file is assembled with `as --64` and linked as ELF64 at
 * KERNEL64_BASE.  Its first byte is deliberately the payload's entry point:
 * boot.S tail-jumps to KERNEL64_BASE with no ELF lookup involved, so
 * linker.ld pins .text.boot64 first in the payload image and build.sh
 * asserts that _start64 == KERNEL64_BASE.
 *
 * Register state on entry (set up by the 32-bit stub in boot.S):
 *   EDI = Multiboot magic (0x2BADB002)
 *   ESI = physical address of the Multiboot info structure
 *   RSP = 16-byte aligned stack in the identity-mapped low 1 GiB
 *   CS  = 64-bit flat code selector, DS/ES/SS/FS/GS = flat data
 *   Paging on (identity map of the first 1 GiB), interrupts disabled
 */

        .section .text.boot64, "ax", @progbits
        .code64
        .globl  _start64
        .type   _start64, @function
_start64:
        cld
        xorl    %ebp, %ebp

        /* Hand the two contract registers to C as (magic, mbi). */
        call    kmain

        /* kmain is declared noreturn, but if it ever returns, stop dead. */
1:      cli
        hlt
        jmp     1b
        .size   _start64, . - _start64

        .section .note.GNU-stack, "", @progbits
