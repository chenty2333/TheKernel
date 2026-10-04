# N305 verbose boot: reproduced configurations, unresolved hardware symptom

The real 2026-10-03 boot stopped visually at `Initialize alarm...` with
`loglevel=7`; quiet reached shell. **Root cause unknown; not fixed.** A screen
stopping at this line is not by itself proof that the alarm worker or the
whole kernel stopped. No speculative kernel change is made without a repro.

## Additive boot-argument option

`tools/thekernel.py run --kernel-cmdline 'loglevel=7'` appends literal tokens
to the existing Multiboot2 line. The repository GRUB template, existing
arguments and normal ESP are preserved. A separate ESP/config is created in
the run directory while the established artifact lock is held. This also
works with `--no-build` and the existing drive rootfs transport. GRUB commands,
variables, quoting, comments, NUL and multiline input are rejected, rather
than executing the supplied string as a GRUB script. These are literal tokens,
not a general-purpose GRUB scripting option.

The fbcon suite accepts the same option; other suites reject it rather than
silently ignoring it. Three host tests cover append preservation, malformed
input/ambiguous templates, and CLI selection. Actual QEMU `/proc/cmdline`
reported `loglevel=7` and `quiet` respectively.

## Measured on current dev, 2026-10-04

- N305 profile, firmware-fb, KVM, 4 CPUs, explicit loglevel=7: shell command
  executed, then forced userspace poweroff produced QEMU exit 0.
- Same profile, 8 CPUs, explicit loglevel=7 and quiet: both executed shell
  commands and cleanly exited. No >60-second alarm stall was reproduced.
- N305 profile, firmware-fb, TCG, loglevel=7: fbcon gate passed. Its pixel/font
  check read the KTAP banner, first guest case marker and mirrored alarm line
  on the actual firmware framebuffer, not merely the serial/kernel log;
  2174 inked cells, zero foreign/out-of-grid/border pixels. The gate deliberately
  stops QEMU after the userspace marker; this is not a full guest-suite result.

These observations do **not** establish that verbose boot works on N305
hardware. QEMU has a UART and a different GOP aperture/timing. The next real
boot must compare quiet/loglevel=7 using the new driver and collect whether
network/userspace are alive while HDMI is stuck. Do not infer a console
lock, an alarm defect, or a hardware fix from the last visible log line.
