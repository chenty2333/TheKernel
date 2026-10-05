# ACPICA integration

## Architecture and safety boundary

ACPICA 20260930 is compiled unchanged as freestanding x86_64 C in `tk-acpica`,
using its BSD-3-Clause option. Original Rust OSL delegates scheduler, mapping,
ECAM, port and SCI services to a kernel-owned backend. This avoids a
platform-to-scheduler dependency cycle. Owned bounded values/resources/table
copies keep ACPICA allocations and handles out of the public Rust API.

`acpi=acpica` is explicit opt-in. Default boot retains the static discovery,
fixed-power button and literal-S5 fallback. Firmware AML can change hardware;
this round has **not tested physical hardware**. No S3 or native PCIe `_OSC`
control ownership is enabled. The system-wide `_OSC` query advertises no
unimplemented ownership. ACPICA's pinned default `_OSI` Windows vendor strings
(up to Windows 2022) are retained; `Linux` is not advertised.

The safe current startup point is after kernel runtime/process-domain setup,
before publishing init. ACPICA initialization requires subsystem globals before
table initialization/load, then hardware enable, custom handlers and object
initialization. The independent static table discovery still runs early.
An attempted pre-PCI runtime insertion produced EEVDF accounting failures and
empty instruction-pointer faults; it was removed, not treated as passing.
Consequently INTx assignment before initial driver probing is **not completed**.
The library can decode `_PRT`, resolve names and read already-programmed link
resources, but does not invent IRQ allocation or execute `_SRS` on inactive links.

SCI only accepts the platform's admitted single-IOAPIC, GSI-zero, legacy SCI
routing and validated MADT overrides. Unsupported routing fails closed and
restores static power handling. IRQ work admission uses a fixed 128-entry queue;
AML runs on a task, never inline in SCI. Shutdown masks/synchronizes SCI and
drains work before namespace teardown. ACPICA handles edge/level GPE dispatch;
the adapter additionally bounds sustained SCI traffic and disables GPEs from a
task. Detailed wake-source policy remains limited by the absence of suspend.

Fixed events and `PNP0C0C` Notify 0x80 latch the existing ordered shutdown worker:
SIGPWR to init, one-second grace, filesystem flush, sleep-state preparation and
S5. Panic/IRQ paths never block on AML and retain static S5 as a fallback.

## Measured validation (2026-10-05)

- Original synthetic AML regression exercises namespace loading, `_S5`, `_STA`,
  `_PRT`, asynchronous Notify and malformed-result rejection.
- External N305 DSDT/SSDT: 7392 namespace nodes, 295 devices, 1923 methods;
  `_S5` returned a package, `_PRT` 28/28 evaluated, zero logged AML errors.
  One control-method button, one EC and one thermal zone were discovered.
  Hardware accesses were **simulated** and a synthetic FADT was used; these
  results are not native `_INI`, EC, IRQ or power-transition acceptance.
- Q35/OVMF DSDT exported from the running guest and loaded offline: 309 nodes,
  44 devices, 92 methods; `_S5` package, `_PRT` 1/1, zero logged AML errors.
  iASL counted 42 authored devices and 91 methods; ACPICA adds the predefined
  `_SB_`/`_TZ_` devices and `_OSI` method, accounting for the difference.
- Q35/OVMF ACPICA guest suite with inspection payload: **69/69**, clean S5 exit.
  `acpidump -s`, `iasl -d`, exact DSDT length/checksum and non-root table access
  denial ran as actual guest programs. Table files use mode **0400**, matching
  Linux's ACPI sysfs table policy. Namespace path/HID/status nodes are live.
- QMP `system_powerdown` with init held in a 60-second sleep produced the button
  event, filesystem flush and `THEKERNEL_ACPICA_S5_PREPARED`, then clean exit.
  Earlier noninteractive shell exits were not accepted as power-button proof.
- A 96 MiB guest run failed only the direct-I/O fixture fsync (EINVAL); that
  exact case passed in an isolated 128 MiB payload run. No filesystem or syscall
  semantic change was made. Final acceptance uses the explicit inspection
  payload, without changing the ordinary rootfs size.

## Reproduce

Use a separate state directory and `CARGO_BUILD_JOBS=6`; host builds and VM
commands are nicened. No sudo, host services, host-network changes, push,
physical disk writes or firmware-table submission is part of this workflow.

```sh
THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-acpica \
THEKERNEL_TEST_CMDLINE=acpi=acpica CARGO_BUILD_JOBS=6 \
nice -n 10 python3 tools/thekernel.py test --suite guest --toolchain acpica --accel kvm

CARGO_TARGET_DIR=/home/ava/.cache/thekernel-targets/wt-acpica/target/acpica-probe \
CARGO_BUILD_JOBS=6 nice -n 10 cargo run -p tk-acpica --features host \
  --example table_probe -- EXTERNAL_TABLE_DIRECTORY
```

The probe reads only DSDT/SSDT and prints counts/status, never OEM AML or MSDM.
An unavailable external directory reports an explicit skip (exit 77).
Only the project-authored synthetic fixture is checked in. The programmer
reference and its text conversion live outside Git in `refs/acpica/`.

## Remaining acceptance and known differences

PCI INTx allocation/probe ordering, inactive-link `_SRS` policy and a real INTx
RX/TX test remain pending. EC SystemIO operation regions are installed before object initialization. ECDT
bootstrap handlers are installed before table AML loading, then checked against
PNP0C09 namespace resources. Transactions serialize byte commands, use bounded
100 ms waits and honor namespace `_GLK`. S0 query handling polls every 25 ms with
a 64-query budget and masks its global GPE; GPE-block packages and EC wake are
not supported. Q35 has no EC: protocol/timeout/ECDT validation tests plus a Q35
no-EC guest run are **not** proof of native EC transactions. The prohibited
hardware run is the outstanding acceptance blocker. Thermal integration is
being committed separately.
Root sysfs currently exports admitted ACPICA table descriptors; `dynamic/` is
present but separate dynamic-load attribution is not implemented. Namespace
views do not claim Linux modalias/driver binding or full device-power policy.

S3 requires driver suspend/resume ownership (PCI, storage, display, USB, network),
wake `_PRW`/GPE policy, a saved CPU/resume-vector path, memory/device ordering,
and native failure/recovery tests. None is implied by S5 or offline AML success.

## References

[ACPICA release](https://github.com/acpica/acpica/releases/tag/20260930),
[programmer reference](https://cdrdv2.intel.com/v1/dl/getContent/772726),
[FreeBSD integration](https://github.com/freebsd/freebsd-src/blob/main/sys/dev/acpica/acpi.c),
[Haiku integration](https://github.com/haiku/haiku/blob/master/src/add-ons/kernel/bus_managers/acpi/BusManager.cpp).
These OS integrations were consulted for architecture, not copied.
