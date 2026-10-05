# ACPICA integration

## Default and recovery contract

ACPICA 20260930 is the **default** firmware implementation. No ACPI boot option
is required; `acpi=acpica` selects the same implementation. `acpi=static` is a
minimal rescue for unsupported firmware or initialization failures, not a second
complete ACPI implementation. Unknown ACPI options explicitly warn and retain
static rescue without executing AML. Static MADT/MCFG/HPET/FADT discovery still
runs early because memory mappings, APIC and ECAM must exist before the runtime
can host an interpreter.

Initialization errors are reported with status and
`THEKERNEL_ACPICA_INIT_FAILED_STATIC_RESCUE`. Teardown masks/synchronizes SCI,
drains ACPICA work, stops EC work and restores the existing static fixed-power
owner. Native PCI routing is published only after all fallible initialization
steps succeed. No failed engine is exported as ready. The rescue's actual
fixed-button availability is logged; rescue cannot provide EC, AML devices or
arbitrary firmware IRQ allocation. Cleanup revokes interpreter/IRQ ownership;
it does not undo arbitrary side effects already performed by firmware AML.
Use explicit static rescue from boot when recovering from such firmware faults.
Panic/IRQ shutdown never blocks on AML and
retains the literal-S5/emergency power fallback.

Firmware AML can change hardware. This default has **not been tested on physical
hardware**, and does not authorize an N305 run or writes to its BitLocker NVMe.
No S3 or native PCIe `_OSC` control ownership is enabled. System-wide `_OSC`
advertises no unimplemented ownership. ACPICA's pinned Windows `_OSI` strings
(up to Windows 2022) are retained; `Linux` is not advertised.

## Implementation and initialization phases

Unchanged upstream C is compiled freestanding x86_64 in `tk-acpica` under its
BSD-3-Clause option. Original Rust OSL delegates scheduler, mapping, ECAM, port
and SCI services to a kernel-owned backend, avoiding a platform-to-scheduler
cycle. Owned bounded values/resources/table copies keep C allocations/handles
out of public Rust APIs. Table copy/release runs under the ACPICA table mutex
and balances references on success and bounded-output failure.

The boot order is heap/mappings -> BSP scheduler -> IRQ broker/timer ->
constructors -> enabled BSP interrupts -> blocking firmware services -> initial
PCI/device probe -> AP startup -> full SMP rendezvous -> user init. ACPICA starts
its subsystem before tables, bootstraps ECDT before AML load, enables hardware,
installs custom handlers, initializes objects, selects IOAPIC `_PIC`, prepares
PCI routes/GPEs/EC/thermal policy, then publishes ownership. Fixed PM1 power
handling is admitted only when FADT describes it; errors installing an admitted
fixed event propagate instead of being hidden as a missing button.

Earlier experiments that started AP scheduling before device initialization
produced EEVDF accounting failures and invalid instruction-pointer faults.
The same early-AP ordering also crashed an `acpi=static` control, so an AML-only
cause is ruled out. GDB observed invalid AP current-task/accounting state, but
the precise corruption mechanism of that discarded order remains **unproven**.
The shipped order retains the original supported AP-after-device boundary and
uses a separate non-inlined device phase. It does not weaken assertions, change
scheduler accounting, substitute polling or reprobe devices. BSP pre-probe
services plus eventual SMP4 passed the guest suite; the UP experiment's
SMP-dependent test failure was not counted as passing.

## PCI INTx

Before the first driver probe, `_PIC(1)` selects IOAPIC routing. Root/bridge
`_PRT` entries resolve direct GSIs or already-programmed, enabled PNP0C0F `_CRS`
links. Exact function entries precede wildcards; bridges lacking a `_PRT`
swizzle downstream pins. Namespace-described bridges are matched to actual
PCI configuration bus numbers. Only segment zero and bounded topology are
admitted. Missing entries, conflicting polarity, invalid resources or cycles
never authorize a guessed config-space line. Inactive links requiring `_SRS`,
nonzero link resource indices and unsupported segments explicitly fail
initialization to rescue; there is no unvalidated IRQ allocator.

