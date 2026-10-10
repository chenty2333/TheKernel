// SPDX-License-Identifier: MIT
// Translated under the MIT option of "GPL-2.0 OR MIT" from Linux v7.2.3
// drivers/gpu/drm/ttm/ttm_device.c, ttm_pool.c, ttm_tt.c, ttm_resource.c and
// ttm_sys_manager.c. Copyright (c) 2006-2009 VMware, Inc., Palo Alto, CA., USA;
// Copyright 2020 Advanced Micro Devices, Inc.
//
// Scope: the device-level chain reached from ttm_device_init()/ttm_device_fini():
// global state, pool-type init/fini and the pool shrinker, tt-manager limits,
// resource-manager init and the system manager. Page-level pool allocation
// (ttm_pool_alloc/free, ttm_pool_shrink), ttm_resource_init and the BO layer are
// not reached by device init/fini and are not ported; each place that would
// reach them fails closed.
//
// Layouts are the x86_64 Linux 7.2.3 wt-dev layouts (probed from the upstream
// headers), asserted below.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn, dead_code)]

use core::{
    cell::UnsafeCell,
    ffi::{c_int, c_long, c_ulong, c_void},
    mem::{offset_of, size_of, zeroed},
    ptr::{addr_of_mut, null_mut},
    sync::atomic::{AtomicI32, AtomicU64, AtomicI64, Ordering},
};

use crate::{
    i915_gem_object_types_upstream::Page,
    intel_engine_cs_upstream::{ListHead, Spinlock},
    linux::{
        gem_memory::Shrinker,
        shmem::{alloc_pages, totalram_pages, __free_pages_c},
    },
    linux_config::{EINVAL, ENOMEM, GFP_KERNEL, __GFP_NOWARN},
    linux_list::{INIT_LIST_HEAD, list_add_tail, list_del, list_empty},
    linux_locks::{spin_lock, spin_lock_init, spin_unlock},
    linux_memory::{kzalloc, shrinker_alloc, shrinker_free, shrinker_register},
    linux_workqueue::{alloc_workqueue, destroy_workqueue, drain_workqueue},
};

pub const TTM_NUM_MEM_TYPES: usize = 9;
pub const TTM_MAX_BO_PRIORITY: usize = 4;
pub const TTM_NUM_MOVE_FENCES: usize = 8;
pub const TTM_NUM_CACHING_TYPES: usize = 3;
pub const TTM_PL_SYSTEM: usize = 0;

/// `enum ttm_caching` from include/drm/ttm/ttm_caching.h.
pub const TTM_CACHING_UNCACHED: u32 = 0;
pub const TTM_CACHING_WRITE_COMBINED: u32 = 1;
pub const TTM_CACHING_CACHED: u32 = 2;

/// TTM_ALLOCATION_POOL_* from include/drm/ttm/ttm_allocation.h.
pub const TTM_ALLOCATION_POOL_USE_DMA_ALLOC: u32 = 1 << 8;
pub const TTM_ALLOCATION_POOL_USE_DMA32: u32 = 1 << 9;

/// MAX_PAGE_ORDER on x86_64 is 10, so NR_PAGE_ORDERS is 11.
pub const NR_PAGE_ORDERS: usize = 11;
/// TTM_SHRINKER_BATCH = (1 << (MAX_PAGE_ORDER / 2)) * NR_PAGE_ORDERS.
const TTM_SHRINKER_BATCH: c_long = (1 << (10 / 2)) * NR_PAGE_ORDERS as c_long;
/// SHRINKER_NUMA_AWARE = BIT(2); SHRINK_EMPTY = ~0UL - 1.
const SHRINKER_NUMA_AWARE: u32 = 1 << 2;
const SHRINK_STOP: c_ulong = !0;
const SHRINK_EMPTY: c_ulong = !0 - 1;
/// NUMA_NO_NODE.
const NUMA_NO_NODE: i32 = -1;
const PAGE_SHIFT: u32 = 12;
/// `page_pool_size` module parameter at its default of 0.
const PAGE_POOL_SIZE: u64 = 0;
/// WQ_UNBOUND, WQ_MEM_RECLAIM, WQ_HIGHPRI.
const WQ_UNBOUND: u32 = 1 << 1;
const WQ_MEM_RECLAIM: u32 = 1 << 3;
const WQ_HIGHPRI: u32 = 1 << 4;
const TTM_DEVICE_WQ_MAX_ACTIVE: c_int = 16;
const __GFP_ZERO: u32 = 1 << 8;

