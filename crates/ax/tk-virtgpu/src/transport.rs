// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors.
//! DRM-facing owner for the existing asynchronous display transport.
//!
//! This layer deliberately does not know about virtqueues, descriptors, DMA
//! mappings or protocol request structures.  Those are owned by the
//! `DisplayDriverOps` implementation below it.  It only assigns DRM fence
//! identities and does not make resource IDs usable until the lower layer has
//! delivered a successful terminal completion.

use alloc::vec::Vec;

use axdriver_display::{
    DevError, DevResult, DisplayDriverOps, GpuBatch, GpuCompletion, GpuCompletionData, GpuFeatures,
    GpuQueue, GpuResetResult, RenderCapsetInfo,
};

const COMPLETION_BATCH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FenceStatus {
    Pending,
    Complete,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FenceSnapshot {
    pub fence_id: u64,
    pub queue: GpuQueue,
    pub status: FenceStatus,
    /// A newly-created host resource is published only after successful
    /// completion.  This is `None` for ordinary submissions and failures.
    pub resource_id: Option<u32>,
    /// A host context identity is published under the same terminal rule as a
    /// resource identity.
    pub context_id: Option<u32>,
}

struct FenceRecord {
    fence_id: u64,
    queue: GpuQueue,
    status: FenceStatus,
    resource_id: Option<u32>,
    context_id: Option<u32>,
    completion_data: Option<GpuCompletionData>,
}

/// Per-device fence/resource ownership for `drm_submit`.
///
/// The caller serializes access (normally with the device's DRM mutex).  This
/// matters because `DisplayDriverOps` intentionally exposes a mutable
/// transport; it must not be called concurrently through aliases.
pub struct GpuEngine {
    features: GpuFeatures,
    host_visible_len: Option<u64>,
    next_fence_id: u64,
    records: Vec<FenceRecord>,
    capsets: Vec<(u32, RenderCapsetInfo)>,
}

impl GpuEngine {
    pub fn new(display: &mut dyn DisplayDriverOps) -> DevResult<Self> {
        if !display.supports_drm_transport() {
            return Err(DevError::Unsupported);
        }
        let (features, host_visible_len) = display
            .render_transport()
            .map(|transport| (transport.modern_features(), transport.host_visible_len()))
            .unwrap_or_else(|| (GpuFeatures::empty(), None));
        Ok(Self {
            features,
            host_visible_len,
            next_fence_id: 1,
            records: Vec::new(),
            capsets: Vec::new(),
        })
    }

    /// Negotiated capabilities from the selected device, not from a driver's
    /// mere implementation of modern protocol commands.
    pub fn modern_features(&self) -> GpuFeatures {
        self.features
    }

    pub fn host_visible_len(&self) -> Option<u64> {
        self.host_visible_len
    }

    /// Cache the host's ordered capset list once at probe time, matching the
    /// VirtIO GPU `num_capsets` query order and rejecting duplicate/out-of-range
    /// host IDs before advertising a bitmask to userspace.
    pub fn discover_capsets(&mut self, display: &mut dyn DisplayDriverOps) -> DevResult<()> {
        if !self.features.contains(GpuFeatures::VIRGL_3D)
            && !self.features.contains(GpuFeatures::RESOURCE_BLOB)
        {
            return Ok(());
        }
        self.capsets
            .try_reserve_exact(63)
            .map_err(|_| DevError::NoMemory)?;
        for index in 0..63 {
            let fence = self.submit(display, GpuQueue::Control, GpuBatch::CapsetInfo { index })?;
            let snapshot = self.wait(display, fence, 5_000_000_000)?;
            let data = self.take_completion_data(fence)?;
            self.reap(fence)?;
            let GpuCompletionData::CapsetInfo {
                id,
                max_version,
                max_size,
            } = data
            else {
                return Err(DevError::BadState);
            };
            if id == 0 {
                break;
            }
            if id > 63 || self.capsets.iter().any(|(_, info)| info.id == id) {
                return Err(DevError::BadState);
            }
            self.capsets.push((
                index,
                RenderCapsetInfo {
                    id,
                    max_version,
                    max_size,
                },
            ));
            let _ = snapshot;
        }
        Ok(())
    }

    pub fn capset_mask(&self) -> u64 {
        self.capsets
            .iter()
            .fold(0, |mask, (_, info)| mask | (1u64 << info.id))
    }

    pub fn capset(&self, id: u32) -> Option<(u32, RenderCapsetInfo)> {
        self.capsets.iter().copied().find(|(_, info)| info.id == id)
    }

    /// Queue an owned command batch.  The lower transport owns all request
    /// and response backing through terminal completion; this layer retains
    /// the DRM-visible fence and any guest-reserved resource identity.
    pub fn submit(
        &mut self,
        display: &mut dyn DisplayDriverOps,
        queue: GpuQueue,
        batch: GpuBatch,
    ) -> DevResult<u64> {
        let fence_id = self.next_fence_id;
        let next = fence_id.checked_add(1).ok_or(DevError::BadState)?;
        self.records
            .try_reserve(1)
            .map_err(|_| DevError::NoMemory)?;

        let submission = display.drm_submit(queue, batch, fence_id)?;
        if submission.fence_id != fence_id {
            // The lower contract requires identity preservation.  The batch
            // still belongs to the lower owner until terminal completion.
            return Err(DevError::BadState);
        }

        self.next_fence_id = next;
        self.records.push(FenceRecord {
            fence_id,
            queue,
            status: FenceStatus::Pending,
            resource_id: submission.resource_id,
            context_id: submission.context_id,
            completion_data: None,
        });
        // Do not publish a guest-reserved resource ID to the caller here.
        // `fence()` exposes it only after the successful terminal record.
        Ok(fence_id)
    }

    /// Drain at most 32 terminal records from one queue and update the
    /// corresponding DRM fences. Completions from another queue or with an
    /// unknown/reused identity are rejected rather than cross-signaling work.
    pub fn drain(
        &mut self,
        display: &mut dyn DisplayDriverOps,
        queue: GpuQueue,
    ) -> DevResult<usize> {
        let mut out = Vec::new();
        out.try_reserve_exact(COMPLETION_BATCH)
            .map_err(|_| DevError::NoMemory)?;
        for _ in 0..COMPLETION_BATCH {
            out.push(GpuCompletion {
                fence_id: 0,
                result: Err(DevError::BadState),
                data: GpuCompletionData::None,
            });
        }
        let count = display.drm_drain_completions(queue, &mut out)?;
        if count > out.len() {
            return Err(DevError::BadState);
        }

        // Validate the whole batch before publishing any completion.  A
        // malformed record must not leave some fences signaled and others
        // pending when the queue owner needs a reset.
        for (index, completion) in out[..count].iter().enumerate() {
            if out[..index]
                .iter()
                .any(|previous| previous.fence_id == completion.fence_id)
            {
                return Err(DevError::BadState);
            }
            let record = self
                .records
                .iter()
                .find(|record| record.fence_id == completion.fence_id);
            match record {
                Some(record) if record.queue == queue && record.status == FenceStatus::Pending => {}
                _ => return Err(DevError::BadState),
            }
        }

        for completion in &mut out[..count] {
            let record = self
                .records
                .iter_mut()
                .find(|record| record.fence_id == completion.fence_id)
                .ok_or(DevError::BadState)?;
            record.status = if completion.result.is_ok() {
                record.completion_data = Some(core::mem::replace(
                    &mut completion.data,
                    GpuCompletionData::None,
                ));
                FenceStatus::Complete
            } else {
                record.resource_id = None;
                record.completion_data = None;
                FenceStatus::Failed
            };
        }
        Ok(count)
    }

    /// Mark all records on a reset queue failed only when the display transport
    /// confirms that DMA has stopped. An unconfirmed reset leaves them pending
    /// so callers retain their backing references until a later quiesce.
    pub fn reset(&mut self, display: &mut dyn DisplayDriverOps, queue: GpuQueue) -> GpuResetResult {
        let mut out = Vec::new();
        if out.try_reserve_exact(COMPLETION_BATCH).is_err() {
            let reset = display.drm_reset(queue, &mut []);
            if reset.quiesced {
                self.fail_queue_records(queue);
            }
            return reset;
        }
        for _ in 0..COMPLETION_BATCH {
            out.push(GpuCompletion {
                fence_id: 0,
                result: Err(DevError::BadState),
                data: GpuCompletionData::None,
            });
        }
        let reset = display.drm_reset(queue, &mut out);
        if reset.quiesced {
            self.fail_queue_records(queue);
        }
        reset
    }

    fn fail_queue_records(&mut self, queue: GpuQueue) {
        for record in self
            .records
            .iter_mut()
            .filter(|record| record.queue == queue)
        {
            if record.status == FenceStatus::Pending {
                record.status = FenceStatus::Failed;
                record.resource_id = None;
                record.completion_data = None;
            }
        }
    }

    pub fn fence(&self, fence_id: u64) -> Option<FenceSnapshot> {
        self.records
            .iter()
            .find(|record| record.fence_id == fence_id)
            .map(|record| FenceSnapshot {
                fence_id: record.fence_id,
                queue: record.queue,
                status: record.status,
                resource_id: if record.status == FenceStatus::Complete {
                    record.resource_id
                } else {
                    None
                },
                context_id: if record.status == FenceStatus::Complete {
                    record.context_id
                } else {
                    None
                },
            })
    }

    /// Forget a terminal record after its userspace fence/event has been
    /// delivered. Pending records cannot be discarded.
    pub fn reap(&mut self, fence_id: u64) -> DevResult<FenceSnapshot> {
        let index = self
            .records
            .iter()
            .position(|record| record.fence_id == fence_id)
            .ok_or(DevError::InvalidParam)?;
        if self.records[index].status == FenceStatus::Pending {
            return Err(DevError::ResourceBusy);
        }
        let record = self.records.remove(index);
        Ok(FenceSnapshot {
            fence_id: record.fence_id,
            queue: record.queue,
            status: record.status,
            resource_id: if record.status == FenceStatus::Complete {
                record.resource_id
            } else {
                None
            },
            context_id: if record.status == FenceStatus::Complete {
                record.context_id
            } else {
                None
            },
        })
    }

    /// Take the response payload attached to one successful terminal fence.
    /// It is available at most once and never before completion.
    pub fn take_completion_data(&mut self, fence_id: u64) -> DevResult<GpuCompletionData> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.fence_id == fence_id)
            .ok_or(DevError::InvalidParam)?;
        match record.status {
            FenceStatus::Pending => Err(DevError::Again),
            FenceStatus::Failed => Err(DevError::Io),
            FenceStatus::Complete => record.completion_data.take().ok_or(DevError::InvalidParam),
        }
    }

    /// Sleepable fence wait used by ioctls whose Linux semantics require a
    /// terminal response. Polling is bounded; a wedged queue is reset so its
    /// lower-layer DMA owners become terminal before the error is returned.
    pub fn wait(
        &mut self,
        display: &mut dyn DisplayDriverOps,
        fence_id: u64,
        timeout_ns: u64,
    ) -> DevResult<FenceSnapshot> {
        let deadline = axhal::time::monotonic_time_nanos().saturating_add(timeout_ns);
        loop {
            let snapshot = self.fence(fence_id).ok_or(DevError::InvalidParam)?;
            match snapshot.status {
                FenceStatus::Complete => return Ok(snapshot),
                FenceStatus::Failed => return Err(DevError::Io),
                FenceStatus::Pending => {}
            }
            if self.drain(display, snapshot.queue).is_err() {
                self.reset(display, snapshot.queue);
                return Err(DevError::Io);
            }
            if axhal::time::monotonic_time_nanos() >= deadline {
                self.reset(display, snapshot.queue);
                return Err(DevError::Io);
            }
            axtask::yield_now();
        }
    }
}

