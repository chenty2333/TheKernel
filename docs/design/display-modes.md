# Display modes: EDID to a programmable timing

**Component:** `kernel/src/drm/modes/**` (branch `feat/display-modes`).
**Scope:** the hardware-free half of a modeset - turning what a monitor says
about itself into a timing a display engine can be programmed with.

Two rules shaped everything below.  The target machine's only output channel is
its screen, so a malformed EDID must produce a typed error and never a panic,
an out-of-bounds read or an allocation blow-up.  And a wrong pixel clock is a
blank screen, so every timing carries where its numbers came from.

---

## 1. What is implemented

| Area | Where |
|---|---|
| EDID base block and extension blocks, CTA-861 decoding | `edid.rs` |
| Timing representation (clock, edges, polarities, flags, provenance) | `mode.rs` |
| VESA DMT 1.13 table (85 rows) | `dmt.rs` |
| CTA-861 video identification codes (VIC 1..=127) | `vic.rs` |
| VESA CVT, including reduced blanking, in integer arithmetic | `cvt.rs` |
| Enumeration and selection policy, one-call driver interface | `select.rs`, `mod.rs` |

Nothing in the tree allocates: the parser borrows the caller's buffer, the mode
list is a fixed array of `MAX_MODES = 64` entries, and every loop is bounded by
the block structure rather than by a length read out of the data.  Enumeration
runs during display bring-up, before the heap is known to be usable.

## 2. The interface a display driver calls

```rust
use thekernel_kernel::drm::modes::{Constraints, ModePlan, plan_modeset};

// `edid_bytes` is whatever the driver read: 128 bytes, or 128 * (1 + n).
let plan: ModePlan = plan_modeset(edid_bytes, &Constraints::unlimited());
let mode = plan.selection.mode;   // program this
```

`plan_modeset` never fails.  It parses strictly first, retries leniently if the
strict parse failed, and otherwise returns the built-in fallback timing; in
every case it writes the decision to the kernel log through `log_plan`.

`Plan` fields a driver acts on:

| Field | Meaning |
|---|---|
| `selection.mode` | the timing to program: `clock_khz`, `hdisplay`, `hsync_start`, `hsync_end`, `htotal`, `vdisplay`, `vsync_start`, `vsync_end`, `vtotal`, `hsync_positive`, `vsync_positive`, `flags` (`INTERLACE`, `DOUBLE_CLOCK`), `source` |
| `selection.reason` | why this one: `SinkPreferred`, `FirmwareModeRetained`, `EstablishedTiming`, `StandardTiming`, `DetailedTiming`, `CtaVic`, `BuiltinFallback { because }` |
| `selection.range_limits_relaxed` | true when the sink's own range limits had to be set aside |
| `warnings` | what the EDID needed to skip (`EdidWarnings`) |
| `edid_error` / `strict` | the error a lenient recovery absorbed, and whether the parse was strict |
| `report` | enumeration counts, including `overflowed` and the number of advertised timings with no table row |

`Mode::refresh_millihz()` is derived from the programmed timing, not stored; it
doubles for interlaced modes so it reports the field rate, which is the number
sinks and CTA-861 use to name a 1080i format.

Drivers that want to decide for themselves use the pieces:

```rust
let edid = Edid::parse(edid_bytes)?;          // or Edid::parse_lossy
let mut modes = ModeList::new();
let report = collect_modes(&edid, &mut modes); // every advertised timing, tagged with its class
let choice = select(&edid, &modes, &constraints);
```

