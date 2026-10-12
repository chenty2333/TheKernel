// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See the repository MIT license.
//! Per-open Linux DRM-file bridge for the upstream i915 GEM/exec path.
//!
//! The kernel `DrmFile` remains the single userspace handle table for KMS,
//! PRIME, and mmap. This module supplies the source-layout Linux `drm_file`
//! consumed by i915, and mirrors each upstream GEM handle into that same
//! kernel table so core DRM ioctls never see a second, disconnected namespace.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
use core::{
    ffi::{c_int, c_void},
    mem::{size_of, MaybeUninit},
    marker::PhantomData,
    ptr,
    slice,
};

use axerrno::{AxError, AxResult};
use intel_gt::{
    i915_driver_upstream::DrmDriverDesc,
    intel_context_upstream::DrmGemObjectBaseLayout,
    linux::gem,
    linux_i915_private::DrmDevicePrefix,
};
use spin::Mutex;

use crate::{
    drm::{DrmError, DrmFile, DrmResult, GemBacking, GemMemoryCharge, fence::Reservation},
    mm::SharedPages,
};

const GFP_KERNEL: u32 = 0x0cc0;
const GFP_ZERO: u32 = 0x0100;
const MAX_IOCTL_BYTES: usize = 4096;
const I915_MMAP_OFFSET_WB: u64 = 2;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const PAGE_SIZE: usize = 4096;

