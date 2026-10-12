// SPDX-License-Identifier: MIT
//! i915 is the only GGTT and DMA owner of the GPU. Before its probe, the
//! firmware's initial planes are read ([`super::initial_plane`]) so that their
//! GGTT ranges can be reserved in i915's DRM-MM as soon as it exists, before
//! `init_ggtt()` clears every other hole -- the job Linux's initial-plane vma
//! pin does until the display code is ported.
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{ops::Range, ptr};

use intel_gt::{
    intel_context_upstream::DrmMmNode,
    intel_gtt_api_upstream::I915AddressSpace,
    linux::{drm_mm, ggtt_owner},
};

// Each allocation must retain a stable address while its node is linked into
// the C DRM-MM tree; Vec<DrmMmNode> would relocate nodes on growth.
#[allow(clippy::vec_box)]
struct Attached {
    vm: usize,
    nodes: Vec<Box<DrmMmNode>>,
}
// SAFETY: moving Attached between callback invocations never moves the node
// allocations themselves (they remain boxed). The vm value is only an identity
// token used for equality, while linked-node dereferences/removal are serialized
// by OWNER and the callback contract keeps that VM alive until detach. Attach and
// detach are only admitted outside active GT submissions.
unsafe impl Send for Attached {}
struct Owner {
    ranges: Vec<Range<u64>>,
    attached: Option<Attached>,
}
static OWNER: spin::Mutex<Option<Owner>> = spin::Mutex::new(None);

///
/// # Safety
/// `vm` must point to an initialized, live i915 address space exclusively
/// owned for this attach transaction and remain live until the returned nodes
/// are removed.
unsafe fn reserve(vm: *mut I915AddressSpace, ranges: &[Range<u64>]) -> Result<Attached, i32> {
    if vm.is_null() {
        return Err(-22);
    }
    // SAFETY: caller supplies the initialized, live i915 address-space passed to
    // the owner callback; detach is not allowed until this reservation operation ends.
    let total = unsafe { (*vm).total };
    let mut attached = Attached {
        vm: vm as usize,
        nodes: Vec::new(),
    };
    attached
        .nodes
        .try_reserve_exact(ranges.len())
        .map_err(|_| -12)?;
    for range in ranges {
        if range.start >= range.end || range.end > total || (range.start | range.end) & 4095 != 0 {
            // SAFETY: all preceding nodes in attached are linked to this still-live vm;
            // remove holds exclusive callback ownership and vm has not begun teardown.
            unsafe { remove(&mut attached) };
            return Err(-22);
        }
        // Allocate all node storage before linking it into the DRM-MM tree.
        // SAFETY: DrmMmNode is a C ABI record of integer/pointer fields; its all-zero
        // representation is a valid detached node before drm_mm initializes links.
        let mut node = match Box::try_new(unsafe { core::mem::zeroed::<DrmMmNode>() }) {
            Ok(node) => node,
            Err(_) => {
                // SAFETY: earlier nodes in attached are linked to vm; unwind them while
                // this callback still has exclusive ownership of the initialized VM.
                unsafe { remove(&mut attached) };
                return Err(-12);
            }
        };
        node.start = range.start;
        node.size = range.end - range.start;
        node.color = u64::MAX; // I915_COLOR_UNEVICTABLE
        // SAFETY: vm is initialized and live, and node's boxed storage outlives its
        // membership in that VM's DRM-MM tree; range bounds were checked above.
        let result = unsafe { drm_mm::drm_mm_reserve_node(ptr::addr_of_mut!((*vm).mm), &mut *node) };
        if result != 0 {
            // SAFETY: only earlier successful reservations are in attached.nodes;
            // the failed node was not linked, and the VM remains live.
            unsafe { remove(&mut attached) };
            return Err(result);
        }
        attached.nodes.push(node);
    }
    Ok(attached)
}
///
/// # Safety
/// Every boxed node must still be linked into the live DRM-MM tree identified
/// by `attached.vm`, and the caller must hold exclusive tree ownership.
unsafe fn remove(attached: &mut Attached) {
    for node in attached.nodes.iter_mut().rev() {
        // SAFETY: reserve() places only successfully linked node allocations here;
        // attached.vm identifies their still-live tree and OWNER excludes mutation.
        unsafe { drm_mm::drm_mm_remove_node(&mut **node) };
    }
    attached.nodes.clear();
}
///
/// # Safety
/// `vm` must be the initialized live address space supplied by i915's owner
/// callback, with teardown deferred until the matching detach callback.
unsafe fn attach(vm: *mut I915AddressSpace) -> i32 {
    let mut owner = OWNER.lock();
    let Some(owner) = owner.as_mut() else {
        return -19;
    };
    if vm.is_null() {
        return -22;
    }
    if owner.attached.is_some() {
        return -16;
    }
    // SAFETY: callback provides a live initialized VM, OWNER is held exclusively,
    // and reserve() validates each fixed range before linking any node.
    match unsafe { reserve(vm, &owner.ranges) } {
        Ok(attached) => {
            owner.attached = Some(attached);
            0
        }
        Err(error) => error,
    }
}
///
/// # Safety
/// `vm` must be the address-space identity supplied by the matching i915
/// detach callback, which runs before that address space is destroyed.
unsafe fn detach(vm: *mut I915AddressSpace) {
    let mut owner = OWNER.lock();
    let Some(owner) = owner.as_mut() else {
        return;
    };
    if owner.attached.as_ref().is_some_and(|a| a.vm == vm as usize) {
        let mut attached = owner.attached.take().unwrap();
        // SAFETY: OWNER excludes all concurrent attach/detach operations, and the
        // matching detach callback runs before i915 destroys this address space.
        unsafe { remove(&mut attached) };
    }
}
static CALLBACKS: ggtt_owner::GgttOwner = ggtt_owner::GgttOwner { attach, detach };

