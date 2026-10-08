# I2C HID devices

The FreeBSD `sys/dev/iicbus/iichid.c` path is translated as a transport-independent HID-over-I2C protocol client (`tk-i2c-hid`) and a TheKernel adapter (`tk-axdriver/src/i2c_hid.rs`). ACPICA discovers `PNP0C50`/`ACPI0C50` children through `I2cSerialBusV2`, applies `_STA` presence semantics, evaluates the upstream `_DSM` UUID/function to obtain the HID descriptor register, then the adapter reads the 30-byte descriptor, resets/powers the device, and fetches the HID report descriptor. Reports use TheKernel's existing bounded HID parser and are registered with the normal input subsystem, so userspace sees evdev.

The transport implements input-register reads, HID descriptor/report descriptor reads, RESET, SET_POWER, GET_REPORT, SET_REPORT, and output reports. The adapter also exposes the upstream `I2CRDWR` transfer ioctl operation over its parent I2C bus. RESET follows the upstream SET_POWER(ON), 1 ms delay, RESET sequence, waits up to five seconds for the zero-length acknowledgement, and continues with a warning on timeout. The first evdev open powers the device on and the last close powers it off; explicit suspend/resume and Drop also power it down safely. Input packets are read into a payload buffer of `wMaxInputLength - 2` bytes, and malformed/missing input is treated as no data. GET_REPORT truncates to the caller's buffer, matching upstream behavior.

The shared report parser is reused for `hid.c` grammar and `hidbus` descriptor dispatch, including Input/Output/Feature report-size accounting, Report-ID overhead, usage locations, bounded signed/unsigned bitfield get/set, and unit-based resolution calculation. The `hidbus.rs` adapter owns ACPI/HID identity, descriptor access, report-size metadata, and report get/set/write calls; `hmt.rs` checks the required Contact ID, X/Y, Tip Switch, and Contact Count Maximum feature. For touchpads, `hmt_set_input_mode` locates Feature Input Mode and writes mode 3 plus shared Surface/Button switch defaults as hconf does. The adapter reads Contact Count Maximum and Button Type features when available, reuses a same-report Button Type value from the Contact Count Maximum read, caps slots at 32, and reports `INPUT_PROP_BUTTONPAD` when Button Type is zero. PTP integrated button 1 and external primary button 2 both map to `BTN_LEFT`, while button 3 starts at `BTN_RIGHT`. Contact IDs are mapped to persistent Type-B slots, tip/confidence state controls tracking IDs, width/height are scaled into touch-major/minor/orientation, and an empty poll releases all active contacts to avoid stuck touches. Contact Count batching defers SYN until the advertised serial/hybrid packet batch drains and limits each packet to its reported contact subset; non-touch report IDs continue through the shared generic parser. Contact Count/Confidence/Width/Height usages are not misadvertised as pressure, blob, or tracking axes outside finger collections. Finger collection X/Y, pressure, in-range, width/height, contact ID, and tip state are exposed as evdev `ABS_MT_*`/touch keys. The evdev pump drives the upstream `iichid_intr` read path and uses adaptive fast/slow (80/10 Hz) polling when no child GPIO interrupt delivery interface is available.

Not translated: FreeBSD newbus/HID bus plumbing, raw kernel ioctl ABI, device-specific quirk tables, THQA certificate feature handling, and scan-time timestamp options; unsupported quirks remain unsupported rather than guessed. ACPI `GpioInt` descriptors are now bounds-checked and retain controller path, source index, pin table, trigger, polarity, sharing/wake flags, debounce and pin configuration on each I2C-HID child. They are not yet converted into a live interrupt: the evdev pump uses adaptive 80/10 Hz polling; runtime sysctl reconfiguration is not exposed. QEMU has no DesignWare I2C/HID device, so the source path is compile/unit-test validated only.

The checked `GpioInt` decoder now preserves the controller namespace path,
pin-table entries, trigger (edge/level), active polarity, pull configuration,
debounce timeout, wake capability, sharing mode, and `ResourceSourceIndex`.
The remaining integration boundary is: the
ACPI namespace resolver must map that controller path to a registered GPIO
provider before any pin is requested; a raw pin number is not a global IRQ.
The GPIO provider API needs an owned `request_interrupt(controller, pins,
trigger, polarity, debounce, wake, handler)` registration plus teardown/safe
mask/unmask and an IRQ number/handle. The IRQ layer then needs shared GSI
registration with trigger/polarity and affinity semantics. Finally, the I2C-HID
handler must only acknowledge/mask and queue a bounded threaded/task-context
read, because `GET_INPUT` I2C transfers are sleeping operations and cannot run
in hard IRQ context; teardown must stop queued reads before releasing the
controller and pin. None of the ACPI GPIO child binding, GPIO-provider registry,
or generic shared-GSI request/teardown interfaces exists in the current target,
so adding a guessed pin-to-IRQ mapping would be unsafe. Existing adaptive
polling remains the fallback until those framework seams are implemented.

2026-10-09 N305 Intel GPIO follow-up: the current machine's ACPI inventory is
Lenovo 21VG rather than N305 and has no `INT34C8` node, so it cannot provide
board-specific register/resource evidence. Intel's public GPIO configuration
guidance provides select pad-lock examples, but not the complete Alder Lake-N
community/pad table or interrupt routing required to map an ACPI pin safely.
Intel identifies those register details as Alder Lake-N EDS Volume 2 (RDC
645550), access-controlled in its documentation center; the public GPIO docs
are at <https://www.intel.com/content/www/us/en/developer/articles/technical/software-security-guidance/technical-documentation/gpio-configuration-best-practices.html>.
No register writes/provider were added without that exact N305 layout and the
missing shared-GSI API. The next safe implementation boundary remains the
provider registry, ACPI memory/interrupt-resource mapping, community/pad table
from Intel's public or user-provided N305 EDS, and deferred I2C read worker.