unsafe extern "C" {
    fn kmalloc(size: usize, flags: u32) -> *mut c_void;
    fn kfree(object: *mut c_void);
    fn radix_tree_insert(root: *mut c_void, index: u64, item: *mut c_void) -> c_int;
    fn radix_tree_delete(root: *mut c_void, index: u64) -> *mut c_void;
    fn idr_find(idr: *const c_void, id: u64) -> *mut c_void;
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct I915MmapOffsetRequest {
    handle: u32,
    pad: u32,
    offset: u64,
    flags: u64,
    extensions: u64,
}

#[derive(Clone)]
struct FileBinding {
    /// Present only during open/ioctl/close callbacks that may bridge back to
    /// the native file. Never retained across the borrowing call's lifetime.
    native_file: usize,
    handles: BTreeSet<u32>,
}

/// The Linux DRM file's GEM IDR is read by i915 under RCU and updated by GEM
/// create/close callbacks. The current radix-tree LinuxKPI backing is an
/// ordered map, so serialize its tree operations until that owner supplies
/// Linux's lockless radix-tree/RCU slot implementation.
static GEM_IDR_GATE: Mutex<()> = Mutex::new(());
static FILES: Mutex<BTreeMap<usize, FileBinding>> = Mutex::new(BTreeMap::new());

/// Per-open i915 `drm_file` allocation. Pointer values are stored as integers
/// so the kernel's sleeping file mutex can own this record across threads.
pub(crate) struct UpstreamFile {
    drm_file: usize,
    minor: usize,
    filp: usize,
    device: usize,
    driver: usize,
    opened: bool,
}

impl UpstreamFile {
    pub(crate) fn new(native: &DrmFile) -> AxResult<Self> {
        let device = super::upstream_drm::registered_device().ok_or(AxError::NoSuchDevice)?;
        // SAFETY: registered_device() returns the live registered DRM device;
        // its prefix is the layout consumed by the LinuxKPI DRM bridge.
        let driver = unsafe { (*device.cast::<DrmDevicePrefix>()).driver };
        if driver.is_null() {
            return Err(AxError::NoSuchDevice);
        }
        // SAFETY: kmalloc returns suitably aligned storage; null is checked below.
        let minor = unsafe { kmalloc(size_of::<gem::DrmMinor>(), GFP_KERNEL | GFP_ZERO) }
            .cast::<gem::DrmMinor>();
        if minor.is_null() {
            return Err(AxError::NoMemory);
        }
        // SAFETY: kmalloc returns suitably aligned storage; null is checked below.
        let drm_file = unsafe { kmalloc(size_of::<gem::DrmFile>(), GFP_KERNEL | GFP_ZERO) }
            .cast::<gem::DrmFile>();
        if drm_file.is_null() {
            // SAFETY: minor is the still-owned allocation returned above.
            unsafe { kfree(minor.cast()) };
            return Err(AxError::NoMemory);
        }
        // SAFETY: the static C string is NUL-terminated and remains live for the call.
        let filp = unsafe {
            intel_gt::linux::shmem::shmem_file_setup(c"i915-drm-ofd".as_ptr(), 0, 0)
        };
        if intel_gt::linux_config::IS_ERR(filp) {
            let error = linux_errno(intel_gt::linux_config::PTR_ERR(filp));
            // SAFETY: both allocations and the shmem file are still owned here.
            unsafe {
                kfree(drm_file.cast());
                kfree(minor.cast());
            }
            return Err(error);
        }
        // SAFETY: shmem_file_setup returned a live file and its error pointer was rejected.
        unsafe { (*filp).f_flags = native.open_flags() };
        // SAFETY: minor and drm_file are zeroed allocations of these exact layouts;
        // device and filp are live, and this function owns all three until close.
        unsafe {
            (*minor).dev = device;
            (*drm_file).minor = minor;
            // drm_open_init() starts with an empty idr. Keep a real LinuxKPI
            // file owner so i915 can read f_flags and use its page-cache state.
            (*drm_file).filp = filp;
            (*drm_file).driver_priv = ptr::null_mut();
        }
        let file_key = drm_file as usize;
        FILES.lock().insert(
            file_key,
            FileBinding {
                native_file: 0,
                handles: BTreeSet::new(),
            },
        );
        let active = match NativeFileBinding::bind(file_key, native) {
            Ok(active) => active,
            Err(error) => {
                FILES.lock().remove(&file_key);
                // SAFETY: binding setup failed before the driver open callback; these
                // allocations and the shmem file are still owned by this constructor.
                unsafe {
                    kfree(drm_file.cast());
                    kfree(minor.cast());
                    intel_gt::linux::shmem::fput_file(filp);
                }
                return Err(error);
            }
        };
        // SAFETY: the registered DRM device's driver pointer is its live descriptor.
        let descriptor = unsafe { &*driver.cast::<DrmDriverDesc>() };
        // SAFETY: descriptor, device, and initialized drm_file remain owned throughout open().
        let result = unsafe { (descriptor.open)(device, drm_file.cast()) };
        drop(active);
        if result != 0 {
            FILES.lock().remove(&file_key);
            // SAFETY: this branch still owns the allocations and file; open failed.
            unsafe {
                kfree(drm_file.cast());
                kfree(minor.cast());
                intel_gt::linux::shmem::fput_file(filp);
            }
            return Err(linux_errno(result));
        }
        Ok(Self {
            drm_file: file_key,
            minor: minor as usize,
            filp: filp as usize,
            device: device as usize,
            driver: driver as usize,
            opened: true,
        })
    }

    pub(crate) fn ioctl(
        &self,
        native: &DrmFile,
        context: &crate::file::IoctlContext,
        cmd: u32,
        arg: usize,
    ) -> AxResult<usize> {
        if !supports_ioctl(cmd) {
            return Err(AxError::OperationNotSupported);
        }
        let _active = NativeFileBinding::bind(self.drm_file, native)?;
        // SAFETY: self retains the live shmem file until close(); the native file is borrowed.
        unsafe {
            (*(self.filp as *mut intel_gt::linux::shmem::File)).f_flags = native.open_flags();
        }
        let size = ((cmd >> 16) & 0x3fff) as usize;
        if size == 0 || size > MAX_IOCTL_BYTES {
            return Err(AxError::InvalidInput);
        }
        let mut data = Vec::new();
        data.try_reserve_exact(size).map_err(|_| AxError::NoMemory)?;
        data.resize(size, 0);
        // Match drm_ioctl's direction-sensitive top-level copy: `_IOW`
        // inputs are read, `_IOR` outputs start zeroed, and only `_IOR` /
        // `_IOWR` commands copy their completed record back to userspace.
        if ioctl_copies_from_user(cmd) {
            // SAFETY: Vec has `data.len()` initialized bytes and mutable access is exclusive.
            let bytes = unsafe {
                slice::from_raw_parts_mut(data.as_mut_ptr().cast::<MaybeUninit<u8>>(), data.len())
            };
            context
                .user_memory()
                .read_bytes(arg, bytes)
                .map_err(crate::mm::map_usercopy_error)?;
        }
        let data_ptr = data.as_mut_ptr().cast::<c_void>();
        // SAFETY: the device/file belong to self and the ioctl record is a live writable Vec.
        let result = unsafe {
            intel_gt::i915_ioctl_upstream::dispatch_kernel_ioctl(
                self.device as *mut c_void,
                self.drm_file as *mut c_void,
                cmd,
                data_ptr,
            )
        }
        .ok_or(AxError::OperationNotSupported)?;
        if result != 0 {
            return Err(linux_errno(result));
        }
        if (cmd & 0xff) == 0x40 + 0x24 {
            // SAFETY: a successful mmap-offset dispatch validates its fixed request size;
            // read_unaligned handles the Vec's byte alignment.
            let request = unsafe { ptr::read_unaligned(data.as_ptr().cast::<I915MmapOffsetRequest>()) };
            if !matches!(request.flags, 1..=3) {
                // GGTT aperture and local-memory FIXED offsets are not RAM.
                return Err(AxError::OperationNotSupported);
            }
            if request.flags != I915_MMAP_OFFSET_WB && !axhal::boot::intel_cpu_mmap_ready() {
                return Err(AxError::NoSuchDevice);
            }
            let object = native.gem(request.handle).map_err(AxError::from)?;
            let fence = crate::drm::fence::Fence::new(false);
            let predecessor = object.reservation.replace(fence.clone())?;
            let prepare = (|| {
                if let Some(predecessor) = predecessor {
                    predecessor.wait(Some(core::time::Duration::from_millis(500)))?;
                }
                sync_cpu_pages(object.backing.shared_pages().map_err(AxError::from)?)
                    .map_err(|_| AxError::Io)?;
                native.bind_upstream_mmap(request.handle, request.offset, request.flags as u8)
                    .map_err(AxError::from)
            })();
            if prepare.is_ok() { fence.signal(); } else { fence.signal_error(); }
            prepare?;
        }
        if ioctl_copies_to_user(cmd) {
            context
                .user_memory()
                .write_bytes(arg, &data)
                .map_err(crate::mm::map_usercopy_error)?;
        }
        Ok(0)
    }

    pub(crate) fn close_gem_handle(
        &mut self,
        native: &DrmFile,
        handle: u32,
    ) -> DrmResult<bool> {
        let _active = NativeFileBinding::bind(self.drm_file, native)
        .map_err(|_| DrmError::Busy)?;
        // SAFETY: self owns the still-live shmem file; native is borrowed for this call.
        unsafe {
            (*(self.filp as *mut intel_gt::linux::shmem::File)).f_flags = native.open_flags();
        }
        let result = delete_file_handle(
            self.drm_file as *mut gem::DrmFile,
            native,
            handle,
        )?;
        if result {
            if let Some(binding) = FILES.lock().get_mut(&self.drm_file) {
                binding.handles.remove(&handle);
            }
        }
        Ok(result)
    }

    pub(crate) fn bind_existing_handle(
        &self,
        native: &DrmFile,
        handle: u32,
        object: usize,
    ) -> Result<(), c_int> {
        let _active = NativeFileBinding::bind(self.drm_file, native).map_err(|_| -16)?;
        // SAFETY: self owns the still-live shmem file; native is borrowed for this call.
        unsafe {
            (*(self.filp as *mut intel_gt::linux::shmem::File)).f_flags = native.open_flags();
        }
        let drm_file = self.drm_file as *mut gem::DrmFile;
        {
            let _table = DrmFileTableGuard::lock(drm_file);
            let _guard = GEM_IDR_GATE.lock();
            // SAFETY: drm_file is this open's initialized allocation and both DRM-table
            // and radix locks protect its object_idr during the lookup.
            let existing = unsafe { idr_find(ptr::addr_of!((*drm_file).object_idr).cast(), handle as u64) };
            if !existing.is_null() {
                return (existing == object as *mut c_void)
                    .then_some(())
                    .ok_or(-16);
            }
            let base = object as *mut DrmGemObjectBaseLayout;
            // SAFETY: object is the caller's live GEM object and base is its ABI prefix.
            unsafe { gem::drm_gem_handle_get(base) }?;
            // SAFETY: drm_file is initialized and locked; object is a live GEM object
            // with the reference just acquired above.
            let result = unsafe {
                radix_tree_insert(
                    ptr::addr_of_mut!((*drm_file).object_idr).cast(),
                    handle as u64,
                    object as *mut c_void,
                )
            };
            if result != 0 {
                // SAFETY: the just-acquired GEM handle reference must be released on insertion failure.
                unsafe { put_gem_object(object as *mut c_void) };
                return Err(result);
            }
        }
        let base = object as *mut DrmGemObjectBaseLayout;
        // SAFETY: base is live and drm_file is the initialized file whose handle was inserted.
        let open_result = unsafe { gem::drm_gem_object_open(base, drm_file) };
        if open_result != 0 {
            let removed = {
                let _table = DrmFileTableGuard::lock(drm_file);
                let _guard = GEM_IDR_GATE.lock();
                // SAFETY: drm_file is locked and the just-inserted handle is still present.
                unsafe {
                    radix_tree_delete(
                        ptr::addr_of_mut!((*drm_file).object_idr).cast(),
                        handle as u64,
                    )
                }
            };
            if !removed.is_null() {
                // SAFETY: removal succeeded, so drop the reference paired with handle_get().
                unsafe { gem::drm_gem_handle_put(base) };
            }
            return Err(open_result);
        }
        let tracked = FILES
            .lock()
            .get_mut(&self.drm_file)
            .map(|binding| binding.handles.insert(handle))
            .is_some();
        if !tracked {
            // SAFETY: open() succeeded for this live object/file pair.
            unsafe { gem::drm_gem_object_close(base, drm_file) };
            let removed = {
                let _table = DrmFileTableGuard::lock(drm_file);
                let _guard = GEM_IDR_GATE.lock();
                // SAFETY: drm_file is locked and the just-inserted handle is still present.
                unsafe {
                    radix_tree_delete(
                        ptr::addr_of_mut!((*drm_file).object_idr).cast(),
                        handle as u64,
                    )
                }
            };
            if !removed.is_null() {
                // SAFETY: removal succeeded, so drop the reference paired with handle_get().
                unsafe { gem::drm_gem_handle_put(base) };
            }
            return Err(-19);
        }
        Ok(())
    }

}

impl UpstreamFile {
    pub(crate) fn close(&mut self, native: &DrmFile) {
        if !self.opened {
            return;
        }
        let _active = NativeFileBinding::bind(self.drm_file, native).ok();
        // SAFETY: self still owns the live shmem file and native is borrowed for this close.
        unsafe {
            (*(self.filp as *mut intel_gt::linux::shmem::File)).f_flags = native.open_flags();
        }
        let handles = FILES
            .lock()
            .get_mut(&self.drm_file)
            .map(|binding| core::mem::take(&mut binding.handles))
            .unwrap_or_default();
        for handle in handles {
            let _ = delete_file_handle(
                self.drm_file as *mut gem::DrmFile,
                native,
                handle,
            );
        }
        // SAFETY: driver is the descriptor retained from the registered DRM device.
        let descriptor = unsafe { &*(self.driver as *const DrmDriverDesc) };
        // SAFETY: postclose receives the same live device/file pair previously passed to open().
        unsafe { (descriptor.postclose)(self.device as *mut c_void, self.drm_file as *mut c_void) };
        FILES.lock().remove(&self.drm_file);
        // SAFETY: close owns these allocations and the shmem file after postclose returned.
        unsafe {
            intel_gt::linux::shmem::fput_file(self.filp as *mut intel_gt::linux::shmem::File);
            kfree(self.drm_file as *mut c_void);
            kfree(self.minor as *mut c_void);
        }
        self.opened = false;
    }
}

impl Drop for UpstreamFile {
    fn drop(&mut self) {
        if self.opened {
            // The owning native DrmFile must explicitly call close() while it
            // still has a borrowed reference; retaining a raw back-pointer
            // here would permit a use-after-free if that file moved/dropped.
            axlog::error!("upstream i915 drm_file dropped without native close");
        }
    }
}

pub(crate) fn supports_ioctl(cmd: u32) -> bool {
    intel_gt::i915_ioctl_upstream::supports_kernel_ioctl(cmd)
}

#[inline]
fn ioctl_copies_from_user(cmd: u32) -> bool {
    ((cmd >> 30) & IOC_WRITE) != 0
}

#[inline]
fn ioctl_copies_to_user(cmd: u32) -> bool {
    ((cmd >> 30) & IOC_READ) != 0
}

/// Flush the CPU-cached system-RAM pages behind a GEM object before the GPU reads
/// them or the CPU reads GPU output, so the non-snooping engine sees the bytes.
pub(crate) fn sync_cpu_pages(pages: Arc<SharedPages>) -> Result<(), intel_gt::Error> {
    use core::sync::atomic::{Ordering, fence};

    use intel_gt::Error;
    const PAGE: usize = 4096;
    // The same fixed system-RAM cache synchronization serves rendering and
    // display pin-to-GTT preparation. Do not apply the <=16MiB execution-BO
    // limit to a 4K scanout (or its double-height dumb backing).
    if pages.is_external() || pages.page_size() != axhal::paging::PageSize::Size4K {
        return Err(Error::Refused);
    }
    let pin = pages.fixed_view().map_err(|_| Error::Refused)?;
    let count = pin.len() / PAGE;
    if count == 0 || count > 16_384 {
        return Err(Error::Refused);
    }
    // CLFLUSH is optional in the architecture; admit the actual executing
    // CPU's capability/line size before accessing any page. This target is
    // N305 only, not a cache-maintenance fallback for unknown CPU layouts.
    let cpu = core::arch::x86_64::__cpuid(1);
    if cpu.edx & (1 << 19) == 0 || ((cpu.ebx >> 8) & 0xff) * 8 != 64 {
        return Err(Error::Refused);
    }
    for index in 0..count {
        intel_gt::ppgtt::physical(
            pages
                .paddr_at(index)
                .map_err(|_| Error::Refused)?
                .as_usize() as u64,
        )?;
    }
    for index in 0..count {
        let physical = pages.paddr_at(index).map_err(|_| Error::Refused)?;
        let base = axhal::mem::phys_to_virt(physical).as_usize();
        for offset in (0..PAGE).step_by(64) {
            // SAFETY: an ordinary allocator-owned, physically validated 4K
            // page retained by `pages` and its fixed-view pin. CLFLUSH invalidates
            // the WB alias before the non-snooping display/GT reads; no MMIO.
            unsafe { core::arch::x86_64::_mm_clflush((base + offset) as *const u8) };
        }
    }
    fence(Ordering::SeqCst);
    Ok(())
}
pub(crate) fn native_file(file: *mut c_void) -> Option<*mut c_void> {
    FILES
        .lock()
        .get(&(file as usize))
        .and_then(|binding| (binding.native_file != 0).then_some(binding.native_file as *mut c_void))
}

struct NativeFileBinding<'a> {
    file_key: usize,
    native_file: usize,
    _native: PhantomData<&'a DrmFile>,
}