/// Byte offsets inside `struct shrinker` (x86_64 Linux 7.2.3), probed from the
/// upstream header. The Rust `Shrinker` is opaque, so the callbacks are
/// written at these offsets.
const SHRINKER_COUNT_OFFSET: usize = 0;
const SHRINKER_SCAN_OFFSET: usize = 8;
const SHRINKER_BATCH_OFFSET: usize = 16;
const SHRINKER_SEEKS_OFFSET: usize = 24;

/// A TTM callback slot. The Linux signatures are not represented: every slot
/// is only stored by the device code here, and the callbacks that are reached
/// fail closed (see the hooks below).
pub type TtmHook = Option<unsafe extern "C" fn()>;

/// `struct ttm_device_funcs` (14 pointers).
#[repr(C)]
pub struct TtmDeviceFuncs {
    pub ttm_tt_create: TtmHook,
    pub ttm_tt_populate: TtmHook,
    pub ttm_tt_unpopulate: TtmHook,
    pub ttm_tt_destroy: TtmHook,
    pub eviction_valuable: TtmHook,
    pub evict_flags: TtmHook,
    pub move_: TtmHook,
    pub delete_mem_notify: TtmHook,
    pub swap_notify: TtmHook,
    pub io_mem_reserve: TtmHook,
    pub io_mem_free: TtmHook,
    pub io_mem_pfn: TtmHook,
    pub access_memory: TtmHook,
    pub release_notify: TtmHook,
}
const _: [(); 112] = [(); size_of::<TtmDeviceFuncs>()];

/// `struct ttm_resource_manager_func` (5 pointers).
#[repr(C)]
pub struct TtmResourceManagerFunc {
    pub alloc: TtmHook,
    pub free: TtmHook,
    pub intersects: TtmHook,
    pub compatible: TtmHook,
    pub debug: TtmHook,
}
const _: [(); 40] = [(); size_of::<TtmResourceManagerFunc>()];

/// `struct ttm_resource_manager` (184 bytes).
#[repr(C, align(8))]
pub struct TtmResourceManager {
    pub use_type: bool,
    pub use_tt: bool,
    _flags_pad: [u8; 6],
    pub bdev: *mut TtmDevice,
    pub size: u64,
    pub func: *const TtmResourceManagerFunc,
    pub eviction_lock: Spinlock,
    _eviction_lock_pad: [u8; 4],
    pub eviction_fences: [*mut c_void; TTM_NUM_MOVE_FENCES],
    pub lru: [ListHead; TTM_MAX_BO_PRIORITY],
    pub usage: u64,
    pub cg: *mut c_void,
}
const _: [(); 184] = [(); size_of::<TtmResourceManager>()];
const _: [(); 8] = [(); offset_of!(TtmResourceManager, bdev)];
const _: [(); 24] = [(); offset_of!(TtmResourceManager, func)];
const _: [(); 32] = [(); offset_of!(TtmResourceManager, eviction_lock)];
const _: [(); 40] = [(); offset_of!(TtmResourceManager, eviction_fences)];
const _: [(); 104] = [(); offset_of!(TtmResourceManager, lru)];
const _: [(); 168] = [(); offset_of!(TtmResourceManager, usage)];
const _: [(); 176] = [(); offset_of!(TtmResourceManager, cg)];

/// `struct list_lru` (8 bytes: the per-node pointer; memcg is not configured).
#[repr(C)]
pub struct ListLru {
    node: *mut ListLruNode,
}
const _: [(); 8] = [(); size_of::<ListLru>()];

/// The single per-node list of a `ListLru`, with its item count.
#[repr(C)]
struct ListLruNode {
    list: ListHead,
    nr_items: usize,
}

/// `struct ttm_pool_type` (40 bytes).
#[repr(C)]
pub struct TtmPoolType {
    pub pool: *mut TtmPool,
    pub order: u32,
    pub caching: u32,
    pub shrinker_list: ListHead,
    pub pages: ListLru,
}
const _: [(); 40] = [(); size_of::<TtmPoolType>()];
const _: [(); 16] = [(); offset_of!(TtmPoolType, shrinker_list)];
const _: [(); 32] = [(); offset_of!(TtmPoolType, pages)];

