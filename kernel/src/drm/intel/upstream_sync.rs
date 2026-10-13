//! Fence and syncobj identity shared by the native DRM core and LinuxKPI i915.
//!
//! Kernel fence state remains authoritative.  The Linux `dma_fence` wrapper
//! owns a reference to that state and a task mirrors its deferred completion
//! into the upstream fence only after the native fence actually signals.

use alloc::{borrow::Cow, boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    any::Any,
    ffi::{c_char, c_int, c_long, c_void},
    mem::{offset_of, size_of},
    ptr,
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable, PreparedPollRegistration};
use intel_gt::{
    i915_request_upstream::DmaFenceOps,
    intel_context_upstream::{DmaFence, DmaFenceTimestamp, DrmGemObjectBaseLayout, RcuHead},
    intel_engine_cs_upstream::Spinlock,
};

use crate::drm::{
    fence::{Fence, Reservation},
    syncobj::{SyncFile, Syncobj},
};

use crate::file::{FileLike, Kstat, add_file_like, anon_inode_stat, get_typed_file};

unsafe extern "C" {
    /// Generic DRM syncobj refcount helpers. These have `tk_drm_` prefixes to
    /// distinguish them from the LinuxKPI native-Syncobj bridge symbols.
    fn tk_drm_syncobj_get(syncobj: *mut c_void) -> *mut c_void;
    fn tk_drm_syncobj_put(syncobj: *mut c_void);
}

#[inline]
fn syncobj_err_ptr(error: AxError) -> *mut c_void {
    (-(LinuxError::from(error).code() as isize)) as *mut c_void
}

const GFP_KERNEL: u32 = 0x0cc0;
const DMA_RESV_USAGE_WRITE: u32 = 1;
const EIO: c_int = 5;
const EINVAL: c_int = 22;
const ENOMEM: c_int = 12;
const MAX_SCHEDULE_TIMEOUT: c_long = tk_linux_kapi::config::MAX_SCHEDULE_TIMEOUT as c_long;

/// The DRM provider has several void Linux ABI callbacks. A failed bridge in
/// one of those paths must stop the kernel rather than let GPU/KMS work
/// proceed without its reservation edge.
pub(crate) fn require_bridge<T>(result: AxResult<T>, operation: &str) -> T {
    result.unwrap_or_else(|error| panic!("{operation}: fence bridge failed: {error:?}"))
}

#[derive(Clone)]
struct Alias {
    reservation: Arc<Reservation>,
    resv: usize,
    refs: usize,
}

static ALIASES: spin::Mutex<BTreeMap<usize, Alias>> = spin::Mutex::new(BTreeMap::new());
/// LinuxKPI's `sync_file_create()` returns a `struct sync_file *`, but i915
/// only consumes its first field (`file`) before handing that Linux file to
/// `fd_install()`/`fput()`. Keep the complete wrapper here until one of those
/// ownership-transfer paths consumes it.
#[repr(C)]
struct UpstreamSyncFile {
    file: usize,
    native: Arc<SyncFile>,
}

// SAFETY (layout): each Box pointee must stay at a stable address because LinuxKPI
// receives that opaque address and later returns it to fd_install/fput; Vec<Box<_>>
// keeps the allocation stable when its element buffer grows.
#[allow(clippy::vec_box)]
static SYNC_FILES: spin::Mutex<Vec<Box<UpstreamSyncFile>>> = spin::Mutex::new(Vec::new());

/// An installed descriptor owns the upstream ABI wrapper and its Linux file
/// until the final open-file-description close. The actual userspace-facing
/// file remains the native `SyncFile`, so poll/ioctl/import all share its
/// fence semantics.
pub(crate) struct InstalledSyncFile {
    upstream: Box<UpstreamSyncFile>,
}

impl Drop for InstalledSyncFile {
    fn drop(&mut self) {
        // SAFETY: taking this resource transfers the wrapper's single shmem-file
        // reference to the OFD; its final drop releases that same live reference.
        unsafe {
            tk_linux_kapi::shmem::fput_file(self.upstream.file as *mut _);
        }
    }
}

static FENCE_CONTEXT: AtomicU64 = AtomicU64::new(1);
static FENCE_SEQNO: AtomicU64 = AtomicU64::new(1);

/// Acquire one backing-owner reference and return the canonical reservation
/// shared by every native alias of this exact upstream GEM object.
pub(crate) fn register_alias(
    i915_obj: usize,
    candidate: Arc<Reservation>,
) -> AxResult<Arc<Reservation>> {
    if i915_obj == 0 {
        return Err(AxError::InvalidInput);
    }
    let base = i915_obj as *mut DrmGemObjectBaseLayout;
    // SAFETY: callers register a live GEM object whose backing ref is established
    // before publication; its ABI prefix contains the reservation pointer.
    let resv = unsafe { (*base).resv };
    if resv.is_null() {
        return Err(AxError::NoSuchDevice);
    }
    let mut aliases = ALIASES.lock();
    if let Some(alias) = aliases.get_mut(&i915_obj) {
        if alias.resv != resv as usize {
            return Err(AxError::ResourceBusy);
        }
        alias.refs = alias.refs.checked_add(1).ok_or(AxError::ResourceBusy)?;
        return Ok(alias.reservation.clone());
    }
    aliases.insert(
        i915_obj,
        Alias {
            reservation: candidate.clone(),
            resv: resv as usize,
            refs: 1,
        },
    );
    Ok(candidate)
}

/// Release one `I915GemBacking` registration.  Its Arc reservation may remain
/// alive in native GEM objects after this registry entry is removed.
pub(crate) fn unregister_alias(i915_obj: usize) {
    let mut aliases = ALIASES.lock();
    let Some(alias) = aliases.get_mut(&i915_obj) else {
        return;
    };
    alias.refs -= 1;
    if alias.refs == 0 {
        aliases.remove(&i915_obj);
    }
}

/// A kernel-owned completion fence with Linux's ABI prefix. `base` must stay
/// first: every upstream consumer sees only the `dma_fence` pointer.
#[repr(C)]
struct NativeDmaFence {
    base: DmaFence,
    lock: Spinlock,
    kernel: Arc<Fence>,
    timeline: Option<Arc<Syncobj>>,
}