impl<'a> NativeFileBinding<'a> {
    fn bind(file_key: usize, native: &'a DrmFile) -> AxResult<Self> {
        let native_file = ptr::from_ref(native) as usize;
        let mut files = FILES.lock();
        let binding = files.get_mut(&file_key).ok_or(AxError::NotFound)?;
        if binding.native_file != 0 {
            return Err(AxError::ResourceBusy);
        }
        binding.native_file = native_file;
        Ok(Self {
            file_key,
            native_file,
            _native: PhantomData,
        })
    }
}

impl Drop for NativeFileBinding<'_> {
    fn drop(&mut self) {
        if let Some(binding) = FILES.lock().get_mut(&self.file_key)
            && binding.native_file == self.native_file
        {
            binding.native_file = 0;
        }
    }
}

fn linux_errno(errno: c_int) -> AxError {
    match errno.checked_abs().unwrap_or(i32::MAX) {
        1 | 13 => AxError::PermissionDenied,
        2 => AxError::NotFound,
        5 => AxError::Io,
        12 => AxError::NoMemory,
        16 => AxError::ResourceBusy,
        19 => AxError::NoSuchDevice,
        22 => AxError::InvalidInput,
        28 => AxError::StorageFull,
        32 => AxError::BrokenPipe,
        35 => AxError::WouldBlock,
        75 => AxError::InvalidInput,
        95 => AxError::OperationNotSupported,
        _ => AxError::Io,
    }
}

