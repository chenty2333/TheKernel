# tk-intel-display

MIT, `no_std`, allocation-free byte parsing and explicit injected MMIO.
Source authority is Linux 7.2.3 `drivers/gpu/drm/i915/display/`; see NOTICE
for **actually translated** functions and LICENSE-MIT for the full grant.

This first slice contains ADL-P/N identity/stepping and BDB 216–264 board
routing/HDMI capabilities plus OpRegion VBT discovery. It is not a complete
i915 port, hardware modeset driver, or firmware-ownership proof. No register
backend or framebuffer mapping is implicit; parsing cannot touch hardware.

Intentional safety divergences: validate each accessed section boundary and header/signature
sizes; reject incomplete child tails; duplicate-port lookup errors rather than
silently choosing the first; cap external VBT to 64 KiB and reject overlapping
2.1+ RVDA or address overflow. Checksum is observable but, matching i915, not
mandatory for parsing. Unsupported semantic BDB versions return an error;
short children use upstream zero-extension but report unexpected record size.
Unknown stepping preserves upstream next/future decoding but is marked inexact.

Private BIOS/EDID bytes are not licensed for redistribution by their presence
in a capture. The external test reads them directly, does not bundle them:

```
THEKERNEL_N305_CAPTURE=/path/to/n305-20261003T235530Z \
  cargo test -p tk-intel-display --target x86_64-unknown-linux-gnu \
  --test n305_capture -- --ignored --nocapture
```

Synthetic malformed tables and board-routing regressions run in the normal
host suite. Actual captured tests are separate and never silently count a
missing capture as a pass. 未在硬件上验证.
