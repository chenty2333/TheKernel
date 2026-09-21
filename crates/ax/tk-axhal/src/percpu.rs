//! CPU-local data structures.

pub use axplat::percpu::*;

/// Initializes CPU-local data structures for the primary core.
///
/// Host builds shadow `axplat::percpu::init_primary`.  On Linux
/// `percpu::init()` places every area in a `std::alloc::alloc` buffer that it
/// never fills, and the `non-zero-vma` accessors address that buffer rather
/// than the `.percpu` image.  A test thread whose allocator arena hands back
/// reused memory then reads a `LazyInit` state or a `Weak::new()` as heap
/// garbage.  Copy the image into every area before anything can use them.
/// The `Once` also stops a second test thread from reaching the areas before
/// `init()` has published them.
#[cfg(not(target_os = "none"))]
pub fn init_primary(cpu_id: usize) {
    unsafe extern "C" {
        static _percpu_load_start: u8;
        static _percpu_load_end: u8;
    }
    static AREAS: spin::Once = spin::Once::new();
    AREAS.call_once(|| {
        let areas = percpu::init();
        let image = core::ptr::addr_of!(_percpu_load_start);
        let size = percpu::percpu_area_size();
        debug_assert_eq!(
            core::ptr::addr_of!(_percpu_load_end) as usize - image as usize,
            size
        );
        for cpu in 0..areas {
            // SAFETY: `init()` just allocated `areas` areas of at least `size`
            // bytes, separate from the image, and nothing uses them before
            // this returns.
            unsafe {
                core::ptr::copy_nonoverlapping(image, percpu::percpu_area_base(cpu) as *mut u8, size);
            }
        }
    });
    axplat::percpu::init_primary(cpu_id);
}

#[percpu::def_percpu]
static CURRENT_TASK_PTR: usize = 0;

/// Gets the pointer to the current task with preemption-safety.
///
/// Preemption may be enabled when calling this function. This function will
/// guarantee the correctness even the current task is preempted.
#[inline]
pub fn current_task_ptr<T>() -> *const T {
    unsafe {
        // x86_64 reads the per-CPU task pointer with one `gs:[off]` instruction.
        CURRENT_TASK_PTR.read_current_raw() as _
    }
}

/// Sets the pointer to the current task with preemption-safety.
///
/// Preemption may be enabled when calling this function. This function will
/// guarantee the correctness even the current task is preempted.
///
/// # Safety
///
/// The given `ptr` must be pointed to a valid task structure.
#[inline]
pub unsafe fn set_current_task_ptr<T>(ptr: *const T) {
    unsafe { CURRENT_TASK_PTR.write_current_raw(ptr as usize) }
}