/// `struct ttm_pool` (1336 bytes).
#[repr(C, align(8))]
pub struct TtmPool {
    pub dev: *mut c_void,
    pub nid: i32,
    pub alloc_flags: u32,
    pub caching: [[TtmPoolType; NR_PAGE_ORDERS]; TTM_NUM_CACHING_TYPES],
}
const _: [(); 1336] = [(); size_of::<TtmPool>()];
const _: [(); 8] = [(); offset_of!(TtmPool, nid)];
const _: [(); 12] = [(); offset_of!(TtmPool, alloc_flags)];
const _: [(); 16] = [(); offset_of!(TtmPool, caching)];

/// `struct ttm_device` (1672 bytes). Embedded in `drm_i915_private` as `bdev`.
#[repr(C, align(8))]
pub struct TtmDevice {
    pub device_list: ListHead,
    pub alloc_flags: u32,
    _alloc_flags_pad: u32,
    pub funcs: *const TtmDeviceFuncs,
    pub sysman: TtmResourceManager,
    pub man_drv: [*mut TtmResourceManager; TTM_NUM_MEM_TYPES],
    pub vma_manager: *mut c_void,
    pub pool: TtmPool,
    pub lru_lock: Spinlock,
    _lru_lock_pad: [u8; 4],
    pub unevictable: ListHead,
    pub dev_mapping: *mut c_void,
    pub wq: *mut c_void,
}
const _: [(); 1672] = [(); size_of::<TtmDevice>()];
const _: [(); 0] = [(); offset_of!(TtmDevice, device_list)];
const _: [(); 24] = [(); offset_of!(TtmDevice, funcs)];
const _: [(); 32] = [(); offset_of!(TtmDevice, sysman)];
const _: [(); 216] = [(); offset_of!(TtmDevice, man_drv)];
const _: [(); 288] = [(); offset_of!(TtmDevice, vma_manager)];
const _: [(); 296] = [(); offset_of!(TtmDevice, pool)];
const _: [(); 1632] = [(); offset_of!(TtmDevice, lru_lock)];
const _: [(); 1640] = [(); offset_of!(TtmDevice, unevictable)];
const _: [(); 1656] = [(); offset_of!(TtmDevice, dev_mapping)];
const _: [(); 1664] = [(); offset_of!(TtmDevice, wq)];

/// `struct shrink_control` (32 bytes), the parts the pool shrinker reads.
#[repr(C)]
pub struct ShrinkControl {
    pub gfp_mask: u32,
    pub nid: i32,
    pub memcg: *mut c_void,
    pub nr_to_scan: c_ulong,
    pub nr_scanned: c_ulong,
}
const _: [(); 32] = [(); size_of::<ShrinkControl>()];

type ShrinkCountFn = unsafe extern "C" fn(*mut Shrinker, *mut ShrinkControl) -> c_ulong;
type ShrinkScanFn = unsafe extern "C" fn(*mut Shrinker, *mut ShrinkControl) -> c_ulong;

/// `struct ttm_global` (`ttm_glob`). The use count lives in
/// `TTM_GLOBAL_MUTEX`, because Linux's `memset()` in `ttm_global_release()`
/// clears only this struct.
#[repr(C)]
pub struct TtmGlobal {
    pub dummy_read_page: *mut Page,
    pub device_list: ListHead,
    pub bo_count: AtomicI32,
}

/// Storage for the file-scope globals of the TTM sources. Each is mutated
/// through raw pointers under the lock named beside it.
pub struct StaticCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for StaticCell<T> {}
impl<T> StaticCell<T> {
    const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }
    fn get(&self) -> *mut T {
        self.0.get()
    }
}

