//! Descriptor facts: Linux r8169_main.c:583-586, 650-660, 4790-4823.
use core::{
    ptr,
    sync::atomic::{Ordering, fence},
};
pub const OWN: u32 = 1 << 31;
pub const END: u32 = 1 << 30;
pub const FIRST: u32 = 1 << 29;
pub const LAST: u32 = 1 << 28;
pub const ERROR: u32 = 1 << 21;
pub const BUFFER: usize = 2048;
pub const MAX_FRAME: usize = 1514;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Descriptor {
    pub options: u32,
    pub extra: u32,
    pub address: u64,
}
const _: () = assert!(size_of::<Descriptor>() == 16);

/// # Safety
/// The pointer must name a live, aligned descriptor in owned coherent DMA memory.
/// Reading OWN precedes reading payload, which is device-written coherent RAM.
pub unsafe fn status(pointer: *const Descriptor) -> u32 {
    let value = unsafe { ptr::addr_of!((*pointer).options).read_volatile() }.to_le();
    if value & OWN == 0 {
        fence(Ordering::Acquire);
    }
    value
}
/// # Safety
/// The pointer must name an owned descriptor not concurrently accessed by CPU borrowers.
/// Publish all fields before transferring ownership, never as one aggregate write.
pub unsafe fn publish(pointer: *mut Descriptor, address: u64, options: u32) {
    unsafe {
        ptr::addr_of_mut!((*pointer).address).write_volatile(address.to_le());
        ptr::addr_of_mut!((*pointer).extra).write_volatile(0);
        fence(Ordering::Release);
        ptr::addr_of_mut!((*pointer).options).write_volatile(options.to_le());
    }
}
pub fn receive_length(status: u32) -> Option<usize> {
    let bytes = (status & 0x3fff) as usize;
    if status & (OWN | ERROR) != 0
        || status & (FIRST | LAST) != FIRST | LAST
        || !(18..=MAX_FRAME + 4).contains(&bytes)
    {
        return None;
    }
    Some(bytes - 4) // RTL receives the Ethernet FCS; the stack must not see it.
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_fragmentation_error_and_fcs_are_checked() {
        assert_eq!(receive_length(FIRST | LAST | 64), Some(60));
        for bits in [
            OWN | FIRST | LAST | 64,
            ERROR | FIRST | LAST | 64,
            FIRST | 64,
            LAST | 64,
            FIRST | LAST | 3,
            FIRST | LAST | 1600,
        ] {
            assert_eq!(receive_length(bits), None);
        }
    }
    #[test]
    fn descriptor_publication_preserves_a_64_bit_dma_address() {
        let mut d = Descriptor::default();
        unsafe {
            publish(&mut d, 0x1234_5678_9000, OWN | FIRST | LAST | END | 60);
        }
        assert_eq!(d.address, 0x1234_5678_9000);
        assert_eq!(d.extra, 0);
        assert_eq!(unsafe { status(&d) }, OWN | FIRST | LAST | END | 60);
    }
}
