# DRM color-management helper translation

`kernel::drm::color_mgmt_full` translates all 26 function definitions in Linux
7.2.3 `drivers/gpu/drm/drm_color_mgmt.c` (MIT-style grant, 866 Rust lines).
It carries the CTM S31.32 conversion, CRTC/plane color-property setup, gamma
LUT validation, legacy gamma ioctl and LUT/palette conversion routines. DRM
registration, atomic-state lookup/commit, usercopy, lock ownership and
register writes are explicit trait interfaces.

The module is declared and compiles in `tk-kernel`; the current KMS property
registry separately exposes gamma/degamma LUT and CTM blob IDs, immutable LUT
size values, validates blob length/reserved bits, retains references, and
stores the latest 3x3 S31.32 CTM and LUT data in device state. `GAMMA_LUT_SIZE`
is 256 and agrees with `GETCRTC.gamma_size`. However, the translated
`color_mgmt_full` helpers and color property state are **not yet consumed by
the Intel hardware commit path**; state readback does not prove that scanout
pixels were transformed. `cargo check -p tk-kernel --tests --features 'intel-hda nvme watchdog-itco bpf'`
passes. The bare-metal kernel test binary was not executed on the host.
