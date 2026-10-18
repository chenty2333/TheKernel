use alloc::vec::Vec;

use axdriver_base::{BaseDriverOps, DevResult, DeviceType};
use axdriver_display::{
    BlobMem, DisplayDriverOps, DisplayInfo, DrmDisplayConfig, FrameBuffer, GpuBatch, GpuCompletion,
    GpuCompletionData, GpuFeatures, GpuQueue, GpuResetResult, GpuSubmission, GpuTransport,
    PixelLayout,
};
use virtio_drivers::{
    Hal,
    device::gpu::{
        GpuCompletion as InnerCompletion, GpuCompletionData as InnerCompletionData, Rect,
        ResourceId, VirtIOGpu as InnerDev,
    },
    transport::Transport,
};

use crate::as_dev_err;

/// The VirtIO GPU device driver.
pub struct VirtIoGpuDev<H: Hal, T: Transport> {
    inner: InnerDev<H, T>,
    info: DisplayInfo,
    pci_identity: Option<axdriver_display::DisplayPciIdentity>,
    drm_resources: Vec<DrmResource>,
    pending_destroy_resources: Vec<(u64, u32)>,
    drm_fences: DrmFenceMap,
}

struct DrmResource {
    raw: u32,
    resource: ResourceId,
}

/// Translation-only bookkeeping from the transport's private fence namespace
/// to the DRM fence supplied by `GpuEngine`. Public pending/completed/failed
/// state remains exclusively owned by `GpuEngine`.
#[derive(Default)]
struct DrmFenceMap {
    pending: Vec<(GpuQueue, u64, u64)>,
}

impl DrmFenceMap {
    fn reserve(&mut self) -> DevResult {
        self.pending
            .try_reserve(1)
            .map_err(|_| axdriver_base::DevError::NoMemory)
    }

    fn record(&mut self, queue: GpuQueue, transport_id: u64, drm_id: u64) {
        self.pending.push((queue, transport_id, drm_id));
    }

    fn translate(&mut self, queue: GpuQueue, transport_id: u64) -> DevResult<u64> {
        let index = self
            .pending
            .iter()
            .position(|(pending_queue, pending_transport, _)| {
                *pending_queue == queue && *pending_transport == transport_id
            })
            .ok_or(axdriver_base::DevError::BadState)?;
        Ok(self.pending.swap_remove(index).2)
    }

    fn clear_queue(&mut self, queue: GpuQueue) {
        self.pending
            .retain(|(pending_queue, ..)| *pending_queue != queue);
    }
}

#[cfg(test)]
mod drm_fence_map_tests {
    use axdriver_display::GpuQueue;

    use super::DrmFenceMap;

    #[test]
    fn maps_present_stage_fence_gaps_back_to_drm_fences() {
        let mut fences = DrmFenceMap::default();
        for (transport, drm) in [(1, 1), (2, 2), (3, 3), (7, 4)] {
            fences.reserve().unwrap();
            fences.record(GpuQueue::Control, transport, drm);
        }
        fences.reserve().unwrap();
        fences.record(GpuQueue::Cursor, 7, 70);

        // The public Present is inner/DRM fence 3. Its SET_SCANOUT,
        // TRANSFER_TO_HOST, and FLUSH commands privately consume inner IDs
        // 4-6. Therefore the next public submission uses inner ID 7 while the
        // DRM engine assigned ID 4. Intermediate completions must not consume
        // the mapping for that later public fence.
        for inner_stage in 4..=6 {
            assert!(matches!(
                fences.translate(GpuQueue::Control, inner_stage),
                Err(axdriver_base::DevError::BadState)
            ));
        }
        assert!(matches!(fences.translate(GpuQueue::Cursor, 7), Ok(70)));
        assert!(matches!(fences.translate(GpuQueue::Control, 7), Ok(4)));
        assert!(matches!(fences.translate(GpuQueue::Control, 3), Ok(3)));
        assert!(matches!(fences.translate(GpuQueue::Control, 1), Ok(1)));
        assert!(matches!(fences.translate(GpuQueue::Control, 2), Ok(2)));
        assert!(fences.pending.is_empty());
    }
}

