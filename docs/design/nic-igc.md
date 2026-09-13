# Intel i225/i226 (`igc`) Ethernet driver

Status: implemented on `feat/nic-igc`.  **It has never run on the machine it
was written for.**  This document says what is known, what is assumed, what is
measured, and what is unverified — and the last list is the long one.

The target is the Acer mini-PC described in [`n305-bringup.md`](n305-bringup.md):
an Intel i3-N305 (Alder Lake-N) with no operating system installed and no serial
port.  The acceptance plan for that machine has two steps: a person sees the
kernel boot on the screen, and then a checker on another machine reaches it over
the network.  The second step needed a driver for the NIC that is on the board.
This is that driver.

## 1. What the hardware is, and how confident we are

**Every N305 mini-PC in this class we have evidence for ships an Intel i225-V
(`8086:15f3`) or i226-V (`8086:125c`) 2.5 GbE controller.**  That sentence is an
*assumption*.  It comes from the class of machine and from this host's
`pci.ids`; **not one PCI configuration-space byte has ever been read from the
target machine**, and nothing in this driver's design can change that.  What the
driver can do — and does — is make the assumption cheap to confirm or refute
from the target's own log:

* if the part is there, the boot log says which id it found, what its BAR is,
  what its station address is and what speed the link came up at;
* if the part is **not** there, the boot log says so in one line, and if the
  machine has a different Intel network controller, that controller's device id
  is printed next to the verdict.  That line is the whole refutation.

The register-level facts are the vendor driver's, not a datasheet's: Intel does
not publish the i225/i226 datasheet, so every offset, bit, field and sequence in
this driver is cited from Linux v6.12
`drivers/net/ethernet/intel/igc/` (tag `v6.12`, commit
`adc218676eef25575469234709c2d87185ca223a`) — the facts, never the code; this
project is Apache-2.0 and that driver is GPL-2.0.  Each entry in the register
table carries the symbol and line it came from, and the report prints the
citation beside the value, so a reader with the source open can check any of it.

## 2. What is implemented

`crates/ax/thekernel-axdriver-net/src/igc/` — the driver, with no architecture
underneath it, which is what makes it testable on the host:

| module | responsibility |
|---|---|
| `mod.rs` | the HAL and bus traits, the register window over MMIO, and what each phase claims |
| `ids.rs` | the device table: the 16 ids Linux's `igc_pci_tbl` binds, and the `pci.ids` cross-check |
| `regs.rs` | the named-register table, access rules, typed field accessors, bounded access |
| `probe.rs` | the identify-only phase: configuration space, the verdict, the negative case |
| `bringup.rs` | reset, NVM auto-read, station address, PHY link poll, speed and duplex |
| `desc.rs` | descriptor layout, transmit/receive encoding, and the two ring cursors |
| `nic.rs` | the rings, the buffer handover, and `NetDriverOps` |
| `fake.rs` | `#[cfg(test)]` synthetic device: a register file, a device model, a DMA HAL |

`crates/ax/thekernel-axdriver/src/igc.rs` — the platform half: BAR mapping, the
clock, the DMA allocator, configuration-space decoding, and the PCI probe that
runs the three phases.

### The three phases

1. **Identify, do not program.**  Match the device ids, decode configuration
   space (vendor, device, subsystem, revision, class, every BAR, the MSI-X
   capability, the legacy interrupt line), map the 64 KiB window of BAR0, read
   five identification registers, and print one verdict.  It writes nothing: the
   phase's register list has no writable entry and a test drives it against a
   bus that records every write and asserts the record is empty.
2. **Reset, address, link.**  Assert `CTRL.GIO_MASTER_DISABLE` and poll
   `STATUS.GIO_MASTER_ENABLE` clear; mask every interrupt; stop both queues;
   assert `CTRL.RST`; wait for `EECD.AUTO_RD`, which is how the hardware says it
   has finished copying the NVM into the receive-address registers; read the
   station address out of `RAL(0)`/`RAH(0)`; set `CTRL.SLU` with the force bits
   clear so the PHY autonegotiates; poll the PHY's MII status register over
   `MDIC` for link; read speed and duplex out of `STATUS`.  Still no packets.