unsafe extern "C" {
    fn kmalloc(size: usize, flags: u32) -> *mut c_void;
    fn kfree(ptr: *mut c_void);
    fn dma_fence_init(
        fence: *mut DmaFence,
        ops: *const DmaFenceOps,
        lock: *mut Spinlock,
        context: u64,
        seqno: u64,
    );
    fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence;
    fn dma_fence_put(fence: *mut DmaFence);
    fn dma_fence_signal(fence: *mut DmaFence);
    fn dma_fence_set_error(fence: *mut DmaFence, error: c_int);
    fn dma_fence_is_signaled(fence: *mut DmaFence) -> bool;
    fn dma_fence_wait_timeout(fence: *mut DmaFence, intr: bool, timeout: c_long) -> c_long;
    fn dma_resv_reserve_fences(resv: *mut c_void, count: u32) -> c_int;
    fn dma_resv_add_fence(resv: *mut c_void, fence: *mut DmaFence, usage: u32);
    fn thekernel_dma_resv_lock(resv: *mut c_void) -> c_int;
    fn thekernel_dma_resv_trylock(resv: *mut c_void) -> c_int;
    fn thekernel_dma_resv_unlock(resv: *mut c_void);
    fn dma_fence_chain_free(chain: *mut c_void);
    fn thekernel_call_rcu(head: *mut RcuHead, callback: Option<unsafe extern "C" fn(*mut RcuHead)>);
}

static NATIVE_FENCE_OPS: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(native_fence_driver_name),
    get_timeline_name: Some(native_fence_timeline_name),
    enable_signaling: Some(native_fence_enable_signaling),
    // Completion/error bits are published only by the deferred monitor; a
    // signaled predicate here could race ahead of dma_fence_set_error().
    signaled: None,
    wait: Some(native_fence_wait),
    release: Some(native_fence_release),
    set_deadline: None,
};

unsafe extern "C" fn native_fence_driver_name(_: *mut DmaFence) -> *const c_char {
    c"thekernel".as_ptr()
}

unsafe extern "C" fn native_fence_timeline_name(_: *mut DmaFence) -> *const c_char {
    c"drm".as_ptr()
}

unsafe extern "C" fn native_fence_enable_signaling(_: *mut DmaFence) -> bool {
    // A monitor task owns the deferred completion reference from construction.
    true
}

unsafe extern "C" fn native_fence_wait(
    fence: *mut DmaFence,
    _interruptible: bool,
    timeout: c_long,
) -> c_long {
    if fence.is_null() || timeout < 0 {
        return -(EINVAL as c_long);
    }
    // SAFETY: this callback is entered by dma_fence APIs with a live referenced
    // fence, and the null check above rejects an invalid sentinel.
    if unsafe { dma_fence_is_signaled(fence) } {
        return timeout.max(1);
    }
    if timeout == 0 {
        return 0;
    }
    let bridge = fence.cast::<NativeDmaFence>();
    let tick_ns = axhal::time::NANOS_PER_SEC / tk_linux_kapi::config::CONFIG_HZ as u64;
    let unbounded = timeout >= MAX_SCHEDULE_TIMEOUT;
    let duration = if unbounded {
        None
    } else {
        Some(Duration::from_nanos(
            (timeout as u64).saturating_mul(tick_ns),
        ))
    };
    let start = axhal::time::monotonic_time_nanos();
    // SAFETY: NATIVE_FENCE_OPS is installed only on NativeDmaFence values and
    // the caller's DMA reference keeps the initialized kernel Arc alive.
    match unsafe { &*ptr::addr_of!((*bridge).kernel) }.wait(duration) {
        Ok(()) | Err(AxError::Io) => {
            // SAFETY: same callback-owned reference keeps this initialized field live.
            if unsafe { (*bridge).kernel.is_failed() } {
                // SAFETY: fence is the live callback argument and this function
                // publishes the terminal error before signaling it.
                unsafe { dma_fence_set_error(fence, -EIO) };
            }
            // SAFETY: this callback owns a live fence reference and signals only
            // after native completion/error has been observed.
            unsafe { dma_fence_signal(fence) };
            if unbounded {
                1
            } else {
                let elapsed = axhal::time::monotonic_time_nanos().saturating_sub(start);
                (timeout - (elapsed / tick_ns.max(1)) as c_long).max(1)
            }
        }
        Err(AxError::WouldBlock) => 0,
        Err(_) => -(EIO as c_long),
    }
}

unsafe extern "C" fn native_fence_release(fence: *mut DmaFence) {
    if fence.is_null() {
        return;
    }
    // The timestamp union is no longer observed after the final dma_fence
    // reference, so reuse it for the grace-period header before freeing.
    // SAFETY: release is called exactly once after the final reference; this
    // fence was initialized by our constructor and the union slot can hold RCU state.
    let head = unsafe { ptr::addr_of_mut!((*fence).timestamp_union.rcu).cast::<RcuHead>() };
    // SAFETY: head points into this retired fence and RCU owns reclamation until callback.
    unsafe { thekernel_call_rcu(head, Some(free_native_fence_rcu)) };
}

unsafe extern "C" fn free_native_fence_rcu(head: *mut RcuHead) {
    if head.is_null() {
        return;
    }
    // SAFETY: the callback receives the timestamp-union address embedded in a
    // retired NativeDmaFence; offset_of! reverses that exact repr(C) field offset.
    let base = unsafe {
        head.cast::<u8>()
            .sub(offset_of!(DmaFence, timestamp_union))
            .cast::<NativeDmaFence>()
    };
    // SAFETY: RCU grace period is complete, so no readers remain; both Arc fields
    // were initialized before publication and base is their original allocation.
    unsafe {
        ptr::drop_in_place(ptr::addr_of_mut!((*base).kernel));
        ptr::drop_in_place(ptr::addr_of_mut!((*base).timeline));
        kfree(base.cast());
    }
}

