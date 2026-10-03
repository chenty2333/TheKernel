# N305 Platform Bring-up — Discovery, Assumptions, and What Only Hardware Can Settle

**Target machine:** Acer mini PC, Intel Core i3-N305 (Alder Lake-N, eight Gracemont
E-cores, no SMT).
**Current preparation worktree:** `/home/ava/Worktrees/TheKernel/dev`
**Current branch:** `dev`
**Companion profile:** `config/x86_64/n305.toml`

**Evidence rules.** Every factual claim below is tagged:

| Tag | Meaning |
|---|---|
| `[V]` | Verified by running the cited command in this worktree and reading its output. |
| `[R]` | Verified by reading the cited source in this worktree, with `file:line`. |
| `[I]` | Inference from verified code or output. Labelled as an inference, not a fact. |
| `[U]` | **Unverified.** Only a boot on the target machine can settle it. |

The original platform audit below predates the user-reported 2026-10-03 PXE
shell boot (MCFG ECAM 0xc0000000, firmware framebuffer 800x600 at 0x4000000000).
That observation is not a native GPU/NIC validation. The dated runtime update
at the end and n305-tonight.md supersede old readiness statements; configured
fallbacks still must not be presented as hardware measurements.

---

## 1. What is now discovered at runtime

### 1.1 The PCI ECAM base comes from firmware

Before this work the kernel read its memory-mapped PCI configuration base from a
compile-time constant (`axconfig::devices::PCI_ECAM_BASE`, fed by
`config/x86_64/q35-uefi.toml`). `[R]` That is a QEMU fact, not a machine fact.

The kernel now locates the ACPI **MCFG** table during early initialization, selects one
region, and publishes it. Both consumers ask the platform instead of reading the constant:

| Consumer | Before | After |
|---|---|---|
| PCI bus walk | `crates/ax/tk-axdriver/src/bus/pci.rs` read `axconfig::devices::PCI_ECAM_BASE` | `axhal::pci::ecam_base()` `[R]` |
| Uncore PCI accessor | `crates/ax/tk-axplat-x86-pc/src/perf_uncore.rs` read `crate::config::devices::PCI_ECAM_BASE` | `crate::acpi::pci_ecam_base()` `[R]` |

The parser is pure and host-tested: `crates/ax/tk-axplat-x86-pc/src/acpi/mcfg.rs`
takes raw table bytes and returns `(base_address, segment_group, start_bus, end_bus)` per
declared region. `[R]` The ACPI header's length field is authoritative, so a caller that
passes a whole page still gets exactly the declared table; a declared length that exceeds
the supplied bytes is rejected rather than read past. Every failure is a returned error —
this runs before the kernel can report a fault, so it must not panic, and a deterministic
sweep over arbitrary bytes pins that. `[V]`

Discovery reuses the RSDP path that already fed MADT discovery; the RSDP decode and the
root-table walk are now shared by both tables rather than duplicated. `[R]`

### 1.2 Selection is deterministic, and the rule is stated in code

Segment group 0's region wins, first in table order. `[R]` The reasons are recorded at
`crates/ax/tk-axplat-x86-pc/src/acpi.rs::select_pci_ecam`:

* the PCI root this kernel builds addresses one segment, and no segment number is
  expressible in a bus-device-function address, so segment 0 is the only eligible group;
* ACPI defines no precedence among several segment-0 regions, so table order is the only
  rule that does not invent one;
* a region that starts above bus 0 is rejected, because the scan begins at bus 0 and would
  otherwise read configuration space firmware did not describe.

### 1.3 The fallback is honest, and distinguishable

When no usable MCFG region exists, the kernel uses the configured `pci-ecam-base` exactly
as before and says so. The boot log states the base, the segment, the bus range, and
**whether the value was discovered or configured**; a configured value gets a second line
naming it a fallback. `[R]` Selection is a pure function with host tests covering the
no-eligible-region case. `[V]`