/// `ttm_global_mutex` together with `ttm_glob_use_count`.
static TTM_GLOBAL_MUTEX: spin::Mutex<u32> = spin::Mutex::new(0);
static TTM_GLOBAL: StaticCell<TtmGlobal> = StaticCell::new(TtmGlobal {
    dummy_read_page: null_mut(),
    device_list: ListHead {
        next: null_mut(),
        prev: null_mut(),
    },
    bo_count: AtomicI32::new(0),
});
/// `ttm_pages_limit` and `ttm_dma32_pages_limit` (ttm_tt.c).
static TTM_PAGES_LIMIT: AtomicU64 = AtomicU64::new(0);
static TTM_DMA32_PAGES_LIMIT: AtomicU64 = AtomicU64::new(0);

/// The pool-wide statics of ttm_pool.c. `shrinker_lock` guards
/// `shrinker_list` and each pool type's `shrinker_list` link.
struct TtmPoolGlobals {
    shrinker_lock: Spinlock,
    shrinker_list: ListHead,
    global_write_combined: [TtmPoolType; NR_PAGE_ORDERS],
    global_uncached: [TtmPoolType; NR_PAGE_ORDERS],
    global_dma32_write_combined: [TtmPoolType; NR_PAGE_ORDERS],
    global_dma32_uncached: [TtmPoolType; NR_PAGE_ORDERS],
    pool_node_limit: [u64; 1],
    allocated_pages: [AtomicI64; 1],
    mm_shrinker: *mut Shrinker,
}
static TTM_POOL: StaticCell<TtmPoolGlobals> = StaticCell::new(unsafe { zeroed() });
/// `pool_shrink_rwsem`: shrinkers take it shared, `ttm_pool_synchronize_shrinkers()`
/// takes it exclusive. Shrinkers never run here, so the exclusive section is
/// immediate.
static POOL_SHRINK_LOCK: spin::Mutex<()> = spin::Mutex::new(());

// Fail-closed callbacks. Each one is reachable only through a path that
// needs the unported TTM object layer (BO, resource, page pool allocation).
macro_rules! unported_hook {
    ($name:ident, $what:literal) => {
        unsafe extern "C" fn $name() {
            panic!(concat!(
                "TTM callback ",
                $what,
                " is not ported in TheKernel; its path needs LMEM/TTM objects"
            ))
        }
    };
}
unported_hook!(sys_man_alloc_unported, "ttm_sys_man_alloc (ttm_resource_init)");
unported_hook!(sys_man_free_unported, "ttm_sys_man_free (ttm_resource_fini)");

/// `ttm_sys_manager_func`. Linux leaves intersects/compatible/debug NULL.
static TTM_SYS_MANAGER_FUNC: TtmResourceManagerFunc = TtmResourceManagerFunc {
    alloc: Some(sys_man_alloc_unported),
    free: Some(sys_man_free_unported),
    intersects: None,
    compatible: None,
    debug: None,
};

/// Linux `dev_to_node()`. TheKernel's device model records no ACPI proximity
/// data (`_PXM`) and the target is single-node UMA, so Linux's default
/// `dev->numa_node` of `NUMA_NO_NODE` applies.
fn dev_to_node(_dev: *mut c_void) -> i32 {
    NUMA_NO_NODE
}

/// `ttm_get_node_memory_size()` for the single memory node.
fn ttm_get_node_memory_size(_nid: usize) -> u64 {
    totalram_pages() as u64 * (1u64 << PAGE_SHIFT)
}

#[inline]
unsafe fn ttm_pool_uses_dma_alloc(pool: *const TtmPool) -> bool {
    (*pool).alloc_flags & TTM_ALLOCATION_POOL_USE_DMA_ALLOC != 0
}

#[inline]
unsafe fn ttm_pool_uses_dma32(pool: *const TtmPool) -> bool {
    (*pool).alloc_flags & TTM_ALLOCATION_POOL_USE_DMA32 != 0
}

/// Linux `list_lru_init()`. The single node is allocated; a failed allocation
/// is returned and, as in `ttm_pool_type_init()`, not checked by the caller.
unsafe fn list_lru_init(lru: *mut ListLru) -> c_int {
    let node = kzalloc(size_of::<ListLruNode>(), GFP_KERNEL).cast::<ListLruNode>();
    if node.is_null() {
        return -ENOMEM;
    }
    INIT_LIST_HEAD(addr_of_mut!((*node).list));
    (*lru).node = node;
    0
}