pub(crate) fn new_gem_backing(
    object: *mut c_void,
    size: u64,
    charge: Arc<GemMemoryCharge>,
) -> Result<(Arc<dyn GemBacking>, Arc<Reservation>), DrmError> {
    if object.is_null() || size == 0 {
        return Err(DrmError::Invalid);
    }
    let candidate = Arc::try_new(Reservation::new()).map_err(|_| DrmError::NoMemory)?;
    let reservation = super::upstream_sync::register_alias(object as usize, candidate)
        .map_err(|_| DrmError::NoMemory)?;
    // SAFETY: caller supplies a live GEM object; this adds the backing's independent ref.
    unsafe {
        intel_gt::i915_gem_object_header_upstream::i915_gem_object_get(
            object.cast::<intel_gt::i915_gem_object_types_upstream::DrmI915GemObject>(),
        );
    }
    let backing = I915GemBacking {
        object: object as usize,
        size,
        charge,
        reservation: reservation.clone(),
        pages: axsync::Mutex::new(None),
    };
    let backing = Arc::try_new(backing).map_err(|_| DrmError::NoMemory)?;
    let backing: Arc<dyn GemBacking> = backing;
    Ok((backing, reservation))
}

struct I915GemBacking {
    object: usize,
    size: u64,
    charge: Arc<GemMemoryCharge>,
    reservation: Arc<Reservation>,
    pages: axsync::Mutex<Option<Arc<SharedPages>>>,
}