3. **Take over.**  Build one transmit and one receive ring of 256 descriptors,
   program them, fill the receive ring, and implement `NetDriverOps` so the
   network stack can send and receive.

Each phase logs its own outcome before the next begins, so a machine that fails
somewhere in the middle says where it failed instead of going quiet — which
matters on a machine whose only output channel is the screen.

## 3. The shape of the driver

**Registers are named values.**  A caller cannot form an offset by adding to a
base, cannot write a register the table did not declare writable, and cannot
read one it declared write-only.  Three properties are checked rather than
promised:

* every named register lies inside the mapped window, checked at compile time;
* every named register is dword aligned — which is how `IGC_GPHY_VERSION`
  (`igc_regs.h:17`, offset `0x1e`) was caught: it is listed beside `IGC_CTRL` in
  the vendor header but is a *PHY* register read through `MDIC`, and the
  alignment assertion turned that into a compile error;
* a read with a side effect is not a read.  `IGC_ICR` clears the causes it
  reports, so the table marks it read-to-clear and the identify phase's register
  list is a subset that excludes it.

**Rings follow the vendor driver's arithmetic.**  One descriptor is always left
unused, which is what makes full and empty distinguishable
(`igc.h:651-657`), and the tail register is one past the last descriptor handed
over (`igc_alloc_rx_buffers`, `igc_main.c:2229`).  That second convention is an
*inference from the vendor code*, not a datasheet statement, and it is the one
thing in this driver a reader should check hardest if the device behaves oddly.

**The receive path uses the vendor driver's written-back test.**  A non-zero
length in the descriptor's write-back word means the hardware has finished with
it (`igc_main.c:2601`); `IGC_RXD_STAT_EOP` is checked for end-of-packet and the
done bit is reported rather than required, because the vendor driver's hot path
does not look at it.

**Buffers are checked, not trusted.**  Every buffer handed to the stack is
remembered as in flight, and a buffer comes back only if it belongs to this
driver's pool and was actually handed out, so a foreign pointer or a second
return of the same buffer is refused instead of becoming a descriptor the
hardware would write through.

## 4. What is verified, and how

**Host tests are the only automated evidence for anything that depends on
hardware behaviour.**  They live beside the code they test, in the house style
of `kernel/src/drm/intel/`: a synthetic device answers the way the part is
documented to answer, and the driver's decisions are checked against it.  The
crate's suite covers:

* **the device table** — all 16 ids, the family classification (including the
  five ids Linux's own predicates do not classify), the two assumed ids, the
  duplicate-free ordering, and the `pci.ids` cross-check;
* **the register table** — every offset and access mode against the vendor
  source, every bit value against the line it came from, the presence of a
  purpose sentence and a citation on every entry, the compile-time alignment and
  window checks, and the access-rule wrapper's refusals (unwritable, unreadable,
  out-of-window);
* **the typed encodings** — the speed decode (including the non-monotonic
  `SPEED_2500`-without-`SPEED_1000` case, which decodes as 10 Mb/s in the vendor
  driver and does here too), the `MDIC` command and result words, `RCTL`,
  `TCTL`, `SRRCTL`, the queue thresholds, the ring geometry, and the
  receive-address assembly;
* **the bring-up state machine** — the reset handshake, the auto-read poll, the
  link poll and its bound, every failure path (no link, silent PHY, PHY error,
  GIO master that will not stop, blank NVM), and the exact list of registers the
  sequence writes, compared against what the bus actually saw;
* **descriptors** — the transmit command word bit by bit, the payload-length
  word, the fact that the write-back status shares a word with `olinfo_status`,
  the receive write-back decode, and the malformed cases;
