# Intel iTCO watchdog (v2 / v6)

Original Rust mechanism in `tk-axdriver-watchdog` (`ids`, `regs`, `probe`,
`bringup`, `fake`), PCI/IO seam in `tk-axdriver/src/itco.rs`, Linux OFD/ioctl
policy in the kernel. **N305 v6: 未在硬件上验证.** Unknown IDs are never armed.

Only `8086:2918` ICH9 (v2) and measured `8086:54a3` Alder Lake-M SMBus (v6)
are admitted. V2 uses ACPIBASE+0x60 and RCBA+0x3410 GCS bit 5; v6 uses SMBus
TCOBASE 0x50/TCOCTL 0x54 and TCO1_CNT bit 0, never PMC. TCOCTL must already
advertise the IO resource enabled, as Linux requires. No guessed base/enable.
TCO1_CNT writes always mask NMI_NOW to avoid its inversion behavior. Register
readback failures refuse initialization. V2 TCO SMI is disabled under this
explicit opt-in so firmware cannot silently clear its watchdog counter.
The mechanism constructor is passive; start/adopt explicitly acquire control.
SMI clear is read back before reconfiguration. Failed takeover of a still-running
timer retains its owner so it can be fed or stopped. A verified HALT updates
the running state even if NO_REBOOT fails, and the owner may retry that protection.
Only the v2 boot-reset flag is recognized; a zero v6 status is not proof that
no reset occurred.

No `watchdog.timeout=N` means **no IO access and no /dev/watchdog**. The driver
feature is compiled into the normal product but not active by default. Valid
explicit timeouts are 3..614 seconds, matching v2/v6 0.6-second tick geometry.
The timer starts at PCI discovery, so it covers later initialization as well
as userspace. No kernel background feeder hides a stalled init. Linux-compatible
`/dev/watchdog` (10:130, mode 0600) supports GETSUPPORT/STATUS/BOOTSTATUS,
SETOPTIONS, KEEPALIVE, SET/GETTIMEOUT and GETTIMELEFT. Unsupported temperature
or pretimeout ioctls return ENOTTY; support flags do not advertise them.

Nonempty writes feed. One OFD owns the device; duplicates share magic-close
state. A last write containing V stops on final close; ordinary close keeps it
armed and pings once. User-copy faults happen without the hardware spin lock.
Device publication failure attempts to stop it and retains the screen.

## Measured QEMU behavior

N305 profile on Q35/KVM (actual emulated ICH9 v2), timeout 6:

- `thekernel-watchdog-check feed` exercised Linux ioctls and alternating
  write/ioctl feeds for 20 seconds, then magic close and a further 14 seconds.
  It reported `ITCO_FEED_AND_MAGIC_CLOSE_OK` and clean poweroff: no reboot.
- `expire` stopped feeding. With `--allow-reboot`, QEMU actually loaded OVMF/
  GRUB and booted the kernel a second time; then the second shell executed
  `ITCO_SECOND_BOOT` and powered off. No host system_reset command was sent,
  and `ITCO_FAILED_TO_RESET` was absent. QEMU ICH9's reset occurs on its second
  expiration, rather than claiming a single six-second expiration is a reset.

`--allow-reboot` appends QEMU's `-action reboot=reset` and watchdog reset action;
ordinary runs still retain their no-reboot diagnostic policy. Use only with a
bounded timeout. To repeat, put helper invocation, an echo marker and
`poweroff -f` in a command file, then run:

```sh
THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-dev \
python3 tools/thekernel.py run --platform n305 --profile shell --accel kvm \
  --kernel-cmdline watchdog.timeout=6 --allow-reboot --commands COMMANDS --timeout 90
```

Host fake tests cover exact PCI IDs, resource validation, opt-in parsing,
v2/v6 NO_REBOOT separation, reserved-bit preservation, NMI_NOW masking,
timer limits and locked-control rejection. QEMU cannot validate N305's v6.

Reference facts: Linux 7.2.3 `drivers/watchdog/iTCO_wdt.c`,
`drivers/i2c/busses/i2c-i801.c` and `drivers/mfd/lpc_ich.c`; QEMU
[invocation actions](https://www.qemu.org/docs/master/system/invocation.html)
and [ICH9 TCO device](https://github.com/qemu/qemu/blob/master/hw/acpi/ich9_tco.c).
No Linux implementation or quoted prose was translated into the Rust source.
