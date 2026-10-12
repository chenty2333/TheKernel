# The connector: phase 2.1 and the pin a monitor answered on

Status: implemented on `feat/intel-connect` (commits `2c46a26b`, `08d99ca4`,
`80d87d29`, and this document).  This document describes what the AUX/DDC
power-well step (reference §11 phase 2.1) does, why it comes before GMBUS, the
pin → DDI → well table it acts on, how that table was cross-checked against
i915, and every way the step can fail.  It also states, once and plainly, **that
none of it has run on hardware.**

The deep register reference is
[`intel-display-registers.md`](intel-display-registers.md): §4.2 is the power
well table, §4.4 the enable handshake, §9 GMBUS/DDC and hotplug, §11 phases
2.1–2.3 the bring-up steps this workstream implements, and §11.1 the GMBUS
failure modes.  This file does not restate them; it records what the code does
with them and what was verified.

The target machine is an Acer 蜂鸟mini (SQM2270) with an Intel i3-N305 (Alder
Lake-N), display device `8086:46d0`, and **no serial port** — the screen is the
only console.  A step whose failure is not logged is a step whose failure is
invisible, which is why every observation below ends up in the debug file and
not only in the boot log.

## What is implemented

| file | responsibility |
|---|---|
| `kernel/src/drm/intel/connect.rs` | phase 2.1 (the well pair), the composition with `sink::probe_one`, and the `Connector` value |
| `kernel/src/drm/intel/connect/tests.rs` | 12 tests over the composition, driven through the GMBUS tests' controller model |
| `kernel/src/drm/intel/power.rs` | `WellObservation::state_set`, so a well that never came up is recorded in the same shape as one that did |
| `kernel/src/drm/intel/mod.rs` | `bring_up_at_boot` runs `connect::resolve_at_boot` and keeps the report for `/sys/kernel/debug/dri/0/intel_gpu` |

Two functions and one value:

```rust
pub(crate) fn resolve<R: Registers, T: PollTimer>(bdf, regs, timer) -> Result<Connector, ConnectError>
pub(crate) fn resolve_at_boot(report: &ProbeReport) -> ConnectReport
```

`Connector` is the single input the modeset takes: `bdf`, the `pin` a monitor
answered on, the `ddi` that pin carries, the validated `edid`, the `extension`
block when the sink declared one, the mode layer's `plan`, the `hotplug` status
read for that DDI, and `wells` — what phase 2.1 found.

`resolve` does exactly three things, in this order:

1. request `power::AUX_A` and `power::AUX_B` (§11 phase 2.1);
2. call [`sink::probe_one`], which is phases 2.2 and 2.3 — hotplug enabled and
   read once, the EDID read over GMBUS with its header and checksum checks, the
   extension block when declared, and `drm::modes::plan_modeset`;
3. pick the connector: the pin that answered, its DDI, and the hotplug line for
   that DDI out of the four the sink step read.

It duplicates none of step 2.  The EDID read, its validation, the extension
read, the hotplug read and the mode plan are the sink module's, unchanged and
tested by that module's own tests; this one adds the power step in front of them
and the choice afterwards.

`sink::probe_at_boot` — the sink module's own boot entry point — is no longer
called from `bring_up_at_boot`, for two reasons: it reads the bus *without*
powering the pin pair, which is the bug this workstream exists to fix, and
calling it after `connect::resolve_at_boot` would read every EDID twice.  The
sink module itself is untouched: `sink::probe_one` is the single probe path the
composition, the tests and the sink module all use, and the debug file's
`--- the connector …` section is what replaced the sink section it used to
print.

## Why the well comes before GMBUS

GMBUS is not a free-running block: a DDC channel sits behind a power gate, and
the gate is closed until something requests it.  Reference §11 phase 2.1 states
the consequence from the symptom end — *"Looks like it did nothing: GMBUS
returns NAK on every address, always."*  That symptom is worse than a missing
feature, because it is **indistinguishable from an empty port**: a shut gate and
an unplugged monitor produce the same `GMBUS2.SATOER` and the same
[`GmbusError::NoAck`] at the transaction layer.

That is the whole argument for this module.  `sink.rs` already reads the well's
state bit when a transaction NAKs and reports
[`GmbusError::AuxWellDown`](intel-gmbus.md) instead of a bare NAK — naming the
cause is its answer.  But naming a cause is not the same as removing it, and on
a machine where the firmware left the well off, a driver that only ever *reports*
the well reads, on the console, as "no monitor".  Phase 2.1 asks for the well
before the first transaction, so the diagnostic and the fix are not left to the
same run.

