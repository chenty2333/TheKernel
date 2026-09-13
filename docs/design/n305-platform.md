# N305 Platform Bring-up — Discovery, Assumptions, and What Only Hardware Can Settle

**Target machine:** Acer mini PC, Intel Core i3-N305 (Alder Lake-N, eight Gracemont
E-cores, no SMT).
**Target worktree:** `/home/ava/Worktrees/TheKernel/n305-platform`
**Branch:** `feat/n305-platform`
**Companion profile:** `config/x86_64/n305.toml`

**Evidence rules.** Every factual claim below is tagged:

| Tag | Meaning |
|---|---|
| `[V]` | Verified by running the cited command in this worktree and reading its output. |
| `[R]` | Verified by reading the cited source in this worktree, with `file:line`. |
| `[I]` | Inference from verified code or output. Labelled as an inference, not a fact. |
| `[U]` | **Unverified.** Only a boot on the target machine can settle it. |

This document deliberately contains a large `[U]` section. The profile it describes has
never run on the machine it describes, and presenting a plausible memory map as a
measurement would be the most expensive kind of error here.

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
| PCI bus walk | `crates/ax/thekernel-axdriver/src/bus/pci.rs` read `axconfig::devices::PCI_ECAM_BASE` | `axhal::pci::ecam_base()` `[R]` |
| Uncore PCI accessor | `crates/ax/thekernel-axplat-x86-pc/src/perf_uncore.rs` read `crate::config::devices::PCI_ECAM_BASE` | `crate::acpi::pci_ecam_base()` `[R]` |

The parser is pure and host-tested: `crates/ax/thekernel-axplat-x86-pc/src/acpi/mcfg.rs`
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
`crates/ax/thekernel-axplat-x86-pc/src/acpi.rs::select_pci_ecam`:

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

### 1.5 Measured on QEMU (this worktree, q35 profile)

PENDING_QEMU_BOOT_EVIDENCE

`[V]` (exact lines and the command that produced them: §4). Two facts follow.

**QEMU's GRUB does not hand this kernel an MCFG, and the kernel cannot find one itself.**
`mcfg=absent:no-mcfg-table root=none` means the MCFG lookup found no RSDP at all, which is
the strongest of the "absent" outcomes: the kernel had nothing to parse, so this boot
exercises the fallback path and *not* the discovery path. `[V]` `[I]` The cause is that
GRUB passes no Multiboot2 ACPI tag here (`count=13`, no type-14 or type-15 tag in the
inventory) and the legacy `0xe0000..0x100000` / EBDA scan did not find a valid RSDP either;
under OVMF the RSDP is not normally in that window. QEMU therefore **cannot** be used as
evidence that MCFG discovery works — see §4 for how that gap is closed instead.

**`usable_regions=6 coalesced=0` on QEMU is the expected shape**, not a failure of the
coalescing logic: QEMU's firmware map is already six disjoint runs, so there is nothing to
merge. The coalescing behaviour is pinned by host tests instead, including a 4096-descriptor
map of one contiguous run collapsing to a single range. `[V]`

### 1.6 QEMU cannot settle the real question

QEMU's `-machine q35` + OVMF **does** build an MCFG table, but this kernel never sees it
because it boots through GRUB's Multiboot handoff and GRUB does not forward the RSDP. `[I]`
So "does the discovery path work on real firmware?" is precisely the question that remains
open, and it is listed as such in §5.

---

## 2. What is still a compile-time assumption

| Assumption | Where it lives | Consequence if wrong |
|---|---|---|
| Kernel load address `0x200000` | `n305.toml [plat] kernel-base-paddr` | GRUB cannot load the image |
| Kernel virtual base and direct-map offset | `n305.toml [plat]` | Immediate fault at entry |
| `mmio-ranges` (BAR windows, ECAM, APIC, HPET) | `n305.toml [devices]` | RAM marked as device memory, or a device access to unmapped memory |
| `pci-bus-end = 0xff` | `n305.toml [devices]` | A too-low bound silently hides devices |
| `max-cpu-num = 8` | `n305.toml [plat]` | Fewer than eight CPUs come online |
| Timer and IPI vectors | `n305.toml [devices]` | Not machine facts; kernel convention |
| Primary console = COM1 at `0x3f8`, diagnostics = COM2 at `0x2f8` | `crates/ax/thekernel-axplat-x86-pc/src/console.rs` | No console output at all on this machine |

