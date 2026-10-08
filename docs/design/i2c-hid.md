# I2C HID devices

The FreeBSD `sys/dev/iicbus/iichid.c` path is translated as a transport-independent HID-over-I2C protocol client (`tk-i2c-hid`) and a TheKernel adapter (`tk-axdriver/src/i2c_hid.rs`). ACPICA discovers `PNP0C50`/`ACPI0C50` children through `I2cSerialBusV2`, applies `_STA` presence semantics, evaluates the upstream `_DSM` UUID/function to obtain the HID descriptor register, then the adapter reads the 30-byte descriptor, resets/powers the device, and fetches the HID report descriptor. Reports use TheKernel's existing bounded HID parser and are registered with the normal input subsystem, so userspace sees evdev.

The transport implements input-register reads, HID descriptor/report descriptor reads, RESET, SET_POWER, GET_REPORT, SET_REPORT, and output reports. The adapter also exposes the upstream `I2CRDWR` transfer ioctl operation over its parent I2C bus. RESET follows the upstream SET_POWER(ON), 1 ms delay, RESET sequence, waits up to five seconds for the zero-length acknowledgement, and continues with a warning on timeout. The first evdev open powers the device on and the last close powers it off; explicit suspend/resume and Drop also power it down safely. Input packets are read into a payload buffer of `wMaxInputLength - 2` bytes, and malformed/missing input is treated as no data. GET_REPORT truncates to the caller's buffer, matching upstream behavior.

The shared report parser is reused for `hid.c` grammar and `hidbus` descriptor dispatch, including Input/Output/Feature report-size accounting, Report-ID overhead, usage locations, bounded signed/unsigned bitfield get/set, and unit-based resolution calculation. The `hidbus.rs` adapter owns ACPI/HID identity, descriptor access, report-size metadata, and report get/set/write calls; `hmt.rs` checks the required Contact ID, X/Y, Tip Switch, and Contact Count Maximum feature. For touchpads, `hmt_set_input_mode` locates Feature Input Mode and writes mode 3 plus shared Surface/Button switch defaults as hconf does. The adapter reads Contact Count Maximum and Button Type features when available, reuses a same-report Button Type value from the Contact Count Maximum read, caps slots at 32, and reports `INPUT_PROP_BUTTONPAD` when Button Type is zero. PTP integrated button 1 and external primary button 2 both map to `BTN_LEFT`, while button 3 starts at `BTN_RIGHT`. Contact IDs are mapped to persistent Type-B slots, tip/confidence state controls tracking IDs, width/height are scaled into touch-major/minor/orientation, and an empty poll releases all active contacts to avoid stuck touches. Contact Count batching defers SYN until the advertised serial/hybrid packet batch drains and limits each packet to its reported contact subset; non-touch report IDs continue through the shared generic parser. Contact Count/Confidence/Width/Height usages are not misadvertised as pressure, blob, or tracking axes outside finger collections. Finger collection X/Y, pressure, in-range, width/height, contact ID, and tip state are exposed as evdev `ABS_MT_*`/touch keys. The evdev pump drives the upstream `iichid_intr` read path and uses adaptive fast/slow (80/10 Hz) polling when no child GPIO interrupt delivery interface is available.

Not translated: FreeBSD newbus/HID bus plumbing, raw kernel ioctl ABI, device-specific quirk tables, THQA certificate feature handling, and scan-time timestamp options; unsupported quirks remain unsupported rather than guessed. GPIO IRQ resources and IRQ-context I2C delivery are not available through the current ACPI/I2C input interfaces, so the evdev pump polls and the sampling rates are currently fixed to the upstream defaults (the sysctl runtime reconfiguration is not exposed). QEMU has no DesignWare I2C/HID device, so the source path is compile/unit-test validated only.

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