fn allocate_native_dma_fence_for(
    fence: Arc<Fence>,
    timeline: Option<Arc<Syncobj>>,
) -> AxResult<*mut DmaFence> {
    // SAFETY: kmalloc supplies suitably aligned storage for the C-layout fence;
    // null is checked before any field is accessed.
    let raw = unsafe { kmalloc(size_of::<NativeDmaFence>(), GFP_KERNEL) }.cast::<NativeDmaFence>();
    if raw.is_null() {
        return Err(AxError::NoMemory);
    }
    // SAFETY: raw is a fresh correctly aligned allocation. We initialize the
    // embedded spinlock and ABI fence before writing the Rust Arc fields.
    unsafe {
        let lock = ptr::addr_of_mut!((*raw).lock);
        // Linux x86 raw spinlocks have one zero-initialized lock word; this
        // is the same storage initialization as `spin_lock_init()`.
        ptr::write_bytes(lock.cast::<u8>(), 0, size_of::<Spinlock>());
        dma_fence_init(
            ptr::addr_of_mut!((*raw).base),
            &NATIVE_FENCE_OPS,
            lock,
            FENCE_CONTEXT.fetch_add(1, Ordering::Relaxed),
            FENCE_SEQNO.fetch_add(1, Ordering::Relaxed),
        );
        ptr::addr_of_mut!((*raw).kernel).write(fence.clone());
        ptr::addr_of_mut!((*raw).timeline).write(timeline);
    }
    // SAFETY: raw's base field was initialized by dma_fence_init above.
    let base = unsafe { ptr::addr_of_mut!((*raw).base) };
    if fence.is_signaled() {
        if fence.is_failed() {
            // SAFETY: base is initialized and still owns its initial reference.
            unsafe { dma_fence_set_error(base, -EIO) };
        }
        // SAFETY: base is initialized and still owns its initial reference.
        unsafe { dma_fence_signal(base) };
    }
    Ok(base)
}

fn allocate_native_dma_fence(fence: Arc<Fence>) -> AxResult<*mut DmaFence> {
    allocate_native_dma_fence_for(fence, None)
}

struct MonitorFenceRef(usize);
impl Drop for MonitorFenceRef {
    fn drop(&mut self) {
        // SAFETY: this owner holds exactly the reference acquired at creation.
        unsafe { dma_fence_put(self.0 as *mut DmaFence) };
    }
}

/// Preallocate both task storage and scheduler admission before publishing
/// either reservation. Dropping an uncommitted monitor releases its DMA ref.
fn prepare_native_fence_monitor(
    base: *mut DmaFence, fence: Arc<Fence>,
) -> AxResult<Option<axtask::PreparedTaskPublication>> {
    if fence.is_signaled() {
        // SAFETY: the caller owns the initialized DMA fence until preparation returns.
        unsafe {
            if fence.is_failed() { dma_fence_set_error(base, -EIO); }
            dma_fence_signal(base);
        }
        return Ok(None);
    }
    // SAFETY: the caller's reference pins base while the monitor acquires its own.
    // SAFETY: caller retains the initialized base fence throughout this call; get
    // creates the independent ref transferred to MonitorFenceRef.
    let monitor_ref = MonitorFenceRef(unsafe { dma_fence_get(base) } as usize);
    let mut name = alloc::string::String::new();
    name.try_reserve_exact(16).map_err(|_| AxError::NoMemory)?;
    name.push_str("native-drm-fence");
    let task = axtask::TaskInner::try_new(move || {
        let _ = fence.wait(None);
        let dma = monitor_ref.0 as *mut DmaFence;
        // SAFETY: monitor_ref retains the DMA fence across the wait and signal.
        // SAFETY: monitor_ref holds the added reference through the completion writes.
        unsafe {
            if fence.is_failed() { dma_fence_set_error(dma, -EIO); }
            dma_fence_signal(dma);
        }
        // Capture the full RAII owner, not only its integer pointer field.
        drop(monitor_ref);
    }, name, axconfig::TASK_STACK_SIZE).map_err(|_| AxError::NoMemory)?;
    let task = axtask::prepare_task_with_sched_from(task, axtask::SchedState::default(), &axtask::current())?;
    axtask::reserve_prepared_task(task).map(Some).map_err(|error| error.into_ax_error())
}

fn start_native_fence_monitor(base: *mut DmaFence, fence: Arc<Fence>) -> AxResult<()> {
    if let Some(monitor) = prepare_native_fence_monitor(base, fence)? {
        axtask::publish_prepared_task(monitor);
    }
    Ok(())
}

fn native_fence_to_dma(fence: Arc<Fence>) -> AxResult<*mut DmaFence> {
    let dma = allocate_native_dma_fence(fence.clone())?;
    if let Err(error) = start_native_fence_monitor(dma, fence) {
        // SAFETY: failed monitor setup leaves only the initial DMA reference to release.
        unsafe { dma_fence_put(dma) };
        return Err(error);
    }
    Ok(dma)
}

fn native_syncobj_fence_to_dma(object: Arc<Syncobj>, fence: Arc<Fence>) -> AxResult<*mut DmaFence> {
    let dma = allocate_native_dma_fence_for(fence.clone(), Some(object))?;
    if let Err(error) = start_native_fence_monitor(dma, fence) {
        // SAFETY: failed monitor setup leaves only the initial DMA reference to release.
        unsafe { dma_fence_put(dma) };
        return Err(error);
    }
    Ok(dma)
}