#[cfg(test)]
mod reset_quiescence_tests {
    use axdriver_display::{BaseDriverOps, DeviceType, DisplayInfo, FrameBuffer, PixelLayout};

    use super::*;

    struct ResetDisplay {
        reset: GpuResetResult,
    }

    impl BaseDriverOps for ResetDisplay {
        fn device_name(&self) -> &str {
            "reset-test"
        }

        fn device_type(&self) -> DeviceType {
            DeviceType::Display
        }
    }

    impl DisplayDriverOps for ResetDisplay {
        fn info(&self) -> DisplayInfo {
            DisplayInfo::without_framebuffer(1, 1, PixelLayout::B8G8R8A8)
        }

        fn fb(&self) -> FrameBuffer<'_> {
            // SAFETY: An empty framebuffer is not read by this reset-only mock;
            // dangling is non-null and correctly aligned for a zero-length slice.
            unsafe {
                FrameBuffer::from_raw_parts_mut(core::ptr::NonNull::<u8>::dangling().as_ptr(), 0)
            }
        }

        fn need_flush(&self) -> bool {
            false
        }

        fn flush(&mut self) -> DevResult {
            Ok(())
        }

        fn supports_drm_transport(&self) -> bool {
            true
        }

        fn drm_reset(&mut self, _queue: GpuQueue, _out: &mut [GpuCompletion]) -> GpuResetResult {
            self.reset
        }
    }

    #[test]
    fn reset_failure_keeps_fences_pending_until_transport_quiesces() {
        let mut display = ResetDisplay {
            reset: GpuResetResult {
                completions: 0,
                quiesced: false,
            },
        };
        let mut engine = GpuEngine::new(&mut display).unwrap();
        engine.records.push(FenceRecord {
            fence_id: 7,
            queue: GpuQueue::Control,
            status: FenceStatus::Pending,
            resource_id: None,
            context_id: None,
            completion_data: None,
        });

        assert!(!engine.reset(&mut display, GpuQueue::Control).quiesced);
        assert_eq!(engine.fence(7).unwrap().status, FenceStatus::Pending);

        display.reset.quiesced = true;
        assert!(engine.reset(&mut display, GpuQueue::Control).quiesced);
        assert_eq!(engine.fence(7).unwrap().status, FenceStatus::Failed);
    }
}
