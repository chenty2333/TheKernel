/*
 * hello.c -- x86_64 payload of the Phase 2a "nested QEMU hello" kernel.
 *
 * Runs in 64-bit long mode, having been entered from the 32-bit Multiboot v1
 * stub in boot.S.  It does exactly four things:
 *
 *   1. Brings up COM1 (16550 UART at I/O port 0x3F8).
 *   2. Verifies the Multiboot v1 boot contract it was entered under: EAX
 *      must have contained MULTIBOOT_BOOTLOADER_MAGIC.  A mismatch is
 *      reported on the serial port and terminates QEMU with a distinct
 *      status instead of running on.
 *   3. Prints an INNER_-prefixed banner describing the contract it actually
 *      observed (magic value, Multiboot info contents, CPU mode read back
 *      from CS/CR0/EFER).
 *   4. Terminates QEMU through the isa-debug-exit device with a fixed code.
 *
 * There is no C library here: no <stdio.h>, no <string.h>, no libgcc, no
 * dynamic linker.  The build uses -nostdinc, so a stray libc include is a
 * compile error rather than a silent dependency.  The handful of helpers a
 * freestanding compiler is still allowed to synthesise (memset/memcpy) are
 * defined at the bottom of this file.
 */

#include "multiboot.h"

/* ==================================================================== */
/* Constants                                                             */
/* ==================================================================== */

/* 16550 register offsets from the COM1 base port. */
#define COM1_BASE       0x3F8u
#define UART_THR        0       /* transmit holding register (write)        */
#define UART_IER        1       /* interrupt enable register                */
#define UART_FCR        2       /* FIFO control register                    */
#define UART_LCR        3       /* line control register                    */
#define UART_MCR        4       /* modem control register                   */
#define UART_LSR        5       /* line status register                     */

#define LSR_THRE        0x20u   /* transmit holding register empty          */
#define LSR_TEMT        0x40u   /* transmitter empty (shift register drained)*/

/*
 * isa-debug-exit.  QEMU's device (hw/misc/isa-debug-exit.c) is created with
 * iobase 0x501 and iosize 1 by default -- the memory region is exactly one
 * byte wide.  A byte store is therefore the unambiguous way to hit it; a
 * 32-bit `outl` would straddle the region and the unassigned bytes after it.
 *
 * On a write of `value`, QEMU calls exit((value << 1) | 1).  Writing 0x10
 * therefore makes the qemu-system-x86_64 process exit with status 33.
 */
#define ISA_DEBUG_EXIT_PORT 0x0501u
#define ISA_DEBUG_EXIT_OK   0x10u   /* -> process exit status 33            */
#define ISA_DEBUG_EXIT_BAD  0x11u   /* -> process exit status 35            */

#define EFER_MSR            0xC0000080u
#define EFER_LME            0x00000100ull
#define EFER_LMA            0x00000400ull

/* ==================================================================== */
/* Port I/O and control-register access                                  */
/*                                                                       */
/* Written by hand with inline assembly: freestanding means exactly this. */
/* ==================================================================== */

static inline void outb(mb_u16 port, mb_u8 value)
{
    __asm__ __volatile__("outb %0, %1" : : "a"(value), "Nd"(port));
}

static inline void outl(mb_u16 port, mb_u32 value)
{
    __asm__ __volatile__("outl %0, %1" : : "a"(value), "Nd"(port));
}

static inline mb_u8 inb(mb_u16 port)
{
    mb_u8 value;

    __asm__ __volatile__("inb %1, %0" : "=a"(value) : "Nd"(port));
    return value;
}

static inline mb_u64 read_cr0(void)
{
    mb_u64 value;

    __asm__ __volatile__("movq %%cr0, %0" : "=r"(value));
    return value;
}

static inline mb_u64 read_cr4(void)
{
    mb_u64 value;

    __asm__ __volatile__("movq %%cr4, %0" : "=r"(value));
    return value;
}

static inline mb_u16 read_cs(void)
{
    mb_u16 value;

    __asm__ __volatile__("movw %%cs, %0" : "=r"(value));
    return value;
}

static inline mb_u16 read_ss(void)
{
    mb_u16 value;

    __asm__ __volatile__("movw %%ss, %0" : "=r"(value));
    return value;
}

static inline mb_u64 read_msr(mb_u32 msr)
{
    mb_u32 low, high;

    __asm__ __volatile__("rdmsr" : "=a"(low), "=d"(high) : "c"(msr));
    return ((mb_u64)high << 32) | (mb_u64)low;
}

/* Operand of SGDT: 2-byte limit followed by the 64-bit GDT base (long mode). */
struct gdtr {
    mb_u16 limit;
    mb_u64 base;
} __attribute__((packed));

/*
 * Re-read the descriptor CS currently points at and return its L bit.
 *
 * This is the strongest available in-band proof that the CPU really is in
 * 64-bit long mode: EFER.LMA can only be set if CS.L is set, and CS.L is a
 * bit in the GDT entry the CPU is executing under, not a value we chose to
 * print.  Descriptor bit 53 (0x0020_0000_0000_0000) is the L flag.
 */