/// LinuxKPI `sync_file_create()`. The returned pointer has the source Linux
/// ABI's first `file` field and stays registered until `fd_install()` or
/// final `fput()` transfers/releases its ownership.
///
/// # Safety
/// `fence` must be null or point to a live `dma_fence` reference owned by the
/// calling LinuxKPI path for the duration of this call.
pub(crate) unsafe fn sync_file_create(fence: *mut c_void) -> *mut c_void {
    if fence.is_null() {
        return ptr::null_mut();
    }
    let native_fence = match kernel_fence_for_dma(fence.cast()) {
        Ok(fence) => fence,
        Err(_) => return ptr::null_mut(),
    };
    let native = match SyncFile::try_new(native_fence) {
        Ok(sync_file) => sync_file,
        Err(_) => return ptr::null_mut(),
    };
    // SAFETY: the C string is static and NUL-terminated for the shmem constructor.
    let file = unsafe {
        tk_linux_kapi::shmem::shmem_file_setup(c"thekernel-sync-file".as_ptr(), 0, 0)
    };
    if file.is_null() || tk_linux_kapi::config::IS_ERR(file) {
        return ptr::null_mut();
    }
    let wrapper = match Box::try_new(UpstreamSyncFile {
        file: file as usize,
        native,
    }) {
        Ok(wrapper) => wrapper,
        Err(_) => {
            // SAFETY: file is the live reference returned by shmem_file_setup above.
            unsafe { tk_linux_kapi::shmem::fput_file(file) };
            return ptr::null_mut();
        }
    };
    let pointer = (&*wrapper as *const UpstreamSyncFile).cast_mut().cast::<c_void>();
    let mut files = SYNC_FILES.lock();
    if files.try_reserve(1).is_err() {
        drop(files);
        // SAFETY: no wrapper was published, so this function still owns file's ref.
        unsafe { tk_linux_kapi::shmem::fput_file(file) };
        return ptr::null_mut();
    }
    files.push(wrapper);
    pointer
}

/// Convert an `fd_install()` ownership transfer to the native descriptor
/// table. Returns the native pollable file and a final-OFD resource owning the
/// upstream wrapper/Linux shmem file, or `None` for ordinary LinuxKPI files.
pub(crate) fn take_sync_file_for_install(
    file: *mut c_void,
) -> AxResult<Option<(Arc<SyncFile>, Box<dyn Any + Send + Sync>)>> {
    let key = file as usize;
    let upstream = {
        let mut files = SYNC_FILES.lock();
        let Some(index) = files
            .iter()
            .position(|candidate| candidate.file == key)
        else {
            return Ok(None);
        };
        files.remove(index)
    };
    let native = upstream.native.clone();
    let resource = match Box::try_new(InstalledSyncFile { upstream }) {
        Ok(resource) => resource as Box<dyn Any + Send + Sync>,
        Err(_) => {
            // fd_install is a void ABI; return the allocation failure so its
            // caller can fail-stop rather than expose a descriptor without
            // the backing Linux file owner.
            return Err(AxError::NoMemory);
        }
    };
    Ok(Some((native, resource)))
}

/// `fput()` final-release hook. A wrapper not consumed by `fd_install()` is
/// removed and freed here; installed wrappers are owned by their OFD resource.
///
/// # Safety
/// `file` must be the `struct file *` identity previously returned from
/// `sync_file_create`; callers must obey Linux's final-release ordering.
pub(crate) unsafe fn sync_file_release(file: *mut c_void) {
    if file.is_null() {
        return;
    }
    let wrapper = {
        let mut files = SYNC_FILES.lock();
        let Some(index) = files
            .iter()
            .position(|candidate| candidate.file == file as usize)
        else {
            return;
        };
        files.remove(index)
    };
    drop(wrapper);
}

/// LinuxKPI `sync_file_get_fence()`: resolve the same native descriptor used
/// by poll and SYNC_IOC_MERGE and return one owned Linux fence reference.
///
/// # Safety
/// The caller must use the returned pointer only as an owned `dma_fence`
/// reference and eventually release it with `dma_fence_put`.
pub(crate) unsafe fn sync_file_get_fence(fd: c_int) -> *mut c_void {
    let Ok(file) = crate::file::get_typed_file::<SyncFile>(fd) else {
        return ptr::null_mut();
    };
    match native_fence_to_dma(file.fence()) {
        Ok(fence) => fence.cast(),
        Err(_) => ptr::null_mut(),
    }
}

/// Holds aliased Linux reservation locks across the corresponding native
/// reservation write, then installs a preallocated DMA fence before unlock.
pub(crate) struct NativeReservationUpdate {
    resvs: Vec<usize>,
    i915_objects: Vec<usize>,
    dma_fence: *mut DmaFence,
    monitor: Option<axtask::PreparedTaskPublication>,
}

impl NativeReservationUpdate {
    pub(crate) fn publish(mut self) {
        for resv in &self.resvs {
            // SAFETY: prepare() acquired this reservation lock and reserved one
            // fence slot; self retains both that lock and the fence's initial ref.
            unsafe {
                dma_resv_add_fence(*resv as *mut c_void, self.dma_fence, DMA_RESV_USAGE_WRITE)
            };
        }
        if let Some(monitor) = self.monitor.take() {
            axtask::publish_prepared_task(monitor);
        }
        self.unlock_all();
        // SAFETY: each successful add took its own reference; release this update's
        // initial reference after the monitor was published and locks released.
        unsafe { dma_fence_put(self.dma_fence) };
        self.dma_fence = ptr::null_mut();
    }

    fn unlock_all(&mut self) {
        for resv in self.resvs.drain(..).rev() {
            // SAFETY: self contains only locks acquired by prepare() and not yet released.
            unsafe { thekernel_dma_resv_unlock(resv as *mut c_void) };
        }
    }

    fn put_i915_objects(&mut self) {
        for object in self.i915_objects.drain(..) {
            // SAFETY: prepare() acquired exactly one GEM reference for each vector entry.
            unsafe {
                intel_gt::i915_gem_object_header_upstream::i915_gem_object_put(
                    object as *mut intel_gt::i915_gem_object_types_upstream::DrmI915GemObject,
                )
            };
        }
    }
}

impl Drop for NativeReservationUpdate {
    fn drop(&mut self) {
        self.unlock_all();
        self.put_i915_objects();
        if !self.dma_fence.is_null() {
            // SAFETY: the update still owns its one initial fence reference.
            unsafe { dma_fence_put(self.dma_fence) };
            self.dma_fence = ptr::null_mut();
        }
    }
}

