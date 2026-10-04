# Bounded general HID Input reports

Original parser and stateful decoder; no translated Linux parser/quirk routines.
Grammar facts come from the local USB HID 1.11 (6.2.2 and 8) and HUT 1.5 references
in the reference index. Usage-to-evdev assignments follow Linux 7.2.3
`drivers/hid/hid-input.c` and `include/uapi/linux/input-event-codes.h`; they are
mapping facts, not copied driver code. The existing original keyboard table is
retained and extended in `hid_usage.rs`.

All USB HID inputs now use report protocol and the **same** descriptor parser.
Boot keyboards are no longer a parallel fixed 8-byte decoder. SET_PROTOCOL(1)
is sent to boot-capable interfaces; non-boot HID already uses report protocol.
The report descriptor and actual endpoint capacity, not interface protocol,
define the packet layout and evdev capabilities/ranges.

Supported inputs:

- keyboard modifier bitmaps, NKRO variable keys and selector arrays (duplicates
  coalesce, releases are generated, ErrorRollOver does not release held keys);
- common Consumer controls, including play/pause, volume, brightness, launcher
  and browser keys, either variables or arrays;
- relative mouse, absolute pointers (including CH9329-like descriptors), buttons,
  legacy/high-resolution vertical and horizontal wheel events;
- joystick/gamepad axes, appropriate BTN_JOYSTICK/BTN_GAMEPAD/TRIGGER_HAPPY
  button codes, and hat-switch x/y with null-state centering;
- all nonzero report IDs through 255, separate bit layouts and per-ID key
  ownership, global Push/Pop and nested collections.

Unknown short tags/usages/pages are skipped; constant and unmapped Input bits
still advance layout. Long items are opaque, **length-checked and skipped**.
Output/Feature items do not move the Input cursor. Alternate delimiter sets,
unsupported/dynamic layouts and inconsistent mixed ID/no-ID Input reports are
refused rather than guessed. There is no device-quirk database, digitizer VM,
LED Output support or gamepad force feedback. Unknown Consumer usages do not
synthesize KEY_UNKNOWN. Arrays describe selections/keys, not numeric axes.

Bounds: descriptor 4096 bytes, report including ID 64 bytes, scalar 32 bits,
256 fields, 1024 local usages, 4096 retained array usage entries, global stack 8,
collection stack 16 and 1024 retained key owners. Malformed lengths, underflow,
range expansion and arithmetic overflow return errors; parser allocations use
fallible reservation. Truncated/unknown-ID/rollover packets leave prior state
unchanged. Capabilities are derived from the exact retained fields, not guessed
from keyboard/mouse interface labels. Relative deltas preserve sign; absolute
values clamp to logical ranges. Debug input tracing remains sourced as USB,
not a serial byte path.

## Verification boundary

Host tests cover keyboard arrays/modifiers/rollover/duplicates/releases, NKRO,
opaque long items, Consumer arrays with separate IDs, signed relative motion,
absolute coordinates, report ID 255, gamepad/hat/buttons, Push/Pop, unknown-page
and Output alignment, wheel compatibility, truncated inputs and deterministic
malformed descriptor/report cases. CH9329, Consumer controllers and gamepads
have **not been verified on N305 hardware**.

`python3 scripts/ci/usb-hid-qemu-smoke.py` uses the product runner, qemu-xhci plus
usb-kbd/mouse/tablet and marker-gated QMP injection. Its guest helper requires
BUS_USB capabilities, an actual KEY_A press, REL_X=17 and ABS_X=16384, then exits
normally and powers off. Successful device enumeration alone cannot pass.