### 1.4 The memory map no longer depends on firmware verbosity

The owned Multiboot2 parser stored every usable memory entry verbatim against a 16-entry
array, and the 17th returned `MemoryMapCapacity`, which the caller turned into a panic —
before the UART exists and before a framebuffer console can. `[R]` A UEFI memory map on a
real machine describes physically contiguous RAM as many separate `EfiConventionalMemory`
descriptors, so this was a plausible permanent blank screen on exactly this machine.

Usable entries are now coalesced into sorted, disjoint ranges while parsing, the capacity
is 64, and overflow truncates and records the truncation instead of failing. `[R]` The
boot log reports the declared entry count, the retained region count, and whether the map
was truncated. `[V]`

### 1.5 Measured on QEMU (this worktree, q35 profile, GRUB + OVMF)

Command: `python3 tools/thekernel.py test --suite guest --smp 4 --memory 512M --accel tcg
--timeout 300`, log at `/home/ava/.cache/thekernel-targets/n305-platform/runs/system-*/kernel.log`.

```
MB2 tag inventory: protocol=Multiboot2 count=13 truncated=0
MB2 memory map: entries=7 usable_regions=7 capacity=64 coalesced=0 truncated=0
MB2 framebuffer: declined reason=UnusableAddress
<4>[0.006398 ... WARN target=axruntime module=axruntime] boot framebuffer: declined: framebuffer address is unusable
<6>[0.100469 ... INFO target=axplat_x86_pc::acpi module=axplat_x86_pc::acpi] pci-ecam: base=0xe0000000 segment=0 bus=0x00-0xff source=mcfg mcfg=selected root=xsdt regions=1
```

`[V]` Four facts follow.

**The discovery path runs, on real firmware tables.** `source=mcfg mcfg=selected
root=xsdt regions=1` means the MCFG was located through the ACPI root table and its single
segment-0 region selected — the value the kernel now uses came from firmware, not from
`pci-ecam-base`. `[V]` GRUB supplied both ACPI tags here (type 14 size 28 and type 15 size
44), and the RSDP in the type-15 tag names the XSDT the MCFG was found through. `[V]` This
is the evidence that the parser and the walk are exercised end to end, and it also shows
the discovery and the fallback are distinguishable in one line: on a boot where the table
is absent the same line reads `source=configured mcfg=absent:...`, followed by the extra
line naming the value a fallback.

**The reported fallback was never used in this boot**, because firmware answered. The
configured `0xe000_0000` and the discovered base happen to be equal here; that coincidence
is exactly why the `source=` field exists and why a reader must check it rather than
recognising the address.

**`usable_regions=7 coalesced=0` is the expected shape**, not a failure of the coalescing
logic: this firmware map is already seven disjoint runs, so there is nothing to merge. The
coalescing behaviour is pinned by host tests instead, including a 4096-descriptor map of
one contiguous run collapsing to a single range. `[V]`

**The framebuffer verdict reaches the ordinary kernel log.** The `WARN` line is an
`axruntime` log record, not a COM2 diagnostic, so on a machine with no serial port it is
what a framebuffer console mirrors to the screen. This boot declined the firmware
framebuffer (`UnusableAddress`), so the screen stays blank here — but the reason is now
recorded where the screen can show it, which is the point of the relay. `[V]`

### 1.6 What QEMU does and does not settle

QEMU settles that the MCFG lookup works against a real ACPI table set: the RSDP arrives in
a Multiboot2 ACPI tag, the XSDT is walked, the MCFG is found and parsed, and its region is
selected and published. `[V]`

It does not settle that the *value* is right for the N305. `0xe000_0000` is what OVMF's
q35 firmware declares; the target machine's firmware declares its own answer, and the
fallback and the `mmio-ranges` in `n305.toml` are still the guesses listed in §5. `[U]`

---

## 2. What is still a compile-time assumption

