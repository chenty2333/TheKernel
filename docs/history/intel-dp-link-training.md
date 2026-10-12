# Intel DisplayPort link training

`crates/ax/tk-intel-display/src/intel_dp_link_training_full.rs` translates
66/78 functions from Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_dp_link_training.c` (MIT, Copyright
© 2008-2015 Intel Corporation). It preserves the display-12/13 LTTPR/DPRX
capability and mode setup, 8b/10b clock recovery/channel equalization and
fallbacks, 128b/132b EQ/CDS, post-training adjustment, retrain/HPD work gates,
and the pure force-rate/lane parsers. Twelve omitted functions are DRM debugfs
show/write/add wrappers requiring seq_file, user-copy, DRM object, and
connection-modeset framework machinery.

The mechanism calls `LinkTrainingIo` for DPCD/AUX traffic, source PHY and DDI
signal-level programming, clocks, delays, HPD, and modeset retry work. The
display crate owns protocol sequencing and retry policy. The kernel now has
source-mapped TC1/TC2 AUX D/E transport and power refs plus checked DKL register
access, but no `LinkTrainingIo` adapter was added: the source's start routine
blocks/unblocks HPD and can queue retrain/modeset work, while the kernel has no
TC equivalents. Several source hooks return `()` or a scalar, so an adapter-side
sticky error alone cannot prevent the translated routine from continuing and
the caller from accepting a trained link. Keep DP output disabled until a
checked-run wrapper must inspect a terminal status and real TC HPD/retry hooks
exist. The MIT grant is in `crates/ax/tk-intel-display/LICENSE-MIT`.
The MIT grant is in `crates/ax/tk-intel-display/LICENSE-MIT`.