impl GemBacking for I915GemBacking {
    fn shared_pages(&self) -> crate::drm::DrmResult<Arc<SharedPages>> {
        let mut pages = self.pages.lock();
        if let Some(pages) = pages.as_ref() {
            return Ok(pages.clone());
        }
        // SAFETY: I915GemBacking owns a GEM ref from construction until Drop, so its object
        // is live while page_set acquires a pin and returns an owning MmapPageSet.
        let set = unsafe {
            intel_gt::i915_gem_mman_upstream::i915_object_page_set(
                self.object as *mut intel_gt::i915_gem_object_types_upstream::DrmI915GemObject,
            )
        }
        .map_err(|errno| match errno.checked_abs().unwrap_or(i32::MAX) {
            12 => DrmError::NoMemory,
            13 => DrmError::PermissionDenied,
            19 => DrmError::DeviceLost,
            22 => DrmError::Invalid,
            _ => DrmError::Unsupported,
        })?;
        if (set.physical_pages().len() as u64).checked_mul(PAGE_SIZE as u64) != Some(self.size) {
            return Err(DrmError::Invalid);
        }
        let mut physical = Vec::new();
        physical
            .try_reserve_exact(set.physical_pages().len())
            .map_err(|_| DrmError::NoMemory)?;
        physical.extend(
            set.physical_pages()
                .iter()
                .copied()
                .map(axhal::mem::PhysAddr::from_usize),
        );
        let owner = Arc::try_new(set).map_err(|_| DrmError::NoMemory)?;
        let owner: Arc<dyn Send + Sync> = owner;
        // SAFETY: MmapPageSet owns a GEM reference and an upstream pin on each
        // cacheable shmem RAM page until the final backing/VMA reference drops.
        let shared_pages = unsafe { SharedPages::new_page_cache_4k(physical, owner) }
            .map_err(|_| DrmError::NoMemory)?;
        shared_pages
            .retain_allocation_owner(self.charge.clone())
            .map_err(|_| DrmError::NoMemory)?;
        let backing_pages = Arc::try_new(shared_pages).map_err(|_| DrmError::NoMemory)?;
        *pages = Some(backing_pages.clone());
        Ok(backing_pages)
    }