/// Read the firmware's initial planes, take the requester's identity DMA
/// lease with their stolen pages, and install the GGTT reservation callback.
/// Once per boot; nothing here writes the device.
#[cfg(target_os = "none")]
pub(super) fn prepare(
    bdf: super::pci::Bdf,
    window: &super::regs::RegisterWindow,
    device: *mut core::ffi::c_void,
) -> Result<(), alloc::string::String> {
    use alloc::{format, string::String};

    let mut owner = OWNER.lock();
    if owner.is_some() {
        return Err(String::from("upstream i915: GGTT already owned"));
    }
    let ecam = super::pci::Ecam::platform()
        .ok_or_else(|| String::from("upstream i915: PCI facts unavailable"))?;
    let info = super::pci::DeviceInfo::read(&ecam, bdf)
        .ok_or_else(|| String::from("upstream i915: PCI device unavailable"))?;
    let bar0 = info.bars[0]
        .address()
        .filter(|&address| address != 0)
        .ok_or_else(|| String::from("upstream i915: GTTMMADR unassigned"))?;
    let planes = match super::initial_plane::stolen_range(&ecam, window, bdf) {
        Some(stolen) => super::initial_plane::read_initial_planes(window, bar0, &stolen),
        None => {
            axlog::warn!("upstream i915: stolen memory not decoded; no initial plane kept");
            Vec::new()
        }
    };
    let mut pages = Vec::new();
    let mut ranges = Vec::new();
    for plane in &planes {
        pages.extend_from_slice(&plane.pages);
        ranges.push(plane.ggtt.clone());
    }
    let lease = super::dma::acquire_identity_lease(bdf, &pages)
        .map_err(|e| format!("upstream i915: identity DMA lease refused: {e:?}"))?;
    let lease = Arc::try_new(lease)
        .map_err(|_| String::from("upstream i915: identity DMA lease allocation failed"))?;
    tk_linux_kapi::dma::register_identity_device(device, lease).map_err(String::from)?;
    ggtt_owner::install(&CALLBACKS).map_err(String::from)?;
    *owner = Some(Owner {
        ranges,
        attached: None,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservations_exclude_native_and_firmware_ranges_and_unwind_conflicts() {
        // SAFETY: test owns a zero-initialized local VM, initializes/takes down its
        // DRM-MM tree in this scope, and removes every successful test reservation.
        unsafe {
            let mut vm = Box::new(core::mem::zeroed::<I915AddressSpace>());
            let total = 16 * 4096;
            vm.total = total;
            drm_mm::drm_mm_init(&mut vm.mm, 0, total);
            let ranges = [4096..8192, 8 * 4096..12 * 4096];
            let mut attached = reserve(&mut *vm, &ranges).unwrap();
            assert!(reserve(&mut *vm, &[0..4096, 4096..8192]).is_err());
            // The first node from the failed batch was removed, the original
            // firmware/native reservations remain live.
            let mut low = reserve(&mut *vm, &[0..4096]).unwrap();
            assert!(reserve(&mut *vm, &[8 * 4096..9 * 4096]).is_err());
            remove(&mut low);
            remove(&mut attached);
            drm_mm::drm_mm_takedown(&mut vm.mm);
        }
    }
}
