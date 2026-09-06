//! Read-only graphics resource accounting.
//!
//! This intentionally resembles a small DRM debugfs leaf rather than a new
//! device ABI.  Every read is a bounded snapshot under existing short locks;
//! it never services transport completions, retries retirement, or mutates
//! evdev bookkeeping.

use alloc::{format, string::String, sync::Arc};

use axfs_ng_vfs::{Filesystem, VfsResult};

use crate::{
    drm::{fence_metrics, primary_device},
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
    SimpleDir::new_maker(fs, Arc::new(root))
}

fn snapshot_json() -> String {
    #[cfg(feature = "input")]
    let (input_devices, input_clients) = super::dev::event::input_metrics();
    #[cfg(not(feature = "input"))]
    let (input_devices, input_clients) = (0, 0);
    let (fences_pending, fences_error) = fence_metrics();
    let Some(device) = primary_device() else {
        return format!(
            "{{\"schema\":1,\"gpu_present\":0,\"fences_pending\":{fences_pending},\"fences_error\"\
             :{fences_error},\"input_devices\":{input_devices},\"input_clients\":\
             {input_clients}}}\n"
        );
    };
    let metric = device.metrics();
    let adapter = metric.adapter;
    format!(
        concat!(
            "{{\"schema\":1,\"gpu_present\":1,",
            "\"drm_open_ofds\":{},\"gem_handles\":{},\"gem_handle_bytes\":{},\"resource_blobs\":\
             {},\"resource_blob_bytes\":{},",
            "\"framebuffers\":{},\"property_blobs\":{},\"property_blob_bytes\":{},",
            "\"render_contexts\":{},\"pending_atomic_commits\":{},\"pending_vblank_events\":{},",
            "\"atomic_commits\":{},\"vblanks\":{},\"fences_pending\":{},\"fences_error\":{},",
            "\"resources\":{},\"retired_2d_resources\":{},\"retired_render_resources\":{},",
            "\"render_jobs\":{},\"render_pending\":{},\"present_jobs\":{},\"cursor_jobs\":{},",
            "\"final_2d_leaks\":{},\"final_render_leaks\":{},",
            "\"input_devices\":{},\"input_clients\":{}}}\n"
        ),
        metric.open_ofds,
        metric.gem_handles,
        metric.gem_handle_bytes,
        metric.resource_blobs,
        metric.resource_blob_bytes,
        metric.framebuffers,
        metric.property_blobs,
        metric.property_blob_bytes,
        metric.render_contexts,
        metric.pending_atomic_commits,
        metric.pending_vblank_events,
        metric.atomic_commits,
        metric.vblanks,
        fences_pending,
        fences_error,
        adapter.resources,
        adapter.retired_2d,
        adapter.retired_render,
        adapter.render_jobs,
        adapter.render_pending,
        adapter.present_jobs,
        adapter.cursor_jobs,
        adapter.final_2d_leaks,
        adapter.final_render_leaks,
        input_devices,
        input_clients,
    )
}