/// Prepare an atomic cross-layer edge without mutating either reservation.
/// Multi-resv acquisition uses sorted nonblocking WW attempts, releasing the
/// entire prefix on contention rather than sleeping with an inversion-prone
/// partial lock set.
pub(crate) fn prepare_native_reservation_update(
    reservations: &[&Reservation],
    fence: Arc<Fence>,
) -> AxResult<Option<NativeReservationUpdate>> {
    let mut resvs = Vec::new();
    let mut i915_objects = Vec::new();
    {
        let aliases = ALIASES.lock();
        resvs
            .try_reserve_exact(aliases.len())
            .map_err(|_| AxError::NoMemory)?;
        i915_objects
            .try_reserve_exact(aliases.len())
            .map_err(|_| AxError::NoMemory)?;
        for reservation in reservations {
            let key = ptr::from_ref(*reservation) as usize;
            for (&object, alias) in aliases.iter() {
                if Arc::as_ptr(&alias.reservation) as usize == key {
                    resvs.push(alias.resv);
                    if !i915_objects.contains(&object) {
                        // SAFETY: while ALIASES is locked, this alias's backing
                        // reference keeps object alive; get adds an update-owned ref.
                        unsafe {
                            intel_gt::i915_gem_object_header_upstream::i915_gem_object_get(
                                object as *mut intel_gt::i915_gem_object_types_upstream::DrmI915GemObject,
                            )
                        };
                        i915_objects.push(object);
                    }
                }
            }
        }
    }
    resvs.sort_unstable();
    resvs.dedup();
    if resvs.is_empty() {
        return Ok(None);
    }

    // Allocate the Linux fence before locking; defer its task until commit.
    let dma_fence = match allocate_native_dma_fence(fence.clone()) {
        Ok(dma_fence) => dma_fence,
        Err(error) => {
            put_i915_objects(&mut i915_objects);
            return Err(error);
        }
    };
    let monitor = match prepare_native_fence_monitor(dma_fence, fence) {
        Ok(monitor) => monitor,
        Err(error) => {
            // SAFETY: no reservation was changed; retire our private initial reference.
            unsafe { dma_fence_put(dma_fence) };
            put_i915_objects(&mut i915_objects);
            return Err(error);
        }
    };
    for _ in 0..64 {
        let mut locked = 0usize;
        let mut contended = false;
        for resv in &resvs {
            let status = if resvs.len() == 1 {
                // SAFETY: resv was read from a registered live GEM object's reservation
                // while holding ALIASES; the owner ref was retained above.
                unsafe { thekernel_dma_resv_lock(*resv as *mut c_void) }
            } else {
                // SAFETY: same live registered reservation invariant; trylock avoids
                // sleeping while holding an earlier reservation in sorted order.
                unsafe { thekernel_dma_resv_trylock(*resv as *mut c_void) }
            };
            if status != 0 {
                contended = true;
                break;
            }
            locked += 1;
        }
        if contended {
            for resv in resvs[..locked].iter().rev() {
                // SAFETY: this prefix contains only reservations successfully locked
                // during the current attempt.
                unsafe { thekernel_dma_resv_unlock(*resv as *mut c_void) };
            }
            axtask::yield_now();
            continue;
        }
        let mut reserve_failed = false;
        for resv in &resvs {
            // SAFETY: this loop runs only after every resv lock succeeded; the lock
            // protects the reservation's fence array while reserving an insertion slot.
            if unsafe { dma_resv_reserve_fences(*resv as *mut c_void, 1) } != 0 {
                reserve_failed = true;
                break;
            }
        }
        if reserve_failed {
            for resv in resvs.iter().rev() {
                // SAFETY: the full set was successfully locked above and is released here.
                unsafe { thekernel_dma_resv_unlock(*resv as *mut c_void) };
            }
            // SAFETY: reserve failure leaves no publication; release the private fence ref.
            unsafe { dma_fence_put(dma_fence) };
            put_i915_objects(&mut i915_objects);
            return Err(AxError::NoMemory);
        }
        return Ok(Some(NativeReservationUpdate {
            resvs,
            i915_objects,
            dma_fence,
            monitor,
        }));
    }
    // SAFETY: all lock attempts failed and no reservation retained this private fence.
    unsafe { dma_fence_put(dma_fence) };
    put_i915_objects(&mut i915_objects);
    Err(AxError::WouldBlock)
}

fn put_i915_objects(objects: &mut Vec<usize>) {
    for object in objects.drain(..) {
        // SAFETY: each entry is a GEM reference retained from the alias registry.
        unsafe {
            intel_gt::i915_gem_object_header_upstream::i915_gem_object_put(
                object as *mut intel_gt::i915_gem_object_types_upstream::DrmI915GemObject,
            )
        };
    }
}

/// Convert a Linux fence into the native monotonic fence state. The mirror is
/// completed by a deferred task, never signalled just because it was imported.
pub(crate) fn kernel_fence_for_dma(fence: *mut DmaFence) -> AxResult<Arc<Fence>> {
    if fence.is_null() {
        return Err(AxError::InvalidInput);
    }
    // SAFETY: callers pass a live DMA-fence reference, which remains borrowed for
    // this conversion; every fence starts with the ABI-compatible ops pointer.
    if unsafe { (*fence).ops } == ptr::addr_of!(NATIVE_FENCE_OPS).cast() {
        // SAFETY: exact ops identity proves this allocation was constructed as
        // NativeDmaFence, and the caller's reference keeps its initialized Arc live.
        return Ok(unsafe { (*fence.cast::<NativeDmaFence>()).kernel.clone() });
    }
    // SAFETY: the same caller-owned reference keeps the non-null DMA fence alive.
    if unsafe { dma_fence_is_signaled(fence) } {
        let native = Fence::new(false);
        // SAFETY: error is part of the live DMA-fence ABI prefix and may be updated
        // concurrently by its owner, so use the volatile read required by the ABI.
        if unsafe { ptr::read_volatile(ptr::addr_of!((*fence).error)) } != 0 {
            native.signal_error();
        } else {
            native.signal();
        }
        return Ok(native);
    }

    let native = Fence::new(false);
    let monitor = native.clone();
    // SAFETY: caller's reference remains live through get(); retained owns the
    // additional reference transferred into the spawned monitor task.
    let retained = unsafe { dma_fence_get(fence) } as usize;
    if axtask::spawn_with_name(
        move || {
            let raw = retained as *mut DmaFence;
            loop {
                // SAFETY: retained is the reference acquired above and is kept until
                // after this wait loop completes.
                let result = unsafe { dma_fence_wait_timeout(raw, false, MAX_SCHEDULE_TIMEOUT) };
                if result < 0 {
                    monitor.signal_error();
                    break;
                }
                // SAFETY: raw remains protected by the monitor's retained reference.
                if unsafe { dma_fence_is_signaled(raw) } {
                    // SAFETY: raw is the same live ABI fence; volatile read observes
                    // the externally published completion error field.
                    if unsafe { ptr::read_volatile(ptr::addr_of!((*raw).error)) } != 0 {
                        monitor.signal_error();
                    } else {
                        monitor.signal();
                    }
                    break;
                }
                // A zero return is timeout, not completion. MAX_SCHEDULE_TIMEOUT
                // is normally unbounded, but preserve the Linux convention.
                axtask::yield_now();
            }
            // SAFETY: terminate the one reference transferred to this task.
            unsafe { dma_fence_put(raw) };
        },
        "i915-fence-mirror".into(),
    )
    .is_err()
    {
        // SAFETY: task creation failed, so the monitor will not consume its ref.
        unsafe { dma_fence_put(retained as *mut DmaFence) };
        return Err(AxError::NoMemory);
    }
    Ok(native)
}

