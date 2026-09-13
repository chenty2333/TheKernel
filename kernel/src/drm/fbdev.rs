//! DRM-owned fbdev emulation for the sole primary connector.
//!
//! fbdev is represented by a real dumb GEM object and a normal DRM
//! framebuffer.  It intentionally owns DRM master while the text console is
//! active, yielding to the first userspace primary opener or a VT/session
//! handoff. There is no second, raw-display scanout path.

use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axhal::mem::phys_to_virt;
use axsync::Mutex;
use memory_addr::{PAGE_SIZE_4K, PhysAddr};

use super::{
    DrmDevice, DrmError, DrmFile, DrmResult, DumbRequest, FramebufferId, Mode, atomic::Change,
    property,
};
use crate::{
    mm::SharedPages,
    pseudofs::{
        DeviceMmap,
        dev::scanout::{PixelLayout, ScanoutSurface},
    },
};

/// The kernel's single fbdev scanout.  The GEM object remains owned by this
/// private DRM file for its entire lifetime, so fbcon and `/dev/fb0` always
/// draw into the same buffer that KMS presents.
pub struct DrmFbdev {
    file: DrmFile,
    framebuffer: FramebufferId,
    mode: Mode,
    virtual_height: u32,
    pitch: u32,
    size: usize,
    pages: Arc<SharedPages>,
    yoffset: Mutex<u32>,
}

