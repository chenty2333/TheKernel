# Intel display probe

Status: implemented on `feat/intel-gpu-probe`.  This document describes what the
probe does, what it proves, what it does not, and where the next step toward a
modeset begins.  The deep register reference — offsets, bit fields, sequences —
is [`intel-display-registers.md`](intel-display-registers.md); this file does
not duplicate it.

The target machine is an Acer mini PC with an Intel i3-N305 (Alder Lake-N) whose
integrated graphics is PCI `8086:46d0`, a Gen12 Xe-LP block with the display
engine the firmware leaves running the firmware framebuffer.  On that machine
the kernel's console *is* that framebuffer, so the probe is deliberately built
so that it cannot disturb what is already on screen.

## What is implemented

`kernel/src/drm/intel/` — the first Intel-specific code in the kernel:

| module | responsibility |
|---|---|
| `pci.rs` | BDF arithmetic, ECAM configuration-space reads, header and BAR decoding |
| `id.rs` | the device table: device ids, generation, display version, steppings, quirks, apertures |
| `regs.rs` | the named-register table, the mapped window, typed 32-bit/64-bit access and its ordering |
| `probe.rs` | the bus walk, the identity decision, the register reads, and the report text |
| `debugfs.rs` | the report, served from the DRM debug filesystem |
| `testbus.rs` | `#[cfg(test)]` synthetic PCI bus, used only by the host tests |

The probe runs once, from the DRM initialization path, before devfs is mounted.
It walks PCI configuration space through the platform's ECAM aperture — the same
mechanism `axdriver`'s bus probe and the platform's uncore performance monitor
already use — finds Intel display functions, identifies them against the table,
maps the register aperture of a device it has a model for, reads the registers
the model says are safe, and prints one text report.  The same text is served
from `/sys/kernel/debug/dri/0/intel_gpu` for a person who was not watching the
boot; the target machine has no serial port, so a report that only reaches the
log is a report that scrolls away.

Three properties are load-bearing:

* **Configuration space is read and never written.**  `ConfigSpace` has no write
  method.  Sizing or assigning a BAR requires writing the header, and on the
  target machine the aperture in question is the one the console lives in.
* **Only registers in forcewake-free bands are named, and the check is a
  compile-time assertion.**  Most of the aperture is power gated; a read of a
  gated register returns zero, which is indistinguishable from a legitimate
  zero and would make every conclusion drawn from it meaningless.
* **An unknown device is a first-class answer.**  An Intel display device that
  is not in the table is reported — configuration space is a standard and
  decoding it is not guesswork — and its apertures are left untouched, because
  interpreting a register without a model of the part is guessing.

### The device table

One entry per known PCI device id, carrying the facts a driver needs and no
opinion: graphics generation, display version (`None` where public
documentation does not establish it), an aperture list keyed by *role* rather
than by BAR number, a quirk list asked for by name, and a PCI-revision to
silicon-stepping table.  A revision the table does not map becomes
`DisplayStepping::Unknown(revision)` rather than being rounded to the nearest
known stepping.

Alder Lake-N (`0x46d0`–`0x46d4`) is the one family modelled: Gen12 graphics,
`GTTMMADR` at BAR0 (16 MiB, non-prefetchable, 64-bit), `GMADR` at BAR2 (64-bit,
prefetchable, size chosen by firmware through the multi-size aperture control
register, so no size is stated), forcewake required, and **no `GMD_ID`** — that
register arrived with Meteor Lake, so the probe skips it on this part and says
why instead of reading a register that is not there.

### The registers it reads

`GMD_ID` (0x0d8c) and `GMD_ID_DISPLAY` (0x510a0), both skipped on Alder Lake-N
by the quirk above; `GFX_MSTR_IRQ` (0x190010), whose bit 31 is the global
graphics interrupt enable and which is therefore read and never written;
`GGC` (0x108040); and the 64-bit `DSMBASE` (0x1080c0) and `GSMBASE` (0x108100).
Every one is in a band the Gen12 forcewake map marks as readable without
forcewake, and no register in the first 2 KiB of the aperture is named at all
because that range is reserved.

The GT interrupt dword registers (`0x190018`/`0x19001c`) are deliberately *not*
read: reading one locks it until a bit is written back, which is the opposite of
a harmless probe.  The per-bank interrupt identity registers need a selector
*write* first, so a bare read would report whatever the firmware left selected.

## What it proves

* The platform's configuration space is reachable and the walk terminates: every
  bus the platform declares is probed, and the multi-function rule is honoured.
* A device matching the table's PCI identity is present at a specific bus
  address, with a specific revision, subsystem id, class code and BAR set.
* The register aperture was mapped uncached and answered: the reported values
  came back from hardware, not from the identity table.
* Where a value can be checked against a second, independent source, the report
  says so.  `GGC`'s VGA-addressing bit predicts which of the two display
  subclasses the function must report, and the class code comes over PCI while
  the register comes out of the aperture — agreement is evidence the aperture
  belongs to the function being described, and disagreement is printed loudly.
  The modelled aperture size is checked against the BAR's own alignment, and the
  BAR encoding against the datasheet, both without writing anything.

## What it does not prove

* **No register value in any report is verified against real hardware.**  No
  register has ever been read from a real Gen12 GPU: the target machine has not
  run this code, and the first run on it is the first time these registers are
  read.  Every register value a report has ever shown came from QEMU, which has
  no Intel display device to find, or from a synthetic aperture in a host test.
  The *decoding* around those values is tested; the values themselves are not
  evidence of anything until a boot log comes back from the machine.
* QEMU cannot present an `8086:46d0`-class device: `virtio-gpu`, `pci-testdev`
  and `ivshmem` are different vendors, different classes, or both, so a QEMU
  boot exercises the ECAM walk, the multi-function rule, the rejection path and
  the "no Intel display device present" verdict against real (emulated)
  configuration space, and never a real 8080-family display device.  That is
  what the boot log says, in those words.
* The device table's facts are taken from public documentation and from the
  vendor driver's register definitions; they have not been confirmed against the
  target board.  Where a fact could not be established it is `None` or an empty
  table, and the report states the absence.
* Nothing is programmed: no power well, no PLL, no pipe, no plane, no DDI.  No
  mode is set and no pixel is scanned out by this kernel.

## Where a modeset driver plugs in

* **Scanout.**  When the display engine can be programmed, the driver registers
  a candidate with `drm::screen::register(Candidate::new(..., rank::DRIVER,
  ...))` and implements `pseudofs::dev::scanout::ScanoutSurface` over its own
  aperture.  Nothing in this module precludes that, and nothing here needs to
  change for it: the probe's job is to establish that the device is there and
  that its registers answer.
* **Registers.**  The register window is mapped once, by the probe, and stays
  mapped in the kernel address space; a driver that owns the device reuses it
  through the same `RegisterWindow`, and adds the writable registers it needs to
  the table as named entries rather than by loosening the access rules.
* **Identity.**  A driver asks `id::DisplayDevice` for apertures, stepping and
  quirks instead of switching on a device id of its own.  Adding a part is one
  table entry.

## The exact next step toward a modeset

Power well and forcewake, in that order, because everything else is behind them.
The registers this probe reads are precisely the ones that need neither, which
is why the probe can be honest today and why the next commit must be able to
turn a power well on and read it back before any display register is touched.
Concretely: add the display power-well registers and the forcewake request/ack
registers to the table as `ReadWrite`, prove the ack comes back with a timeout
and a rollback, and only then extend the probe's register set into the
display-engine range.  The second step is `GMBUS`/DDC and an EDID read, which is
the first thing that produces a fact the firmware did not give us.
