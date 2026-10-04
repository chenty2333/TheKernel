# VT NUL input: bounded provenance, not a speculative filter

**The real N305 source of `^@` is unresolved.** UART receive error-byte
filtering did not remove it on the machine; no claim is made that another
UART filter or an HID change fixes it. A genuine Ctrl-Space NUL is legal input
and is deliberately not suppressed.

`tty.input_trace=1` opts into logging the first **64 accepted VT input bytes
per boot**, shared across transports. No flag means no tracing. The last
`tty.input_trace=` statement wins. Each record includes a sequence number,
VT, exact hexadecimal byte and source: Serial, UsbKeyboard, VirtualKeyboard,
or OtherEvdev. The evdev bus identity travels with the retained keyboard
packet, so a backpressure retry cannot rename USB input as serial input.
Only successful line-discipline admission is recorded; revoked/flushed or
backpressured batches are not counted twice. Logging happens outside the VT
route and line-discipline locks. The diagnostic neither rewrites nor drops
input, and its budget never wraps or grows indefinitely.

Serial bytes refer to the existing UART-backed root console route. USB and
virtual labels come from the input driver's Linux bus type; OtherEvdev is
an explicit unknown-other-device category, not a guessed USB identity.

## Next boot

The PXE prepare command now accepts `--kernel-cmdline 'tty.input_trace=1'`
in kernel mode and appends those literal tokens to its existing GRUB line.
It does not start host services or change networking. In the no-keyboard
N305 boot, inspect `dmesg | grep vt-input` and find the first `byte=0x00`.
Report its source/sequence/VT before changing that driver's behavior.

This can expose up to 64 typed bytes (including secrets) in the retained
kernel log and its userspace relay. Use only for this diagnostic boot, do not
type a password, and remove the flag afterwards. There is no automatic host
archive or unbounded input logger.

## Host/model coverage

The trace tests preserve NUL and 0xff with their source, share/cap the budget
across paths, check exact opt-in/last-statement selection, and classify USB,
virtual and other buses. The netboot preparation test mocks GRUB only, checks
literal appended tokens and that preparation invokes no host service.
The hardware source remains **未在硬件上验证**, even if injected QEMU NULs
are correctly attributed.

Measured injected-input run: N305-profile KVM, `quiet tty.input_trace=1`,
serial command stream beginning with NUL. Guest `dmesg` reported exactly
`vt-input seq=0 source=Serial vt=1 byte=0x00`, executed later commands and
powered off with QEMU exit 0. This proves this diagnostic can expose a real
accepted NUL in the emulated UART path; it does not identify the N305's byte.