unsafe impl<H: Hal, T: Transport> Send for VirtIoGpuDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoGpuDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T) -> DevResult<Self> {
        let mut virtio = InnerDev::new(transport).map_err(as_dev_err)?;

        let (width, height) = virtio.resolution().map_err(as_dev_err)?;
        // DRM takes this device before devfs creates fb0. Do not create a
        // compatibility resource here: it would be a second scanout owner, so
        // no linear framebuffer is published.  The scanout format is still
        // stated: it is what a compatible buffer must be allocated as, and it
        // is true whether or not this driver publishes an address.
        let info = DisplayInfo::without_framebuffer(width, height, PixelLayout::B8G8R8A8);

        Ok(Self {
            inner: virtio,
            info,
            pci_identity: None,
            drm_resources: Vec::new(),
            pending_destroy_resources: Vec::new(),
            drm_fences: DrmFenceMap::default(),
        })
    }
}

impl<H: Hal, T: Transport> BaseDriverOps for VirtIoGpuDev<H, T> {
    fn device_name(&self) -> &str {
        "virtio-gpu"
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Display
    }
}

impl<H: Hal, T: Transport> DisplayDriverOps for VirtIoGpuDev<H, T> {
    fn pci_identity(&self) -> Option<axdriver_display::DisplayPciIdentity> {
        self.pci_identity
    }
    fn set_pci_identity(&mut self, identity: axdriver_display::DisplayPciIdentity) {
        self.pci_identity = Some(identity);
    }
    fn info(&self) -> DisplayInfo {
        self.info
    }

    fn fb(&self) -> FrameBuffer<'_> {
        unsafe {
            FrameBuffer::from_raw_parts_mut(self.info.fb_base_vaddr as *mut u8, self.info.fb_size)
        }
    }

    fn need_flush(&self) -> bool {
        true
    }

    fn flush(&mut self) -> DevResult {
        Err(axdriver_base::DevError::Unsupported)
    }

    fn supports_drm_transport(&self) -> bool {
        true
    }

    fn render_transport(&mut self) -> Option<&mut dyn GpuTransport> {
        // Venus/blob contexts are render transports even when the legacy
        // VIRGL feature was deliberately not negotiated.
        (self.inner.virgl_supported() || self.inner.resource_blob_supported()).then_some(self)
    }

    fn drm_submit(
        &mut self,
        queue: GpuQueue,
        batch: GpuBatch,
        fence_id: u64,
    ) -> DevResult<GpuSubmission> {
        <Self as GpuTransport>::submit(self, queue, batch, fence_id)
    }

    fn drm_drain_completions(
        &mut self,
        queue: GpuQueue,
        out: &mut [GpuCompletion],
    ) -> DevResult<usize> {
        <Self as GpuTransport>::drain_completions(self, queue, out)
    }

    fn drm_reset(&mut self, queue: GpuQueue, out: &mut [GpuCompletion]) -> GpuResetResult {
        <Self as GpuTransport>::reset(self, queue, out)
    }

    fn drm_display_config_changed(&mut self) -> DevResult<Option<DrmDisplayConfig>> {
        let events = self.inner.read_config_events().map_err(as_dev_err)?;
        if events & virtio_drivers::device::gpu::EVENT_DISPLAY == 0 {
            return Ok(None);
        }
        let info = self.inner.display_info().map_err(as_dev_err)?;
        self.inner.ack_config_events(events).map_err(as_dev_err)?;
        let Some((_, scanout)) = info.first_enabled() else {
            return Ok(Some(DrmDisplayConfig {
                connected: false,
                width: 0,
                height: 0,
            }));
        };
        Ok(Some(DrmDisplayConfig {
            connected: true,
            width: scanout.rect.width,
            height: scanout.rect.height,
        }))
    }
}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
    fn modern_features(&self) -> GpuFeatures {
        let mut features = GpuFeatures::empty();
        if self.inner.resource_uuid_supported() {
            features = features.union(GpuFeatures::RESOURCE_UUID);
        }
        if self.inner.resource_blob_supported() {
            features = features.union(GpuFeatures::RESOURCE_BLOB);
        }
        if self.inner.resource_blob_supported() && self.inner.context_init_supported() {
            features = features.union(GpuFeatures::CONTEXT_INIT);
        }
        if self.inner.virgl_supported() {
            features = features.union(GpuFeatures::VIRGL_3D);
        }
        // Expose HOST_VISIBLE only when the lower transport has a validated
        // shared-memory aperture. MAP_BLOB is converted to exact physical
        // pages and retained through the final external SharedPages lease.
        if self.inner.resource_blob_supported() && self.inner.hostmem_supported() {
            features = features.union(GpuFeatures::HOST_VISIBLE);
        }
        features
    }
}