/// Linux `list_lru_walk()`. Pool pages reach a `list_lru` only through
/// `ttm_pool_free_range()`, which this port does not reach, so the walk always
/// sees an empty list. A non-empty list means a page entered the pool through a
/// path that is not ported, and the walk fails closed.
unsafe fn list_lru_walk(lru: *mut ListLru) -> c_long {
    let node = (*lru).node;
    assert!(!node.is_null(), "list_lru_walk on an uninitialized list_lru");
    assert!(
        (*node).nr_items == 0,
        "list_lru_walk: pool pages exist, but ttm_pool_free_range is not ported"
    );
    0
}

/// Linux `ttm_pool_dispose_list()`. The list is only ever filled by
/// `list_lru_walk()`, which cannot produce entries, so a non-empty list is a
/// programming error here.
unsafe fn ttm_pool_dispose_list(_pt: *mut TtmPoolType, dispose: *mut ListHead) {
    assert!(
        list_empty(&*dispose),
        "ttm_pool_dispose_list: ttm_pool_free_page is not ported"
    );
}

/// Linux `ttm_pool_type_init()`.
unsafe fn ttm_pool_type_init(
    pt: *mut TtmPoolType,
    pool: *mut TtmPool,
    caching: u32,
    order: u32,
) {
    (*pt).pool = pool;
    (*pt).caching = caching;
    (*pt).order = order;
    let _ = list_lru_init(addr_of_mut!((*pt).pages));

    let g = TTM_POOL.get();
    spin_lock(&mut (*g).shrinker_lock);
    list_add_tail(addr_of_mut!((*pt).shrinker_list), addr_of_mut!((*g).shrinker_list));
    spin_unlock(&mut (*g).shrinker_lock);
}

/// Linux `ttm_pool_type_fini()`.
unsafe fn ttm_pool_type_fini(pt: *mut TtmPoolType) {
    let mut dispose = ListHead {
        next: null_mut(),
        prev: null_mut(),
    };
    INIT_LIST_HEAD(&mut dispose);

    let g = TTM_POOL.get();
    spin_lock(&mut (*g).shrinker_lock);
    list_del(addr_of_mut!((*pt).shrinker_list));
    spin_unlock(&mut (*g).shrinker_lock);

    // Linux: list_lru_walk(&pt->pages, pool_move_to_dispose_list, &dispose, LONG_MAX).
    let _ = list_lru_walk(addr_of_mut!((*pt).pages));
    ttm_pool_dispose_list(pt, &mut dispose);
}

/// Linux `ttm_pool_select_type()`. Returns null for cached pages, whose pool
/// types live in the device's own pool.
unsafe fn ttm_pool_select_type(
    pool: *mut TtmPool,
    caching: u32,
    order: usize,
) -> *mut TtmPoolType {
    if ttm_pool_uses_dma_alloc(pool) {
        return addr_of_mut!((*pool).caching[caching as usize][order]);
    }

    let g = TTM_POOL.get();
    match caching {
        TTM_CACHING_WRITE_COMBINED => {
            if ttm_pool_uses_dma32(pool) {
                addr_of_mut!((*g).global_dma32_write_combined[order])
            } else {
                addr_of_mut!((*g).global_write_combined[order])
            }
        }
        TTM_CACHING_UNCACHED => {
            if ttm_pool_uses_dma32(pool) {
                addr_of_mut!((*g).global_dma32_uncached[order])
            } else {
                addr_of_mut!((*g).global_uncached[order])
            }
        }
        _ => null_mut(),
    }
}

/// Linux `ttm_pool_synchronize_shrinkers()`.
unsafe fn ttm_pool_synchronize_shrinkers() {
    drop(POOL_SHRINK_LOCK.lock());
}

/// Linux `ttm_pool_init()`.
pub unsafe fn ttm_pool_init(pool: *mut TtmPool, dev: *mut c_void, nid: i32, alloc_flags: u32) {
    WARN_ON!(dev.is_null() && ttm_pool_uses_dma_alloc(pool));

    (*pool).dev = dev;
    (*pool).nid = nid;
    (*pool).alloc_flags = alloc_flags;

    for i in 0..TTM_NUM_CACHING_TYPES {
        for j in 0..NR_PAGE_ORDERS {
            let pt = ttm_pool_select_type(pool, i as u32, j);
            // Only pool types owned by this pool are initialized here.
            if pt != addr_of_mut!((*pool).caching[i][j]) {
                continue;
            }
            ttm_pool_type_init(pt, pool, i as u32, j as u32);
        }
    }
}

