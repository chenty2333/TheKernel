//! DRM transport adapter for the sole VirtIO GPU selected by `axdisplay`.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use axdriver_display::{DevError, DisplayDriverOps, RenderResource3D, RenderTransfer3D};
use axhal::paging::PageSize;
use axsync::Mutex;

use super::{
    DisplayAdapter, DrmDevice, DrmError, DrmResult, DumbRequest, GemBacking, RenderAdapter,
    Scanout,
    render::{RenderResource, RenderTransfer},
};
use crate::mm::{SharedPages, checked_align_up};

trait GpuTransport: Send {
    fn preferred_mode(&self) -> (u32, u32) {
        (1024, 768)
    }
    fn create_resource(
        &mut self,
        width: u32,
        height: u32,
        entries: &[(u64, u32)],
    ) -> Result<u32, DevError>;
    fn present_resource(&mut self, resource: u32, width: u32, height: u32) -> Result<(), DevError>;
    /// Must detach the guest backing before unrefing `resource`; on either
    /// failure it must leave the host resource retryable.
    fn destroy_resource(&mut self, resource: u32) -> Result<(), DevError>;
    fn render_capset_info(&mut self, _: u32) -> Result<(u32, u32, u32), DevError> {
        Err(DevError::Unsupported)
    }
    fn render_capset(&mut self, _: u32, _: u32, _: &mut [u8]) -> Result<usize, DevError> {
        Err(DevError::Unsupported)
    }
    fn render_create_context(&mut self, _: &[u8]) -> Result<u32, DevError> {
        Err(DevError::Unsupported)
    }
    fn render_destroy_context(&mut self, _: u32) -> Result<(), DevError> {
        Err(DevError::Unsupported)
    }
    fn render_create_resource(&mut self, _: RenderResource) -> Result<u32, DevError> {
        Err(DevError::Unsupported)
    }
    fn render_attach_backing(
        &mut self,
        resource: u32,
        entries: &[(u64, u32)],
    ) -> Result<(), DevError> {
        Err(DevError::Unsupported)
    }
    fn render_detach_backing(&mut self, _: u32) -> Result<(), DevError> {
        Err(DevError::Unsupported)
    }
    fn render_unref(&mut self, _: u32) -> Result<(), DevError> {
        Err(DevError::Unsupported)
    }
    fn render_attach_resource(&mut self, _: u32, _: u32) -> Result<(), DevError> {
        Err(DevError::Unsupported)
    }
    fn render_detach_resource(&mut self, _: u32, _: u32) -> Result<(), DevError> {
        Err(DevError::Unsupported)
    }
    fn render_transfer(
        &mut self,
        context: u32,
        resource: u32,
        transfer: RenderTransfer,
        to_host: bool,
    ) -> Result<(), DevError> {
        Err(DevError::Unsupported)
    }
    fn render_submit(
        &mut self,
        context: u32,
        commands: &[u8],
        resources: &[u32],
    ) -> Result<(), DevError> {
        Err(DevError::Unsupported)
    }
}

struct DisplayTransport(Box<dyn DisplayDriverOps>);

impl GpuTransport for DisplayTransport {
    fn preferred_mode(&self) -> (u32, u32) {
        let info = self.0.info();
        (info.width, info.height)
    }
    fn create_resource(
        &mut self,
        width: u32,
        height: u32,
        entries: &[(u64, u32)],
    ) -> Result<u32, DevError> {
        self.0.drm_create_resource(width, height, entries)
    }

    fn present_resource(&mut self, resource: u32, width: u32, height: u32) -> Result<(), DevError> {
        self.0.drm_present_resource(resource, width, height)
    }