/// LinuxKPI callback for an upstream reservation insertion. Only a registered
/// alias is mirrored; unrelated Linux reservations remain Linux-only.
pub(crate) fn upstream_reservation_fence_add(resv: *mut c_void, fence: *mut DmaFence, _usage: u32) {
    if resv.is_null() || fence.is_null() {
        return;
    }
    let reservation = ALIASES
        .lock()
        .values()
        .find(|alias| alias.resv == resv as usize)
        .map(|alias| alias.reservation.clone());
    if let Some(reservation) = reservation {
        let fence = require_bridge(
            kernel_fence_for_dma(fence),
            "failed to mirror i915 dma_fence to native reservation",
        );
        require_bridge(
            reservation.append_from_upstream(fence),
            "failed to publish i915 reservation edge",
        );
    }
}

/// Upstream `drm_syncobj_find()`: each returned raw pointer owns one Arc ref.
///
/// # Safety
/// `file` must be the live DRM file supplied to the provider callback; the
/// returned non-null handle must be released exactly once through `syncobj_put`.
pub(crate) unsafe extern "C" fn syncobj_find(file: *mut c_void, handle: u32) -> *mut c_void {
    let Some(native_file) = super::upstream_file::native_file(file) else {
        return ptr::null_mut();
    };
    // SAFETY: native_file() exposes the pointer only during an active per-file
    // callback binding, whose NativeFileBinding guard borrows this DrmFile.
    let native = unsafe { &*native_file.cast::<crate::drm::DrmFile>() };
    let Ok(object) = native.syncobj(handle) else {
        return ptr::null_mut();
    };
    Arc::try_new(SyncobjBridge { object })
        .map(Arc::into_raw)
        .map(|bridge| bridge.cast_mut().cast())
        .unwrap_or(ptr::null_mut())
}

struct SyncobjBridge {
    object: Arc<Syncobj>,
}

/// VFS open-file description retaining a reference to the exact generic
/// Linux `drm_syncobj` object. This is intentionally distinct from
/// `SyncobjBridge`, which backs i915/native timeline operations.
struct UpstreamSyncobjFile {
    syncobj: usize,
}

impl UpstreamSyncobjFile {
    fn try_new(syncobj: *mut c_void) -> AxResult<Arc<Self>> {
        if syncobj.is_null() {
            return Err(AxError::InvalidInput);
        }
        // SAFETY: caller passes a live generic DRM syncobj borrowed for this
        // operation; the helper acquires the VFS OFD's independent ref.
        let owned = unsafe { tk_drm_syncobj_get(syncobj) };
        if owned.is_null() {
            return Err(AxError::NoSuchDevice);
        }
        match Arc::try_new(Self {
            syncobj: owned as usize,
        }) {
            Ok(file) => Ok(file),
            Err(_) => {
                // SAFETY: balance the ref acquired above after OFD allocation failed.
                unsafe { tk_drm_syncobj_put(owned) };
                Err(AxError::NoMemory)
            }
        }
    }

    fn get_ref(&self) -> *mut c_void {
        // SAFETY: the VFS OFD owns this generic syncobj reference throughout the call.
        unsafe { tk_drm_syncobj_get(self.syncobj as *mut c_void) }
    }
}

impl Drop for UpstreamSyncobjFile {
    fn drop(&mut self) {
        if self.syncobj != 0 {
            // SAFETY: the VFS OFD owns exactly this reference until final drop.
            unsafe { tk_drm_syncobj_put(self.syncobj as *mut c_void) };
        }
    }
}

impl FileLike for UpstreamSyncobjFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[drm-syncobj]",
        )))
    }

    fn set_nonblocking(&self, _: bool) -> AxResult<()> {
        Ok(())
    }
}

impl Pollable for UpstreamSyncobjFile {
    fn poll(&self) -> IoEvents {
        IoEvents::empty()
    }

    fn register<'a>(
        &'a self,
        _: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PreparedPollRegistration::try_new(0)?.commit()
    }
}

/// Linux DRM syncobj export helper. The returned descriptor retains the exact
/// generic `drm_syncobj` object via the kernel's existing VFS OFD table.
///
/// # Safety
/// `syncobj` must be a live generic DRM syncobj pointer held by the caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thekernel_drm_syncobj_export_fd(syncobj: *mut c_void) -> c_int {
    let file = match UpstreamSyncobjFile::try_new(syncobj) {
        Ok(file) => file,
        Err(error) => return -LinuxError::from(error).code(),
    };
    match add_file_like(file, true) {
        Ok(fd) => fd,
        Err(error) => -LinuxError::from(error).code(),
    }
}

/// Linux DRM syncobj import helper. On success the returned pointer owns one
/// reference to the same generic object stored in the VFS OFD. Errors use the
/// Linux `ERR_PTR(-errno)` convention.
#[unsafe(no_mangle)]
pub extern "C" fn thekernel_drm_syncobj_import_fd(fd: c_int) -> *mut c_void {
    let file = match get_typed_file::<UpstreamSyncobjFile>(fd) {
        Ok(file) => file,
        Err(error) => return syncobj_err_ptr(error),
    };
    let syncobj = file.get_ref();
    if syncobj.is_null() {
        return syncobj_err_ptr(AxError::NoSuchDevice);
    }
    syncobj
}