    fn upstream_i915_object(&self) -> Option<usize> {
        Some(self.object)
    }
}

impl Drop for I915GemBacking {
    fn drop(&mut self) {
        self.pages.lock().take();
        super::upstream_sync::unregister_alias(self.object);
        // SAFETY: construction acquired exactly one GEM ref for this backing; pages are
        // dropped first so their pins are gone before releasing that final owner ref.
        unsafe {
            intel_gt::i915_gem_object_header_upstream::i915_gem_object_put(
                self.object as *mut intel_gt::i915_gem_object_types_upstream::DrmI915GemObject,
            );
        }
    }
}

fn put_gem_object(object: *mut c_void) {
    // SAFETY: callers pass a non-null GEM object for which this path owns one handle ref.
    unsafe { gem::drm_gem_handle_put(object.cast::<DrmGemObjectBaseLayout>()) };
}

/// Serialize object_idr changes with `drm_file.table_lock` readers such as
/// i915 fdinfo, then take GEM_IDR_GATE to protect the LinuxKPI radix backend.
/// Keep this ordering identical for every create, import, and close path.
struct DrmFileTableGuard(*mut gem::DrmFile);

impl DrmFileTableGuard {
    fn lock(file: *mut gem::DrmFile) -> Self {
        // SAFETY: callers pass the initialized drm_file allocation owned by UpstreamFile;
        // it remains live until the returned guard is dropped.
        unsafe { gem::drm_file_table_lock(file) };
        Self(file)
    }
}

impl Drop for DrmFileTableGuard {
    fn drop(&mut self) {
        // SAFETY: this guard holds the matching table lock and its file remains live.
        unsafe { gem::drm_file_table_unlock(self.0) };
    }
}