static int cs_long_bit(void)
{
    struct gdtr gdtr;
    const mb_u64 *gdt;
    mb_u64 descriptor;

    __asm__ __volatile__("sgdt %0" : "=m"(gdtr));
    gdt = (const mb_u64 *)(unsigned long)gdtr.base;
    descriptor = gdt[read_cs() >> 3];

    return (descriptor & 0x0020000000000000ull) != 0;
}

/* ==================================================================== */
/* COM1 driver                                                           */
/* ==================================================================== */

static void serial_init(void)
{
    outb(COM1_BASE + UART_IER, 0x00);   /* mask all UART interrupts        */
    outb(COM1_BASE + UART_LCR, 0x80);   /* DLAB=1: divisor latch access    */
    outb(COM1_BASE + UART_THR, 0x01);   /* divisor low  = 1  (115200 baud) */
    outb(COM1_BASE + UART_IER, 0x00);   /* divisor high = 0                */
    outb(COM1_BASE + UART_LCR, 0x03);   /* 8 data bits, no parity, 1 stop  */
    outb(COM1_BASE + UART_FCR, 0xC7);   /* FIFOs on, cleared, 14-byte trig */
    outb(COM1_BASE + UART_MCR, 0x0B);   /* DTR, RTS, OUT2                  */
}

/* Blocks until the transmitter can accept a byte, then sends it. */
static void serial_putc(char c)
{
    while (!(inb(COM1_BASE + UART_LSR) & LSR_THRE)) {
        /* spin */
    }
    outb(COM1_BASE + UART_THR, (mb_u8)c);
}

/*
 * Line endings are bare LF (0x0a); no CR is emitted.
 *
 * This artifact exists to be observed from outside QEMU, so the banner is
 * written to be matched by line-oriented tooling: a captured transcript has
 * clean `^INNER_...$` lines.  On an interactive tty served by QEMU's
 * raw-mode stdio chardev the missing carriage return can make the text
 * staircase; pipe the output through `cat` or `sed` if that matters.
 */
static void serial_puts(const char *s)
{
    while (*s != '\0') {
        serial_putc(*s);
        s++;
    }
}

/* Lowercase hexadecimal, fixed width, no "0x" prefix. */
static void serial_put_hex(mb_u64 value, int digits)
{
    static const char hex[] = "0123456789abcdef";
    char buf[16];
    int i;

    for (i = 0; i < digits; i++) {
        buf[i] = hex[value & 0xFull];
        value >>= 4;
    }
    for (i = digits - 1; i >= 0; i--) {
        serial_putc(buf[i]);
    }
}

static void serial_put_dec(mb_u64 value)
{
    char buf[24];
    int i = 0;

    if (value == 0) {
        serial_putc('0');
        return;
    }
    while (value != 0) {
        buf[i++] = (char)('0' + (int)(value % 10));
        value /= 10;
    }
    while (i > 0) {
        serial_putc(buf[--i]);
    }
}

static void serial_put_u64_line(const char *key, mb_u64 value)
{
    serial_puts(key);
    serial_puts("=0x");
    serial_put_hex(value, 16);
    serial_putc('\n');
}

/* Wait until the shift register has drained, so the banner is not lost. */
static void serial_drain(void)
{
    while (!(inb(COM1_BASE + UART_LSR) & LSR_TEMT)) {
        /* spin */
    }
}

/* ==================================================================== */
/* Termination                                                           */
/* ==================================================================== */

/*
 * Exit QEMU via isa-debug-exit.  `code` is the value written to the port;
 * the resulting host process status is (code << 1) | 1.  Never returns.
 *
 * `unused` on outl() is deliberate: the byte-wide store above is the correct
 * access for this device, and outl() is kept only to document the
 * alternative that does *not* hit the 1-byte region reliably.
 */
static void __attribute__((noreturn)) qemu_exit(mb_u8 code)
{
    serial_puts("INNER_HELLO_EXIT port=0x");
    serial_put_hex(ISA_DEBUG_EXIT_PORT, 4);
    serial_puts(" value=0x");
    serial_put_hex(code, 2);
    serial_puts(" expected_qemu_status=");
    serial_put_dec((mb_u64)((code << 1) | 1));
    serial_putc('\n');
    serial_drain();

    outb((mb_u16)ISA_DEBUG_EXIT_PORT, code);

    /* isa-debug-exit is not wired up: halt instead of running off the end. */
    for (;;) {
        __asm__ __volatile__("cli; hlt");
    }
}

/* ==================================================================== */
/* Entry point                                                           */
/* ==================================================================== */

void kmain(mb_u32 magic, const struct multiboot_info *mbi)
    __attribute__((noreturn));

