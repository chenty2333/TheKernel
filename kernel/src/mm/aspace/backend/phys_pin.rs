use alloc::collections::BTreeMap;

use axerrno::{AxError, AxResult};
use axhal::paging::PageSize;
use kspin::SpinNoIrq;
use memory_addr::PhysAddr;

use super::dealloc_frame_now;

#[derive(Clone, Copy)]
struct PinnedFrame {
    pins: u32,
    pending_free: Option<PageSize>,
}

impl PinnedFrame {
    const fn new() -> Self {
        Self {
            pins: 1,
            pending_free: None,
        }
    }
}

#[derive(Default)]
struct PinnedFrameTable {
    frames: BTreeMap<PhysAddr, PinnedFrame>,
}

static PINNED_FRAMES: SpinNoIrq<PinnedFrameTable> = SpinNoIrq::new(PinnedFrameTable {
    frames: BTreeMap::new(),
});

pub(crate) struct PhysicalFramePin {
    paddr: PhysAddr,
}

impl Drop for PhysicalFramePin {
    fn drop(&mut self) {
        let pending_free = {
            let mut table = PINNED_FRAMES.lock();
            let Some(frame) = table.frames.get_mut(&self.paddr) else {
                warn!(
                    "PhysicalFramePin::drop: missing pinned frame entry for {:?}",
                    self.paddr
                );
                return;
            };
            assert!(frame.pins > 0, "dropping unpinned frame");
            frame.pins -= 1;
            if frame.pins == 0 {
                let pending_free = frame.pending_free;
                table.frames.remove(&self.paddr);
                pending_free
            } else {
                None
            }
        };

        if let Some(page_size) = pending_free {
            dealloc_frame_now(self.paddr, page_size);
        }
    }
}

pub(crate) fn pin_frame(paddr: PhysAddr) -> AxResult<PhysicalFramePin> {
    let mut table = PINNED_FRAMES.lock();
    match table.frames.get_mut(&paddr) {
        Some(frame) => {
            frame.pins = frame.pins.checked_add(1).ok_or(AxError::NoMemory)?;
        }
        None => {
            table.frames.insert(paddr, PinnedFrame::new());
        }
    }
    Ok(PhysicalFramePin { paddr })
}

pub(crate) fn defer_frame_dealloc_if_pinned(paddr: PhysAddr, page_size: PageSize) -> bool {
    let mut table = PINNED_FRAMES.lock();
    let Some(frame) = table.frames.get_mut(&paddr) else {
        return false;
    };
    if let Some(existing) = frame.pending_free {
        assert_eq!(existing, page_size, "pinned frame free size changed");
    } else {
        frame.pending_free = Some(page_size);
    }
    true
}