* **the ring cursors** — wrapping, the one-unused-descriptor rule, out-of-order
  recycling, and the invariant that free space is the distance around the ring;
* **the buffer handover** — allocate, write, transmit, look at the descriptor
  word for word, complete it, recycle, look again; plus ring-full, oversized
  frame, malformed descriptor, foreign pointer and double-recycle.

**The negative case is boot-tested.**  QEMU 10.2.2 has no i225/i226 device
model (`qemu-system-x86_64 -device help` lists `e1000e` and `igb`, and nothing
whose name contains `i22`, `igc` or `2.5g`), so a QEMU boot with the driver
built in exercises exactly the path a machine without the part takes: the bus
walk finds no matching id, and the log carries the "no supported device present"
verdict.  That is the run `scripts/…`/`--net-igc` exists for.

**What is *not* verified by any of that:** every claim that depends on real
silicon.  See §6.

## 5. What is not implemented

Each of these is a deliberate omission with a stated consequence, not an
oversight.  They are the differences a reader will find against Linux's `igc`.

* **No PHY register is written.**  `igc_phy_setup_autoneg` (`igc_phy.c:135`)
  rewrites MII registers 4 and 9 and the 2.5 Gb/s bit in the MMD register 7.32
  so the PHY advertises exactly the speeds Linux wants.  This driver reads the
  advertisement and reports it, and relies on what the firmware left.  **This is
  the most likely reason a link would come up slower than 2.5 Gb/s on the
  target**, and it is the first thing to change if the boot log shows a link at
  1000 Mb/s on a cable and switch that can do better.
* **No flow control.**  `igc_setup_link` initialises the pause-frame registers
  and `CTRL.RFCE`/`CTRL.TFCE`; this driver sets none of them, so a link that
  negotiates pause frames will not have them honoured by the MAC.
* **The receive filter beyond entry 0 is untouched.**  `igc_init_rx_addrs` writes
  `RAL(0)`/`RAH(0)` and clears the other fifteen entries; this driver writes none
  of them.  A stale filter entry the firmware left enabled stays enabled, and the
  driver depends on the NVM auto-read having armed entry 0 — which is why the
  station address's `RAH.AV` bit is printed rather than assumed.
* **No interrupts.**  Every source is masked once, during reset, and nothing
  unmasks one.  The driver polls, which is what `NetDriverOps` describes.
* **No segmentation, no checksum offload, no VLAN insertion, no timestamping.**
  A frame goes out exactly as the stack wrote it, with the MAC appending the
  CRC because the descriptor asks for it.  On receive the CRC is stripped by the
  hardware, so the descriptor's length is the frame length and the driver does
  not adjust it.
* **No RSS, no multiple queues, no multicast hash programming.**  Linux's
  `igc_setup_mrqc` programs the redirection table, the hash key and `MRQC`; with
  one queue and no hashing this driver programs none of them.  Linux's
  `igc_init_hw_base` also zeroes 128 dwords each of `MTA` and `UTA`; this driver
  does not, so the multicast tables hold whatever reset left in them.
* **The RX FIFO flush is not run.**  `igc_rx_fifo_flush_base` is a firmware/TCO
  workaround gated on `MANC.RCV_TCO_EN`, and the registers it needs are not in
  this driver's table.  If firmware had TCO receive enabled, a small number of
  packets that arrived while the MAC was being configured may reach the host.
* **The NVM is never read directly.**  The station address comes out of the
  receive-address registers after a reset, which is what `igc_read_mac_addr`
  does.  A blank-NVM part therefore fails the bring-up with a clear reason
  instead of being driven with a garbage address.
* **`MAX_JUMBO_FRAME_SIZE` is not used.**  Linux bounds receive by the jumbo
  size (`igc_main.c:3978`); this driver sets `RLPML` to its own 2 KiB buffer
  size, because a receive bound larger than the buffer a frame is written into
  is a bound the driver cannot honour.