| Assumption | Where it lives | Consequence if wrong |
|---|---|---|
| Kernel load address `0x200000` | `n305.toml [plat] kernel-base-paddr` | GRUB cannot load the image |
| Kernel virtual base and direct-map offset | `n305.toml [plat]` | Immediate fault at entry |
| Configured `mmio-ranges` fallback | `n305.toml [devices]` plus discovered ranges | MADT/HPET/MCFG addresses are added at runtime; remaining configured windows are assumptions |
| `pci-bus-end = 0xff` fallback | `n305.toml [devices]` | Only used without usable MCFG; runtime end bus wins |
| `max-cpu-num = 8` | `n305.toml [plat]` | Fewer than eight CPUs come online |
| Timer and IPI vectors | `n305.toml [devices]` | Not machine facts; kernel convention |
| UART console = COM1 at `0x3f8` | `crates/ax/tk-axplat-x86-pc/src/console.rs` | It may answer without an exposed port; the firmware screen remains the human console |
| Which UART carries the log | *not* an assumption any more: `console.rs::select_sink` probes COM2, COM3 and COM4 and falls back to sharing COM1 | Log drain lands on a port nobody is watching; the fallback keeps it on the console wire |

`plat.phys-memory-size` is **not** on that list because it is inert: no Rust source in
`crates/` or `kernel/` mentions `PHYS_MEMORY` `[V]`, and installed RAM comes from the
Multiboot2 memory map (`crates/ax/tk-axplat-x86-pc/src/mem.rs::init` `[R]`). Do not
use it to describe this machine's 16 GiB. A host test fails if any Rust source starts
reading it, so the comment cannot silently go stale. `[V]`

---

## 3. This machine has no 16550 UART, and the schema cannot say so

The N305 has no legacy COM port. What that breaks, and what it does not:

* **GRUB's console: broken, not fixed here.** `config/x86_64/grub.cfg` sets
  `terminal_output serial` with `serial --unit=0`. On a machine with no COM1, GRUB has no
  output terminal at all, so a failure *inside GRUB* is invisible on the only display the
  machine has. `[R]` `grub.cfg` is owned by another workstream and needs a QEMU
  re-verification of the framebuffer tag; this is recorded, not changed.
* **The kernel's diagnostic channel: discovered, not assumed.**
  `console.rs::init_diagnostic()` probes the UARTs beyond COM1 in turn and keeps
  the first that answers its scratch-register test, so a board whose second port
  is COM3 or COM4 gets its log there; a board with none of the three shares
  COM1's lock, and a board with no port at all stays memory-only. Every write
  is gated on that selection. `[R]` The selection itself is host-tested
  (`console::diagnostic_tests`), which is what "discovered" buys: no boot is
  needed to know the branch logic works, only to know which branch this machine
  takes. `[V]`
* **The kernel's primary console: silently useless, but harmless.** `putchar` writes to a
  hardcoded `SerialPort::new(0x3f8)`. `[R]` An absent port reads back `0xff`, which sets the
  transmitter-ready bit, so the write path does not spin or fault — the bytes simply go
  nowhere. `[I]`
* **The `[devices]` schema cannot express "no serial".** There is no serial key in
  `config/x86_64/q35-uefi.toml`, in `crates/ax/tk-axplat-x86-pc/axconfig.toml`, or in
  any Rust source: `serial` appears in the platform crate only as the literal port base and
  the `uart_16550` dependency. `[V]` The schema is the profile TOML itself plus
  `axconfig-gen`'s key dump; a new key would need Rust code to read it. **This is reported
  rather than invented**: `n305.toml` says nothing about serial, and the limitation is
  this section.

The consequence for the target machine is that the framebuffer console is not a
convenience, it is the only output channel — which is why the framebuffer verdict is now
reported to the kernel log (§1.5; `axhal::boot::framebuffer_rejection` and the boot-path
`info!`/`warn!` line in `crates/ax/tk-axruntime/src/lib.rs`). `[R]` `[V]`