    fn destroy_resource(&mut self, resource: u32) -> Result<(), DevError> {
        self.0.drm_destroy_resource(resource)
    }
    fn render_capset_info(&mut self, index: u32) -> Result<(u32, u32, u32), DevError> {
        let t = self.0.render_transport().ok_or(DevError::Unsupported)?;
        let i = t.capset_info(index)?;
        Ok((i.id, i.max_version, i.max_size))
    }
    fn render_capset(&mut self, id: u32, v: u32, data: &mut [u8]) -> Result<usize, DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .capset(id, v, data)
    }
    fn render_create_context(&mut self, n: &[u8]) -> Result<u32, DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .create_context(n)
    }
    fn render_destroy_context(&mut self, c: u32) -> Result<(), DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .destroy_context(c)
    }
    fn render_create_resource(&mut self, r: RenderResource) -> Result<u32, DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .create_resource_3d(RenderResource3D {
                target: r.target,
                format: r.format,
                bind: r.bind,
                width: r.width,
                height: r.height,
                depth: r.depth,
                array_size: r.array_size,
                last_level: r.last_level,
                nr_samples: r.nr_samples,
                flags: r.flags,
            })
    }
    fn render_attach_backing(&mut self, r: u32, e: &[(u64, u32)]) -> Result<(), DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .attach_backing(r, e)
    }
    fn render_detach_backing(&mut self, r: u32) -> Result<(), DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .detach_backing(r)
    }
    fn render_unref(&mut self, r: u32) -> Result<(), DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .unref_resource(r)
    }
    fn render_attach_resource(&mut self, c: u32, r: u32) -> Result<(), DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .attach_resource(c, r)
    }
    fn render_detach_resource(&mut self, c: u32, r: u32) -> Result<(), DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .detach_resource(c, r)
    }
    fn render_transfer(
        &mut self,
        c: u32,
        r: u32,
        t: RenderTransfer,
        h: bool,
    ) -> Result<(), DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .transfer_3d(
                c,
                r,
                RenderTransfer3D {
                    x: t.x,
                    y: t.y,
                    z: t.z,
                    width: t.width,
                    height: t.height,
                    depth: t.depth,
                    offset: t.offset,
                    level: t.level,
                    stride: t.stride,
                    layer_stride: t.layer_stride,
                },
                h,
            )
    }
    fn render_submit(&mut self, c: u32, cmd: &[u8], r: &[u32]) -> Result<(), DevError> {
        self.0
            .render_transport()
            .ok_or(DevError::Unsupported)?
            .submit_3d(c, cmd, r)
    }
}

/// The DRM-side owner of the display transport. Its backing map translates a
/// type-erased GEM object back to the VirtIO resource which owns its DMA range.
struct VirtioGpuAdapter<T: GpuTransport> {
    state: Arc<AdapterState<T>>,
}

struct AdapterState<T: GpuTransport> {
    transport: Mutex<T>,
    resources: Mutex<Vec<(usize, u32)>>,
    retired_2d_resources: Mutex<Vec<Retired2dResource>>,
    retired_render_resources: Mutex<Vec<RetiredRenderResource>>,
    final_2d_leaks: AtomicUsize,
    final_render_leaks: AtomicUsize,
}

struct Retired2dResource {
    resource: u32,
    pages: Arc<SharedPages>,
}