/// Linux `ttm_pool_fini()`.
pub unsafe fn ttm_pool_fini(pool: *mut TtmPool) {
    for i in 0..TTM_NUM_CACHING_TYPES {
        for j in 0..NR_PAGE_ORDERS {
            let pt = ttm_pool_select_type(pool, i as u32, j);
            if pt != addr_of_mut!((*pool).caching[i][j]) {
                continue;
            }
            ttm_pool_type_fini(pt);
        }
    }

    // No shrinker may be freeing pages from the pool concurrently.
    ttm_pool_synchronize_shrinkers();
}

/// Linux `ttm_pool_shrinker_count()`.
unsafe extern "C" fn ttm_pool_shrinker_count(_shrink: *mut Shrinker, sc: *mut ShrinkControl) -> c_ulong {
    let nid = (*sc).nid as usize;
    assert!(nid < 1, "ttm_pool_shrinker_count: node outside the single TTM node");
    let num_pages = (*TTM_POOL.get()).allocated_pages[nid].load(Ordering::Relaxed);
    if num_pages != 0 {
        num_pages as c_ulong
    } else {
        SHRINK_EMPTY
    }
}

/// Linux `ttm_pool_shrinker_scan()`. Freeing pool pages (`ttm_pool_shrink()`)
/// is not ported, but with no pool pages allocated its result is the Linux
/// one: nothing freed, so SHRINK_STOP.
unsafe extern "C" fn ttm_pool_shrinker_scan(_shrink: *mut Shrinker, sc: *mut ShrinkControl) -> c_ulong {
    let nid = (*sc).nid as usize;
    assert!(nid < 1, "ttm_pool_shrinker_scan: node outside the single TTM node");
    assert!(
        (*TTM_POOL.get()).allocated_pages[nid].load(Ordering::Relaxed) == 0,
        "ttm_pool_shrinker_scan: pool pages exist, but ttm_pool_shrink is not ported"
    );
    (*sc).nr_scanned = 0;
    SHRINK_STOP
}

/// Linux `ttm_pool_mgr_init()`.
unsafe fn ttm_pool_mgr_init(_num_pages: u64) -> c_int {
    let g = TTM_POOL.get();

    // for_each_node(nid): TheKernel exposes a single memory node.
    for nid in 0..1usize {
        if PAGE_POOL_SIZE == 0 {
            let node_size = ttm_get_node_memory_size(nid);
            (*g).pool_node_limit[nid] = (node_size >> PAGE_SHIFT) / 2;
        } else {
            (*g).pool_node_limit[nid] = PAGE_POOL_SIZE;
        }
    }

    spin_lock_init(&mut (*g).shrinker_lock);
    INIT_LIST_HEAD(addr_of_mut!((*g).shrinker_list));

    for i in 0..NR_PAGE_ORDERS {
        ttm_pool_type_init(addr_of_mut!((*g).global_write_combined[i]), null_mut(), TTM_CACHING_WRITE_COMBINED, i as u32);
        ttm_pool_type_init(addr_of_mut!((*g).global_uncached[i]), null_mut(), TTM_CACHING_UNCACHED, i as u32);
        ttm_pool_type_init(addr_of_mut!((*g).global_dma32_write_combined[i]), null_mut(), TTM_CACHING_WRITE_COMBINED, i as u32);
        ttm_pool_type_init(addr_of_mut!((*g).global_dma32_uncached[i]), null_mut(), TTM_CACHING_UNCACHED, i as u32);
    }

    // debugfs "page_pool" and "page_pool_shrink" files: CONFIG_DEBUG_FS is not
    // observable in TheKernel (no debugfs), so nothing is registered.

    let shrinker = shrinker_alloc(SHRINKER_NUMA_AWARE, c"drm-ttm_pool".as_ptr());
    if shrinker.is_null() {
        return -ENOMEM;
    }
    (*g).mm_shrinker = shrinker;

    let base = shrinker.cast::<u8>();
    base.add(SHRINKER_COUNT_OFFSET)
        .cast::<ShrinkCountFn>()
        .write(ttm_pool_shrinker_count);
    base.add(SHRINKER_SCAN_OFFSET)
        .cast::<ShrinkScanFn>()
        .write(ttm_pool_shrinker_scan);
    base.add(SHRINKER_BATCH_OFFSET).cast::<c_long>().write(TTM_SHRINKER_BATCH);
    base.add(SHRINKER_SEEKS_OFFSET).cast::<c_int>().write(1);

    shrinker_register(shrinker);
    0
}