`plat.phys-memory-size` is **not** on that list because it is inert: no Rust source in
`crates/` or `kernel/` mentions `PHYS_MEMORY` `[V]`, and installed RAM comes from the
Multiboot2 memory map (`crates/ax/thekernel-axplat-x86-pc/src/mem.rs::init` `[R]`). Do not
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
* **The kernel's diagnostic channel: already safe.** `console.rs::init_diagnostic()`
  probes COM2's scratch register and gates every write on `DIAGNOSTIC_PRESENT`. `[R]`
* **The kernel's primary console: silently useless, but harmless.** `putchar` writes to a
  hardcoded `SerialPort::new(0x3f8)`. `[R]` An absent port reads back `0xff`, which sets the
  transmitter-ready bit, so the write path does not spin or fault — the bytes simply go
  nowhere. `[I]`
* **The `[devices]` schema cannot express "no serial".** There is no serial key in
  `config/x86_64/q35-uefi.toml`, in `crates/ax/thekernel-axplat-x86-pc/axconfig.toml`, or in
  any Rust source: `serial` appears in the platform crate only as the literal port base and
  the `uart_16550` dependency. `[V]` The schema is the profile TOML itself plus
  `axconfig-gen`'s key dump; a new key would need Rust code to read it. **This is reported
  rather than invented**: `n305.toml` says nothing about serial, and the limitation is
  this section.

The consequence for the target machine is that the framebuffer console is not a
convenience, it is the only output channel — which is why the framebuffer verdict is now
reported to the kernel log (§1.5 of the commit history; `axhal::boot::framebuffer_rejection`
and the boot-path `info!`/`warn!` line in `crates/ax/thekernel-axruntime/src/lib.rs`). `[R]`

---

## 4. How to test this without the machine, and what that is worth

```sh
# Host unit tests, including the pure MCFG parser and the selection logic.
python3 tools/thekernel.py test --suite host

# Cross-check the parser against a real table: this host's own MCFG.
python3 - <<'PY'
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

# Boot the profile that is known to work, and read the ECAM decision.
python3 tools/thekernel.py run --smp 4 --memory 512M
```

The host MCFG cross-check requires `root` to read
`/sys/firmware/acpi/tables/MCFG`; it is a *parser* cross-check only, since that table
describes the build host and not the N305. `[V]`

A stronger check exists and is worth running once before the hardware boot: boot the
kernel **without** GRUB's Multiboot handoff stealing the RSDP path — that is, on a
firmware whose RSDP this kernel can find. On the evidence in §1.5 the q35 profile cannot
provide that, so the discovery path's first real execution is expected to be the N305
itself. Treat the first hardware boot as a test of untested code, not as a confirmation.

---

## 5. Unverified on hardware — the list a real boot must settle

Each item names what would confirm it, so one boot with `AX_LOG=info` settles all of them.

1. **`[U]` The ECAM base and bus range from MCFG.** Confirm with the `pci-ecam:` line:
   `source=mcfg` means discovery worked; `source=configured` means the value below was used
   instead. Expected on Alder Lake: base `0xe0000000`, segment 0, buses `0x00-0xff`; the
   profile's fallback is that guess, not a measurement.
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
5. **`[U]` The kernel's own boot path completes with no serial port.** Nothing here has
   run on a machine whose COM1 and COM2 are absent.
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
   and the retained region count. On QEMU these are equal (6 and 6). A real UEFI map is the
   reason the coalescing exists; if `truncated=1` appears, the capacity is still too small
   and the highest regions are being dropped.
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
* **QEMU cannot verify the discovery path here** (§1.6). That is the single largest
  remaining risk and it is settled by item 1 of §5 or not at all.