One read-only HAL provider serves IRQ-consuming VirtIO block/net/input
transports. An Interrupt Line of 0xff does not discard a valid firmware route.
A native lookup failure does not fall back to the old line or silently admit a
polling IRQ consumer. Acknowledgment ownership precedes device INTx enable,
shared ISR latches are captured before EOI, and actual firmware level polarity
is passed to the IOAPIC. Q35 APIC links are **active high**, not the previous
unconditional PCI active-low assumption. Other drivers' MSI/MSI-X paths are not
changed; this is not proof of D's GPU IRQ or arbitrary native PCI binding.

The Q35 NIC regression removes MSI-X with `vectors=0` and reads the actual PCI
capability list to require neither MSI nor MSI-X. It checks all 65536 TCP echo
bytes through SLirp to a temporary host-loopback peer, and requires increasing
GSI22/vector54 counts, the actual INTA route and clean AML-prepared S5. This is
real QEMU device IRQ/I/O acceptance, not just `_PRT` parsing or N305 acceptance.
The peer exits with the test; no host service/network change is installed.

## Events, EC and thermal boundaries

SCI accepts only the admitted single-IOAPIC, GSI-zero, legacy SCI routing and
validated MADT overrides. A fixed 128-entry work queue moves AML out of IRQ
context. ACPICA handles edge/level GPE dispatch. A 1024-callback/second budget
masks sustained traffic and disables GPEs on a task; exactly one recovery is
allowed per boot. A repeated storm remains masked until reboot. Budget host
tests do not establish a live hardware storm test.

`_PRW` global/block-device references are registered before automatic GPE enable;
wake-only sources are not blindly enabled in S0. S0-capable sources and method
buttons retain runtime delivery. Sleep masks/wake power resources remain
inactive: no suspend/resume lifecycle is implemented. Fixed events and PNP0C0C
Notify 0x80 use the existing SIGPWR-to-init, grace, filesystem-flush, AML S5 flow.
When init reacts to SIGPWR before the grace expires, its completion path also
flushes filesystems; both that path and the force-after-grace worker report
`THEKERNEL_ACPI_FILESYSTEMS_FLUSHED` only after success. Tests require event ->
successful flush -> S5 on the guest stream, not one particular worker winning
the shutdown race or a best-effort diagnostic UART line. QMP produces Q35's
fixed button; authored Notify tests prove the method-button
consumer, not a physical button producer.

S0 SystemIO EC/ECDT regions are installed before object initialization; ECDT
handlers precede table AML load and are checked against namespace resources.
Serialized byte transactions use bounded 100 ms waits and `_GLK`; queries poll
every 25 ms with a 64-query budget and mask the EC's global GPE. Q35 has no EC.
Native transactions, EC wake and GPE-block packages remain unverified/unsupported.

Thermal sysfs exposes `acpitz`, `_TMP`, valid `_CRT`/`_PSV` values and types, using
Linux integer-decikelvin conversion rather than fabricated zero on AML errors.
Invalid 64-bit sensor sentinels cannot cause false critical shutdown. A five-second
monitor requests orderly shutdown at valid `_CRT`; there is no passive cooling
governor. Authored QEMU values 26800 mC / critical 36800 / passive 31800 and a
later critical transition exercise policy, not an N305 sensor or overheating test.

Root-only 0400 table exports include private OEM tables: **never publish raw
MSDM, product keys, AML or disassembly**. Device path/HID/status are live.
`dynamic/` exists but separate dynamic-load attribution, full modalias/binding
and general device-power policy are not implemented.

## Validation and reproduction

Default-native Q35/OVMF SMP4 guest: **69/69**, actual acpidump/iASL,
root-only exports and AML S5. With no ACPI option, the no-MSI/no-MSI-X NIC
used INTA -> GSI22/vector54 active high, compared 65536 external TCP bytes and
increased that interrupt count 0 -> 153. The normal 96 MiB/no-tools image also
passed native exports/permissions/devices and S5, reporting tools not staged.
All six default/rescue policy cases passed after unifying successful-flush
reporting; the earlier missing method-button flush report was not called passing.