///
/// # Safety
/// A non-null handle must be an outstanding `Arc::into_raw` result from
/// `syncobj_find` or `syncobj_create`, retained by the caller for the borrow.
unsafe fn syncobj_bridge<'a>(syncobj: *mut c_void) -> Option<&'a SyncobjBridge> {
    if syncobj.is_null() {
        None
    } else {
        // SAFETY: callers receive these opaque handles only from syncobj_find/create;
        // their matching put callback owns the Arc reference for the whole call.
        Some(unsafe { &*syncobj.cast::<SyncobjBridge>() })
    }
}

/// Linux DRM-core internal syncobj creation, used by i915 contexts.
///
/// # Safety
/// `out` must point to a writable output slot valid for this call;
/// a non-null `fence` must be a live `dma_fence` reference owned by the caller.
pub(crate) unsafe extern "C" fn syncobj_create(
    out: *mut *mut c_void,
    flags: u32,
    fence: *mut c_void,
) -> c_int {
    if out.is_null() || flags & !1 != 0 {
        return -EINVAL;
    }
    let object = Syncobj::new(flags & 1 != 0);
    if !fence.is_null() {
        match kernel_fence_for_dma(fence.cast()) {
            Ok(fence) => object.import_fence(fence),
            Err(_) => return -ENOMEM,
        }
    }
    let Ok(bridge) = Arc::try_new(SyncobjBridge { object }) else {
        return -ENOMEM;
    };
    // SAFETY: the DRM callback contract supplies a writable output slot; bridge's
    // raw Arc reference is intentionally transferred to the caller for syncobj_put.
    unsafe { out.write(Arc::into_raw(bridge).cast_mut().cast()) };
    0
}

/// Replace a binary native syncobj payload with a DMA-fence completion view.
///
/// # Safety
/// `syncobj` must be an outstanding handle from `syncobj_find/create`; a
/// non-null `fence` must be a live DMA-fence reference for the call duration.
pub(crate) unsafe extern "C" fn syncobj_replace_fence(syncobj: *mut c_void, fence: *mut c_void) {
    // SAFETY: upstream invokes this callback with a live handle returned by find/create.
    let Some(syncobj) = (unsafe { syncobj_bridge(syncobj) }) else {
        return;
    };
    if fence.is_null() {
        syncobj.object.reset();
        return;
    }
    let fence = require_bridge(
        kernel_fence_for_dma(fence.cast()),
        "i915 syncobj fence mirror",
    );
    syncobj.object.import_fence(fence);
}

/// Add a timeline point to the native syncobj. The native immutable chain is
/// the authoritative predecessor chain; the allocated upstream node is freed
/// after its fence payload has been imported.
///
/// # Safety
/// `syncobj` must be an outstanding handle; non-null `chain` and `fence` must
/// be live LinuxKPI objects owned by the caller for this callback.
pub(crate) unsafe extern "C" fn syncobj_add_point(
    syncobj: *mut c_void,
    chain: *mut c_void,
    fence: *mut c_void,
    point: u64,
) {
    // SAFETY: upstream invokes this callback with a live handle returned by find/create.
    let Some(syncobj) = (unsafe { syncobj_bridge(syncobj) }) else {
        return;
    };
    if fence.is_null() || point == 0 {
        return;
    }
    let fence = require_bridge(
        kernel_fence_for_dma(fence.cast()),
        "i915 timeline fence mirror",
    );
    require_bridge(
        syncobj.object.submit_point(point, fence),
        "i915 syncobj timeline point",
    );
    if !chain.is_null() {
        // SAFETY: non-null chain is the owned node handed to this callback by i915;
        // its fence was imported before the source node is discarded.
        unsafe { dma_fence_chain_free(chain) };
    }
}

/// Get a Linux reference to the current native syncobj fence, if one exists.
///
/// # Safety
/// `syncobj` must be an outstanding handle from `syncobj_find/create`.
pub(crate) unsafe extern "C" fn syncobj_fence_get(syncobj: *mut c_void) -> *mut c_void {
    // SAFETY: upstream invokes this callback with a live handle returned by find/create.
    let Some(syncobj) = (unsafe { syncobj_bridge(syncobj) }) else {
        return ptr::null_mut();
    };
    match syncobj.object.latest_fence() {
        Some(fence) => native_syncobj_fence_to_dma(syncobj.object.clone(), fence)
            .map(|fence| fence.cast())
            .unwrap_or(ptr::null_mut()),
        None => ptr::null_mut(),
    }
}

/// Resolve an i915 timeline wait point on a native syncobj fence wrapper.
/// Return 1 when the input is not ours so Linux's ordinary chain walker can
/// handle it; otherwise return 0 or a Linux-style negative errno.
///
/// # Safety
/// `fence` must be a writable slot containing null or a live, owned Linux DMA
/// fence reference; replacement preserves that slot's owned-reference contract.
pub(crate) unsafe extern "C" fn syncobj_timeline_fence_find(
    fence: *mut *mut c_void,
    point: u64,
) -> c_int {
    // SAFETY: the Linux callback supplies a writable fence slot; the non-null check
    // protects the slot read and its contents are checked before dereference.
    if fence.is_null() || unsafe { (*fence).is_null() } {
        return -EINVAL;
    }
    // SAFETY: `fence` is a non-null callback-owned slot validated above.
    let current = unsafe { *fence }.cast::<DmaFence>();
    // SAFETY: current came from that live slot and is borrowed for this callback.
    if unsafe { (*current).ops } != ptr::addr_of!(NATIVE_FENCE_OPS).cast() {
        return 1;
    }
    // SAFETY: exact ops identity above proves this is our repr(C) wrapper and
    // the caller owns a fence ref throughout this timeline lookup.
    let bridge = unsafe { &*current.cast::<NativeDmaFence>() };
    let Some(object) = bridge.timeline.clone() else {
        return 1;
    };
    let native = match object.fence_at(point) {
        Ok(fence) => fence,
        Err(_) => return -EINVAL,
    };
    let replacement = match native_syncobj_fence_to_dma(object, native) {
        Ok(fence) => fence,
        Err(_) => return -ENOMEM,
    };
    // SAFETY: caller supplied a writable slot and replacement owns its initial ref;
    // current's slot reference is released only after replacement is installed.
    unsafe {
        *fence = replacement.cast();
        dma_fence_put(current);
    }
    0
}

