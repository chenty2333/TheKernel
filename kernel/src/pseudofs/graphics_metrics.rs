//! Read-only graphics resource accounting.
//!
//! This intentionally resembles a small DRM debugfs leaf rather than a new
//! device ABI.  Every read is a bounded snapshot under existing short locks;
//! it never services transport completions, retries retirement, or mutates
//! evdev bookkeeping.

use alloc::{format, string::String, sync::Arc};

use axfs_ng_vfs::{Filesystem, VfsResult};

use crate::{
    pseudofs::{DirMapping, SimpleDir, SimpleFile, SimpleFs},
};

pub(crate) fn new_graphics_metricsfs() -> Filesystem {
    SimpleFs::new_with("drm-metrics".into(), 0x6472_6d74, builder)
}

fn builder(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut root = DirMapping::new();
    root.add(
        "thekernel_metrics",
        SimpleFile::new_regular(fs.clone(), || -> VfsResult<String> { Ok(snapshot_json()) }),
    );
    // The Intel display probe's report.  It is a text file rather than part of
    // the metrics snapshot above because it is read by a person looking at a
    // machine with no serial port, not by a collector.
    root.add(
        "intel_gpu",
        SimpleFile::new_regular(fs.clone(), crate::drm::intel::debugfs::report),
    );
    SimpleDir::new_maker(fs, Arc::new(root))
}

/// The metrics that still have an upstream source: the input devices. The
/// legacy core's GEM, framebuffer, atomic, fence and render counters had no
/// upstream equivalent and are not reported.
fn snapshot_json() -> String {
    #[cfg(feature = "input")]
    let (input_devices, input_clients) = super::dev::event::input_metrics();
    #[cfg(not(feature = "input"))]
    let (input_devices, input_clients) = (0, 0);
    format!(
        "{{\"schema\":2,\"input_devices\":{input_devices},\"input_clients\":{input_clients}}}\n"
    )
}
