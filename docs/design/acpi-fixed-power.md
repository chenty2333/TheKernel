# Fixed ACPI power button and static S5

Original Rust implementation; firmware bytes and ACPI 6.6 register/AML grammar
facts, not a translation of ACPICA. [ACPI software model](https://uefi.org/specs/ACPI/6.6/05_ACPI_Software_Programming_Model.html),
[AML grammar](https://uefi.org/specs/ACPI/6.6/20_AML_Specification.html),
[sleep transitions](https://uefi.org/specs/ACPI/6.6/16_Waking_and_Sleeping.html).

During the temporary firmware mapping, checksum-validated FADT/DSDT/SSDT bytes
are consumed and only ports, IRQ and literal sleep types are retained. The
scanner accepts a root `Name(_S5_, Package(...))`, including `Scope(\\)` and
root-qualified names. It validates package lengths, names, integer widths and
all elements, caps nesting at 16, and rejects duplicate declarations. Buffers,
strings, methods, field bodies, conditional execution and non-root namespace
bodies are **not** searched for byte signatures. Unknown root terms invalidate
that table; bounded arithmetic operands can be skipped in OperationRegion
objects but are never evaluated. Dynamic `_S5`, `_PTS`, `_GTS`, control-method
button notifications and HW-reduced sleep are unsupported. No full AML VM is
claimed. Missing/unsupported S5 retains the existing emulator-port/halt fallback
and does not enable the new button path.

System-I/O GAS takes precedence over legacy addresses only when its space,
offset, width and range are valid. PM1a/b SLP_TYP are staged separately, then
SLP_EN asserted, preserving unrelated control bits. The fixed-button path owns
PM1 enables and enables only PWRBTN, acknowledges its W1C status in SCI, and
latches one coalesced event. SCI routing requires a single GSI-zero IOAPIC,
untruncated MADT overrides and a legacy SCI whose GSI is unchanged. Level
triggering and override polarity are respected (not blindly PCI active-low).
Unsupported routing degrades without unmasking an unhandled source.

A deferred task, created **after** PID 1 publication, sends SIGPWR to init,
gives it one second, flushes filesystems, then enters S5. No signal delivery,
filesystem operation or logger executes in SCI. PID 1 may handle SIGPWR; the
kernel does not require userspace cooperation to turn off. Final flush outcome
uses the bounded emergency log path, so shutting down init's kmsg reader cannot
hide the result. A failed worker allocation is reported rather than fatal.

## Facts and validation

The readable 2026-10-03 N305 acpidump was reconstructed in the local state
area and passed checksum validation. This exact parser decoded FADT fixed
button, SCI 9, event 0x1800, control 0x1804, SMI 0xb2/enable 0xa0; DSDT literal
S5 **[7, 0]**. MADT IRQ9->GSI9 flags 0x000d means level/**active-high**.
These are firmware observations, **not hardware-tested SCI/S5 transitions**.

Host tests cover truncated/malformed packages, opaque buffer/method false
matches, root scopes, duplicates, range checks and FADT/GAS rejection. Q35
publishes literal S5 [0, 0]. The real QMP `system_powerdown` is a button event,
not QMP quit or a runner kill; validate guest notification and final flush,
then require QEMU clean exit. The runner has additive typed checkpoint
`powerdown=True` and CLI `--powerdown-after-marker`.

```sh
THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-dev \
python3 tools/thekernel.py run --platform n305 --profile shell --accel kvm \
  --powerdown-after-marker THEKERNEL_SHELL_READY --timeout 100
```

Require `acpi-power: ... fixed-button=true S5=Some(...)`, notification of init,
`acpi-power: filesystems flushed; entering S5`, and `qemu-runner exit=0` without
host-forced termination. Keep watchdog disabled for this test. On N305 first
boot with quiet/netconsole, confirm discovery before pressing the power button;
if unsupported keep shell and existing manual recovery path, do not guess ports.
