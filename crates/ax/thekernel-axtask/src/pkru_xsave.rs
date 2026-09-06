//! Full per-task standard XSAVE transport.
//!
//! This is deliberately not a PKRU side channel: the same image carries every
//! XCR0-enabled user component (x87/SSE/AVX/AVX-512/AMX where available).

use core::fmt;

use axhal::context::{TaskContext, XsaveLayout, XsaveUnavailable};
use kernel_guard::NoPreemptIrqSave;

const ALIGNMENT: usize = 64;

#[repr(C, align(64))]
#[derive(Clone)]
struct Block([u8; ALIGNMENT]);

/// Owned, exactly sized, 64-byte-aligned full XSAVE image.
pub struct XsaveImage {
    layout: XsaveLayout,
    blocks: alloc::vec::Vec<Block>,
}

impl XsaveImage {
    pub fn new(layout: XsaveLayout) -> Result<Self, XsaveImageError> {
        let blocks = layout.xstate_size.div_ceil(ALIGNMENT);
        let mut storage = alloc::vec::Vec::new();
        storage
            .try_reserve_exact(blocks)
            .map_err(|_| XsaveImageError::Allocation)?;
        // Capacity was reserved above, so resize cannot perform another
        // allocation.  Signal delivery can now fail before publication rather
        // than panic halfway through its frame transaction.
        storage.resize(blocks, Block([0; ALIGNMENT]));
        Ok(Self {
            layout,
            blocks: storage,
        })
    }
    pub fn from_bytes(layout: XsaveLayout, bytes: &[u8]) -> Result<Self, XsaveImageError> {
        if bytes.len() != layout.xstate_size {
            return Err(XsaveImageError::WrongSize {
                expected: layout.xstate_size,
                actual: bytes.len(),
            });
        }
        if !axhal::asm::xsave_image_mxcsr_valid(bytes)
            || !axhal::asm::xsave_image_header_valid(layout, bytes)
        {
            return Err(XsaveImageError::InvalidMxcsr);
        }
        let mut image = Self::new(layout)?;
        image.as_mut_bytes().copy_from_slice(bytes);
        Ok(image)
    }
    pub const fn layout(&self) -> XsaveLayout {
        self.layout
    }
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.blocks.as_ptr().cast(), self.layout.xstate_size) }
    }
    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(
                self.blocks.as_mut_ptr().cast(),
                self.layout.xstate_size,
            )
        }
    }
}

impl fmt::Debug for XsaveImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XsaveImage")
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum XsaveImageError {
    WrongSize { expected: usize, actual: usize },
    Allocation,
    InvalidMxcsr,
}
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum XsaveTaskError {
    Unavailable(XsaveUnavailable),
    InvalidImage,
    Allocation,
}

/// CPU-pinned, prevalidated full-XSAVE commit.
#[must_use = "dropping releases the CPU pin without changing task state"]
pub struct XsaveCommit<'a> {
    guard: NoPreemptIrqSave,
    context: *mut TaskContext,
    layout: XsaveLayout,
    image: Option<&'a XsaveImage>,
}

impl XsaveCommit<'_> {
    pub fn commit(self) {
        let _pin = &self.guard;
        unsafe {
            let ctx = &mut *self.context;
            if let Some(image) = self.image {
                assert!(axhal::asm::restore_xsave_pinned(
                    self.layout,
                    image.as_bytes()
                ));
                assert!(
                    ctx.ext_state
                        .replace_snapshot(self.layout, image.as_bytes())
                );
            } else {
                ctx.reset_extended_state();
            }
        }
    }
}

/// Saves every enabled xfeature from the live current task.
pub fn snapshot_current_task_xsave() -> Result<XsaveImage, XsaveTaskError> {
    let layout = axhal::asm::xsave_layout().map_err(XsaveTaskError::Unavailable)?;
    let _guard = NoPreemptIrqSave::new();
    let task = crate::api::current();
    let mut image = XsaveImage::new(layout).map_err(|error| match error {
        XsaveImageError::Allocation => XsaveTaskError::Allocation,
        _ => XsaveTaskError::InvalidImage,
    })?;
    if !axhal::asm::save_xsave(layout, image.as_mut_bytes()) {
        return Err(XsaveTaskError::InvalidImage);
    }
    unsafe {
        assert!(
            (*task.ctx_mut_ptr())
                .ext_state
                .replace_snapshot(layout, image.as_bytes())
        );
    }
    Ok(image)
}

/// Pins the current task and validates the exact CPU-wide layout before a
/// later non-fallible signal-return transition.
pub fn prepare_current_task_xsave_commit(
    image: &XsaveImage,
) -> Result<XsaveCommit<'_>, XsaveTaskError> {
    let guard = NoPreemptIrqSave::new();
    let layout = axhal::asm::xsave_layout().map_err(XsaveTaskError::Unavailable)?;
    if layout != image.layout
        || !axhal::asm::xsave_image_mxcsr_valid(image.as_bytes())
        || !axhal::asm::xsave_image_header_valid(layout, image.as_bytes())
    {
        return Err(XsaveTaskError::InvalidImage);
    }
    let task = crate::api::current();
    Ok(XsaveCommit {
        guard,
        context: unsafe { task.ctx_mut_ptr() },
        layout,
        image: Some(image),
    })
}

pub fn prepare_current_task_xsave_reset() -> Result<XsaveCommit<'static>, XsaveTaskError> {
    let guard = NoPreemptIrqSave::new();
    let layout = axhal::asm::xsave_layout().map_err(XsaveTaskError::Unavailable)?;
    let task = crate::api::current();
    Ok(XsaveCommit {
        guard,
        context: unsafe { task.ctx_mut_ptr() },
        layout,
        image: None,
    })
}
