# Gate legacy UART reception on actual device evidence

The user's 2026-10-04 hardware trace establishes that the first VT NUL came from
**Serial**, not USB keyboard. N305 has no physical serial connector. That alone
does **not** establish whether a logical UART exists at 0x3f8; this change checks
register behavior instead of deleting zero bytes.

Original bounded 8250-family probe; behavior/register reference is Linux 7.2.3
`drivers/tty/serial/8250/8250_port.c` autoconfig and the standard 16550 register
layout. No Linux function or comment text was translated/copied.

Existing scratch preservation and floating-LSR rejection are extended with:

1. clear DLAB and IER before classification;
2. save/test/restore scratch with two distinct values;
3. clear firmware's FIFOs, then require a plausible THRE status (a busy firmware
   transmitter must not be rejected merely before FIFO clear);
4. IIR must report no interrupt pending while IER=0, valid reserved bits and
   either no FIFO or the normal FIFO-enabled signature;
5. two modem-loopback states must produce independent MSR upper-nibble signatures
   0x90 and 0x60. No character is sent to the RX data register as a test;
6. remove loopback/OUT2, then configure the admitted UART at existing 115200 8N1.

There is no polling/retry loop: at most two LSR reads plus a fixed number of
scratch/IIR/MCR/MSR accesses. A rejected loopback leaves loopback and external
IRQ output disabled. A failed probe publishes absent even after earlier success.
Console reads, writes and receive IRQ registration/enabling are all gated by that
answer. The diagnostic-port candidates use the same probe, rather than another
weak scratch-only implementation. Genuine received NUL is **not filtered**.

Before enabling reception, late boot records `uart-console: probe=... LSR=...`
with IIR and both loopback results. Failed presence uses priority 3 to survive
quiet=4 and explicitly says UART/IRQ/VT reception is disabled. Supported UART
facts are INFO and retained for dmesg/netconsole. Firmware framebuffer fallback
is unchanged. No BIOS/Super-I/O configuration or SPI programming is performed.

Host fake tests model floating/zero/hollow ports, scratch-only readback with a
phantom receiver, illegal IIR signatures, zero status after FIFO reset, a real
UART with firmware TX pending/data-ready, both loopback states, restoration and
constant bounds. QEMU's real emulated UART must still admit serial commands and
complete the guest suite; framebuffer glyph regression remains applicable.

**The new N305 presence check has not been hardware-validated.** On next boot
inspect the report and `tty.input_trace=1`: an absent UART should admit no Serial
VT bytes. If `probe=present` with both correct loopback signatures yet NUL remains,
retain the trace and raw status facts: the UART core may exist despite no external
connector, and the source still needs investigation. Do not claim the original
N305 symptom resolved from host/QEMU tests alone, or mask it by filtering NUL.