/// Linux `ttm_pool_mgr_fini()`.
unsafe fn ttm_pool_mgr_fini() {
    let g = TTM_POOL.get();
    for i in 0..NR_PAGE_ORDERS {
        ttm_pool_type_fini(addr_of_mut!((*g).global_write_combined[i]));
        ttm_pool_type_fini(addr_of_mut!((*g).global_uncached[i]));
        ttm_pool_type_fini(addr_of_mut!((*g).global_dma32_write_combined[i]));
        ttm_pool_type_fini(addr_of_mut!((*g).global_dma32_uncached[i]));
    }

    shrinker_free((*g).mm_shrinker);
    WARN_ON!(!list_empty(&(*g).shrinker_list));
}

/// Linux `ttm_tt_mgr_init()`: the limits are set once, from the first caller.
unsafe fn ttm_tt_mgr_init(num_pages: u64, num_dma32_pages: u64) {
    if TTM_PAGES_LIMIT.load(Ordering::Relaxed) == 0 {
        TTM_PAGES_LIMIT.store(num_pages, Ordering::Relaxed);
    }
    if TTM_DMA32_PAGES_LIMIT.load(Ordering::Relaxed) == 0 {
        TTM_DMA32_PAGES_LIMIT.store(num_dma32_pages, Ordering::Relaxed);
    }
}

/// Linux `ttm_global_init()`.
unsafe fn ttm_global_init() -> c_int {
    let mut use_count = TTM_GLOBAL_MUTEX.lock();
    *use_count += 1;
    if *use_count > 1 {
        return 0;
    }

    // si_meminfo(): totalram in pages; totalhigh is 0 on x86_64.
    let totalram = totalram_pages() as u64;
    // Limit the pool to about 50% of system memory.
    let num_pages = totalram / 2;
    // DMA32 is limited to 2 GiB.
    let num_dma32 = totalram.min(2u64 << (30 - PAGE_SHIFT));

    // Return value ignored, as in Linux.
    let _ = ttm_pool_mgr_init(num_pages);
    ttm_tt_mgr_init(num_pages, num_dma32);

    // Linux first tries GFP_DMA32 and falls back to plain pages. TheKernel's
    // page allocator has no DMA32 zone, so the one attempt is the fallback.
    let glob = TTM_GLOBAL.get();
    (*glob).dummy_read_page = alloc_pages(GFP_KERNEL | __GFP_ZERO | __GFP_NOWARN, 0);
    if (*glob).dummy_read_page.is_null() {
        *use_count -= 1;
        return -ENOMEM;
    }

    INIT_LIST_HEAD(addr_of_mut!((*glob).device_list));
    (*glob).bo_count.store(0, Ordering::Relaxed);
    0
}

/// Linux `ttm_global_release()`.
unsafe fn ttm_global_release() {
    let mut use_count = TTM_GLOBAL_MUTEX.lock();
    *use_count -= 1;
    if *use_count > 0 {
        return;
    }

    ttm_pool_mgr_fini();
    // debugfs_remove(ttm_debugfs_root): no debugfs root exists.
    let glob = TTM_GLOBAL.get();
    __free_pages_c((*glob).dummy_read_page, 0);
    // memset(glob, 0, sizeof(*glob))
    core::ptr::write_bytes(glob, 0, 1);
}

/// Linux `ttm_resource_manager_init()`.
pub unsafe fn ttm_resource_manager_init(man: *mut TtmResourceManager, bdev: *mut TtmDevice, size: u64) {
    (*man).bdev = bdev;
    (*man).size = size;
    (*man).usage = 0;

    for i in 0..TTM_MAX_BO_PRIORITY {
        INIT_LIST_HEAD(addr_of_mut!((*man).lru[i]));
    }
    spin_lock_init(&mut (*man).eviction_lock);
    for i in 0..TTM_NUM_MOVE_FENCES {
        (*man).eviction_fences[i] = null_mut();
    }
}