The order inside the step is the reference's too: §11 phase 2.3 says a monitor's
EDID "will appear on exactly one" of pin index 1 (DDI A) and pin index 2 (DDI
B), so the two wells those pins sit behind are requested first, in that order,
and both before any transaction.

## The pin → DDI → AUX-well table

The two candidate pins, with the register each one's well is requested through
(`kernel/src/drm/intel/connect.rs`, `CANDIDATES`):

| pin index (`GMBUS0[4:0]`) | DDC for | DDI | `power::Well` | register | index | REQ bit | STATE bit |
|---:|---|---|---|---|---:|---:|---:|
| 1 | DDI A | A | `AUX_A` | `ICL_PWR_WELL_CTL_AUX2` `0x45444` | 0 | `0x2` | `0x1` |
| 2 | DDI B | B | `AUX_B` | `ICL_PWR_WELL_CTL_AUX2` `0x45444` | 1 | `0x8` | `0x4` |
| 3 | DDI C | C | *(not requested)* | `ICL_PWR_WELL_CTL_AUX2` `0x45444` | 2 | `0x20` | `0x10` |
| 9–12 | Type-C 1–4 | *(none)* | *(not requested)* | `ICL_PWR_WELL_CTL_AUX2` `0x45444` | 3–6 | — | — |

Sources, all read as facts from a local **Linux v6.12** checkout of
`drivers/gpu/drm/i915/` (GPL-2.0; nothing is copied from it):

* the pin indices are `gmbus_pins_icp` (`display/intel_gmbus.c:113-124`), the
  table i915 selects for an ICP-or-later PCH — the reference's `[INF]`, item 6
  of §13.2;
* the register and the bit arithmetic: `i915_reg.h:3630-3631`
  (`HSW_PWR_WELL_CTL_REQ(pw_idx) = 0x2 << (pw_idx * 2)`,
  `..._STATE(pw_idx) = 0x1 << (pw_idx * 2)`) and `i915_reg.h:3663`
  (`ICL_PWR_WELL_CTL_AUX2 = _MMIO(0x45444)`);
* the indices: `i915_reg.h:3686-3688` (`ICL_PW_CTL_IDX_AUX_C = 2`, `_B = 1`,
  `_A = 0`) and `i915_reg.h:3679-3685` (`TGL_PW_CTL_IDX_AUX_TC4 = 6` down to
  `_TC1 = 3`), which is what the `XE_LPD` table uses for `AUX_USBC1`–`4`;
* that the *driver* requests through the `...2` instance of the set:
  `i915_reg.h:3616-3624` (the comment block naming BIOS = `...1`,
  DRIVER = `...2`, DEBUG = `...4`) and
  `display/intel_display_power_well.c:1964-1968`
  (`icl_aux_power_well_regs = { .bios = ICL_PWR_WELL_CTL_AUX1,
  .driver = ICL_PWR_WELL_CTL_AUX2, .debug = ICL_PWR_WELL_CTL_AUX4 }`).

Three things that follow, which the code acts on:

* **The handshake is the plain one.**  `icl_aux_power_well_enable`
  (`display/intel_display_power_well.c:545-557`) dispatches a Type-C PHY to the
  TC path and an Icelake part to a combo-PHY path, and everything else — which
  is ADL-N, a combo PHY that is neither — to `hsw_power_well_enable`
  (`:342-384`).  That function's `has_fuses` block is skipped when the well has
  no fuses, and the `XE_LPD` `AUX_A`/`AUX_B` entries
  (`display/intel_display_power_map.c:1377-1383`) set neither `has_fuses` nor
  `enable_timeout`, so what is left is: rmw the request bit into the driver
  register, then wait for the state bit.  `power::enable_well` does exactly that
  for a `Well` with `pg: None`, which `AUX_A`/`AUX_B` are
  (`power.rs:336-351`).  The one ADL workaround in front of it
  (`Wa_16013190616`, `GEN8_CHICKEN_DCPR_1.DISABLE_FLR_SRC`) is gated on
  `pg == SKL_PG1` and therefore does not apply to these wells — it has already
  run once, for `PW_1`, in phase 1.3.
