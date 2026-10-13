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
    DevError, DevResult, DisplayDriverOps, GpuBatch, GpuCompletion, GpuCompletionData,
    GpuFeatures, GpuQueue,
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
}

struct FenceRecord {
    fence_id: u64,
    queue: GpuQueue,
    status: FenceStatus,
    resource_id: Option<u32>,
    completion_data: Option<GpuCompletionData>,
}

/// Per-device fence/resource ownership for `drm_submit`.
///
/// The caller serializes access (normally with the device's DRM mutex).  This
/// matters because `DisplayDriverOps` intentionally exposes a mutable
/// transport; it must not be called concurrently through aliases.
pub struct GpuEngine<'a> {
    display: &'a mut dyn DisplayDriverOps,
    features: GpuFeatures,
    host_visible_len: Option<u64>,
    next_fence_id: u64,
    records: Vec<FenceRecord>,
}

impl<'a> GpuEngine<'a> {
    pub fn new(display: &'a mut dyn DisplayDriverOps) -> DevResult<Self> {
        if !display.supports_drm_transport() {
            return Err(DevError::Unsupported);
        }
        let (features, host_visible_len) = display
            .render_transport()
            .map(|transport| (transport.modern_features(), transport.host_visible_len()))
            .unwrap_or_else(|| (GpuFeatures::empty(), None));
        Ok(Self {
            display,
            features,
            host_visible_len,
            next_fence_id: 1,
            records: Vec::new(),
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

    /// Queue an owned command batch.  The lower transport owns all request
    /// and response backing through terminal completion; this layer retains
    /// the DRM-visible fence and any guest-reserved resource identity.
    pub fn submit(&mut self, queue: GpuQueue, batch: GpuBatch) -> DevResult<u64> {
        let fence_id = self.next_fence_id;
        let next = fence_id.checked_add(1).ok_or(DevError::BadState)?;
        self.records.try_reserve(1).map_err(|_| DevError::NoMemory)?;

        let submission = self.display.drm_submit(queue, batch, fence_id)?;
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
            completion_data: None,
        });
        // Do not publish a guest-reserved resource ID to the caller here.
        // `fence()` exposes it only after the successful terminal record.
        Ok(fence_id)
    }

    /// Drain at most 32 terminal records from one queue and update the
    /// corresponding DRM fences. Completions from another queue or with an
    /// unknown/reused identity are rejected rather than cross-signaling work.
    pub fn drain(&mut self, queue: GpuQueue) -> DevResult<usize> {
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
        let count = self.display.drm_drain_completions(queue, &mut out)?;
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
                Some(record)
                    if record.queue == queue && record.status == FenceStatus::Pending => {}
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

    /// Reset is terminal failure, never successful execution. Any resource
    /// ID reserved by an in-flight create is discarded.
    pub fn reset(&mut self, queue: GpuQueue) -> usize {
        let mut out = Vec::new();
        if out.try_reserve_exact(COMPLETION_BATCH).is_err() {
            // The lower reset contract is still applied. If there are more
            // records than this bounded buffer can report, remaining fences
            // are failed below rather than left indefinitely pending.
            let reset = self.display.drm_reset(queue, &mut []);
            for record in self.records.iter_mut().filter(|record| record.queue == queue) {
                if record.status == FenceStatus::Pending {
                    record.status = FenceStatus::Failed;
                    record.resource_id = None;
                    record.completion_data = None;
                }
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
        let count = self.display.drm_reset(queue, &mut out);
        for completion in out.iter().take(count) {
            if let Some(record) = self
                .records
                .iter_mut()
                .find(|record| record.fence_id == completion.fence_id && record.queue == queue)
            {
                record.status = FenceStatus::Failed;
                record.resource_id = None;
                record.completion_data = None;
            }
        }
        // Reset of this queue invalidates any record the transport could not
        // fit in the bounded response, so fail all remaining queue fences.
        for record in self.records.iter_mut().filter(|record| record.queue == queue) {
            if record.status == FenceStatus::Pending {
                record.status = FenceStatus::Failed;
                record.resource_id = None;
                record.completion_data = None;
            }
        }
        count
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
            FenceStatus::Complete => record
                .completion_data
                .take()
                .ok_or(DevError::InvalidParam),
        }
    }
}