Final formal host: 657 Python tests (three existing skips), 2617 kernel tests,
ACPICA 23, VirtIO 19, platform 119. The affected kernel host tests were rerun
after the flush-report change; guest and both product/N305 lints use that final
runtime source. Both lints passed with 784 baseline kernel warnings. New Rust
ACPI-module/Linux-excerpt scans found zero matches. The final complete Linux
7.2.3 ABI differential passed **40 programs, 257/257 contracts** on both guests.
The TheKernel guest had an empty ACPI command line, ACPICA ready/PCI-before-probe
markers, routed VirtIO block rootfs (GSI22) and NIC (GSI23), and AML-prepared S5.
This is fresh default-native acceptance, not the older static-mode result.


Historical offline interpretation (simulated hardware, synthetic FADT): external
N305 DSDT/SSDT loaded 7392 nodes / 295 devices / 1923 methods; `_S5` package,
`_PRT` 28/28, `_PRW` 60/60, AML-errors=0. Q35 loaded 309 / 44 / 92, `_PRT` 1/1,
errors=0. Differences from iASL device/method counts are ACPICA predefined nodes.
These results are not native `_INI`, EC, IRQ or power-transition acceptance.
The previous default-static 257/257 ABI run is **not** acceptance of this default.

Use an independent state directory, `CARGO_BUILD_JOBS=6` and `nice -n 10`:

```sh
export THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-acpica
export CARGO_BUILD_JOBS=6
nice -n 10 python3 tools/thekernel.py test --suite guest --toolchain acpica --accel kvm
nice -n 10 python3 scripts/ci/acpica-intx-qemu-smoke.py
nice -n 10 python3 scripts/ci/acpica-policy-qemu-smoke.py
nice -n 10 python3 tools/thekernel.py test --suite host
nice -n 10 python3 tools/thekernel.py lint
nice -n 10 python3 tools/thekernel.py lint --platform n305
nice -n 10 python3 tools/thekernel.py test --suite abi --toolchain acpica --accel kvm --linux-kernel OWNED_LINUX_7_2_3_BZIMAGE
```

Guest/policy/INTx and ABI use **no** `acpi=acpica` option, proving default selection.
Inspection payload runs actual `acpidump -s` and `iasl -d`; the normal 96 MiB
image keeps these optional tools out, but its guest case still verifies native
exports/permissions/devices and explicitly reports tools not staged. Use
`thekernel-acpi-smoke require-tools` when requiring inspection tools explicitly.
The 128 MiB inspection image avoids an earlier 96 MiB direct-I/O fixture fsync
failure; that failure was not called passing or repaired with syscall changes.

Authored policy fixtures cover thermal, method/fixed buttons, early unsupported
ECDT failure, post-SCI unsupported PCI-segment failure and explicit static rescue.
They require actual ordered shutdown, not shell EOF or host-forced termination.
OEM tables are read only externally; the probe prints counts/status, never bytes,
and reports absent external input with exit 77. No hardware tables enter Git.

S3 additionally needs driver suspend/resume ownership, saved CPU/resume-vector
state, `_PRW`/GPE wake policy, memory/device ordering and native recovery tests.
Live storms, native EC and sleep wake are future authorized tasks, not requirements
that indefinitely expand this default-path completion.

`config/linux-contracts.toml` and its progress counts are unchanged: no syscall
semantics changed. The dependency-layer gate's six baseline metadata differences
in unchanged DBC/HDA/NVMe/watchdog manifests are outside this worktree's task.

## References

[ACPICA release](https://github.com/acpica/acpica/releases/tag/20260930),
[programmer reference](https://cdrdv2.intel.com/v1/dl/getContent/772726),
[FreeBSD integration](https://github.com/freebsd/freebsd-src/blob/main/sys/dev/acpica/acpi.c),
[Haiku integration](https://github.com/haiku/haiku/blob/master/src/add-ons/kernel/bus_managers/acpi/BusManager.cpp).
These integrations informed architecture; their implementation was not copied.
