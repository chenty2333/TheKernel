//! Display ownership and legacy framebuffer access.

#![no_std]

#[macro_use]
extern crate log;

#[doc(no_inline)]
pub use axdriver::prelude::DisplayInfo;
use axdriver::{AxDeviceContainer, AxDisplayDevice, prelude::*};
use axsync::Mutex;
use lazyinit::LazyInit;

static MAIN_DISPLAY: LazyInit<Mutex<Option<AxDisplayDevice>>> = LazyInit::new();

pub fn init_display(mut display_devs: AxDeviceContainer<AxDisplayDevice>) {
    info!("Initialize display subsystem...");
    if let Some(dev) = display_devs.take_one() {
        info!("  use display device 0: {:?}", dev.device_name());
        MAIN_DISPLAY.init_once(Mutex::new(Some(dev)));
    } else {
        warn!("  No display device found!");
    }
}

pub fn has_display() -> bool {
    MAIN_DISPLAY.is_inited() && MAIN_DISPLAY.lock().is_some()
}

/// Transfers the one display device to DRM only if it implements the pinned
/// backing transport. After success, framebuffer users observe no display.
pub fn take_drm_display() -> Option<AxDisplayDevice> {
    if !MAIN_DISPLAY.is_inited() {
        return None;
    }
    let mut display = MAIN_DISPLAY.lock();
    display
        .as_ref()
        .is_some_and(|device| device.supports_drm_transport())
        .then(|| display.take())
        .flatten()
}

pub fn framebuffer_info() -> DisplayInfo {
    MAIN_DISPLAY
        .lock()
        .as_ref()
        .expect("display unavailable")
        .info()
}

pub fn framebuffer_flush() -> bool {
    MAIN_DISPLAY
        .lock()
        .as_mut()
        .is_some_and(|display| display.flush().is_ok())
}
