# DRM color-management helper translation

`kernel::drm::color_mgmt_full` translates all 26 function definitions in Linux
7.2.3 `drivers/gpu/drm/drm_color_mgmt.c` (MIT-style grant, 866 Rust lines).
It carries the CTM S31.32 conversion, CRTC/plane color-property setup, gamma
LUT validation, legacy gamma ioctl and LUT/palette conversion routines. DRM
registration, atomic-state lookup/commit, usercopy, lock ownership and
register writes are explicit trait interfaces.

The module is declared and compiles in `tk-kernel`; the current KMS atomic
property registry and Intel hardware commit path have not yet been replaced by
these translated helpers. `cargo check -p tk-kernel --tests --features 'intel-hda nvme watchdog-itco bpf'`
passes. The bare-metal kernel test binary was not executed on the host.
