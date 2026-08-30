use alloc::vec::Vec;

use axdriver_base::{BaseDriverOps, DevResult, DeviceType};
use axdriver_display::{
    DisplayDriverOps, DisplayInfo, FrameBuffer, RenderCapsetInfo, RenderResource3D,
    RenderTransfer3D, RenderTransport,
};
use log::error;
use virtio_drivers::{
    Hal,
    device::gpu::{Rect, ResourceId, VirtIOGpu as InnerDev},
    transport::Transport,
};

use crate::as_dev_err;

/// The VirtIO GPU device driver.
pub struct VirtIoGpuDev<H: Hal, T: Transport> {
    inner: InnerDev<H, T>,
    info: DisplayInfo,
    drm_resources: Vec<DrmResource>,
    /// Resource IDs known detached but whose unref completion failed. These
    /// retain the host-side lifetime token without retaining caller pages.
    pending_unrefs: Vec<ResourceId>,
}

struct DrmResource {
    raw: u32,
    resource: ResourceId,
    attached: bool,
}

unsafe impl<H: Hal, T: Transport> Send for VirtIoGpuDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoGpuDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T) -> DevResult<Self> {
        let mut virtio = InnerDev::new(transport).map_err(as_dev_err)?;

        let (width, height) = virtio.resolution().map_err(as_dev_err)?;
        let info = DisplayInfo {
            width,
            height,
            // DRM takes this device before devfs creates fb0. Do not create a
            // compatibility resource here: it would be a second scanout owner.
            fb_base_vaddr: 0,
            fb_size: 0,
        };

        Ok(Self {
            inner: virtio,
            info,
            drm_resources: Vec::new(),
            pending_unrefs: Vec::new(),
        })
    }

    /// Retry a bounded, caller-driven cleanup pass.  A failed UNREF does not
    /// permit forgetting the resource ID, but it is safe to retry because a
    /// successful detach already proved the host no longer owns guest DMA.
    fn retry_pending_unrefs(&mut self) {
        let mut index = 0;
        while index < self.pending_unrefs.len() {
            if self.inner.unref(self.pending_unrefs[index]).is_ok() {
                self.pending_unrefs.swap_remove(index);
            } else {
                index += 1;
            }
        }
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
        self.inner.flush().map_err(as_dev_err)
    }

    fn supports_drm_transport(&self) -> bool {
        true
    }

    fn render_transport(&mut self) -> Option<&mut dyn RenderTransport> {
        self.inner.virgl_supported().then_some(self)
    }

    fn drm_create_resource(
        &mut self,
        width: u32,
        height: u32,
        entries: &[(u64, u32)],
    ) -> DevResult<u32> {
        self.retry_pending_unrefs();
        // Reserve bookkeeping before attaching caller-owned DMA. Once the
        // command is submitted, even an error can leave host DMA ownership
        // uncertain, so every path needs a retained token.
        self.drm_resources
            .try_reserve(1)
            .map_err(|_| axdriver_base::DevError::NoMemory)?;
        self.pending_unrefs
            .try_reserve(1)
            .map_err(|_| axdriver_base::DevError::NoMemory)?;
        let resource = self.inner.create_2d(width, height).map_err(as_dev_err)?;
        if let Err(error) = self.inner.attach_backing_entries(resource, entries) {
            // `attach_backing_entries` leaves its lower-layer resource in an
            // explicit uncertain state.  DETACH_BACKING is therefore sent
            // even though the attach response failed.
            if let Err(detach_error) = self.inner.detach_backing(resource) {
                // Do not return an error here: the caller would drop its
                // pinned pages.  Publish a destroyable resource token so its
                // normal retirement path keeps the pages alive and retries
                // DETACH_BACKING.  Presenting it may fail, but it cannot UAF.
                error!(
                    "virtio-gpu attach for resource {} failed ({:?}) and detach is uncertain \
                     ({:?}); retaining caller DMA",
                    resource.get(),
                    error,
                    detach_error
                );
                let raw = resource.get();
                self.drm_resources.push(DrmResource {
                    raw,
                    resource,
                    attached: true,
                });
                return Ok(raw);
            }
            if let Err(unref_error) = self.inner.unref(resource) {
                // Detach completed, so caller pages are safe to release, but
                // retain the resource token for a future bounded retry.
                error!(
                    "virtio-gpu unref for detached failed attach resource {} failed: {:?}",
                    resource.get(),
                    unref_error
                );
                self.pending_unrefs.push(resource);
            }
            return Err(as_dev_err(error));
        }
        let raw = resource.get();
        self.drm_resources.push(DrmResource {
            raw,
            resource,
            attached: true,
        });
        Ok(raw)
    }

    fn drm_present_resource(&mut self, resource: u32, width: u32, height: u32) -> DevResult {
        let resource = self
            .drm_resources
            .iter()
            .find_map(|entry| (entry.raw == resource).then_some(entry.resource))
            .ok_or(axdriver_base::DevError::InvalidParam)?;
        let rect = Rect::new(0, 0, width, height);
        self.inner
            .set_scanout(rect, 0, Some(resource))
            .and_then(|_| self.inner.transfer_to_host(resource, rect))
            .and_then(|_| self.inner.resource_flush(resource, rect))
            .map_err(as_dev_err)
    }

    fn drm_destroy_resource(&mut self, resource: u32) -> DevResult {
        self.retry_pending_unrefs();
        let index = self
            .drm_resources
            .iter()
            .position(|entry| entry.raw == resource)
            .ok_or(axdriver_base::DevError::InvalidParam)?;
        // Reserve the retry token before detaching: after a successful
        // detach, pages are safe but a failed UNREF must not leave the public
        // resource table occupying a live DRM slot forever.
        self.pending_unrefs
            .try_reserve(1)
            .map_err(|_| axdriver_base::DevError::NoMemory)?;
        let entry = &mut self.drm_resources[index];
        if entry.attached {
            // Keep the table entry on failure: the caller must keep its DMA
            // backing alive and may retry destruction without losing the token.
            self.inner
                .detach_backing(entry.resource)
                .map_err(as_dev_err)?;
            entry.attached = false;
        }
        let resource = entry.resource;
        match self.inner.unref(resource) {
            Ok(()) => {
                self.drm_resources.remove(index);
                Ok(())
            }
            Err(error) => {
                // Backing is detached, so moving the token out of the public
                // resource table cannot release caller pages. The lower layer
                // performs at most one retry before conservatively retiring
                // an unref completion with an unknown host outcome.
                self.pending_unrefs.push(resource);
                self.drm_resources.remove(index);
                Err(as_dev_err(error))
            }
        }
    }
}

