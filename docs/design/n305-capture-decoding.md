# N305 capture corrections (2026-10-04)

The same POSIX sh/od/awk helper is installed beside the payload in both the
PXE overlay and USB capture image. It is read-only; no host sudo, service or
network configuration is used for its tests.

## Corrected data handling

- MCFG addresses are composed from eight separately formatted bytes, **never**
  converted through awk signed-integer printf or a floating-point u64. This
  preserves bases above 2 GiB and above the double-precision integer range.
  Signature, declared/actual length, complete allocations and bus order are
  checked. Invalid input returns failure, not a fabricated address.
- FACS is explicitly `n/a`: it has no checksum field. Actual SDT checksum
  errors still print BAD; the exception does not mute DSDT/MCFG failures.
- A retained kernel log with no ECAM text is not grep failure. The collector
  falls back to an **explicitly labelled kernel /proc/iomem ECAM window**;
  if neither exists it records UNAVAILABLE. Real read/decoder failures remain
  FAIL. MMCONFIG text is admitted as well as ECAM/MCFG.
- HDA discovery loads the live Linux `snd_hda_intel` module (only on the DUT)
  and copies `/proc/asound/cards`, `pcm`, and every `card*/codec#*` report.
  Discovery count is not a count of successful copies: per-file capture status
  is authoritative. No report means the codec model remains unknown.
- FADT flags/PWR_BUTTON, SCI and legacy PM1 event/control addresses are decoded;
  actual `/sys/bus/acpi/devices/*/hid == PNP0C0C` devices are enumerated with
  path/status/modalias. No ASCII-only AML search is used to infer absence of
  an EISA-encoded device ID.

## Replayed against last night's bundle

The original raw table files were root-only. No permissions or original files
were changed. The same bundle's readable `acpi/acpidump.txt` carries complete
hex bytes; MCFG/FACP/FACS were reconstructed in temporary disk fixtures,
checked for continuous offsets, decoded by the shipped helper, and discarded.

Observed results:

- MCFG: **0xc0000000**, segment 0, buses 00–ff (60-byte table).
- FACS: **n/a, no checksum field** (64 bytes), not BAD.
- FADT: **flags 0x0003c6e5, PWR_BUTTON=0, fixed PM1 button, SCI 9**;
  PM1a event block 0x1800, control block 0x1804, PM1b control 0.
- Old `logs/dmesg.txt` retained no ECAM line; old `proc/iomem.txt` supplied
  `c0000000-cfffffff : PCI ECAM 0000 [bus 00-ff]`. The corrected fallback
  reports that source instead of marking grep's no-match as a probe failure.

No codec or PNP0C0C enumeration result is invented for the old bundle. The
new live collectors are **未在硬件上验证**; a fresh capture is still needed
for the independent HD Audio workstream's codec model.

## Tests

Eight host tests exercise high u64/string formatting, malformed lengths and
bus ranges, FACS versus bad SDTs, high-bit FADT flags/fixed/control-method
classification, ECAM log/iomem/unavailable/read-error behavior, multiple codec
cards, absent versus empty ACPI device directories, and both helper deployments.
The decoder tests also run against the existing Alpine **BusyBox 1.37.0-r31**
package and its matching musl, isolated in temporary disk files (no installation).
Syntax checks include the actual BusyBox ash payload parser. These do not test
Linux HDA module loading or real codec acquisition on the N305.