/// Linux `ttm_resource_manager_set_used()` (static inline in ttm_resource.h).
pub unsafe fn ttm_resource_manager_set_used(man: *mut TtmResourceManager, used: bool) {
    for i in 0..TTM_MAX_BO_PRIORITY {
        WARN_ON!(!list_empty(&(*man).lru[i]));
    }
    (*man).use_type = used;
}

/// Linux `ttm_manager_type()` (static inline in ttm_device.h).
pub unsafe fn ttm_manager_type(bdev: *mut TtmDevice, mem_type: usize) -> *mut TtmResourceManager {
    (*bdev).man_drv[mem_type]
}

/// Linux `ttm_set_driver_manager()` (static inline in ttm_device.h).
pub unsafe fn ttm_set_driver_manager(bdev: *mut TtmDevice, mem_type: usize, manager: *mut TtmResourceManager) {
    (*bdev).man_drv[mem_type] = manager;
}

/// Linux `ttm_sys_man_init()`.
pub unsafe fn ttm_sys_man_init(bdev: *mut TtmDevice) {
    let man = addr_of_mut!((*bdev).sysman);

    // System memory is the one type every device has; other types are set up
    // by the driver.
    (*man).use_tt = true;
    (*man).func = &TTM_SYS_MANAGER_FUNC;

    ttm_resource_manager_init(man, bdev, 0);
    ttm_set_driver_manager(bdev, TTM_PL_SYSTEM, man);
    ttm_resource_manager_set_used(man, true);
}

/// Linux `ttm_device_init()`.
pub unsafe fn ttm_device_init(
    bdev: *mut TtmDevice,
    funcs: *const TtmDeviceFuncs,
    dev: *mut c_void,
    mapping: *mut c_void,
    vma_manager: *mut c_void,
    alloc_flags: u32,
) -> c_int {
    if WARN_ON!(vma_manager.is_null()) {
        return -EINVAL;
    }

    let ret = ttm_global_init();
    if ret != 0 {
        return ret;
    }

    let wq = alloc_workqueue(
        c"ttm".as_ptr(),
        WQ_MEM_RECLAIM | WQ_HIGHPRI | WQ_UNBOUND,
        TTM_DEVICE_WQ_MAX_ACTIVE,
    );
    if wq.is_null() {
        ttm_global_release();
        return -ENOMEM;
    }
    (*bdev).wq = wq;

    (*bdev).alloc_flags = alloc_flags;
    (*bdev).funcs = funcs;

    ttm_sys_man_init(bdev);

    let nid = if dev.is_null() {
        NUMA_NO_NODE
    } else {
        dev_to_node(dev)
    };
    ttm_pool_init(addr_of_mut!((*bdev).pool), dev, nid, alloc_flags);

    (*bdev).vma_manager = vma_manager;
    spin_lock_init(&mut (*bdev).lru_lock);
    INIT_LIST_HEAD(addr_of_mut!((*bdev).unevictable));
    (*bdev).dev_mapping = mapping;

    let _global = TTM_GLOBAL_MUTEX.lock();
    list_add_tail(addr_of_mut!((*bdev).device_list), addr_of_mut!((*TTM_GLOBAL.get()).device_list));
    0
}

/// Linux `ttm_device_fini()`.
pub unsafe fn ttm_device_fini(bdev: *mut TtmDevice) {
    {
        let _global = TTM_GLOBAL_MUTEX.lock();
        list_del(addr_of_mut!((*bdev).device_list));
    }

    drain_workqueue((*bdev).wq);
    destroy_workqueue((*bdev).wq);

    let man = ttm_manager_type(bdev, TTM_PL_SYSTEM);
    ttm_resource_manager_set_used(man, false);
    ttm_set_driver_manager(bdev, TTM_PL_SYSTEM, null_mut());

    // Linux walks man->lru[] under lru_lock and only feeds pr_debug(), which is
    // compiled out in this configuration; the walk has no other effect.

    ttm_pool_fini(addr_of_mut!((*bdev).pool));
    ttm_global_release();
}