impl DrmFbdev {
    pub fn new(device: Arc<DrmDevice>) -> DrmResult<Self> {
        let mode = device.preferred_mode();
        if mode.width == 0 || mode.height == 0 {
            return Err(DrmError::Invalid);
        }
        let file = device.open_fbdev_primary();
        file.become_master()?;
        let virtual_height = mode.height.checked_mul(2).ok_or(DrmError::Overflow)?;
        let dumb = file.create_dumb(DumbRequest {
            width: mode.width,
            height: virtual_height,
            bpp: 32,
        })?;
        let framebuffer =
            file.add_framebuffer(dumb.handle, mode.width, virtual_height, dumb.pitch, 32)?;
        let pages = file.gem(dumb.handle)?.backing.shared_pages()?;
        let size = usize::try_from(dumb.size).map_err(|_| DrmError::Overflow)?;
        let fbdev = Self {
            file,
            framebuffer,
            mode,
            virtual_height,
            pitch: dumb.pitch,
            size,
            pages,
            yoffset: Mutex::new(0),
        };
        fbdev.commit_mode()?;
        Ok(fbdev)
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn pitch(&self) -> u32 {
        self.pitch
    }

    pub fn virtual_height(&self) -> u32 {
        self.virtual_height
    }

    pub fn yoffset(&self) -> u32 {
        *self.yoffset.lock()
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn pages(&self) -> Arc<SharedPages> {
        self.pages.clone()
    }

    /// Publish current fbdev content through the same atomic state machine as
    /// all KMS clients.  A completed synchronous commit is the fbdev
    /// equivalent of a zero-offset page flip.
    pub fn present(&self) -> DrmResult<()> {
        self.file.submit_legacy_atomic(&[], None, None, false)
    }

    /// Implements FBIOPAN_DISPLAY by changing the atomic primary-plane source
    /// rectangle.  This is a real page flip: adapters that cannot submit a
    /// non-origin virtio rectangle return Unsupported rather than displaying
    /// the wrong page.
    pub fn pan(&self, yoffset: u32) -> DrmResult<()> {
        if yoffset > self.virtual_height.saturating_sub(self.mode.height) {
            return Err(DrmError::Invalid);
        }
        let resources = self.file.resources();
        self.file.submit_legacy_atomic(
            &[Change {
                object: resources.primary_plane_id,
                property: property::PLANE_SRC_Y,
                value: yoffset.checked_shl(16).ok_or(DrmError::Overflow)? as u64,
            }],
            None,
            None,
            false,
        )?;
        *self.yoffset.lock() = yoffset;
        Ok(())
    }

    /// DPMS is an atomic connector property.  We retain the framebuffer while
    /// blanked so unblank restores the exact same scanout without creating a
    /// raw-display ownership side channel.
    pub fn set_blank(&self, blank: bool) -> DrmResult<()> {
        let resources = self.file.resources();
        self.file.submit_legacy_atomic(
            &[Change {
                object: resources.connector.id,
                property: property::CONNECTOR_DPMS,
                value: if blank {
                    super::atomic::DPMS_OFF
                } else {
                    super::atomic::DPMS_ON
                } as u64,
            }],
            None,
            None,
            false,
        )
    }

    /// VT text restoration is a normal atomic fbdev present, never a direct
    /// driver flush.
    pub fn restore_text(&self) -> DrmResult<()> {
        // fbcon renders its bounded text surface at the beginning of the GEM
        // backing. Returning to KD_TEXT therefore first selects page zero;
        // the subsequent fbcon repaint and damage commit target that same
        // scanout region.
        if self.yoffset() != 0 {
            self.pan(0)
        } else {
            // `DisplayCore::resume_refresh` performs the one full repaint
            // after ownership returns; avoid an otherwise duplicate commit.
            Ok(())
        }
    }

    /// Seat rollback must not wait indefinitely for a host presentation.  A
    /// nonblocking atomic restore preserves the same state machine and lets
    /// the vblank worker complete it after the VT transaction has released
    /// all slow locks.
    pub fn restore_text_nonblocking(&self) -> DrmResult<()> {
        self.commit_mode_with_policy(true)?;
        *self.yoffset.lock() = 0;
        Ok(())
    }

    /// Relinquish the primary-node master while a graphics VT owns the seat.
    /// The fbdev GEM remains allocated, but it cannot submit or modeset until
    /// mastership is reacquired for text mode.
    pub fn release_master(&self) {
        self.file.drop_master();
    }

    pub fn acquire_master(&self) -> DrmResult<()> {
        self.file.become_master()
    }

    fn commit_mode(&self) -> DrmResult<()> {
        self.commit_mode_with_policy(false)
    }
    fn commit_mode_with_policy(&self, nonblocking: bool) -> DrmResult<()> {
        let resources = self.file.resources();
        let source_width = self.mode.width.checked_shl(16).ok_or(DrmError::Overflow)?;
        let source_height = self.mode.height.checked_shl(16).ok_or(DrmError::Overflow)?;
        self.file.submit_legacy_atomic(
            &[
                Change {
                    object: resources.connector.id,
                    property: property::CONNECTOR_CRTC_ID,
                    value: resources.crtc.id as u64,
                },
                Change {
                    object: resources.connector.id,
                    property: property::CONNECTOR_DPMS,
                    value: super::atomic::DPMS_ON as u64,
                },
                Change {
                    object: resources.crtc.id,
                    property: property::CRTC_ACTIVE,
                    value: 1,
                },
                Change {
                    object: resources.crtc.id,
                    property: property::CRTC_MODE_ID,
                    value: 0,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_FB_ID,
                    value: self.framebuffer as u64,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_CRTC_ID,
                    value: resources.crtc.id as u64,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_SRC_X,
                    value: 0,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_SRC_Y,
                    value: 0,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_SRC_W,
                    value: source_width as u64,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_SRC_H,
                    value: source_height as u64,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_CRTC_X,
                    value: 0,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_CRTC_Y,
                    value: 0,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_CRTC_W,
                    value: self.mode.width as u64,
                },
                Change {
                    object: resources.primary_plane_id,
                    property: property::PLANE_CRTC_H,
                    value: self.mode.height as u64,
                },
            ],
            Some(self.mode),
            None,
            nonblocking,
        )
    }
}

impl ScanoutSurface for DrmFbdev {
    fn width(&self) -> u32 {
        self.mode.width
    }
    fn height(&self) -> u32 {
        self.mode.height
    }

    fn pitch(&self) -> u32 {
        self.pitch
    }

    fn pixel_layout(&self) -> PixelLayout {
        // The dumb buffer is created at `bpp: 32` and the adapter's scanout is
        // B8G8R8A8, whose little-endian word is the canonical colour value.
        PixelLayout::B8G8R8A8
    }

    fn write_pixel(&self, offset: usize, color: u32) {
        let mut pixel = [0u8; size_of::<u32>()];
        let width = self.pixel_layout().encode_into(color, &mut pixel);
        let _ = self.write_bytes(offset, &pixel[..width]);
    }

    fn virtual_height(&self) -> u32 {
        self.virtual_height
    }

    fn yoffset(&self) -> u32 {
        *self.yoffset.lock()
    }

    fn len(&self) -> usize {
        self.size
    }

    fn read_bytes(&self, mut offset: usize, mut dst: &mut [u8]) -> AxResult<()> {
        while !dst.is_empty() {
            let in_page = offset % PAGE_SIZE_4K;
            let count = dst.len().min(PAGE_SIZE_4K - in_page);
            let page = self.pages.paddr_at(offset / PAGE_SIZE_4K)?;
            // SAFETY: the page index and the chunk length are both bounded by
            // the GEM backing's own size, and the direct map covers that
            // backing for as long as this surface exists.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    phys_to_virt(PhysAddr::from(page.as_usize() + in_page)).as_ptr(),
                    dst.as_mut_ptr(),
                    count,
                )
            };
            offset += count;
            dst = &mut dst[count..];
        }
        Ok(())
    }