impl<H: Hal, T: Transport> GpuTransport for VirtIoGpuDev<H, T> {
    fn modern_features(&self) -> GpuFeatures {
        VirtIoGpuDev::modern_features(self)
    }
    fn host_visible_len(&self) -> Option<u64> {
        self.inner.hostmem_len()
    }

    fn submit(
        &mut self,
        queue: GpuQueue,
        batch: GpuBatch,
        fence_id: u64,
    ) -> DevResult<GpuSubmission> {
        // The DRM engine owns the public fence namespace. The VirtIO GPU
        // implementation also allocates private fences for stages inside a
        // Present batch, so remember the lower ID before exposing completion.
        self.drm_fences.reserve()?;
        let mut submission = match (queue, batch) {
            // CREATE_2D and ATTACH_BACKING have independent protocol
            // completions.  The DRM adapter submits the latter only after
            // this creation completion; accepting backing here would make
            // its caller-owned pages outlive an unobservable second fence.
            (
                GpuQueue::Control,
                GpuBatch::Create2d {
                    width,
                    height,
                    format,
                    entries,
                },
            ) => {
                if !entries.is_empty() {
                    return Err(axdriver_base::DevError::InvalidParam);
                }
                self.drm_resources
                    .try_reserve(1)
                    .map_err(|_| axdriver_base::DevError::NoMemory)?;
                self.inner
                    .submit_create_2d(width, height, format)
                    .map(|(resource, submission)| {
                        self.drm_resources.push(DrmResource {
                            raw: resource.get(),
                            resource,
                        });
                        GpuSubmission {
                            fence_id: submission.fence_id,
                            resource_id: Some(resource.get()),
                            context_id: None,
                        }
                    })
                    .map_err(as_dev_err)
            }
            (
                GpuQueue::Control,
                GpuBatch::Present {
                    resource,
                    width,
                    height,
                    source_x,
                    source_y,
                    damage,
                },
            ) => {
                let resource = ResourceId::from_raw(resource);
                let visible = Rect::new(source_x, source_y, width, height);
                let damage = damage.map_or(visible, |damage| {
                    Rect::new(damage.x, damage.y, damage.width, damage.height)
                });
                self.inner
                    .submit_present(resource, visible, damage)
                    .map(|submission| GpuSubmission {
                        fence_id: submission.fence_id,
                        resource_id: None,
                        context_id: None,
                    })
                    .map_err(as_dev_err)
            }
            (GpuQueue::Control, GpuBatch::DisableScanout) => self
                .inner
                .submit_set_scanout(Rect::new(0, 0, 0, 0), 0, None)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (
                GpuQueue::Control,
                GpuBatch::PresentBlob {
                    resource,
                    source_x,
                    source_y,
                    width,
                    height,
                    framebuffer_width,
                    framebuffer_height,
                    format,
                    stride,
                    offset,
                    damage,
                },
            ) => self
                .inner
                .submit_present_blob(
                    ResourceId::from_raw(resource),
                    source_x,
                    source_y,
                    width,
                    height,
                    framebuffer_width,
                    framebuffer_height,
                    format,
                    stride,
                    offset,
                    damage.map(|damage| {
                        virtio_drivers::device::gpu::Rect::new(
                            damage.x,
                            damage.y,
                            damage.width,
                            damage.height,
                        )
                    }),
                )
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::DestroyResource { resource }) => {
                self.pending_destroy_resources
                    .try_reserve(1)
                    .map_err(|_| axdriver_base::DevError::NoMemory)?;
                let submission = self
                    .inner
                    .submit_unref(ResourceId::from_raw(resource))
                    .map_err(as_dev_err)?;
                self.pending_destroy_resources
                    .push((submission.fence_id, resource));
                Ok(GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
            }
            (
                GpuQueue::Control,
                GpuBatch::Submit3d {
                    context,
                    ring_idx,
                    commands,
                    resources,
                },
            ) => self
                .inner
                .submit_3d(context, ring_idx, &commands, &resources)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::CreateResource3d { resource }) => self
                .inner
                .submit_create_3d(
                    resource.target,
                    resource.format,
                    resource.bind,
                    resource.width,
                    resource.height,
                    resource.depth,
                    resource.array_size,
                    resource.last_level,
                    resource.nr_samples,
                    resource.flags,
                )
                .map(|(id, submission)| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: Some(id.get()),
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::CreateBlob { resource, entries }) => {
                let resource = virtio_drivers::device::gpu::BlobResource {
                    mem: match resource.mem {
                        BlobMem::Guest => virtio_drivers::device::gpu::BlobMem::Guest,
                        BlobMem::Host3d => virtio_drivers::device::gpu::BlobMem::Host3d,
                        BlobMem::Host3dGuest => virtio_drivers::device::gpu::BlobMem::Host3dGuest,
                    },
                    flags: resource.flags,
                    size: resource.size,
                    blob_id: resource.blob_id,
                };
                self.inner
                    .submit_create_blob(resource, &entries)
                    .map(|(id, submission)| GpuSubmission {
                        fence_id: submission.fence_id,
                        resource_id: Some(id.get()),
                        context_id: None,
                    })
                    .map_err(as_dev_err)
            }
            (GpuQueue::Control, GpuBatch::CapsetInfo { index }) => self
                .inner
                .submit_capset_info(index)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::Capset { id, version, bytes }) => self
                .inner
                .submit_capset(id, version, bytes)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::MapBlob { resource, offset }) => self
                .inner
                .submit_map_blob(ResourceId::from_raw(resource), offset)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::AssignUuid { resource }) => self
                .inner
                .submit_assign_uuid(ResourceId::from_raw(resource))
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::UnmapBlob { resource }) => self
                .inner
                .submit_unmap_blob(ResourceId::from_raw(resource))
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::AttachBacking { resource, entries }) => self
                .inner
                .submit_attach_backing_entries(ResourceId::from_raw(resource), &entries)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::DetachBacking { resource }) => self
                .inner
                .submit_detach_backing(ResourceId::from_raw(resource))
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::UnrefResource { resource }) => self
                .inner
                .submit_unref(ResourceId::from_raw(resource))
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::CreateContext { name, init }) => self
                .inner
                .submit_create_context(
                    &name,
                    virtio_drivers::device::gpu::ContextInit {
                        capset_id: init.capset_id,
                        num_rings: init.num_rings,
                        poll_rings_mask: init.poll_rings_mask,
                        debug_name: init.debug_name,
                        debug_name_len: init.debug_name_len,
                    },
                )
                .map(|(id, submission)| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: Some(id.get()),
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::DestroyContext { context }) => self
                .inner
                .submit_destroy_context(context)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::AttachResource { context, resource }) => self
                .inner
                .submit_context_attach_resource(context, resource)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (GpuQueue::Control, GpuBatch::DetachResource { context, resource }) => self
                .inner
                .submit_context_detach_resource(context, resource)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (
                GpuQueue::Control,
                GpuBatch::Transfer3d {
                    context,
                    resource,
                    transfer,
                    to_host,
                },
            ) => self
                .inner
                .submit_transfer_3d(
                    context,
                    resource,
                    transfer.x,
                    transfer.y,
                    transfer.z,
                    transfer.width,
                    transfer.height,
                    transfer.depth,
                    transfer.offset,
                    transfer.level,
                    transfer.stride,
                    transfer.layer_stride,
                    to_host,
                )
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            (
                GpuQueue::Control,
                GpuBatch::Transfer2d {
                    resource,
                    width,
                    height,
                },
            ) => {
                let resource = self
                    .drm_resources
                    .iter()
                    .find_map(|entry| (entry.raw == resource).then_some(entry.resource))
                    .ok_or(axdriver_base::DevError::InvalidParam)?;
                self.inner
                    .submit_transfer_to_host(
                        resource,
                        Rect {
                            x: 0,
                            y: 0,
                            width,
                            height,
                        },
                    )
                    .map(|submission| GpuSubmission {
                        fence_id: submission.fence_id,
                        resource_id: None,
                        context_id: None,
                    })
                    .map_err(as_dev_err)
            }
            (
                GpuQueue::Control,
                GpuBatch::Transfer2dRect {
                    resource,
                    x,
                    y,
                    width,
                    height,
                    offset,
                },
            ) => {
                let resource = self
                    .drm_resources
                    .iter()
                    .find_map(|entry| (entry.raw == resource).then_some(entry.resource))
                    .ok_or(axdriver_base::DevError::InvalidParam)?;
                self.inner
                    .submit_transfer_to_host_offset(
                        resource,
                        Rect {
                            x,
                            y,
                            width,
                            height,
                        },
                        offset as u64,
                    )
                    .map(|submission| GpuSubmission {
                        fence_id: submission.fence_id,
                        resource_id: None,
                        context_id: None,
                    })
                    .map_err(as_dev_err)
            }
            (GpuQueue::Cursor, GpuBatch::UpdateCursor(cursor)) => {
                let resource = self
                    .drm_resources
                    .iter()
                    .find_map(|entry| (entry.raw == cursor.resource).then_some(entry.resource))
                    .ok_or(axdriver_base::DevError::InvalidParam)?;
                if cursor.width != 64
                    || cursor.height != 64
                    || cursor.hot_x >= cursor.width
                    || cursor.hot_y >= cursor.height
                {
                    return Err(axdriver_base::DevError::InvalidParam);
                }
                self.inner
                    .update_cursor(
                        resource,
                        cursor.width,
                        cursor.height,
                        cursor.hot_x,
                        cursor.hot_y,
                        cursor.x,
                        cursor.y,
                    )
                    .map(|submission| GpuSubmission {
                        fence_id: submission.fence_id,
                        resource_id: None,
                        context_id: None,
                    })
                    .map_err(as_dev_err)
            }
            (GpuQueue::Cursor, GpuBatch::MoveCursor { x, y }) => self
                .inner
                .move_cursor(x, y)
                .map(|submission| GpuSubmission {
                    fence_id: submission.fence_id,
                    resource_id: None,
                    context_id: None,
                })
                .map_err(as_dev_err),
            // These controlq variants are admitted by the public batch ABI
            // but require the lower VirtIO resource/context state machine to
            // retain a pending lifecycle transition through completion.
            // Never route them through a synchronous helper: doing so would
            // release caller DMA before terminal ownership is observable.
            _ => Err(axdriver_base::DevError::Unsupported),
        }?;
        self.drm_fences.record(queue, submission.fence_id, fence_id);
        submission.fence_id = fence_id;
        Ok(submission)
    }

    fn drain_completions(
        &mut self,
        queue: GpuQueue,
        out: &mut [GpuCompletion],
    ) -> DevResult<usize> {
        match queue {
            GpuQueue::Control => self.drain_control_queue(out),
            GpuQueue::Cursor => {
                let mut inner: [InnerCompletion; 8] = core::array::from_fn(|_| InnerCompletion {
                    fence_id: 0,
                    result: Ok(()),
                    data: InnerCompletionData::None,
                });
                let capacity = out.len().min(inner.len());
                let count = self
                    .inner
                    .drain_cursor_completions(&mut inner[..capacity])
                    .map_err(as_dev_err)?;
                for (destination, source) in out.iter_mut().zip(inner.into_iter()).take(count) {
                    *destination = self.convert_completion(GpuQueue::Cursor, source)?;
                }
                Ok(count)
            }
        }
    }

    fn reset(&mut self, queue: GpuQueue, out: &mut [GpuCompletion]) -> GpuResetResult {
        match queue {
            GpuQueue::Control => self.reset_control_queue(out),
            GpuQueue::Cursor => {
                let mut inner: [InnerCompletion; 8] = core::array::from_fn(|_| InnerCompletion {
                    fence_id: 0,
                    result: Ok(()),
                    data: InnerCompletionData::None,
                });
                let capacity = out.len().min(inner.len());
                let reset = self.inner.reset_cursor(&mut inner[..capacity]);
                for (destination, source) in out
                    .iter_mut()
                    .zip(inner.into_iter())
                    .take(reset.completions)
                {
                    *destination = self
                        .convert_completion(GpuQueue::Cursor, source)
                        .unwrap_or_else(|error| GpuCompletion {
                            fence_id: 0,
                            result: Err(error),
                            data: GpuCompletionData::None,
                        });
                }
                if reset.quiesced {
                    self.drm_fences.clear_queue(GpuQueue::Cursor);
                }
                GpuResetResult {
                    completions: reset.completions,
                    quiesced: reset.quiesced,
                }
            }
        }
    }
}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
    fn drain_control_queue(&mut self, out: &mut [GpuCompletion]) -> DevResult<usize> {
        let mut count = 0;
        for slot in out {
            let mut completion: [InnerCompletion; 1] = core::array::from_fn(|_| InnerCompletion {
                fence_id: 0,
                result: Ok(()),
                data: InnerCompletionData::None,
            });
            if self
                .inner
                .drain_control_completions(&mut completion)
                .map_err(as_dev_err)?
                == 0
            {
                break;
            }
            let completion = completion
                .into_iter()
                .next()
                .expect("single completion slot");
            *slot = self.convert_completion(GpuQueue::Control, completion)?;
            count += 1;
        }
        Ok(count)
    }

    fn reset_control_queue(&mut self, out: &mut [GpuCompletion]) -> GpuResetResult {
        if out.is_empty() {
            let reset = self.inner.reset_control(&mut []);
            if reset.quiesced {
                self.drm_fences.clear_queue(GpuQueue::Control);
            }
            return GpuResetResult {
                completions: 0,
                quiesced: reset.quiesced,
            };
        }
        let mut count = 0;
        let mut quiesced = false;
        for slot in out {
            let mut completion: [InnerCompletion; 1] = core::array::from_fn(|_| InnerCompletion {
                fence_id: 0,
                result: Ok(()),
                data: InnerCompletionData::None,
            });
            let reset = self.inner.reset_control(&mut completion);
            if !reset.quiesced {
                return GpuResetResult {
                    completions: count,
                    quiesced: false,
                };
            }
            quiesced = true;
            if reset.completions == 0 {
                break;
            }
            let completion = completion
                .into_iter()
                .next()
                .expect("single completion slot");
            *slot = match self.convert_completion(GpuQueue::Control, completion) {
                Ok(completion) => completion,
                Err(_) => {
                    self.drm_fences.clear_queue(GpuQueue::Control);
                    return GpuResetResult {
                        completions: count,
                        quiesced: true,
                    };
                }
            };
            count += 1;
        }
        if quiesced {
            self.drm_fences.clear_queue(GpuQueue::Control);
        }
        GpuResetResult {
            completions: count,
            quiesced,
        }
    }
    fn convert_completion(
        &mut self,
        queue: GpuQueue,
        completion: InnerCompletion,
    ) -> DevResult<GpuCompletion> {
        let drm_fence_id = self.drm_fences.translate(queue, completion.fence_id)?;
        if completion.result.is_ok() {
            if let Some(index) = self
                .pending_destroy_resources
                .iter()
                .position(|(fence, _)| *fence == completion.fence_id)
            {
                let (_, resource) = self.pending_destroy_resources.swap_remove(index);
                self.drm_resources.retain(|entry| entry.raw != resource);
            }
        }
        let data = match completion.data {
            InnerCompletionData::None => GpuCompletionData::None,
            InnerCompletionData::MapInfo {
                aperture_offset,
                aperture_base,
                physical_base,
                cache_policy,
            } => GpuCompletionData::MapInfo(axdriver_display::BlobMapInfo {
                aperture_offset,
                aperture_base,
                physical_base,
                cache_policy,
            }),
            InnerCompletionData::Uuid(uuid) => GpuCompletionData::Uuid(uuid),
            InnerCompletionData::CapsetInfo {
                id,
                max_version,
                max_size,
            } => GpuCompletionData::CapsetInfo {
                id,
                max_version,
                max_size,
            },
            InnerCompletionData::Capset(bytes) => GpuCompletionData::Capset(bytes),
        };
        Ok(GpuCompletion {
            fence_id: drm_fence_id,
            result: completion.result.map_err(as_dev_err),
            data,
        })
    }
}