void kmain(mb_u32 magic, const struct multiboot_info *mbi)
{
    mb_u64 efer, cr0, cr4;
    mb_u16 cs, ss;

    serial_init();

    /* ---- 1. Verify the boot contract instead of assuming it ---------- */
    if (magic != MULTIBOOT_BOOTLOADER_MAGIC) {
        serial_puts("INNER_HELLO_FAIL reason=bad-multiboot-magic observed=0x");
        serial_put_hex(magic, 8);
        serial_puts(" expected=0x");
        serial_put_hex(MULTIBOOT_BOOTLOADER_MAGIC, 8);
        serial_putc('\n');
        qemu_exit(ISA_DEBUG_EXIT_BAD);
    }

    /* ---- 2. The required banner line -------------------------------- */
    serial_puts("INNER_HELLO_OK\n");

    serial_puts("INNER_HELLO_BUILD=phase2a-multiboot1-elf32-plus-elf64-payload\n");

    /* ---- 3. Boot contract evidence ---------------------------------- */
    serial_puts("INNER_BOOT_MAGIC=0x");
    serial_put_hex(magic, 8);
    serial_puts(" (multiboot1, expected 0x2badb002)\n");

    serial_puts("INNER_BOOT_MODE=32-bit-protected-mode-at-entry,now-64-bit-long-mode\n");

    serial_put_u64_line("INNER_MBI_PTR", (mb_u64)(unsigned long)mbi);
    serial_put_u64_line("INNER_MBI_FLAGS",
                        mbi != 0 ? (mb_u64)mbi->flags : 0);

    if (mbi != 0 && (mbi->flags & MULTIBOOT_INFO_MEMORY) != 0) {
        serial_puts("INNER_MEM_LOWER_KB=");
        serial_put_dec(mbi->mem_lower);
        serial_puts(" INNER_MEM_UPPER_KB=");
        serial_put_dec(mbi->mem_upper);
        serial_putc('\n');
    } else {
        serial_puts("INNER_MEM_LOWER_KB=(not provided by loader)\n");
    }

    if (mbi != 0 && (mbi->flags & MULTIBOOT_INFO_BOOTLOADER) != 0 &&
        mbi->boot_loader_name != 0) {
        const char *name = (const char *)(unsigned long)mbi->boot_loader_name;

        serial_puts("INNER_BOOTLOADER_NAME=");
        serial_puts(name);
        serial_putc('\n');
    } else {
        serial_puts("INNER_BOOTLOADER_NAME=(not provided by loader)\n");
    }

    /* ---- 4. CPU mode, read back from the hardware rather than assumed */
    efer = read_msr(EFER_MSR);
    cr0 = read_cr0();
    cr4 = read_cr4();
    cs = read_cs();
    ss = read_ss();

    serial_puts("INNER_CPU_MODE=x86_64-long-mode\n");

    serial_puts("INNER_CPU_CS=0x");
    serial_put_hex(cs, 4);
    serial_puts(" INNER_CPU_SS=0x");
    serial_put_hex(ss, 4);
    serial_putc('\n');

    serial_put_u64_line("INNER_CPU_CR0", cr0);
    serial_put_u64_line("INNER_CPU_CR4", cr4);
    serial_put_u64_line("INNER_CPU_EFER", efer);

    serial_puts("INNER_CPU_MODE_EVIDENCE=efer.lme=");
    serial_put_dec((efer & EFER_LME) != 0);
    serial_puts(" efer.lma=");
    serial_put_dec((efer & EFER_LMA) != 0);
    serial_puts(" cr0.pg=");
    serial_put_dec((cr0 & 0x80000000ull) != 0);
    serial_puts(" cr4.pae=");
    serial_put_dec((cr4 & 0x00000020ull) != 0);
    serial_puts(" cs.l=");
    serial_put_dec(cs_long_bit() != 0);
    serial_putc('\n');

    /* ---- 5. Deterministic termination: exit status (0x10 << 1) | 1 = 33 */
    qemu_exit(ISA_DEBUG_EXIT_OK);
}

/* ==================================================================== */
/* Freestanding support routines                                         */
/*                                                                       */
/* A freestanding compiler may still emit calls to these for struct       */
/* copies and aggregate initialisation.  They are defined here so the     */
/* link never reaches for a host libc.                                    */
/* ==================================================================== */

void *memset(void *dest, int byte, unsigned long count)
{
    unsigned char *p = (unsigned char *)dest;

    while (count-- != 0) {
        *p++ = (unsigned char)byte;
    }
    return dest;
}

void *memcpy(void *dest, const void *src, unsigned long count)
{
    unsigned char *d = (unsigned char *)dest;
    const unsigned char *s = (const unsigned char *)src;

    while (count-- != 0) {
        *d++ = *s++;
    }
    return dest;
}

void *memmove(void *dest, const void *src, unsigned long count)
{
    unsigned char *d = (unsigned char *)dest;
    const unsigned char *s = (const unsigned char *)src;

    if (d < s) {
        while (count-- != 0) {
            *d++ = *s++;
        }
    } else {
        d += count;
        s += count;
        while (count-- != 0) {
            *--d = *--s;
        }
    }
    return dest;
}