    fn write_bytes(&self, mut offset: usize, mut src: &[u8]) -> AxResult<()> {
        while !src.is_empty() {
            let in_page = offset % PAGE_SIZE_4K;
            let count = src.len().min(PAGE_SIZE_4K - in_page);
            let page = self.pages.paddr_at(offset / PAGE_SIZE_4K)?;
            // SAFETY: as for `read_bytes`; the destination is the same bounded
            // GEM backing.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    phys_to_virt(PhysAddr::from(page.as_usize() + in_page)).as_mut_ptr(),
                    count,
                )
            };
            offset += count;
            src = &src[count..];
        }
        Ok(())
    }

    fn mmap(&self) -> DeviceMmap {
        // The scanout is a scatter-gather GEM allocation, so mapping its real
        // pages is what lets a userspace writer see the same buffer KMS
        // presents.  There is no fictitious linear physical base to report.
        DeviceMmap::SharedPages(self.pages())
    }

    fn present(&self) -> AxResult<()> {
        self.file
            .submit_legacy_atomic(&[], None, None, false)
            .map_err(Into::into)
    }

    fn pan(&self, yoffset: u32) -> AxResult<()> {
        DrmFbdev::pan(self, yoffset).map_err(Into::into)
    }

    fn set_blank(&self, blank: bool) -> AxResult<()> {
        DrmFbdev::set_blank(self, blank).map_err(Into::into)
    }

    fn restore_text(&self, nonblocking: bool) -> AxResult<()> {
        if nonblocking {
            self.restore_text_nonblocking()
        } else {
            self.restore_text()
        }
        .map_err(Into::into)
    }

    fn set_master(&self, master: bool) -> AxResult<()> {
        if master {
            self.acquire_master().map_err(Into::into)
        } else {
            self.release_master();
            Ok(())
        }
    }
}

/// Build the DRM-backed scanout surface for a primary display device.
///
/// The fbdev layer takes a [`ScanoutSurface`] instead of discovering a DRM
/// device itself, so that a machine with no virtio-gpu can still publish
/// `/dev/fb0` over a surface the firmware programmed.  This is the DRM side of
/// that choice; the caller decides which surface the machine actually has.
pub(crate) fn drm_scanout(device: Arc<DrmDevice>) -> AxResult<Arc<dyn ScanoutSurface>> {
    let scanout = DrmFbdev::new(device)?;
    Arc::try_new(scanout)
        .map(|scanout| scanout as Arc<dyn ScanoutSurface>)
        .map_err(|_| AxError::NoMemory)
}