/// Linux DRM-core provider callback: create one upstream IDR handle and its
/// same-ID kernel GemObject alias as one operation.
pub(crate) unsafe extern "C" fn gem_handle_create(
    file: *mut c_void,
    object: *mut c_void,
    handle: *mut u32,
) -> c_int {
    if file.is_null() || object.is_null() || handle.is_null() {
        return -22;
    }
    let (native_file, upstream_file) = match FILES.lock().get(&(file as usize)) {
        Some(binding) if binding.native_file != 0 => {
            (binding.native_file, file.cast::<gem::DrmFile>())
        }
        _ => return -19,
    };
    // SAFETY: DRM calls this provider for a live GEM object and checked non-null above.
    let size = unsafe { (*object.cast::<DrmGemObjectBaseLayout>()).size };
    // SAFETY: NativeFileBinding scopes this registry pointer to the active callback and
    // its PhantomData ties that binding to the borrowed DrmFile.
    let native = unsafe { &*(native_file as *const DrmFile) };
    let handle_id = match native.create_upstream_gem_handle(object, size) {
        Ok(handle) => handle,
        Err(error) => return match error {
            DrmError::Invalid | DrmError::Overflow => -22,
            DrmError::NotFound => -2,
            DrmError::PermissionDenied => -13,
            DrmError::Busy => -16,
            DrmError::NoMemory => -12,
            DrmError::QueueFull => -11,
            DrmError::DeviceLost => -19,
            DrmError::Unsupported => -95,
        },
    };
    let insert = {
        let _table = DrmFileTableGuard::lock(upstream_file);
        let _guard = GEM_IDR_GATE.lock();
        // SAFETY: upstream_file is the initialized per-open allocation, protected by
        // both table and radix locks for this lookup.
        let existing = unsafe { idr_find(ptr::addr_of!((*upstream_file).object_idr).cast(), handle_id as u64) };
        if !existing.is_null() {
            -16
        } else {
            // SAFETY: the file is locked and initialized, and object is the live GEM
            // instance being inserted with the reference acquired immediately below.
            unsafe {
                let base = object.cast::<DrmGemObjectBaseLayout>();
                if gem::drm_gem_handle_get(base).is_err() {
                    -24
                } else {
                    let root = ptr::addr_of_mut!((*upstream_file).object_idr).cast();
                    let result = radix_tree_insert(root, handle_id as u64, object);
                    if result != 0 {
                        put_gem_object(object);
                    }
                    result
                }
            }
        }
    };
    if insert != 0 {
        let _ = native.close_handle(handle_id);
        return insert;
    }
    let base = object.cast::<DrmGemObjectBaseLayout>();
    // SAFETY: base is the live object and its new handle has been inserted into this file.
    let open_result = unsafe { gem::drm_gem_object_open(base, upstream_file) };
    if open_result != 0 {
        let removed = {
            let _table = DrmFileTableGuard::lock(upstream_file);
            let _guard = GEM_IDR_GATE.lock();
            // SAFETY: the file is locked and the just-added handle remains in its IDR.
            unsafe {
                radix_tree_delete(
                    ptr::addr_of_mut!((*upstream_file).object_idr).cast(),
                    handle_id as u64,
                )
            }
        };
        if !removed.is_null() {
            // SAFETY: deletion succeeded, so release the reference paired with handle_get().
            unsafe { gem::drm_gem_handle_put(base) };
        }
        let _ = native.close_handle(handle_id);
        return open_result;
    }
    let tracked = FILES
        .lock()
        .get_mut(&(file as usize))
        .map(|binding| binding.handles.insert(handle_id))
        .is_some();
    if !tracked {
        // SAFETY: object_open succeeded for this still-live file/object pair.
        unsafe { gem::drm_gem_object_close(base, upstream_file) };
        let removed = {
            let _table = DrmFileTableGuard::lock(upstream_file);
            let _guard = GEM_IDR_GATE.lock();
            // SAFETY: the file is locked and the just-added handle remains in its IDR.
            unsafe {
                radix_tree_delete(
                    ptr::addr_of_mut!((*upstream_file).object_idr).cast(),
                    handle_id as u64,
                )
            }
        };
        if !removed.is_null() {
            // SAFETY: deletion succeeded, so release the reference paired with handle_get().
            unsafe { gem::drm_gem_handle_put(base) };
        }
        let _ = native.close_handle(handle_id);
        return -19;
    }
    // SAFETY: the DRM provider contract supplies a writable handle output pointer.
    unsafe { *handle = handle_id };
    0
}

