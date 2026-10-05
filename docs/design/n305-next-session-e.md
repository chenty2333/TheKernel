# N305 ACPICA next session

**未在硬件上验证。** This document describes a future authorized capture, not a
result or permission to write the N305's Windows/BitLocker NVMe.

## Safe first boot

- Keep a known-good image with **explicit `acpi=static`** as rescue. Normal
  boot now runs ACPICA; omitting the option no longer bypasses AML. Only boot
  the default-native test image after a separately authorized physical session.
- Use the existing owned netboot/capture procedure, only after the user authorizes
  a physical session. Keep disks read-only; do not add `nvme.allow_write=1`.
- Do not enable S3, display modesetting, CPU power experiments or watchdog changes
  as part of this ACPI check. Preserve the firmware console/rescue path.

## What to inspect

- Boot must finish; look for `acpica: ready version=20260930`, the namespace/device
  counts, AML-error count and method-button discovery. The offline N305 counts
  (7392 nodes / 295 devices) are a comparison aid, not a required native match.
- Record `_OSC` status, EC resource/handler messages and any unsupported routing,
  timeout or fallback messages. Before PCI inventory, require `PCI IRQ scopes=...`
  and `_PIC=IOAPIC before initial probe`; ready follows successful publication.
  Check each admitted device's BDF/pin/GSI/polarity. Do not interpret those messages
  as I/O proof: compare real traffic/storage reads with interrupt-count changes.
  Current links and namespace-described bridges are supported; inactive `_SRS`
  allocation/nonzero segments are not. D's GPU IRQ has not been accepted here.
- As root, check ACPI device `hid`, `path`, `status` and table permissions (0400).
  Run the bundled `acpidump -s` and `iasl` only into the external private capture
  directory. Table export includes MSDM: **never publish raw tables, product
  keys, AML content or disassembly**.
- EC must show live, bounded transactions without AML EC-region errors. A no-EC
  Q35 run and the simulated offline table load do not establish this. EC wake
  and GPE-block packages are unsupported; stop if firmware requires them.
- Check thermal-zone `type`, `temp`, critical/passive trip attributes against
  sensible ambient readings. No cooling governor is provided. Do not deliberately
  heat the machine to `_CRT`; the authored QEMU thermal test covers shutdown
  policy without physical overheating.
- Test the physical control-method power button only after init is ready. Verify
  button event, SIGPWR to init, filesystem flush and
  `THEKERNEL_ACPICA_S5_PREPARED` before actual power-off. Merely losing the console
  is not S5 proof. QMP proves only the QEMU fixed-button path.

## Recovery

If startup fails or stalls, return to the rescue image with **`acpi=static`**;
omitting the option selects ACPICA again. An initialization error must show its
status, `THEKERNEL_ACPICA_INIT_FAILED_STATIC_RESCUE`, `static fallback restored`
and actual fixed-button availability. Native routing must not remain published.
Literal S5/fixed-power rescue is limited to admitted FADT/SCI hardware; do not
assume a method-only physical button works under static rescue. A persistent SCI storm is masked;
that is a fail-closed boundary, not successful GPE service. Keep the failure logs
private and fix the affected ACPI path before another authorized native run.