`Constraints` carries the link's ceiling (`max_clock_khz`, `max_hdisplay`,
`max_vdisplay`), a refresh floor, whether interlaced modes are allowed (off by
default), how to treat the sink's range limits (`RangeCheck::Ignore/Prefer/
Require`, default `Prefer`) and the mode the firmware already programmed.

## 3. EDID parsing

Parsed from the base block: header, checksum, manufacturer PNP id, product
code, serial number, week/year, version and revision, the video input
definition (digital bit depth and interface, or the analog level and sync
flags), screen size and aspect-ratio encoding, gamma, the feature byte
(DPMS bits, sRGB, native/preferred, continuous frequency, YCbCr 4:4:4/4:2:2),
all eight chromaticity coordinates, established timing bitmaps I/II, the eight
standard timing codes, and all four 18-byte descriptors in every form the
standard defines:

| Sub-tag | Descriptor |
|---|---|
| - | detailed timing descriptor |
| `0xFF` / `0xFE` / `0xFC` | monitor serial, unspecified text, monitor name |
| `0xFD` | display range limits (GTF, bare, secondary GTF, CVT) |
| `0xFA` / `0xF7` / `0xF8` / `0xFB` | additional standard timing ids, established timings III, CVT three-byte codes, colour point |
| `0x10` | dummy |
| anything else (e.g. `0xF9`) | `Descriptor::Undecoded { sub_tag }`, bytes dropped rather than guessed at |

Interlaced detailed timings written in field lines for a frame CTA-861 defines
(1080i, 480i, 576i, and their pixel-repeated widths) are converted to frame
lines, with the odd frame total the standard publishes (1080i = 1125 lines).

Extension blocks: every block's checksum is validated.  Byte 0 is the tag;
`0x02` selects CTA-861, whose revision must be 3 or later for the data block
collection to be decoded.  A CTA block is checked for a well-formed data block
collection and programmable detailed timings.  Anything else - DisplayID,
block maps, vendor blocks, older CTA revisions - is checksum-validated and then
exposed as `Extension::Unknown`; its contents are never half-interpreted and
never fail the parse.

Error policy, deliberately two-sided:

* `Edid::parse` is strict and is what the driver tries first.  A bad header, a
  bad checksum anywhere, a missing extension block the base block declared, an
  unsupported major version, an unprogrammable detailed timing or a malformed
  CTA collection is a typed `EdidError`.
* `Edid::parse_lossy` accepts the same buffer as long as the base block itself
  is intact, and counts what it skipped in `EdidWarnings`
  (`bad_extension_checksums`, `missing_extension_blocks`,
  `invalid_descriptors`, `malformed_cta_blocks`).  A decision made from a
  damaged EDID is therefore visible in the boot log, never implicit.

## 4. Timing tables and their provenance

The tables are re-expressed from facts published by the standards.  Two
independent machine-generated transcriptions were compared entry by entry
before anything was committed: libdisplay-info's `dmt-table.c` /
`cta-vic-table.c` (MIT, generated from `VESA-DMT-1.13.pdf` and
`ANSI-CTA-861-I Errata FINAL.pdf`) and Linux's `drm_dmt_modes[]` /
`edid_cea_modes_1[]`.  All 85 DMT rows and all 127 VIC rows agree; the
comparison is reproducible from those two sources and the rules below.

### DMT (`dmt.rs`)

* 85 rows, codes `0x01`..`0x56`.  Each row keeps its DMT code, the 2-byte EDID
  standard timing identifier (`0x0000` when the timing cannot be expressed in
  that format) and the 3-byte CVT identifier (`0x000000` when the timing is not
  CVT-derived).
* The DMT table lists a horizontal/vertical **border** separately from the
  blanking.  A border sits between the active area and the front porch, so this
  table folds it in: `blank = blank + 2 * border`, `front porch = front porch +
  border`, which reproduces the classic totals (640x480@60 becomes 800x525 and
  sync at 656/752, 490/492).
* DMT 0x0F (1024x768 at 43 Hz, the IBM 8514/A timing) is omitted: it is the
  standard's only interlaced row and the transcription carries no interlace
  flag, so the entry cannot be reproduced without guessing.  No digital panel
  advertises it.
* Sync polarity comes from the DMT standard's polarity column (transcribed by
  the same cross-check).  The standard does not treat polarity as optional:
  DMT 1920x1080@60 is negative sync, while CTA-861 VIC 16 is positive, and
  both are correct for their own source.
* DMT prints rounded refresh labels (its 640x480 row says 60 Hz for a timing
  that computes to 59.94 Hz), so lookups match the label within one hertz.

### CTA-861 VICs (`vic.rs`)

* VIC 1..=127 only: a short video descriptor carries a 7-bit code, so nothing
  above 127 can reach a sink through an EDID.
* Pixel-repeated formats are listed the way CTA-861 lists them: at the repeated
  width with the multiplied pixel clock (VIC 21 is 1440x576 at 27.000 MHz).
  The kernel's cross-check table stores the same formats at half the clock with
  a `DBLCLK` flag; the comparison halves this table's clock and horizontal
  edges before comparing.
* Interlaced formats carry **frame** line totals, which the standard publishes
  and which are not always the sum of the field blanking: 1080i is 1125 lines
  and 576i is 625, because the field blanking is truncated to whole lines.
* The 200 Hz and 240 Hz SD formats (VIC 52..59) genuinely are pixel-repeated
  formats at those field rates; the nominal refresh is what the timing
  computes, not a transcription error.

### CVT (`cvt.rs`)

* Integer arithmetic only, in units where the formulas are exact:
  picoseconds for periods, milli-percent for the ideal duty cycle, kilohertz
  for the clock.  No floating point in the kernel, and the results are
  bit-for-bit reproducible on any host.
* Variants: standard blanking, reduced blanking v1 (160 pixel blank, 3 line
  front porch), reduced blanking v2 (80 pixel blank, 8 line sync, 1 kHz clock
  granularity).  Clock granularity is 0.25 MHz for standard and RBv1, 1 kHz
  for RBv2.
* Validation: the test `cvt::tests::reproduces_every_cvt_derived_dmt_row`
  regenerates all 28 rows of the DMT table that the DMT standard itself
  produced with CVT, and compares the clock and all six edges exactly.  That
  pins the duty cycle, the granularity and the clock rounding at once.
* `generate` returns `None` rather than a bad timing when a request has no
  representable answer (zero rate or size, totals that do not fit 16 bits, a
  rate whose frame period is shorter than the minimum vertical blanking).
* CVT requires 8-pixel horizontal granularity for standard and RBv1 blanking,
  so a request for an odd width (1366, for example) generates the nearest
  multiple of 8.  RBv2 has 1-pixel granularity.  The generated mode reports the
  width it actually uses.

### Established timings and the fallback

* The established-timing I/II bitmaps name DMT rows by resolution and refresh;
  `select.rs` resolves each set bit through the DMT table so the blanking is
  the standard's.  Six of the seventeen bits name legacy modes DMT 1.13 does
  not define (720x400 at 70 and 88 Hz, 640x480 at 67 Hz, 832x624 at 75 Hz,
  1024x768 at 87 Hz interlaced, 1152x870 at 75 Hz); those are counted as
  skipped rather than approximated.
* Established timings III is a bitmap of 44 DMT codes and is resolved by code.
* The built-in fallback is DMT 0x04, 640x480 at 60 Hz.  It is the timing every
  VGA-compatible sink accepts, and it is also CTA-861 VIC 1, which every HDMI
  sink must accept.  It is a "get something on the panel and log it" timing,
  not a guess at a native mode.

Every table entry is checked by tests that can fail: the tables' own integrity
(edges ordered, totals consistent, computed refresh within the label the
standard prints), exact values for well-known rows (640x480@60 = 25.175 MHz /
800x525, 1920x1080@60 = 148.500 MHz / 2200x1125, 1920x1200 RB = 154.000 MHz /
2080x1235, 1280x720 = 74.250 MHz / 1650x750, CVT-RB 1080p60 = 138.50 MHz /
2080x1111, CVT-RBv2 1080p60 = 133.32 MHz / 2000x1111).

## 5. Selection policy

Stated once, in `select.rs`'s module documentation and implemented there:

1. the sink's **preferred timing** - the first detailed timing descriptor of
   the base block, when the feature byte designates one (EDID 1.4 always does);
2. the **firmware mode**, when the caller passes the mode the firmware already
   programmed and the sink advertises the same timing: an already-validated
   timing is safer than a fresh one during bring-up;
3. **established timings**;
4. **standard timings**, resolved through the DMT standard timing identifiers
   first - E-EDID 1.4 Appendix B requires that timing's exact blanking - and
   generated with CVT only when DMT has no row (choosing reduced blanking when
   the sink's CVT range descriptor says so, and by default when it says
   nothing);
5. other **detailed timing descriptors** (base block, then CTA-861);
6. **CTA-861 VICs**;
7. the **built-in fallback**, always logged as a warning with the reason.

Within one step the better mode wins: larger active area, then higher refresh
rate, then lower pixel clock, then lower horizontal total.  The comparison is
total, so the same EDID and the same constraints always produce the same mode.

The sink's own display range limits are *preferred, not enforced*: a mode
inside them wins, but when nothing inside them can be programmed the limits are
set aside rather than falling back to 640x480, and `Selection::range_limits_relaxed`
says so.  `RangeCheck::Require` makes them binding, and `RangeCheck::Ignore`
skips the check entirely.

## 6. Deliberately not implemented

| Not implemented | Why, and what happens instead |
|---|---|
| DisplayID and DisplayID-2 extensions | Checksum-validated, exposed as `Extension::Unknown`, ignored for mode enumeration.  CTA-861 is how the target's HDMI sink advertises modes. |
| CTA-861-H extended VICs (193..219) and the HDMI Forum vendor block | Unreachable through a short video descriptor (7-bit code); logged as unknown VIC. |
| YCbCr 4:2:0 video data blocks and capability maps | Not decoded; the corresponding SVDs are still enumerated as their RGB timings.  Pixel-format choice is the encoder's business, not part of a timing. |
| Audio, speaker allocation, HDR, colourimetry data blocks | Parsed as opaque data blocks with their tag; the driver picks formats from elsewhere. |
| CVT reduced blanking v3 and the RBv2 "video-optimized" 1000/1001 multiplier | RBv1 and RBv2 cover the panels this targets; a request for v3 is not expressible through `CvtBlanking`. |
| Secondary-GTF (and default-GTF) timing generation | The secondary GTF parameters are parsed and exposed, but no mode is generated from them.  CVT is preferred by every sink this targets; standard timing descriptors resolve through DMT first in any case. |
| Interlaced CVT generation | `cvt::generate` is progressive only; the interlaced formats come from the CTA-861 table, which defines them exactly. |
| DMT 0x0F (1024x768 at 43 Hz interlaced) | See above: the source cannot represent it without guessing. |
| The six legacy established-timing bits with no DMT row | Counted in `CollectReport::skipped_without_a_table_row`; a sink that wants one carries a detailed timing. |
| EDID 2.0 / DisplayID base blocks | Rejected as `EdidError::UnsupportedVersion` (major version is not 1). |

## 7. Verification

* `tools/thekernel.py test --suite host` runs the unit tests: parser cases built
  field by field from named fields (`fixtures.rs`), table integrity and exact
  value tests, CVT reference timings and the 28 DMT CVT rows, selection policy
  cases, and a 4000-case structured-random corpus that must never panic and
  must leave every accepted mode well formed.
* `tools/thekernel.py lint --smp 4 --memory 512M` runs clippy for the product
  configuration.
* The tables' fidelity rests on the entry-by-entry cross-check described in
  section 4; no real monitor's EDID was available to this work, and the guest
  suite boots QEMU rather than the target machine, so the end-to-end path
  (DDC read, modeset, lit panel) is not verified here - it is the display
  driver's half of stage 2.