---

## 4. How to test this without the machine, and what that is worth

```sh
# Host unit tests: the pure MCFG parser, the selection logic, the memory-map
# coalescing, and the machine-profile invariants.
python3 tools/thekernel.py test --suite host

# Cross-check the parser against a real table: this build host's own MCFG.
sudo python3 - <<'PY'
import struct, pathlib
b = pathlib.Path('/sys/firmware/acpi/tables/MCFG').read_bytes()
o = 44
while o + 16 <= len(b):
    base, seg, sb, eb, _ = struct.unpack_from('<QHBBH', b, o)
    print(f"base={base:#x} segment={seg} bus={sb:#04x}-{eb:#04x}")
    o += 16
PY

# Build the profile without booting it.
python3 tools/thekernel.py build --platform n305 --smp 4 --memory 512M

# Boot a profile whose firmware provides an MCFG, and read the ECAM decision.
python3 tools/thekernel.py test --suite guest --smp 4 --memory 512M --accel tcg --timeout 300
# then: grep pci-ecam "$THEKERNEL_STATE_DIR"/runs/system-*/kernel.log
```

The build host's MCFG describes the build host, not the N305, so that cross-check validates
the parser's understanding of the table layout and nothing else; it also needs `root` to
read the file. `[V]`

The guest run above is what turned §1.5 from an expectation into evidence: q35 + OVMF does
provide an MCFG and GRUB does forward the RSDP in a Multiboot2 ACPI tag, so the discovery
path is exercised on every guest run. `[V]` What no QEMU profile can settle is whether the
*selected address* is right for the target — that is §5's first item and it needs the
machine.

---

## 5. Unverified on hardware — the list a real boot must settle

Each item names what would confirm it, so one boot with `AX_LOG=info` settles all of them.

1. **The original ECAM expectation (superseded).** Confirm new boots with the `pci-ecam:` line:
   `source=mcfg` means discovery worked; `source=configured` means the configured value was
   used instead. Discovery itself is already exercised on QEMU (§1.5), so what is unverified
   here was whether this machine matched the old guess. The user now reports
   `0xc0000000`, segment 0, buses `0x00-0xff`; `0xe0000000` remains a configured
   fallback, not the target's observed ECAM base.
2. **`[U]` `pci-bus-end`.** The profile uses `0xff` (every bus) because a bound below the
   true top bus hides devices silently. If `source=mcfg` and the MCFG region's end bus is
   lower, that lower number is the real one.
3. **`[U]` The `mmio-ranges` list.** The profile's windows are the documented Alder Lake
   layout, not this machine's firmware allocation. Read the `Memory regions` lines and
   compare; a wrong window is a real fault, not a cosmetic error, because these ranges are
   mapped uncached and excluded from allocation.
4. **`[U]` `max-cpu-num = 8` is enough and not too much.** MADT decides what exists, so a
   machine reporting fewer cores still boots; the risk is the reverse, and eight is the
   part's core count.
5. **`[U]` The kernel's own boot path completes with no serial port.** The sink
   selection has host tests for every branch, but nothing here has run on a
   machine that takes the "no UART at all" branch, where the log ring stays in
   memory and only the screen can report anything.
6. **`[U]` The firmware framebuffer survives to a console.** The `boot framebuffer:` line
   says whether the tag was accepted or declined and why; a decline now reaches the kernel
   log, and therefore the screen, instead of only COM2.
7. **`[U]` GRUB reaches the kernel at all.** See §3: a GRUB-stage failure has no output
   terminal on this machine.
8. **`[U]` The tag inventory has headroom.** `MAX_TAG_INVENTORY = 32` and QEMU's GRUB
   produced 13 tags with none truncated. `[V]` A real UEFI GRUB may emit more (memory map,
   modules, EFI tags); truncation is graceful and recorded by `truncated=`, but the number
   is worth reading on the first hardware boot.