impl<H: Hal, T: Transport> RenderTransport for VirtIoGpuDev<H, T> {
    fn capset_info(&mut self, index: u32) -> DevResult<RenderCapsetInfo> {
        self.inner
            .capset_info(index)
            .map(|v| RenderCapsetInfo {
                id: v.id,
                max_version: v.max_version,
                max_size: v.max_size,
            })
            .map_err(as_dev_err)
    }
    fn capset(&mut self, id: u32, version: u32, data: &mut [u8]) -> DevResult<usize> {
        self.inner.capset(id, version, data).map_err(as_dev_err)
    }
    fn create_context(&mut self, name: &[u8]) -> DevResult<u32> {
        self.inner
            .create_context(name)
            .map(|v| v.get())
            .map_err(as_dev_err)
    }
    fn destroy_context(&mut self, context: u32) -> DevResult {
        self.inner.destroy_context(context).map_err(as_dev_err)
    }
    fn attach_resource(&mut self, context: u32, resource: u32) -> DevResult {
        self.inner
            .context_attach_resource(context, resource)
            .map_err(as_dev_err)
    }
    fn detach_resource(&mut self, context: u32, resource: u32) -> DevResult {
        self.inner
            .context_detach_resource(context, resource)
            .map_err(as_dev_err)
    }
    fn create_resource_3d(&mut self, r: RenderResource3D) -> DevResult<u32> {
        self.inner
            .create_3d(
                r.target,
                r.format,
                r.bind,
                r.width,
                r.height,
                r.depth,
                r.array_size,
                r.last_level,
                r.nr_samples,
                r.flags,
            )
            .map(|v| v.get())
            .map_err(as_dev_err)
    }
    fn attach_backing(&mut self, resource: u32, entries: &[(u64, u32)]) -> DevResult {
        self.inner
            .attach_backing_entries(ResourceId::from_raw(resource), entries)
            .map_err(as_dev_err)
    }
    fn detach_backing(&mut self, resource: u32) -> DevResult {
        self.inner
            .detach_backing(ResourceId::from_raw(resource))
            .map_err(as_dev_err)
    }
    fn unref_resource(&mut self, resource: u32) -> DevResult {
        // Do not route 3D resources through the legacy 2D DRM table.  The
        // lower layer retains its token until this command completes.
        self.inner
            .unref(ResourceId::from_raw(resource))
            .map_err(as_dev_err)
    }
    fn transfer_3d(
        &mut self,
        context: u32,
        resource: u32,
        t: RenderTransfer3D,
        to_host: bool,
    ) -> DevResult {
        self.inner
            .transfer_3d(
                context,
                resource,
                t.x,
                t.y,
                t.z,
                t.width,
                t.height,
                t.depth,
                t.offset,
                t.level,
                t.stride,
                t.layer_stride,
                to_host,
            )
            .map_err(as_dev_err)
    }
    fn submit_3d(&mut self, context: u32, commands: &[u8], resources: &[u32]) -> DevResult {
        self.inner
            .submit_3d(context, commands, resources)
            .map_err(as_dev_err)
    }
}