/// Drop one reference returned by `syncobj_find` or `syncobj_create`.
///
/// # Safety
/// A non-null `syncobj` must be one outstanding raw Arc reference returned by
/// `syncobj_find/create`, and must not have been passed to `syncobj_put` before.
pub(crate) unsafe extern "C" fn syncobj_put(syncobj: *mut c_void) {
    if !syncobj.is_null() {
        // SAFETY: find/create transfer exactly one Arc::into_raw reference to each
        // non-null handle, and DRM calls put once for that owned reference.
        unsafe { drop(Arc::from_raw(syncobj.cast::<SyncobjBridge>())) };
    }
}

const _: () = {
    assert!(offset_of!(NativeDmaFence, base) == 0);
    assert!(offset_of!(NativeDmaFence, lock) == size_of::<DmaFence>());
    assert!(offset_of!(DmaFence, timestamp_union) == 16);
    assert!(size_of::<DmaFenceTimestamp>() >= size_of::<RcuHead>());
};

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn unpublished_native_fence_monitor_releases_its_dma_reference() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let fence = Fence::new(false);
        let dma = allocate_native_dma_fence(fence.clone()).unwrap();
        // SAFETY: dma is the live allocation's initialized base fence and the test
        // retains its initial reference until the final put below.
        let before = unsafe { (*dma).refcount.refcount.refs.counter };
        let monitor = prepare_native_fence_monitor(dma, fence).unwrap();
        assert!(monitor.is_some());
        // SAFETY: the same retained reference keeps the initialized test fence live.
        assert_eq!(unsafe { (*dma).refcount.refcount.refs.counter }, before + 1);
        drop(monitor);
        // SAFETY: the same retained reference remains live after dropping the task reservation.
        assert_eq!(unsafe { (*dma).refcount.refcount.refs.counter }, before);
        // SAFETY: this is the final release for the fence's initial owned reference.
        unsafe { dma_fence_put(dma) };
    }

    #[test]
    fn void_provider_does_not_continue_after_bridge_failure() {
        let result = std::panic::catch_unwind(|| {
            let _: () = require_bridge(Err(AxError::NoMemory), "injected failure");
        });
        assert!(result.is_err());
    }

    #[test]
    fn syncobj_vfs_fd_import_preserves_exact_object_and_reference_lifetime() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let mut drm = unsafe {
            core::mem::zeroed::<tk_drm_upstream::drm_device_types::DrmDevice>()
        };
        drm.driver_features = tk_drm_upstream::drm_ioctl_upstream::DRIVER_SYNCOBJ;
        let mut file = unsafe {
            core::mem::zeroed::<tk_drm_upstream::drm_file_types::DrmFile>()
        };
        tk_linux_kapi::xarray::xa_init_flags(&mut file.syncobj_xa, 0);
        let mut create = tk_drm_upstream::uapi_drm_types::DrmSyncobjCreate {
            handle: 0,
            flags: 0,
        };
        assert_eq!(
            unsafe {
                tk_drm_upstream::drm_syncobj_upstream::drm_syncobj_create_ioctl(
                    &mut drm,
                    (&mut create as *mut tk_drm_upstream::uapi_drm_types::DrmSyncobjCreate)
                        .cast(),
                    &mut file,
                )
            },
            0
        );
        assert_ne!(create.handle, 0);

        let object = unsafe {
            tk_drm_upstream::drm_syncobj_upstream::tk_drm_syncobj_find(&mut file, create.handle)
        };
        assert!(!object.is_null());
        // SAFETY: `find` returns one independently owned syncobj reference.
        let base = unsafe { Arc::from_raw(object) };
        assert_eq!(
            Arc::strong_count(&base),
            2,
            "file handle plus this reference"
        );

        // The hosted scheduler task has no userspace Thread/FD table. Exercise
        // the real VFS OFD in an explicit table instead of resolving current().
        let table = crate::file::FdTable::new().unwrap();
        let ofd = unsafe { UpstreamSyncobjFile::try_new(Arc::as_ptr(&base).cast_mut().cast()) }.unwrap();
        let fd = table.add_file_like(ofd, true, 64).unwrap();
        assert_eq!(Arc::strong_count(&base), 3, "the VFS OFD owns a reference");

        let imported_ptr = table.get_description(fd).unwrap().file_handle()
            .downcast::<UpstreamSyncobjFile>().unwrap().get_ref();
        assert!(!imported_ptr.is_null());
        assert!((imported_ptr as isize) >= 0);
        // SAFETY: successful import returns one owned reference to this object.
        let imported = unsafe {
            Arc::from_raw(
                imported_ptr.cast::<tk_drm_upstream::drm_syncobj_upstream::DrmSyncobj>(),
            )
        };
        assert!(Arc::ptr_eq(&base, &imported));
        assert_eq!(Arc::strong_count(&base), 4);
        drop(imported);
        assert_eq!(Arc::strong_count(&base), 3);

        drop(table.close(fd).unwrap());
        assert_eq!(
            Arc::strong_count(&base),
            2,
            "closing the fd drops its owner ref"
        );
        let still_live = unsafe {
            tk_drm_upstream::drm_syncobj_upstream::tk_drm_syncobj_find(&mut file, create.handle)
        };
        assert_eq!(still_live, Arc::as_ptr(&base).cast_mut());
        unsafe { tk_drm_upstream::drm_syncobj_upstream::tk_drm_syncobj_put(still_live) };
        drop(base);
        unsafe { tk_drm_upstream::drm_syncobj_upstream::tk_drm_syncobj_release(&mut file) };

        assert!(matches!(table.get_description(i32::MAX), Err(AxError::BadFileDescriptor)));
    }
}