The shared parser's per-report size helper follows `hid_report_size()` and the
maximum-size helper follows `hid_report_size_max()`: it selects the largest
report size while retaining the first nonzero Report ID as FreeBSD does. A
multi-ID regression covers the distinction.

The report-size helper mapping now covers the FreeBSD `hid_report_size()` and
`hid_report_size_max()` semantics, while report descriptor retrieval maps both
`hidbus_get_rdesc()` and its `hid_get_report_descr()` implementation. Other HID
parser code remains a bounded TheKernel-owned grammar/decoder rather than a
source-copy of FreeBSD's parser state machine.

The HID report model now retains each usage's top-level-collection index.
`hidbus_locate()` is adapted as a collection-scoped parser lookup, and `hmt`
selects its Touchpad/Touchscreen TLC before resolving Contact Count Maximum,
Button Type and Input Mode features. The Contact Count decoder is likewise
scoped to that TLC, avoiding feature/report-ID collisions in composite HID
interfaces.

The HID bus adapter now exposes the source's generic `get_report`, `set_report`,
`read`, `write`, `set_idle`, and `set_protocol` wrapper operations over the
transport-independent I2C-HID client. These keep `hidbus` policy distinct from
`iichid` wire commands while mapping the newbus dispatch to TheKernel methods.

The parser also retains nested collection usages, so `hidbus_is_collection()`
now resolves a collection within one TLC. HMT probing scans top-level collections
and pins Contact ID/Tip/X/Y and feature controls to the selected touch
collection rather than assuming a single collection or using report-global
feature locations.

Feature-report reads for Contact Count Maximum now size the transfer from the
selected report ID rather than the descriptor-wide maximum; this avoids mixing
lengths when several feature Report IDs coexist.

2026-10-09 follow-up: `kernel/src/acpi/pchgpio.rs` now contains the OpenBSD ISC provider tables and pad/interrupt logic, and ACPI enumerates HID-matched controllers from assigned `_CRS` memory and IRQ resources. The axhal/ x86 platform now exposes a fail-closed directly routable GSI installation path (only one IOAPIC at GSI base zero, no colliding MADT overrides, non-legacy IRQs); GPIO providers on unsupported topologies are not registered. I2C-HID requests each pin from its `GpioInt` resources, shares an atomic pending bit across those pins, and its ordinary read path services `GET_INPUT` outside hard IRQ context. Adaptive 80/10 Hz sampling remains as fallback. IRQ request setup is currently enabled only while the input device is open. S3 lifecycle wiring, affinity/shared-GSI arbitration beyond the supported direct route, and N305 physical validation remain open.

The generic `hid.c` transport wrappers are now explicitly represented by the
shared `hidbus` adapter for report get/set, read/write, idle and protocol
operations; interrupt start/stop/poll map to evdev open/close and the input
read-event pump. The I2C-HID report-register reader maps `hid_get_rdesc()`.
Function markers now cover 21/32 `hid.c` entry points. Remaining parser-state
helpers (`hid_clear_local`, `hid_switch_rid`, `hid_start_parse`,
`hid_end_parse`, `hid_get_byte`, `hid_get_item`), quirk registration/dispatch,
and generic `hid_ioctl` are not yet direct source translations; the shared
bounded parser remains independently implemented.

All eight `hmt.c` entry points now have a TheKernel mapping, including detach
through the owning evdev driver's RAII teardown. That function coverage does
not imply full source behavior: THQA handling, scan timestamps, and model
quirks remain unsupported, and the full `hmt_intr()` geometry/options path is
partly implemented in the generic report decoder rather than as a copied
function body.

The remaining `iichid.c` interrupt/callout entry points map to per-pin GPIO
registration/RAII release and to evdev's scheduled polling/read lifecycle;
`iichid.c` coverage is 38/39 entry points. Its one omitted callback is
`iichid_sysctl_sampling_rate_handler()`: this checkout has no per-device sysctl
control plane for input drivers, and polling currently uses the translated
80/10 Hz adaptive behavior with fixed defaults. This does not claim the source
sysctl behavior or caller runtime reconfiguration.

2026-10-09 THQA follow-up: HMT now locates Microsoft's `0xff00:0x00c5`
feature usage within the selected multitouch top-level collection and fetches
its feature report during attach (unless already fetched through the shared
Contact Count Maximum report). A composite keyboard+touch descriptor test
checks TLC scoping. This mirrors the upstream unlock side effect; hardware
acceptance remains unavailable. Scan-time `MSC_TIMESTAMP` emission and model
quirk selection remain open. The shared parser already handles HID global/local
state, push/pop, collection/report IDs, arrays, variables, and report sizes,
but does not expose FreeBSD's item-at-a-time iterator or its dynamic quirk
registry.

2026-10-09 GPIO S3 handoff: `pchgpio::save_all()` / `restore_all()` now walk
attached providers and preserve/restore each pad's two config dwords plus GPI
interrupt-enable bit. ACPI exports `prepare_s3_gpio()` and `resume_s3_gpio()` at
the corresponding entry/return boundaries. The kernel still has no ACPI S3
entry mechanism, so these lifecycle hooks are compile-checked but cannot yet be
invoked by a real suspend/resume cycle; this does not claim S3 acceptance.