* **`AUX_A` and `AUX_B` are not south-display domains.**  The reference's §4.3
  reading is confirmed: `xelpd_pwdoms_pw_2`
  (`display/intel_display_power_map.c:1278-1281`) contains
  `XELPD_DC_OFF_PORT_POWER_DOMAINS` (`:1247-1270`), whose `AUX` entries begin at
  `POWER_DOMAIN_AUX_C` (`:1260`); `AUX_A` and `AUX_B` appear instead in the
  `DC_off` well's domain set (`:1299-1307`, the two entries at `:1305-1306`),
  and `PW_1`/`PG_1` is documented at `:1283-1287` as carrying DDI A and DDI B.
  The kernel's phase 1 enables `PW_1` and disables DC states, so both candidate
  pins' channels are inside what is already powered; only the per-pair gate is
  left to request.
* **A monitor on DDI C would need `PW_2` first.**  `POWER_DOMAIN_AUX_C` is in
  `PW_2`'s domain set (same lines as above), and `PW_2` is not enabled by this
  kernel's phase 1 — `power.rs` documents it as "needed for pipes B..D and for
  the south display, which are later phases".  So `connect.rs` requests the two
  wells §11 phase 2.3 names as the candidates and leaves `AUX_C` alone; the
  GMBUS layer still asks pin 3, and a NAK there is reported as `AUX_C` reading
  back off.  This is a deliberate limit, not an oversight: requesting a well
  whose parent is down would produce a state bit that reads 0 for a reason this
  module cannot fix, and reporting that as "no monitor on DDI C" is exactly the
  confusion the step exists to remove.

### A well that does not come up is recorded, not fatal

`power::enable_well` reports a state bit that never set as an error, because its
caller is expected to stop.  This caller does not stop:

* the firmware may already hold the well on under a request bit this driver does
  not own, which §11 phase 2.1 lists first among the causes of a state bit that
  is already set;
* `enable_well` withdraws the request bit it added when the state never comes
  up, so a failed attempt leaves the register as it was found;
* what the machine then does is the bus's answer, and the bus already names it.

So a failure is turned back into the record a successful enable produces
(`connect.rs`, `observation_of`), `WellObservation::state_set` becomes `false`,
the line reads `state NEVER CAME UP`, and the probe runs anyway.  If a monitor
answers on the other pin, the result is a `Connector` that carries both the
working well and the dead one in `Connector::wells` — which is what the debug
file prints.  If nothing answers, the well failure is the error, because it is
the actionable half of the answer.

## The failure taxonomy

One variant per way the composition can fail to produce a connector
(`ConnectError`), each with a `describe()` that names the registers and bits
involved:

| variant | when | what the next move is |
|---|---|---|
| `NoMonitor { pins }` | no pin's read produced a valid block and nothing said bytes came back | the list carries every pin's own answer: a NAK is the cable, the port or the address; a floating bus is no device; `AUX_C` reading off is the limit above |
| `EdidRejected { pin, error }` | a pin answered and its block failed the header or the checksum **twice**, at 100 kHz and then 50 kHz | the display is there and its bytes are not trustworthy: re-read, slow the bus further, or fix the cable. Kept apart from `NoMonitor` because a log that says "no EDID" for both wastes a day |
| `NoDdiForPin { pin }` | the pin that answered carries no DDI (a Type-C pin) | nothing to program: reaching those channels needs the DKL PHY, which §8.8 defers |
| `HotplugUnreadable { ddi, error }` | the DDI's hotplug status could not be read | a window that does not reach `SHOTPLUG_CTL_DDI` or `SDEISR`; a kernel bug, named |
| `HotplugNotRecorded { ddi }` | the sink step read hotplug for all four DDIs and this one is absent from its ledger | a bug in this kernel, not a state of the machine |
| `WellDown { well, observation, cause }` | no monitor answered and a candidate well did not come up | the reference's own symptom, from the power side: `cause` is `power::enable_well`'s account and `observation` the register words §11 phase 1.3 asks a reader to compare |
| `SinkIncomplete` | the sink step answered with a pin but no EDID, or an EDID but no plan | a bug in this kernel: `sink::probe_one` builds those three from one answer. Refused rather than unwrapped |

The ordering inside `resolve` is deliberate: a block that came back and did not
validate is reported as itself before a well failure is considered, because
bytes arriving at all is the stronger fact; and a well failure is reported
before "no monitor", because "the channel was shut" is actionable and "nothing
answered" is not.

## The register cross-check (deliverable 3)

