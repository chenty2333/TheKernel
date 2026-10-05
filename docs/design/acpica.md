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
the adapter additionally admits at most 1024 SCI callbacks per second, masks
sustained traffic and disables GPEs from a task. Exactly one recovery is allowed
per boot; a repeated storm remains masked until reboot. The rate/recovery budget
has host regression coverage, not a measured hardware storm claim.

`_PRW` global/block-device GPE references are validated and registered before
automatic GPE enable. Wake-only sources are not blindly enabled as runtime
sources; S0-capable sources and control-method power buttons explicitly retain
runtime delivery. Sleep wake masks and wake power resources are not activated:
there is no suspend/resume lifecycle in this task. External N305 `_PRW` decoded
60/60 with zero AML errors in simulation. An authored Q35 button fixture registered
one wake-capable S0 GPE; this does not establish physical wake.

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

- Q35 thermal policy fixture: real sysfs values and critical -> orderly flush ->
  ACPICA S5 passed; ordinary no-zone guest suite remained **69/69**.

- An authored PNP0C0C `_INI` Notify 0x80 exercised the kernel method-button
  consumer and orderly ACPICA S5; q35 QMP still produces a fixed button, not
  a method-button event. A deliberately unsupported authored ECDT forced
  initialization failure; after repairing static SCI ownership, QMP still caused
  init notification, filesystem flush and clean static S5. The initial failing
  fallback test exposed duplicate SCI registration/disabled PM1 events; it was
  fixed rather than counted as a successful fallback.

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

For the authored thermal/button/fallback regressions, use the same environment with
`nice -n 10 python3 scripts/ci/acpica-policy-qemu-smoke.py`. It does not accept
host termination or shell EOF as a critical-trip result.

The probe reads only DSDT/SSDT and prints counts/status, never OEM AML or MSDM.
An unavailable external directory reports an explicit skip (exit 77).
Only the project-authored synthetic fixture is checked in. The programmer
reference and its text conversion live outside Git in `refs/acpica/`.

## Final verification and completion status

The final runtime source passed the formal host suite (655 Python tests, three
existing skips; 2616 kernel tests; ACPICA 19 tests), native opt-in Q35/OVMF guest
69/69, product lint and N305 build lint (baseline warnings). All four policy
regressions passed: thermal critical trip, authored control-method Notify, QMP
fixed button and initialization-failure/static rollback. The complete ABI
comparison ran 40 real programs and passed 257/257 contracts on both TheKernel
and Linux 7.2.3. ABI used the default static boot mode; native ACPICA acceptance
is the separate guest/policy runs, not the ABI result.

Final review also repaired leaked table-validation references (both successful
copies and bounded-output failures now balance under the table mutex) and a
false critical trip for an unrepresentable 64-bit sensor sentinel. Their host
regressions failed/identified the old behaviour and pass after repair. Linux
excerpt scans found no matching Rust lines in the new crate/kernel ACPI modules
or pseudofs scope. The dependency-layer check still reports six pre-existing
manifest-policy differences in DBC/HDA/NVMe/watchdog; those manifests are
unchanged from the worktree base and were not repaired outside this task.

- E1: completed, including external q35/N305 offline interpretation.
- E2.1–3 and E2.7–8: completed at the stated QEMU scope (startup/failure rollback,
  S5, button consumers, thermal policy, tables/devices). QMP is fixed-button
  production; the method-button consumer uses an authored Notify fixture.
- E2.4: implementation delivered (upstream edge/level dispatch, `_PRW` policy,
  bounded queue/SCI recovery). Live GPE-storm and physical wake acceptance remain
  unverified: no admitted controllable storm producer, and no suspend lifecycle
  or authorized physical run. Host budget tests are not substituted for that.
- E2.5: blocked. The safe current kernel initialization phase follows initial
  PCI driver probing. The earlier insertion faulted; no scheduler changes or
  unsafe device reprobe were retained. IRQ assignment, inactive-link `_SRS`
  allocation and an INTx RX/TX acceptance case require a safe pre-probe service
  phase before this can be called complete.
- E2.6: S0 SystemIO EC/ECDT implementation delivered. Native EC transactions and
  wake acceptance are blocked by q35 lacking an EC and this round forbidding
  physical testing. GPE-block packages are explicitly unsupported.
- E3: guest/tools/power/docs completed; INTx and native EC acceptance remain
  blocked as above. This is **not a claim of complete ACPI hardware support**.

`config/linux-contracts.toml` is unchanged, including `[progress]`. No syscall
semantics were changed. No physical run, push, branch creation or merge occurred.

## Remaining acceptance and known differences

PCI INTx allocation/probe ordering, inactive-link `_SRS` policy and a real INTx
RX/TX test are blocked as described above. EC SystemIO operation regions are installed before object initialization. ECDT
bootstrap handlers are installed before table AML loading, then checked against
PNP0C09 namespace resources. Transactions serialize byte commands, use bounded
100 ms waits and honor namespace `_GLK`. S0 query handling polls every 25 ms with
a 64-query budget and masks its global GPE; GPE-block packages and EC wake are
not supported. Q35 has no EC: protocol/timeout/ECDT validation tests plus a Q35
no-EC guest run are **not** proof of native EC transactions. The prohibited
hardware run is the outstanding native acceptance blocker. Thermal zones expose `acpitz`, `_TMP`, valid `_CRT`/`_PSV` trip values/types in
`/sys/class/thermal`. Reads use the Linux integer-decikelvin offset heuristic,
not fabricated zero on AML failure. A five-second monitor sends the existing
ordered shutdown request at `_CRT`; it is not a passive cooling governor.
An authored injected SSDT was measured at 26800 mC with a 36800 mC critical
trip and 31800 mC passive trip. Ten seconds later its `_TMP` reached `_CRT`;
the log showed the critical request, button event, filesystem flush, AML
preparation and clean QEMU S5. This proves policy, not an N305 sensor.
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


## Default-path follow-up: bootstrap phases

The pre-probe service boundary now uses the BSP's initialized heap/mappings,
scheduler, constructors, IRQ broker and timer. Firmware services finish before
the initial PCI probe. AP startup retains its original post-device boundary;
this is not a disabled scheduler, polling substitute, skipped accounting check
or second runtime. A separate non-inlined device-subsystem phase limits the
long-lived boot frame. Init no longer starts ACPICA after the user task is built.

Reproduction isolated the invalid ordering: bringing AP scheduling ahead of
device initialization also crashed the `acpi=static` control, while the BSP-only
pre-probe phase with the eventual full SMP4 machine passed all 69 guest cases.
The UP early-AP-order experiment booted and ran but failed its SMP-dependent
namespace case; it was not accepted as a full pass. GDB observed failed current
validation in an AP timer and separate invalid instruction-pointer faults; the
precise corruption origin in that discarded early-AP order is not proven.
The shipped phase restores the supported AP boundary, leaves all scheduler
assertions unchanged, and has real SMP4 runtime validation. ACPICA is still
opt-in until routing/INTx and default-native ABI acceptance finish.