## 6. What is unverified

Every item is a claim that depends on real silicon, or on a source this project
could not check.  None of them has been measured on the target machine, and no
register value in any report has ever come from a real i225 or i226.

1. **That the target machine has an i225/i226 at all.**  The assumption this
   workstream rests on, restated here so that a reader never has to infer it.
2. **Every register offset, bit and field.**  Taken from Linux v6.12 `igc`, not
   from a datasheet.  Where Linux and the datasheet disagree — and we cannot
   know where that is — this driver follows Linux.
3. **That BAR0 is at least 64 KiB, is assigned, and lies in a range the kernel
   maps.**  The driver refuses an unassigned or too-small BAR, but a BAR outside
   the platform profile's declared device ranges would fault on the first read
   rather than report anything.
4. **That a read of an undecoded address returns all ones**, which is the
   inference behind the "the aperture did not answer" verdict.  It is what x86
   does; this project has not measured it on this machine.
5. **That the reset sequence works.**  `CTRL.RST` self-clearing,
   `EECD.AUTO_RD` as the completion signal, and the GIO-master handshake are all
   facts about the vendor driver, not measurements of this part.
6. **That the PHY answers at `MDIC` address 0 within 1920 polls of 50 µs**
   (96 ms), and that MII register 1 bit 2 means link on this part.  Linux never
   assigns `hw->phy.addr`, which is why 0 is used here; if the poll times out on
   the target, that assumption is the first suspect.
7. **That link comes up within three seconds**, and that the firmware's
   advertisement is good enough to negotiate 2.5 Gb/s.
8. **That the NVM auto-read leaves a valid unicast address in
   `RAL(0)`/`RAH(0)`**, and that `RAH.AV` is set.
9. **That the descriptor formats are what this hardware reads.**  They are
   `igc_base.h`'s unions, byte for byte, and the host tests prove the driver
   writes what those unions describe — not that the device agrees.
10. **The `RDT` convention**: that the hardware owns `[head, tail)` and that the
    tail is one past the last armed descriptor.  Inferred from
    `igc_alloc_rx_buffers`, and the most load-bearing inference in the driver.
11. **That `RCTL.SECRC` makes the reported length exclude the CRC**, so that the
    driver must not subtract four.
12. **That the platform's DMA memory is reachable by the device.**  The HAL
    allocates pages from the global allocator and uses the direct map's physical
    address, exactly as the virtio HAL does, which assumes no IOMMU and
    identity-mapped bus addresses.
13. **Throughput and behaviour under load.**  Nothing here has moved a real
    frame, so ring sizes, polling latency and error handling under traffic are
    untested by construction.
14. **Any value in any report.**  A report's *decoding* is tested; the values it
    decodes have never been read from this part.

## 7. Where it plugs in

* **Build.**  `python3 tools/thekernel.py build --net-igc …` (or
  `test/run --net-igc`) adds the `net-igc` kernel feature, which selects the
  driver as the one static network device.  Without the flag nothing about this
  driver is compiled: the product's network device stays `virtio-net`, which is
  what the QEMU suites need.
* **Stack.**  `AxNetDevice` becomes `IgcNic<IgcHalImpl, 256>`, which implements
  `NetDriverOps`, so `axnet-ng` uses it exactly as it uses `virtio-net` —
  including the loopback-only fallback when no NIC is found.
* **Target machine.**  Build with `--platform n305 --net-igc`, write the ESP to
  a stick, and read the screen as [`n305-bringup.md`](n305-bringup.md) §2
  describes.  The `igc:` lines to look for, and what each means:

| line | meaning |
|---|---|
| `igc: candidate …: 8086:xxxx … is not a device id this driver binds` | the machine has an Intel NIC that is not an i225/i226: the assumption is refuted, and this is the part to write a driver for |
| `igc: verdict: no supported device present` with no candidate above it | nothing Intel-network-shaped was found at all |
| `igc: … identified as IGC_DEV_ID_… and confirmed by 5 identification registers` | the device is there and its registers answered |
| `igc: bring-up …: station address … (RAH.AV 1)` | the reset and the NVM auto-read worked and the address is usable |
| `igc: bring-up … failed: the PHY reported no link in 300 polls` | the PHY did not answer link; check the cable first, then `IGC_MDIC`'s PHY address |
| `igc: … 256 descriptors in each ring … the interface is ready` | the rings are up; the interface exists |

## 8. Where the sources disagreed

Recorded rather than resolved, because choosing one silently is how a driver
ends up binding a part it does not understand.

1. **Linux's own i225/i226 predicates do not cover Linux's own device table.**
   `igc_pci_tbl` (`igc_main.c:49-68`) has 16 entries;
   `igc_is_device_id_i225` names 7 and `igc_is_device_id_i226` names 4
   (`igc_base.c:397-424`).  Five bound ids — `0x15f7`, `0x5503`, `0x125e`,
   `0x125f`, `0x15fd` — are in neither, so this driver's family column says
   `unclassified` for a part whose marketing name says otherwise.  Nothing in
   the driver depends on the family.
2. **`0x3101` has two names.**  Linux calls it `IGC_DEV_ID_I225_K2`; this host's
   `pci.ids` calls it `Killer E3100X 2.5 Gigabit Ethernet Controller`.  Both are
   kept in the table.
3. **Six bound ids are absent from this host's `pci.ids`** (`0x15f7`, `0x15f8`,
   `0x15fd`, `0x125e`, `0x125f`, `0x3100`).  A gap in the local database, not a
   disagreement, and recorded as such.
4. **`igc_regs.h` files a PHY register among the aperture registers.**
   `IGC_GPHY_VERSION` (`igc_regs.h:17`) sits in the "General Register
   Descriptions" block, but its offset is not dword aligned and
   `igc_read_phy_fw_version` reads it over `MDIC`.  The compile-time alignment
   assertion in `regs.rs` is what caught it, and the register is not in this
   driver's table.
5. **The vendor driver's queue thresholds do not fit the fields a reader would
   infer.**  `igc_configure_rx_ring` ORs `IGC_RX_PTHRESH` (8) into the
   prefetch-threshold field and `IGC_RX_HTHRESH` (8) into the host-threshold
   field; eight does not fit in three bits.  A first version of this driver's
   masks did, and produced `0x02040000` where the vendor driver produces
   `0x02040808`.  The tests caught it; the masks are now the narrowest that pass
   the vendor's constants through unchanged, and both words are pinned.
6. **Fields the vendor driver never assigns.**  `hw->phy.addr` (the `MDIC` PHY
   address) and `hw->mac.mc_filter_type` (the `RCTL` multicast-offset field) are
   only ever read in the `igc` driver, and the `igc_hw` structure is zeroed when
   the adapter is allocated — so the vendor driver uses zero for both.  This
   driver does the same and says so where it matters.

## 9. Sources

* Linux v6.12, tag `v6.12`, commit
  `adc218676eef25575469234709c2d87185ca223a`,
  `drivers/net/ethernet/intel/igc/`: `igc_regs.h`, `igc_defines.h`,
  `igc_base.h`, `igc.h`, `igc_main.c`, `igc_base.c`, `igc_mac.c`, `igc_phy.c`,
  `igc_nvm.c`, `igc_i225.c`, `igc_hw.h`, `igc_dump.c`.  Facts only, cited by
  symbol and line.
* This host's `pci.ids` (`hwdata`): vendor `8086` entries `0d9f`, `125b`,
  `125c`, `125d`, `15f2`, `15f3`, `3101`, `3102`, `5502`, `5503`.
* QEMU 10.2.2 `qemu-system-x86_64 -device help`, for the absence of an
  i225/i226 model.