/// A host render resource whose backing cannot yet be freed.  The token stays
/// owned by the adapter until both host operations complete.
struct RetiredRenderResource {
    resource: u32,
    pages: Arc<SharedPages>,
    backing_attached: bool,
    stage: RenderRetireStage,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RenderRetireStage {
    DetachBacking,
    Unref,
}

const MAX_RETIRED_RESOURCES: usize = 128;

impl<T: GpuTransport> VirtioGpuAdapter<T> {
    fn new(transport: T) -> Self {
        Self {
            state: Arc::new(AdapterState {
                transport: Mutex::new(transport),
                resources: Mutex::new(Vec::new()),
                retired_2d_resources: Mutex::new(Vec::new()),
                retired_render_resources: Mutex::new(Vec::new()),
                final_2d_leaks: AtomicUsize::new(0),
                final_render_leaks: AtomicUsize::new(0),
            }),
        }
    }
}

struct VirtioRenderAdapter<T: GpuTransport> {
    state: Arc<AdapterState<T>>,
}
impl<T: GpuTransport + 'static> RenderAdapter for VirtioRenderAdapter<T> {
    fn capset_info(&self, i: u32) -> DrmResult<(u32, u32, u32)> {
        self.state.retry_retired_render_resources();
        self.state
            .transport
            .lock()
            .render_capset_info(i)
            .map_err(map_dev_error)
    }
    fn capset(&self, id: u32, v: u32, d: &mut [u8]) -> DrmResult<usize> {
        self.state.retry_retired_render_resources();
        self.state
            .transport
            .lock()
            .render_capset(id, v, d)
            .map_err(map_dev_error)
    }
    fn create_context(&self, n: &[u8]) -> DrmResult<u32> {
        self.state.retry_retired_render_resources();
        self.state
            .transport
            .lock()
            .render_create_context(n)
            .map_err(map_dev_error)
    }
    fn destroy_context(&self, c: u32) -> DrmResult<()> {
        self.state.retry_retired_render_resources();
        self.state
            .transport
            .lock()
            .render_destroy_context(c)
            .map_err(map_dev_error)
    }
    fn create_resource(
        &self,
        r: RenderResource,
        e: &[(u64, u32)],
        pages: Arc<SharedPages>,
    ) -> DrmResult<u32> {
        self.state.retry_retired_render_resources();
        let mut t = self.state.transport.lock();
        let id = t.render_create_resource(r).map_err(map_dev_error)?;
        if let Err(err) = t.render_attach_backing(id, e) {
            if t.render_unref(id).is_err() {
                drop(t);
                // An error from attach is not proof that the host did not
                // retain the supplied physical ranges. Keep them pinned and
                // retry detach+unref through the normal retirement path.
                self.state.retire_render_resource(id, pages, true);
            }
            return Err(map_dev_error(err));
        }
        Ok(id)
    }
    fn retire_resource(&self, r: u32, pages: Arc<SharedPages>) {
        self.state.retire_render_resource(r, pages, true);
    }
    fn attach_resource(&self, c: u32, r: u32) -> DrmResult<()> {
        self.state.retry_retired_render_resources();
        self.state
            .transport
            .lock()
            .render_attach_resource(c, r)
            .map_err(map_dev_error)
    }
    fn detach_resource(&self, c: u32, r: u32) -> DrmResult<()> {
        self.state.retry_retired_render_resources();
        self.state
            .transport
            .lock()
            .render_detach_resource(c, r)
            .map_err(map_dev_error)
    }
    fn transfer(&self, c: u32, r: u32, t: RenderTransfer, h: bool) -> DrmResult<()> {
        self.state.retry_retired_render_resources();
        self.state
            .transport
            .lock()
            .render_transfer(c, r, t, h)
            .map_err(map_dev_error)
    }
    fn submit(&self, c: u32, cmd: &[u8], r: &[u32]) -> DrmResult<()> {
        self.state.retry_retired_render_resources();
        self.state
            .transport
            .lock()
            .render_submit(c, cmd, r)
            .map_err(map_dev_error)
    }
}

impl<T: GpuTransport> AdapterState<T> {
    fn resource_for(&self, backing: &Arc<dyn GemBacking>) -> DrmResult<u32> {
        let key = Arc::as_ptr(backing) as *const () as usize;
        self.resources
            .lock()
            .iter()
            .find_map(|(candidate, resource)| (*candidate == key).then_some(*resource))
            .ok_or(DrmError::NotFound)
    }

    fn retire_resource(&self, resource: u32, pages: Arc<SharedPages>) {
        self.retry_retired_2d_resources();
        self.resources
            .lock()
            .retain(|(_, candidate)| *candidate != resource);
        let token = Retired2dResource { resource, pages };
        if self.try_retire_2d_resource(&token) {
            return;
        }
        let mut retired = self.retired_2d_resources.lock();
        if retired.len() < MAX_RETIRED_RESOURCES && retired.try_reserve(1).is_ok() {
            retired.push(token);
        } else {
            self.final_2d_leaks.fetch_add(1, Ordering::Relaxed);
            error!(
                "virtio-gpu: 2D retirement queue full; permanently retaining DMA pages for \
                 resource {}",
                token.resource
            );
            core::mem::forget(token.pages);
        }
    }