9. **`[U]` Memory-map fragmentation.** `MB2 memory map:` reports the declared entry count
   and the retained region count. On QEMU these are equal (7 and 7) and nothing merges. A
   real UEFI map is the reason the coalescing exists; if `truncated=1` appears, the capacity
   is still too small and the highest regions are being dropped.
10. **`[U]` The profile's `phys-memory-size` claim is irrelevant.** Expected, since nothing
    reads it; the `Memory regions` lines come from the firmware map.

---

## 6. What this branch does not do

* **It does not boot on the N305.** Nothing here has.
* **It does not change `config/x86_64/grub.cfg`**, so the GRUB-stage blindness in §3
  remains.
* **It does not add serial configuration to the `[devices]` schema**, because that schema
  has no mechanism for it and inventing one without a consumer would be worse than
  reporting the gap (§3).
* **`tools/thekernel.py` changes are confined to profile selection**: a `--platform`
  option, the machine-profile registry lookup in `artifacts_for`, and `generate_config`
  taking the profile's path and `max_cpus` instead of hardcoded values. The test-suite
  plumbing in that file is owned by another workstream and was not touched beyond this.
* **The guest suite is not green on this branch, for a reason unrelated to it.**
  `test --suite guest` fails exactly one case, `sysv-shm`, at
  `tests/guest/system-init.c:160`: after the last `shmdt` of an `IPC_RMID`-marked segment
  it expects `shmat()` to fail with `EINVAL`. The kernel returns `EIDRM`, and **Linux does
  too** - `shmat()` validates the segment against `ipc_valid_object()`, which returns
  `EIDRM` for a segment marked for deletion ([LKML, Davidlohr Bueso, 2015-10-12](https://lkml.org/lkml/2015/10/12/483)).
  The kernel's own host test `rmid_allows_new_attachment_until_final_detach_without_resurrection`
  (`kernel/src/syscall/ipc/shm.rs`) encodes the same `EIDRM` result. So the guest
  expectation is the thing that is wrong, and it is in another workstream's file.
  The failure cannot be attributed to this branch: the boot log shows the memory map this
  branch produces on q35 is `entries=7 usable_regions=7 coalesced=0 truncated=0`, which is
  the map the previous code produced too - no range was merged, dropped or reordered, so
  this branch's boot state is identical to the baseline's.

* **It does not verify anything about the target machine.** The discovery path is exercised
  on QEMU (§1.5), but every address in `n305.toml` is still a documented expectation rather
  than a measurement, and the first boot on the N305 is the first time they meet that
  firmware. §5 is the list of what that boot must settle.

## 2026-10-03 runtime-address corrections

The user observed a real boot with MCFG ECAM `0xc0000000`, while the Intel
probe still read the configured `0xe0000000`. This observation supersedes the
older statement that no target boot had occurred; it does not validate the
Intel modeset or either candidate NIC.

* Intel configuration reads now use `axhal::pci` (the same retained MCFG
  decision as the generic PCI bus), map the exact runtime span, and bound
  every read to that mapping. `mmio-ranges` is no longer an allowlist for
  those reads. Mapping failure leaves the firmware screen untouched.
* The MCFG bus limit is authoritative; `pci-bus-end` is only the no-MCFG
  fallback. Boot inventory reports every answering BDF, vendor/device,
  three-byte class and all raw BAR words **before** a driver resets a device.
  Upper words of 64-bit BARs are shown verbatim, not sized as separate BARs.
* LAPIC and the first IOAPIC already used MADT addresses. Their discovered
  pages now join the initial MMIO mapping, so a relocated controller does
  not disappear when the temporary boot page table is replaced.
* HPET previously assumed `0xfed00000`; it now uses a validated system-memory
  GAS in the ACPI HPET table. Absent/malformed HPET means no HPET MMIO reads,
  and the existing CPUID/PIT clock-selection paths remain available.
* The MADT interrupt-source-override decoder incorrectly expected 16 bytes.
  The actual record is 10 bytes. Its bus/IRQ/GSI/flags now decode correctly;
  this explains the observed "only 0 of 2 ... shown" without assuming the
  firmware records were malformed. This change reports overrides; it does
  **not** implement general ACPI interrupt routing.
* Installed RAM and framebuffer address/pitch/channel layout already come
  from the Multiboot handoff; the N305 framebuffer at `0x4000000000` is mapped
  from its actual tag, not a configured low-memory aperture. CPU topology,
  invariant-TSC availability and clock ratios already come from MADT/CPUID.
* `pci-ranges` remains a fallback allocator for **unassigned** PCI BARs, not
  runtime host-bridge `_CRS` discovery. The boot GPU is never sized/reassigned
  by the generic bus. Missing allocation space or malformed BARs now returns
  a probe error rather than panicking. General `_CRS`, multiple segments and
  multiple IOAPIC routing remain outside this preparation.

These changes are **未在硬件上验证**. Host tests cover ECAM geometry, HPET GAS,
raw BAR reads and the real ten-byte MADT layout. QEMU acceptance is recorded
in `n305-tonight.md`; no QEMU result proves a target register works.

## 2026-10-04 measured on the machine

Two boots on the target (Acer SQM2270, board N305MI25-AJ, BIOS 1.01
05/26/2023): the PXE Alpine capture, whose bundle is
`~/.cache/thekernel-targets/hw-prep-2026-10-03/capture/received/n305-20261003T235530Z`,
and TheKernel `e7108b2a` with `loglevel=7`, recorded from the HDMI capture.

Settled by the hardware:

* **Ethernet is a Realtek RTL8111/8168, `10ec:8168` rev 0x15, 1 GbE**, behind
  root port 00:1d.0 at 01:00.0 (I/O BAR plus 64-bit memory BARs at
  0x80504000 and 0x80500000). It is neither the RTL8125 nor an i225/i226: the
  "2.5G" in retail listings is wrong, and both prepared drivers correctly
  reported no matching device and touched nothing. Linux binds `r8169`.
* **ECAM is 0xc0000000, segment 0, buses 0-255** from MCFG (raw bytes
  `00 00 00 c0 00 00 00 00` at offset 44). TheKernel logs `source=mcfg` with
  that base; Linux's `/proc/iomem` agrees. The capture bundle's own MCFG
  decoder printed 0x80000000: that is a decoder bug in the capture tool.
* **The Intel display is `8086:46d0` at 00:02.0** and TheKernel's read-only
  probe now finds it (GSMBASE 0x7c000001, GGC VAMEN=0), then stops before
  power/modeset writes as designed. Linux's i915 binds with DMC.
* **MADT interrupt source overrides parse**: IRQ0 → GSI 2 (flags 0x0) and
  IRQ9 → GSI 9 (flags 0xd), replacing the earlier "0 of 2".
* The PCI inventory lists 18 functions and matches `lspci`, including
  `1cc4:6a13` NVMe, `8086:54c8` HDA, `8086:54f0` CNVi WiFi and an eMMC host.
* The firmware framebuffer stayed 800x600 even with GRUB `gfxpayload` asking
  for 1920x1080; firmware KMS registered `/dev/dri/card0` at that size.
* The HDMI EDID that Linux read belongs to the eEver capture dongle (1080p and
  2160p modes), not to a monitor.

Cautions:

* **The NVMe carries a BitLocker-encrypted Windows.** Never write to it; boot
  media for TheKernel must be USB.
* The ACPI FACS "bad checksum" line in the capture summary is a false alarm:
  FACS has no checksum field.

Open after these boots:

* With `loglevel=7` the screen stopped at `Initialize alarm...` and init's
  output never appeared for over a minute, while the `quiet` boot reached the
  shell within seconds. Cause unknown: a verbose-console stall is the first
  suspect.
* The stray `^@` before the shell prompt persists without a keyboard attached.