The reference at line 478 states: `AUX_A` at `ICL_PWR_WELL_CTL_AUX2` (`0x45444`)
index 0, request `0x2`, state `0x1`; `AUX_B` derived as index 1 (`0x8`/`0x4`).
Because a wrong well means GMBUS NAKs forever on the real machine, that table
was checked claim by claim against i915 (**Linux v6.12**, read as facts from a
local checkout; no code or prose copied):

| claim | i915 evidence | verdict |
|---|---|---|
| `ICL_PWR_WELL_CTL_AUX2` is `0x45444` | `i915_reg.h:3663`; its siblings `ICL_PWR_WELL_CTL_AUX1` `0x45440` and `AUX4` `0x4544C` at `:3662`, `:3664` | **confirmed** |
| `AUX_A` is index 0, REQ `0x2`, STATE `0x1` | `ICL_PW_CTL_IDX_AUX_A = 0` (`i915_reg.h:3688`) with `HSW_PWR_WELL_CTL_REQ/STATE(pw_idx) = 0x2/0x1 << (pw_idx * 2)` (`:3630-3631`) | **confirmed** |
| `AUX_B` is index 1, REQ `0x8`, STATE `0x4` | `ICL_PW_CTL_IDX_AUX_B = 1` (`i915_reg.h:3687`), same formula | **confirmed** |
| `ICL_PWR_WELL_CTL_AUX1`/`AUX2` and the DDI set exist, and the driver uses the `...2` instance | `i915_reg.h:3616-3624` (the set comment: BIOS `...1`, DRIVER `...2`, DEBUG `...4`), `:3690-3692` (`ICL_PWR_WELL_CTL_DDI1/2/4` `0x45450`/`0x45454`/`0x4545C`), and `intel_display_power_well.c:1964-1968` (`icl_aux_power_well_regs.driver = ICL_PWR_WELL_CTL_AUX2`) | **confirmed** |
| `ICL_AUX_PW_TO_CH` exists, and the well-to-channel mapping | it exists: `intel_display_power_well.c:207-208`, `ICL_AUX_PW_TO_CH(pw_idx) = (pw_idx) - ICL_PW_CTL_IDX_AUX_A + AUX_CH_A` — so `AUX_A` → `AUX_CH_A`; `:204-205` is the PHY form, `AUX_A` → `PHY_A` | **confirmed** |
| which table ADL-N uses | `intel_display_device.c:1059` defines `xe_lpd_display` with `.__runtime_defaults.ip.ver = 13` (`:1051`), and `adl_p_desc` points at it (`:1113`) with ADL-N as a subplatform (`:1105`); `intel_display_power_map.c:1702` selects `xelpd_power_wells` for `DISPLAY_VER >= 13`, whose `AUX_A`/`AUX_B`/`AUX_C` instances carry the ICL indices (`:1377-1379`) | **confirmed** |

**Outcome: the reference agrees with i915 on every number checked.  No
discrepancy was found, and no value had to be changed.**  The register and bits
the code uses are the ones the vendor driver writes for this part, through the
same requester register, for the same class of well (a combo-PHY AUX well on a
non-Icelake part, which takes the generic handshake path).  Three secondary
facts fell out of the check and are recorded above: the `AUX_D`/`AUX_E` indices
on `XE_LPD` are 7 and 8, *not* the ICL/TGL 3 and 4 (`i915_reg.h:3676-3678`) —
which is a trap for anyone extending `Pin::aux_well()` towards DDI D or E — the
Type-C indices (`3`–`6`, `i915_reg.h:3679-3685`) match what `Pin::aux_well()`
already says, and the AUX register set has **no KVMR requester**
(`icl_aux_power_well_regs` names only `bios`, `driver` and `debug`).

> **Finding, not fixed here.**  `power::requesters` reads the four *main*
> requester registers (`HSW_PWR_WELL_CTL1`–`4`) for whichever well it is given,
> so for a well in the AUX or DDI set the line it prints is about the wrong
> registers: on this machine `AUX_A`'s "driver" column actually reports
> `HSW_PWR_WELL_CTL2` bit 1, which is `PW_1`'s request.  That is pre-existing
> behaviour shared with `output.rs`'s `DDI_IO_A` observation, and it is outside
> this workstream's file; the well record this workstream produces prints it as
> it finds it.  Fixing it means giving `Well` the requester registers of its own
> set (bios/driver/debug, no kvmr for AUX and DDI), which is a change to the
> power workstream's model.

## What is verified, and by what