fn delete_file_handle(file: *mut gem::DrmFile, native: &DrmFile, handle: u32) -> DrmResult<bool> {
    let object = {
        let _table = DrmFileTableGuard::lock(file);
        let _guard = GEM_IDR_GATE.lock();
        // SAFETY: caller passes this open's initialized file and both table/radix locks
        // exclude mutation while looking up the handle.
        let found = unsafe { idr_find(ptr::addr_of!((*file).object_idr).cast(), handle as u64) };
        if found.is_null() {
            return Ok(false);
        }
        // SAFETY: the same locks remain held and the lookup proved this slot exists.
        unsafe { radix_tree_delete(ptr::addr_of_mut!((*file).object_idr).cast(), handle as u64) }
    };
    // SAFETY: `object` is the live IDR entry removed above and `file` is its owner.
    unsafe { gem::drm_gem_object_close(object.cast(), file) };
    let close_result = native.close_handle(handle);
    let object = object.cast::<DrmGemObjectBaseLayout>();
    // SAFETY: deletion removed the corresponding IDR handle; release its one owned ref.
    unsafe { gem::drm_gem_handle_put(object) };
    close_result.map_err(|_| DrmError::Invalid)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{
        AxError, BTreeSet, FILES, FileBinding, NativeFileBinding, ioctl_copies_from_user,
        ioctl_copies_to_user, native_file,
    };
    use alloc::sync::Arc;
    use core::{
        ffi::c_void,
        mem::MaybeUninit,
        sync::atomic::{AtomicI32, AtomicU32, Ordering},
    };
    use crate::drm::{
        DisplayAdapter, DrmDevice, DrmError, DrmFile, DrmResult, GemBacking, Scanout,
        fence::Fence, gem::DumbRequest,
    };

    struct TestAdapter;

    impl DisplayAdapter for TestAdapter {
        fn create_dumb(
            &self,
            _request: DumbRequest,
            _pitch: u32,
            _size: u64,
            _allocation_owner: Arc<dyn Send + Sync>,
        ) -> DrmResult<Arc<dyn GemBacking>> {
            Err(DrmError::Unsupported)
        }

        fn present(&self, _scanout: Scanout) -> DrmResult<Arc<Fence>> {
            Err(DrmError::Unsupported)
        }
    }

    const fn ioc(direction: u32, size: u32, nr: u32) -> u32 {
        (direction << 30) | (size << 16) | ((b'd' as u32) << 8) | (0x40 + nr)
    }

    struct RemoveTestBinding(usize);

    impl Drop for RemoveTestBinding {
        fn drop(&mut self) {
            FILES.lock().remove(&self.0);
        }
    }

    #[test]
    fn native_file_backpointer_is_scoped_to_active_lifecycle_calls() {
        let file_key = usize::MAX - 0x4d52_4d45;
        FILES.lock().insert(
            file_key,
            FileBinding {
                native_file: 0,
                handles: BTreeSet::new(),
            },
        );
        let _cleanup = RemoveTestBinding(file_key);
        let file = file_key as *mut c_void;
        let native = DrmDevice::new(Arc::new(TestAdapter), 1, 2, 3, 4).open_primary();

        assert_eq!(native_file(file), None);
        let active = NativeFileBinding::bind(file_key, &native).unwrap();
        assert_eq!(
            native_file(file).map(|pointer| pointer as usize),
            Some(core::ptr::from_ref(&native) as usize)
        );
        assert!(matches!(
            NativeFileBinding::bind(file_key, &native),
            Err(AxError::ResourceBusy)
        ));
        drop(active);
        assert_eq!(native_file(file), None);
    }

    #[test]
    fn ioctl_copy_direction_matches_drm_ioc_bits() {
        let iow = ioc(1, 32, 28);
        let ior = ioc(2, 16, 35);
        let iowr = ioc(3, 16, 6);
        assert!(ioctl_copies_from_user(iow));
        assert!(!ioctl_copies_to_user(iow));
        assert!(!ioctl_copies_from_user(ior));
        assert!(ioctl_copies_to_user(ior));
        assert!(ioctl_copies_from_user(iowr));
        assert!(ioctl_copies_to_user(iowr));

        let supports = intel_gt::i915_ioctl_upstream::supports_kernel_ioctl;
        assert!(supports(iow));
        assert!(supports(ior));
        assert!(supports(iowr));
        assert!(!supports(ioc(3, 12, 6))); // malformed GETPARAM size
        assert!(!supports(ioc(3, 32, 28))); // PREAD is _IOW
        assert!(supports(ioc(1, 64, 0x29))); // legacy EXECBUFFER2
        assert!(supports(ioc(3, 64, 0x29))); // EXECBUFFER2_WR
        assert!(!supports(ioc(1, 56, 0x29))); // malformed record size
    }

    #[test]
    fn gem_handle_reference_pair_balances_kref_and_handle_count() {
        use intel_gt::intel_context_upstream::{DrmGemObjectBaseLayout, Kref};

        // Keep one synthetic owner reference so this exercises bridge
        // accounting without invoking the object allocator/free provider.
        // SAFETY: this ABI record contains only integer/pointer fields; the test
        // initializes the refcount before invoking the GEM handle helpers.
        let mut object: DrmGemObjectBaseLayout = unsafe { MaybeUninit::zeroed().assume_init() };
        object.refcount.refcount.refs.counter = 1;
        let object = &mut object as *mut DrmGemObjectBaseLayout;
        // SAFETY: object points to the initialized stack record and remains live
        // and exclusively borrowed for this test scope.
        let kref = unsafe {
            AtomicI32::from_ptr(core::ptr::addr_of_mut!((*object).refcount.refcount.refs.counter))
        };
        // SAFETY: the GEM base layout places the handle count immediately after
        // its Kref prefix; object is aligned and valid for the enclosing scope.
        let handle_count = unsafe {
            AtomicU32::from_ptr(object.cast::<u8>().add(core::mem::size_of::<Kref>()).cast())
        };

        // SAFETY: the synthetic object has one initialized owner reference.
        assert_eq!(unsafe { intel_gt::linux::gem::drm_gem_handle_get(object) }, Ok(()));
        assert_eq!(kref.load(Ordering::Acquire), 2);
        assert_eq!(handle_count.load(Ordering::Acquire), 1);
        // SAFETY: balance the handle reference acquired above before stack teardown.
        unsafe { intel_gt::linux::gem::drm_gem_handle_put(object) };
        assert_eq!(kref.load(Ordering::Acquire), 1);
        assert_eq!(handle_count.load(Ordering::Acquire), 0);
    }
}