    fn retry_retired_2d_resources(&self) {
        let mut retired = self.retired_2d_resources.lock();
        let mut index = 0;
        while index < retired.len() {
            if self.try_retire_2d_resource(&retired[index]) {
                retired.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn try_retire_2d_resource(&self, token: &Retired2dResource) -> bool {
        // DisplayTransport::destroy_resource delegates to the virtio 2D
        // transaction, which detaches backing before unrefing the resource.
        self.transport
            .lock()
            .destroy_resource(token.resource)
            .is_ok()
    }

    fn retire_render_resource(
        &self,
        resource: u32,
        pages: Arc<SharedPages>,
        backing_attached: bool,
    ) {
        self.retry_retired_render_resources();
        let mut token = RetiredRenderResource {
            resource,
            pages,
            backing_attached,
            stage: if backing_attached {
                RenderRetireStage::DetachBacking
            } else {
                RenderRetireStage::Unref
            },
        };
        if self.try_retire_render_resource(&mut token) {
            return;
        }
        let mut retired = self.retired_render_resources.lock();
        if retired.len() < MAX_RETIRED_RESOURCES && retired.try_reserve(1).is_ok() {
            retired.push(token);
        } else {
            self.final_render_leaks.fetch_add(1, Ordering::Relaxed);
            error!(
                "virtio-gpu: render retirement queue full; permanently retaining DMA pages for \\
                 resource {}",
                token.resource
            );
            core::mem::forget(token.pages);
        }
    }

    /// Makes one best-effort pass. This is called before subsequent render
    /// operations, which avoids needing an unbounded background queue.
    fn retry_retired_render_resources(&self) {
        let mut retired = self.retired_render_resources.lock();
        let mut index = 0;
        while index < retired.len() {
            if self.try_retire_render_resource(&mut retired[index]) {
                retired.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn try_retire_render_resource(&self, token: &mut RetiredRenderResource) -> bool {
        let mut transport = self.transport.lock();
        if token.stage == RenderRetireStage::DetachBacking {
            if transport.render_detach_backing(token.resource).is_err() {
                return false;
            }
            token.backing_attached = false;
            token.stage = RenderRetireStage::Unref;
        }
        transport.render_unref(token.resource).is_ok()
    }
}

impl<T: GpuTransport> Drop for AdapterState<T> {
    fn drop(&mut self) {
        let mut retired_2d = core::mem::take(self.retired_2d_resources.get_mut());
        for token in retired_2d.drain(..) {
            if !self.try_retire_2d_resource(&token) {
                self.final_2d_leaks.fetch_add(1, Ordering::Relaxed);
                error!(
                    "virtio-gpu: adapter shutdown retaining 2D DMA pages for resource {}",
                    token.resource
                );
                core::mem::forget(token.pages);
            }
        }
        let mut retired = core::mem::take(self.retired_render_resources.get_mut());
        for token in retired.drain(..) {
            let mut token = token;
            if !self.try_retire_render_resource(&mut token) {
                self.final_render_leaks.fetch_add(1, Ordering::Relaxed);
                error!(
                    "virtio-gpu: adapter shutdown retaining DMA pages for resource {}",
                    token.resource
                );
                core::mem::forget(token.pages);
            }
        }
    }
}

struct VirtioGemBacking<T: GpuTransport> {
    pages: Arc<SharedPages>,
    resource: u32,
    // A backing may be exported and outlive every DRM file/device handle. It
    // must therefore keep the transport state alive until destroy_resource
    // has detached its host DMA backing and unrefed the resource.
    adapter: Arc<AdapterState<T>>,
}

impl<T: GpuTransport> GemBacking for VirtioGemBacking<T> {
    fn shared_pages(&self) -> DrmResult<Arc<SharedPages>> {
        Ok(self.pages.clone())
    }
}

impl<T: GpuTransport> Drop for VirtioGemBacking<T> {
    fn drop(&mut self) {
        self.adapter
            .retire_resource(self.resource, self.pages.clone());
    }
}

impl<T: GpuTransport + 'static> DisplayAdapter for VirtioGpuAdapter<T> {
    fn preferred_mode(&self) -> super::Mode {
        let (width, height) = self.state.transport.lock().preferred_mode();
        super::Mode {
            width,
            height,
            refresh_millihz: 60_000,
        }
    }
    fn create_dumb(
        &self,
        request: DumbRequest,
        pitch: u32,
        size: u64,
    ) -> DrmResult<Arc<dyn GemBacking>> {
        self.state.retry_retired_2d_resources();
        if request.bpp != 32
            || pitch < request.width.checked_mul(4).ok_or(DrmError::Overflow)?
            || pitch % 4 != 0
        {
            return Err(DrmError::Unsupported);
        }
        let size = usize::try_from(size).map_err(|_| DrmError::Overflow)?;
        let bytes = checked_align_up(size, PageSize::Size4K as usize).ok_or(DrmError::Overflow)?;
        let pages =
            Arc::try_new(SharedPages::new_fixed(bytes, PageSize::Size4K).map_err(map_ax_error)?)
                .map_err(|_| DrmError::NoMemory)?;
        let mut entries: Vec<(u64, u32)> = Vec::new();
        entries
            .try_reserve_exact(pages.len())
            .map_err(|_| DrmError::NoMemory)?;
        for index in 0..pages.len() {
            let paddr = pages.paddr_at(index).map_err(map_ax_error)?.as_usize() as u64;
            let merged = if let Some((base, length)) = entries.last_mut() {
                if base.checked_add(*length as u64) == Some(paddr) {
                    *length = length
                        .checked_add(PageSize::Size4K as u32)
                        .ok_or(DrmError::Overflow)?;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !merged {
                entries.push((paddr, PageSize::Size4K as u32));
            }
        }
        let resource_width = pitch / 4;
        let resource = self
            .state
            .transport
            .lock()
            .create_resource(resource_width, request.height, &entries)
            .map_err(map_dev_error)?;
        let backing = match Arc::try_new(VirtioGemBacking {
            pages: pages.clone(),
            resource,
            adapter: self.state.clone(),
        }) {
            Ok(backing) => backing,
            Err(_) => {
                // The resource is already attached to `pages`.  Destroy it
                // before dropping the final caller-side reference; on detach
                // failure `retire_resource` retains (or deliberately leaks)
                // the backing rather than letting the device DMA freed pages.
                self.state.retire_resource(resource, pages);
                return Err(DrmError::NoMemory);
            }
        };
        // `DisplayAdapter` is always reached through its owning Arc. Install
        // the map before publishing the type-erased backing to DRM.
        let key = Arc::as_ptr(&backing) as usize;
        let mut resources = self.state.resources.lock();
        if resources.try_reserve(1).is_err() {
            drop(resources);
            return Err(DrmError::NoMemory);
        }
        resources.push((key, resource));
        drop(resources);
        Ok(backing)
    }

    fn present(&self, scanout: Scanout) -> DrmResult<()> {
        self.state.retry_retired_2d_resources();
        if scanout.bpp != 32
            || scanout.pitch < scanout.width.checked_mul(4).ok_or(DrmError::Overflow)?
        {
            return Err(DrmError::Unsupported);
        }
        let resource = self.state.resource_for(&scanout.backing)?;
        self.state
            .transport
            .lock()
            .present_resource(resource, scanout.width, scanout.height)
            .map_err(map_dev_error)
    }
}

fn map_dev_error(error: DevError) -> DrmError {
    match error {
        DevError::InvalidParam => DrmError::Invalid,
        DevError::NoMemory => DrmError::NoMemory,
        DevError::ResourceBusy | DevError::AlreadyExists => DrmError::Busy,
        DevError::Again => DrmError::QueueFull,
        DevError::Unsupported => DrmError::Unsupported,
        DevError::BadState | DevError::Io => DrmError::NotFound,
    }
}

fn map_ax_error(error: axerrno::AxError) -> DrmError {
    match error {
        axerrno::AxError::NoMemory => DrmError::NoMemory,
        axerrno::AxError::InvalidInput => DrmError::Invalid,
        _ => DrmError::Unsupported,
    }
}

/// Claims a VirtIO GPU from axdisplay and publishes the single DRM device.
/// No compatible GPU simply leaves the legacy display path untouched.
pub fn init() -> DrmResult<bool> {
    let Some(display) = axdisplay::take_drm_display() else {
        return Ok(false);
    };
    let adapter = VirtioGpuAdapter::new(DisplayTransport(Box::new(display)));
    let candidate: Arc<dyn RenderAdapter> = Arc::new(VirtioRenderAdapter {
        state: adapter.state.clone(),
    });
    // Do not create a render node for a plain 2D virtio-gpu.  Querying the
    // first capset also proves the VIRGL transport completed a round trip.
    let render = candidate
        .capset_info(0)
        .ok()
        .and_then(|(id, ..)| (id == 1).then_some(candidate));
    let adapter: Arc<dyn DisplayAdapter> = Arc::new(adapter);
    super::register_primary_device(DrmDevice::with_render(adapter, render, 1, 2, 3, 4))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        created: Vec<(u32, u32, u64, u32)>,
        presented: Vec<(u32, u32, u32)>,
        destroy_fails: bool,
        destroys: u32,
        destroy_calls: Arc<AtomicUsize>,
        transport_drops: Arc<AtomicUsize>,
        render_next: u32,
        render_attach_fails: u32,
        render_detach_fails: u32,
        render_unref_fails: u32,
        render_creates: u32,
        render_detaches: u32,
        render_unrefs: u32,
    }
    impl GpuTransport for FakeTransport {
        fn create_resource(
            &mut self,
            width: u32,
            height: u32,
            entries: &[(u64, u32)],
        ) -> Result<u32, DevError> {
            let (paddr, length) = entries[0];
            self.created.push((width, height, paddr, length));
            Ok(7)
        }
        fn present_resource(
            &mut self,
            resource: u32,
            width: u32,
            height: u32,
        ) -> Result<(), DevError> {
            self.presented.push((resource, width, height));
            Ok(())
        }
        fn destroy_resource(&mut self, _: u32) -> Result<(), DevError> {
            self.destroys += 1;
            self.destroy_calls.fetch_add(1, Ordering::Relaxed);
            if self.destroy_fails {
                Err(DevError::Io)
            } else {
                Ok(())
            }
        }
        fn render_create_resource(&mut self, _: RenderResource) -> Result<u32, DevError> {
            self.render_creates += 1;
            self.render_next += 1;
            Ok(self.render_next)
        }
        fn render_attach_backing(&mut self, _: u32, _: &[(u64, u32)]) -> Result<(), DevError> {
            if self.render_attach_fails > 0 {
                self.render_attach_fails -= 1;
                Err(DevError::Io)
            } else {
                Ok(())
            }
        }
        fn render_detach_backing(&mut self, _: u32) -> Result<(), DevError> {
            self.render_detaches += 1;
            if self.render_detach_fails > 0 {
                self.render_detach_fails -= 1;
                Err(DevError::Io)
            } else {
                Ok(())
            }
        }
        fn render_unref(&mut self, _: u32) -> Result<(), DevError> {
            self.render_unrefs += 1;
            if self.render_unref_fails > 0 {
                self.render_unref_fails -= 1;
                Err(DevError::Io)
            } else {
                Ok(())
            }
        }
    }

    impl Drop for FakeTransport {
        fn drop(&mut self) {
            self.transport_drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn fake_transport_gets_page_aligned_backing_and_scanout() {
        let adapter = VirtioGpuAdapter::new(FakeTransport::default());
        let backing = adapter
            .create_dumb(
                DumbRequest {
                    width: 17,
                    height: 2,
                    bpp: 32,
                },
                128,
                256,
            )
            .unwrap();
        adapter
            .present(Scanout {
                backing,
                width: 17,
                height: 2,
                pitch: 128,
                bpp: 32,
                mode: super::super::Mode {
                    width: 17,
                    height: 2,
                    refresh_millihz: 60_000,
                },
            })
            .unwrap();
        let transport = adapter.state.transport.lock();
        assert_eq!(transport.created[0].0, 32);
        assert_eq!(transport.created[0].3, 4096);
        assert_eq!(transport.presented, [(7, 17, 2)]);
    }

    #[test]
    fn failed_detach_retains_dma_backing() {
        let mut transport = FakeTransport::default();
        transport.destroy_fails = true;
        let adapter = VirtioGpuAdapter::new(transport);
        let backing = adapter
            .create_dumb(
                DumbRequest {
                    width: 16,
                    height: 1,
                    bpp: 32,
                },
                64,
                64,
            )
            .unwrap();
        drop(backing);
        assert_eq!(adapter.state.transport.lock().destroys, 1);
        assert_eq!(adapter.state.retired_2d_resources.lock().len(), 1);
    }

    #[test]
    fn exported_2d_backing_keeps_transport_alive_until_detach_then_unref() {
        let destroys = Arc::new(AtomicUsize::new(0));
        let transport_drops = Arc::new(AtomicUsize::new(0));
        let mut transport = FakeTransport::default();
        transport.destroy_calls = destroys.clone();
        transport.transport_drops = transport_drops.clone();
        let adapter = VirtioGpuAdapter::new(transport);
        let state = Arc::downgrade(&adapter.state);
        let backing = adapter
            .create_dumb(
                DumbRequest {
                    width: 16,
                    height: 1,
                    bpp: 32,
                },
                64,
                64,
            )
            .unwrap();
        drop(adapter);
        assert!(state.upgrade().is_some());
        assert_eq!(destroys.load(Ordering::Relaxed), 0);
        assert_eq!(transport_drops.load(Ordering::Relaxed), 0);
        drop(backing);
        assert!(state.upgrade().is_none());
        assert_eq!(destroys.load(Ordering::Relaxed), 1);
        assert_eq!(transport_drops.load(Ordering::Relaxed), 1);
    }

    fn render_pages() -> Arc<SharedPages> {
        Arc::new(SharedPages::new_fixed(4096, PageSize::Size4K).unwrap())
    }

    #[test]
    fn render_detach_failure_retries_before_freeing_pages() {
        let mut transport = FakeTransport::default();
        transport.render_detach_fails = 1;
        let adapter = VirtioGpuAdapter::new(transport);
        adapter
            .state
            .retire_render_resource(41, render_pages(), true);
        assert_eq!(adapter.state.retired_render_resources.lock().len(), 1);
        adapter.state.retry_retired_render_resources();
        assert!(adapter.state.retired_render_resources.lock().is_empty());
        let transport = adapter.state.transport.lock();
        assert_eq!(transport.render_detaches, 2);
        assert_eq!(transport.render_unrefs, 1);
    }

    #[test]
    fn render_unref_failure_retries_without_a_second_detach() {
        let mut transport = FakeTransport::default();
        transport.render_unref_fails = 1;
        let adapter = VirtioGpuAdapter::new(transport);
        adapter
            .state
            .retire_render_resource(42, render_pages(), true);
        assert_eq!(adapter.state.retired_render_resources.lock().len(), 1);
        adapter.state.retry_retired_render_resources();
        assert!(adapter.state.retired_render_resources.lock().is_empty());
        let transport = adapter.state.transport.lock();
        assert_eq!(transport.render_detaches, 1);
        assert_eq!(transport.render_unrefs, 2);
    }

    #[test]
    fn failed_render_attach_unrefs_and_does_not_exhaust_retirement_capacity() {
        let mut transport = FakeTransport::default();
        transport.render_attach_fails = 1;
        let adapter = VirtioGpuAdapter::new(transport);
        let render: Arc<dyn RenderAdapter> = Arc::new(VirtioRenderAdapter {
            state: adapter.state.clone(),
        });
        let resource = RenderResource {
            target: 2,
            format: 1,
            bind: 0,
            width: 1,
            height: 1,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: 0,
        };
        assert!(
            render
                .create_resource(resource, &[(0, 4096)], render_pages())
                .is_err()
        );
        for _ in 0..129 {
            let pages = render_pages();
            let id = render
                .create_resource(resource, &[(0, 4096)], pages.clone())
                .unwrap();
            render.retire_resource(id, pages);
        }
        let transport = adapter.state.transport.lock();
        assert_eq!(transport.render_creates, 130);
        assert_eq!(transport.render_detaches, 129);
        assert_eq!(transport.render_unrefs, 130);
        assert!(adapter.state.retired_render_resources.lock().is_empty());
        assert_eq!(adapter.state.final_render_leaks.load(Ordering::Relaxed), 0);
    }
}