Host tests only: `cargo test … drm::intel`, **278 passed** (12 of them this
workstream's).  The composition tests drive the real `resolve`, the real
`sink::probe_one`, the real GMBUS state machine and the real mode layer, through
`gmbus::tests::FakeController` — the same device model the GMBUS tests use,
layered on a real `RegisterWindow` over an ordinary buffer — with the AUX well
register modelled in front of it (`connect/tests.rs`, `Bench`).

What the tests establish:

* **both candidate wells are requested before the first GMBUS transaction** —
  the ordered write log is asserted, not just the values: two writes to
  `ICL_PWR_WELL_CTL_AUX2`, each carrying the right request bit, both before the
  first write to `GMBUS0`;
* the candidate table agrees with `Pin::aux_well()` (the GMBUS layer's spelling
  of the same mapping), with the register table, and with the DDI index — so the
  two mappings cannot drift apart, and the register/bit values are pinned
  against the cross-check above;
* **a well that never comes up still probes and reports**: the monitor on DDI B
  is found and becomes a connector, `AUX_A`'s record says `NEVER CAME UP`, the
  rendered report names it next to the connector, and the GMBUS layer
  independently reports pin 1 as `AuxWellDown { well: AUX_A }` — the reference's
  symptom, in both layers' words;
* a well the firmware had already left on is recorded as `was already on`, and
  the request this driver adds is not mistaken for its own;
* **a monitor on DDI A and a monitor on DDI B each produce a connector with the
  right DDI**, the right pin index, the sink's 1920×1080@60 preferred timing
  (148.5 MHz) and the hotplug line for their own DDI, differing only in the two
  places the pin decides;
* **no monitor at all** is `NoMonitor` with all three pins' answers in the text,
  including `AUX_C` reading off for pin 3;
* **a block that does not validate** — a broken checksum and a broken header —
  is `EdidRejected` naming the pin and the check that failed, and not
  `NoMonitor`;
* **a second call does not disturb the first answer**: the first `Connector` is
  compared against itself and against the second result, and only the well
  record differs (the second pass finds the first pass's request still held);
* the sink path is unchanged: `sink::probe_one` against the untouched
  `FakeController` still finds the monitor, the three pins, the strict plan and
  the same report lines;
* a report with no mapped display device produces an empty report rather than a
  panic or a guessed connector.

## What is not verified

**Nothing in this workstream has run on hardware.**  No register in this module
has been written to or read from an Alder Lake-N part; there is no measurement
of any kind behind any number here.  Specifically:

* **The AUX/DDC power wells have never been enabled on a real machine.**  That
  `AUX_A` and `AUX_B` come up when their request bits are written, and that the
  state bits are where the table says, is the vendor driver's behaviour as read
  from its source, plus a synthetic model.  It is not a measurement.
* **Whether the target board's panel is on DDI A or DDI B is unknown.**  Nothing
  here identifies a physical connector: the pin that answers identifies the
  port, and that answer is the first hardware fact this path will produce.
* **Which of `AUX_C` and above this SKU populates is still §4.2's `[GAP]`.**  The
  limit on DDI C described above is an argument from i915's domain graph, not
  from this part's datasheet.
* **`PW_2` is not enabled**, by design, so a monitor on DDI C (or on a Type-C
  port) cannot be brought up by this step even if the EDID reads.
* **The timeouts are chosen, not measured** (`WELL_STATE_TIMEOUT_US = 1000`), and
  the state poll is a count of reads rather than a clock reading
  (`regs::poll_attempts`), so "1 ms" is the vendor driver's figure carried
  across, not this machine's.
* **The boot wiring has not been exercised end to end on the target.**  The host
  test build cannot call `resolve_at_boot` with a real `ProbeReport`; the tests
  cover the empty-report path and `resolve` itself.
* **No interrupt is taken**, so nothing here reacts to a hotplug event: the
  status in `Connector::hotplug` is the one read immediately after enabling
  detection, which is the PRM's procedure (§9.4) and nothing more.

## Where the next step begins

1. Boot the machine and read `/sys/kernel/debug/dri/0/intel_gpu`.  The section
   headed `--- the connector (reference section 11 phase 2) ---` carries the
   well lines and either the connector or the named failure; the GMBUS lines
   from the sink step are in the same file.
2. If a well reads `NEVER CAME UP` and every pin NAKs, that is the finding: the
   register words are in the line, and §11 phase 1.3's causes apply in order.
3. If a connector appears, its `plan` is the mode to program: reference §11
   phases 3 onwards, which is the modeset workstream.
