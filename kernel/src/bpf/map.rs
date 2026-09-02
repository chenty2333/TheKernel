//! BPF map implementations (ArrayMap, HashMap).

use alloc::{borrow::ToOwned, collections::VecDeque, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axtask::current;

use super::defs::*;
use crate::task::AsThread;

/// Object-local freeze publication plus non-exclusive writer accounting.
/// The wrapper deliberately has the small AtomicBool surface used by map
/// implementations, while admission/teardown remains centralized here.
const FROZEN: u64 = 1 << 63;
const ACTIVE_MASK: u64 = !FROZEN;
pub struct AtomicBool {
    state: AtomicU64,
    freeze_lock: spin::Mutex<()>,
}
impl AtomicBool {
    const fn new(value: bool) -> Self {
        Self {
            state: AtomicU64::new(if value { FROZEN } else { 0 }),
            freeze_lock: spin::Mutex::new(()),
        }
    }
    fn load(&self, order: Ordering) -> bool {
        self.state.load(order) & FROZEN != 0
    }
    fn store(&self, value: bool, order: Ordering) {
        if value {
            self.state.fetch_or(FROZEN, order);
        } else {
            self.state.fetch_and(ACTIVE_MASK, order);
        }
    }
}
pub(crate) struct MapWriteActive<'a> {
    state: &'a AtomicBool,
}
impl Drop for MapWriteActive<'_> {
    fn drop(&mut self) {
        self.state.state.fetch_sub(1, Ordering::Release);
    }
}
pub(crate) fn map_write_active(map: &dyn BpfMap) -> AxResult<MapWriteActive<'_>> {
    let state = map.freeze_state();
    let mut current = state.state.load(Ordering::Acquire);
    loop {
        if current & FROZEN != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        if current & ACTIVE_MASK == ACTIVE_MASK {
            return Err(AxError::ResourceBusy);
        }
        match state.state.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(MapWriteActive { state }),
            Err(observed) => current = observed,
        }
    }
}
pub(crate) fn map_freeze_active(map: &dyn BpfMap) -> AxResult<()> {
    let state = map.freeze_state();
    let _serialize = state.freeze_lock.lock();
    match state
        .state
        .compare_exchange(0, FROZEN, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => Ok(()),
        Err(value) if value & FROZEN != 0 => Err(AxError::ResourceBusy),
        Err(_) => Err(AxError::ResourceBusy),
    }
}

/// Trait for all BPF map types.
pub trait BpfMap: Send + Sync {
    fn map_type(&self) -> u32;
    fn key_size(&self) -> u32;
    fn value_size(&self) -> u32;
    /// User-space element operations on per-CPU maps transfer one aligned
    /// value per possible CPU; the verifier and helper ABI keep `value_size`.
    fn user_value_size(&self) -> usize {
        self.value_size() as usize
    }
    fn max_entries(&self) -> u32;
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN];
    fn id(&self) -> u32;
    fn map_flags(&self) -> u32;
    fn freeze_state(&self) -> &AtomicBool;
    fn freeze(&self) -> AxResult<()>;

    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>>;
    /// Atomically obtain and remove one element where the map type requires
    /// it (queue/stack). Other maps retain their established semantics.
    fn lookup_and_delete(&self, key: &[u8]) -> AxResult<Vec<u8>> {
        let value = self.lookup_user(key).ok_or(AxError::NotFound)?;
        self.delete(key)?;
        Ok(value)
    }
    fn lookup_user(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.lookup(key)
    }
    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()>;
    fn update_user(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        self.update(key, value, flags)
    }
    fn delete(&self, key: &[u8]) -> AxResult<()>;
    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>>;

    /// Hash-family batch page.  The cursor is a stable bucket number, never
    /// a key or an insertion-order position.  Providers snapshot a complete
    /// bucket (or delete it) before returning so callers can enforce Linux's
    /// whole-bucket ENOSPC and copyout ordering.
    fn hash_batch_page(
        &self,
        _bucket: u32,
        _capacity: usize,
        _delete: bool,
    ) -> AxResult<HashBatchPage> {
        Err(AxError::OperationNotSupported)
    }
    fn hash_batch_cursor_valid(&self, _bucket: u32) -> bool {
        false
    }
    /// Return the retained program for a tail-call array slot. Numeric FDs
    /// never cross this boundary, so close/reuse cannot retarget execution.
    fn tail_call_program(&self, _index: u32) -> Option<Arc<crate::bpf::prog::BpfProgram>> {
        None
    }

    /// Resolve an execution-only SOCKMAP/SOCKHASH entry to the retained open
    /// file description.  Numeric FDs never escape this boundary, so a
    /// close/reuse race cannot retarget a later redirect operation.
    fn socket_redirect_target(&self, _key: &[u8]) -> Option<Arc<crate::file::Socket>> {
        None
    }

    /// XSKMAP is likewise an execution-only object map.  A target retains the
    /// AF_XDP endpoint itself rather than the integer fd used to install it.
    fn xsk_redirect_target(&self, _index: u32) -> Option<Arc<crate::file::af_xdp::XdpEndpoint>> {
        None
    }

    fn associate_struct_ops(&self, _program: Arc<crate::bpf::prog::BpfProgram>) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    /// Runs an installed struct_ops callback table against a kernel-owned
    /// hook context.  The option distinguishes a non-struct_ops map from an
    /// installed table whose callback returned zero.
    fn run_struct_ops(&self, _context: &mut [u8]) -> AxResult<Option<u64>> {
        Ok(None)
    }

    fn ringbuf_reserve(&self, _size: usize, _flags: u64) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    fn ringbuf_submit(&self, _data: Vec<u8>, _flags: u64) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    fn ringbuf_discard(&self, _size: usize, _flags: u64) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    fn ringbuf_output(&self, _data: &[u8], _flags: u64) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    /// Resolves a perf-event-array slot and copies a payload into the target
    /// event's preallocated data ring.  Only that map type overrides it.
    fn perf_event_output(&self, _index: u32, _data: &[u8]) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    /// Snapshot used by `bpf_perf_event_read_value()`: counter, enabled and
    /// running time.  No other map type exposes a perf descriptor.
    fn perf_event_read_value(&self, _index: u32) -> AxResult<(u64, u64, u64)> {
        Err(AxError::InvalidInput)
    }
}

pub struct HashBatchPage {
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
    pub next_bucket: u32,
    pub exhausted: bool,
}

fn batch_owned(bytes: &[u8]) -> AxResult<Vec<u8>> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

fn batch_bucket(key: &[u8], bucket_count: u32) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in key {
        hash = (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193);
    }
    hash & (bucket_count - 1)
}

/// Typed terminal operation for an XDP `redirect_map` decision. Router code
/// calls this before protocol parsing; neither a raw FD nor AF_PACKET's copy
/// broker participates in the handoff.
pub(crate) fn redirect_xsk(
    map: &Arc<dyn BpfMap>,
    index: u32,
    address: u64,
    length: u32,
    options: u32,
) -> AxResult<bool> {
    map.xsk_redirect_target(index)
        .ok_or(AxError::NotFound)?
        .redirect_rx(address, length, options)
}

/// Create a map of the given type.
pub fn create_map(
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    flags: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
) -> AxResult<Arc<dyn BpfMap>> {
    if flags != 0 && !(map_type == BPF_MAP_TYPE_LPM_TRIE && flags == BPF_F_NO_PREALLOC) {
        return Err(AxError::InvalidInput);
    }

    match map_type {
        BPF_MAP_TYPE_ARRAY => Ok(Arc::new(ArrayMap::new(
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_DEVMAP | BPF_MAP_TYPE_CPUMAP => Ok(Arc::new(ArrayMap::new_kind(
            map_type,
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_XSKMAP => Ok(Arc::new(XskMap::new(
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_PROG_ARRAY => Ok(Arc::new(ProgArrayMap::new(
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_PERCPU_HASH => Ok(Arc::new(PerCpuMap::new(
            BPF_MAP_TYPE_PERCPU_HASH,
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_PERCPU_ARRAY => Ok(Arc::new(PerCpuMap::new(
            BPF_MAP_TYPE_PERCPU_ARRAY,
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_HASH => Ok(Arc::new(BpfHashMap::new(
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_LRU_HASH => Ok(Arc::new(BpfHashMap::new_lru(
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_LPM_TRIE => Ok(Arc::new(LpmTrieMap::new(
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_QUEUE | BPF_MAP_TYPE_STACK => Ok(Arc::new(QueueStackMap::new(
            map_type,
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_SOCKMAP | BPF_MAP_TYPE_SOCKHASH => Ok(Arc::new(SocketMap::new(
            map_type,
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_RINGBUF => Ok(Arc::new(RingBufMap::new(
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_PERF_EVENT_ARRAY => Ok(Arc::new(PerfEventArrayMap::new(
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        BPF_MAP_TYPE_STRUCT_OPS => Ok(Arc::new(StructOpsMap::new(
            key_size,
            value_size,
            max_entries,
            flags,
            name,
            id,
        )?)),
        _ => Err(AxError::InvalidInput),
    }
}

/// A struct_ops map is an opaque one-element object.  Its value is installed
/// only by the struct_ops association command; ordinary element syscalls must
/// not expose partially registered callback tables.
pub struct StructOpsMap {
    value_size: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    program: spin::Mutex<Option<Arc<crate::bpf::prog::BpfProgram>>>,
}
impl StructOpsMap {
    fn new(
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if key_size != 0 || value_size == 0 || max_entries != 1 || flags != 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            value_size,
            name,
            id,
            frozen: AtomicBool::new(false),
            program: spin::Mutex::new(None),
        })
    }
}
impl BpfMap for StructOpsMap {
    fn map_type(&self) -> u32 {
        BPF_MAP_TYPE_STRUCT_OPS
    }
    fn key_size(&self) -> u32 {
        0
    }
    fn value_size(&self) -> u32 {
        self.value_size
    }
    fn max_entries(&self) -> u32 {
        1
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        0
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }
    fn lookup(&self, _key: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn update(&self, _key: &[u8], _value: &[u8], _flags: u64) -> AxResult<()> {
        Err(AxError::OperationNotPermitted)
    }
    fn delete(&self, _key: &[u8]) -> AxResult<()> {
        Err(AxError::OperationNotPermitted)
    }
    fn get_next_key(&self, _key: Option<&[u8]>) -> Option<Vec<u8>> {
        None
    }
    fn associate_struct_ops(&self, program: Arc<crate::bpf::prog::BpfProgram>) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        let mut installed = self.program.lock();
        if installed.is_some() {
            return Err(AxError::AlreadyExists);
        }
        *installed = Some(program);
        Ok(())
    }
    fn run_struct_ops(&self, context: &mut [u8]) -> AxResult<Option<u64>> {
        let program = self.program.lock().clone();
        let Some(program) = program else {
            return Ok(None);
        };
        let stats = crate::bpf::prog::BpfStatsRunGuard::begin();
        let result = crate::bpf::helpers::BpfExecution::new(context, &program.maps, 4096)
            .with_streams(&program.streams)
            .execute(&program.mechanism);
        program.account_run(&stats);
        result.map(|(result, _)| Some(result))
    }
}

// ---------------------------------------------------------------------------
// PerfEventArrayMap
// ---------------------------------------------------------------------------

/// Linux stores a retained perf-event reference in each map slot, not a bare
/// FD number.  Replacing a slot first acquires the new typed object, then
/// swaps it under the map lock, so close/reuse of the source FD cannot change
/// the target selected by an already-loaded BPF program.
pub struct PerfEventArrayMap {
    max_entries: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    slots: spin::Mutex<Vec<Option<PerfEventMapSlot>>>,
}

/// A PERF_EVENT_ARRAY value owns an event reference independently of the
/// numeric descriptor used to install it.  Keeping only the event `Arc` would
/// leave `FileDescription::pre_close` free to quiesce hardware when that FD is
/// closed, so the slot also participates in the event's explicit external
/// reference lifetime.
struct PerfEventMapSlot {
    fd: u32,
    event: Arc<crate::file::PerfEventFile>,
}

impl PerfEventMapSlot {
    fn new(fd: u32, event: Arc<crate::file::PerfEventFile>) -> Self {
        event.retain_perf_map_ref();
        Self { fd, event }
    }
}

impl Drop for PerfEventMapSlot {
    fn drop(&mut self) {
        self.event.release_perf_map_ref();
    }
}

impl PerfEventArrayMap {
    fn new(
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if key_size != 4 || value_size != 4 || max_entries == 0 || flags != 0 {
            return Err(AxError::InvalidInput);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(max_entries as usize)
            .map_err(|_| AxError::NoMemory)?;
        slots.resize_with(max_entries as usize, || None);
        Ok(Self {
            max_entries,
            name,
            id,
            frozen: AtomicBool::new(false),
            slots: spin::Mutex::new(slots),
        })
    }

    fn index(key: &[u8]) -> Option<usize> {
        let array: [u8; 4] = key.try_into().ok()?;
        usize::try_from(u32::from_ne_bytes(array)).ok()
    }
}

impl BpfMap for PerfEventArrayMap {
    fn map_type(&self) -> u32 {
        BPF_MAP_TYPE_PERF_EVENT_ARRAY
    }
    fn key_size(&self) -> u32 {
        4
    }
    fn value_size(&self) -> u32 {
        4
    }
    fn max_entries(&self) -> u32 {
        self.max_entries
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        0
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }
    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        let index = Self::index(key)?;
        self.slots
            .lock()
            .get(index)?
            .as_ref()
            .map(|slot| slot.fd.to_ne_bytes().to_vec())
    }
    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if flags != BPF_ANY || value.len() != 4 {
            return Err(AxError::InvalidInput);
        }
        let index = Self::index(key)
            .filter(|&index| index < self.max_entries as usize)
            .ok_or(AxError::InvalidInput)?;
        let fd = i32::from_ne_bytes(value.try_into().map_err(|_| AxError::InvalidInput)?);
        let handle = crate::file::get_typed_file::<crate::file::PerfEventFile>(fd)?;
        let event = handle.clone_object();
        // Constructing the new slot retains its external event reference
        // before assignment drops a possible old value.
        self.slots.lock()[index] = Some(PerfEventMapSlot::new(fd as u32, event));
        Ok(())
    }
    fn delete(&self, key: &[u8]) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        let index = Self::index(key)
            .filter(|&index| index < self.max_entries as usize)
            .ok_or(AxError::InvalidInput)?;
        let mut slots = self.slots.lock();
        if slots[index].take().is_some() {
            Ok(())
        } else {
            Err(AxError::NotFound)
        }
    }
    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>> {
        let start = key
            .and_then(Self::index)
            .map_or(0, |index| index.saturating_add(1));
        (start < self.max_entries as usize).then(|| (start as u32).to_ne_bytes().to_vec())
    }
    fn perf_event_output(&self, index: u32, data: &[u8]) -> AxResult<()> {
        let event = self
            .slots
            .lock()
            .get(index as usize)
            .and_then(Option::as_ref)
            .map(|slot| slot.event.clone())
            .ok_or(AxError::NotFound)?;
        event.emit_bpf_output(data)
    }
    fn perf_event_read_value(&self, index: u32) -> AxResult<(u64, u64, u64)> {
        let event = self
            .slots
            .lock()
            .get(index as usize)
            .and_then(Option::as_ref)
            .map(|slot| slot.event.clone())
            .ok_or(AxError::NotFound)?;
        let sample = event.bpf_read_value();
        Ok((sample.0, sample.1, sample.2))
    }
}

// ---------------------------------------------------------------------------
// ProgArrayMap
// ---------------------------------------------------------------------------

/// Program arrays retain a program object, rather than the numeric FD passed
/// to `BPF_MAP_UPDATE_ELEM`.  This is essential for tail-call safety: closing
/// and reusing the source descriptor must neither detach nor retarget a slot.
pub struct ProgArrayMap {
    max_entries: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    slots: spin::Mutex<Vec<Option<Arc<crate::bpf::prog::BpfProgram>>>>,
}

impl ProgArrayMap {
    fn new(
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if key_size != 4 || value_size != 4 || max_entries == 0 || flags != 0 {
            return Err(AxError::InvalidInput);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(max_entries as usize)
            .map_err(|_| AxError::NoMemory)?;
        slots.resize_with(max_entries as usize, || None);
        Ok(Self {
            max_entries,
            name,
            id,
            frozen: AtomicBool::new(false),
            slots: spin::Mutex::new(slots),
        })
    }

    fn index(key: &[u8]) -> Option<usize> {
        let raw: [u8; 4] = key.try_into().ok()?;
        usize::try_from(u32::from_ne_bytes(raw)).ok()
    }
}

impl BpfMap for ProgArrayMap {
    fn map_type(&self) -> u32 {
        BPF_MAP_TYPE_PROG_ARRAY
    }
    fn key_size(&self) -> u32 {
        4
    }
    fn value_size(&self) -> u32 {
        4
    }
    fn max_entries(&self) -> u32 {
        self.max_entries
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        0
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }

    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        let index = Self::index(key)?;
        self.slots
            .lock()
            .get(index)?
            .as_ref()
            .map(|program| program.prog_id.to_ne_bytes().to_vec())
    }

    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if flags == BPF_NOEXIST {
            return Err(AxError::AlreadyExists);
        }
        if !matches!(flags, BPF_ANY | BPF_EXIST) || value.len() != 4 {
            return Err(AxError::InvalidInput);
        }
        // The map value is a BPF program FD at the ABI boundary.  Resolve it
        // before acquiring the slot lock, then retain the object itself.
        let fd = i32::from_ne_bytes(value.try_into().map_err(|_| AxError::InvalidInput)?);
        let program = crate::file::get_typed_file::<crate::file::bpf::BpfProgFd>(fd)?
            .prog
            .clone();
        let index = Self::index(key)
            .filter(|index| *index < self.max_entries as usize)
            .ok_or(AxError::InvalidInput)?;
        self.slots.lock()[index] = Some(program);
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        let index = Self::index(key)
            .filter(|index| *index < self.max_entries as usize)
            .ok_or(AxError::InvalidInput)?;
        if self.slots.lock()[index].take().is_some() {
            Ok(())
        } else {
            Err(AxError::NotFound)
        }
    }

    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>> {
        let start = key
            .and_then(Self::index)
            .map_or(0, |index| index.saturating_add(1));
        (start < self.max_entries as usize).then(|| (start as u32).to_ne_bytes().to_vec())
    }
    fn tail_call_program(&self, index: u32) -> Option<Arc<crate::bpf::prog::BpfProgram>> {
        self.slots.lock().get(index as usize)?.clone()
    }
}

// ---------------------------------------------------------------------------
// PerCpuMap
// ---------------------------------------------------------------------------

/// Per-CPU maps keep a packed, `round_up(value_size, 8)` user ABI image per
/// possible CPU while helpers observe only the current CPU's logical value.
/// Keeping those views explicit prevents a BPF load from seeing a forged
/// aggregate value size and makes syscall copies independent of CPU migration.
pub struct PerCpuMap {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    stride: usize,
    cpu_count: usize,
    max_entries: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    entries: spin::Mutex<Vec<(Vec<u8>, Vec<u8>)>>,
}

impl PerCpuMap {
    fn new(
        map_type: u32,
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if !matches!(
            map_type,
            BPF_MAP_TYPE_PERCPU_HASH | BPF_MAP_TYPE_PERCPU_ARRAY
        ) || value_size == 0
            || max_entries == 0
            || flags != 0
            || (map_type == BPF_MAP_TYPE_PERCPU_ARRAY && key_size != 4)
            || (map_type == BPF_MAP_TYPE_PERCPU_HASH && key_size == 0)
        {
            return Err(AxError::InvalidInput);
        }
        let stride = (value_size as usize)
            .checked_add(7)
            .ok_or(AxError::NoMemory)?
            & !7;
        let cpu_count = axhal::cpu_num().max(1);
        stride.checked_mul(cpu_count).ok_or(AxError::NoMemory)?;
        let mut entries = Vec::new();
        entries
            .try_reserve(if map_type == BPF_MAP_TYPE_PERCPU_ARRAY {
                max_entries as usize
            } else {
                0
            })
            .map_err(|_| AxError::NoMemory)?;
        if map_type == BPF_MAP_TYPE_PERCPU_ARRAY {
            let packed_len = stride.checked_mul(cpu_count).ok_or(AxError::NoMemory)?;
            for index in 0..max_entries {
                let mut packed = Vec::new();
                packed
                    .try_reserve_exact(packed_len)
                    .map_err(|_| AxError::NoMemory)?;
                packed.resize(packed_len, 0);
                entries.push((index.to_ne_bytes().to_vec(), packed));
            }
        }
        Ok(Self {
            map_type,
            key_size,
            value_size,
            stride,
            cpu_count,
            max_entries,
            name,
            id,
            frozen: AtomicBool::new(false),
            entries: spin::Mutex::new(entries),
        })
    }
    fn key_valid(&self, key: &[u8]) -> bool {
        if key.len() != self.key_size as usize {
            return false;
        }
        if self.map_type != BPF_MAP_TYPE_PERCPU_ARRAY {
            return true;
        }
        let Ok(raw) = <[u8; 4]>::try_from(key) else {
            return false;
        };
        u32::from_ne_bytes(raw) < self.max_entries
    }
    fn packed_size(&self) -> usize {
        self.stride * self.cpu_count
    }
    fn cpu_offset(&self) -> usize {
        axhal::percpu::this_cpu_id().min(self.cpu_count - 1) * self.stride
    }
    fn find(entries: &[(Vec<u8>, Vec<u8>)], key: &[u8]) -> Option<usize> {
        entries
            .iter()
            .position(|(candidate, _)| candidate.as_slice() == key)
    }
}

impl BpfMap for PerCpuMap {
    fn map_type(&self) -> u32 {
        self.map_type
    }
    fn key_size(&self) -> u32 {
        self.key_size
    }
    fn value_size(&self) -> u32 {
        self.value_size
    }
    fn user_value_size(&self) -> usize {
        self.packed_size()
    }
    fn max_entries(&self) -> u32 {
        self.max_entries
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        0
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }
    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        if !self.key_valid(key) {
            return None;
        }
        let entries = self.entries.lock();
        let entry = entries.get(Self::find(&entries, key)?)?;
        let offset = self.cpu_offset();
        Some(entry.1[offset..offset + self.value_size as usize].to_vec())
    }
    fn lookup_user(&self, key: &[u8]) -> Option<Vec<u8>> {
        if !self.key_valid(key) {
            return None;
        }
        let entries = self.entries.lock();
        entries
            .get(Self::find(&entries, key)?)
            .map(|entry| entry.1.clone())
    }
    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if value.len() != self.value_size as usize {
            return Err(AxError::InvalidInput);
        }
        let mut packed = self
            .lookup_user(key)
            .unwrap_or_else(|| vec![0; self.packed_size()]);
        let offset = self.cpu_offset();
        packed[offset..offset + value.len()].copy_from_slice(value);
        self.update_user(key, &packed, flags)
    }
    fn update_user(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire)
            || !self.key_valid(key)
            || value.len() != self.packed_size()
        {
            return Err(AxError::InvalidInput);
        }
        if !matches!(flags, BPF_ANY | BPF_NOEXIST | BPF_EXIST) {
            return Err(AxError::InvalidInput);
        }
        let mut entries = self.entries.lock();
        let existing = Self::find(&entries, key);
        if self.map_type == BPF_MAP_TYPE_PERCPU_ARRAY {
            if flags == BPF_NOEXIST {
                return Err(AxError::AlreadyExists);
            }
            entries[existing.ok_or(AxError::InvalidInput)?]
                .1
                .copy_from_slice(value);
            return Ok(());
        }
        if flags == BPF_NOEXIST && existing.is_some() {
            return Err(AxError::AlreadyExists);
        }
        if flags == BPF_EXIST && existing.is_none() {
            return Err(AxError::NotFound);
        }
        if let Some(index) = existing {
            entries[index].1.copy_from_slice(value);
            return Ok(());
        }
        if entries.len() == self.max_entries as usize {
            return Err(AxError::StorageFull);
        }
        let mut owned_key = Vec::new();
        owned_key
            .try_reserve_exact(key.len())
            .map_err(|_| AxError::NoMemory)?;
        owned_key.extend_from_slice(key);
        let mut owned_value = Vec::new();
        owned_value
            .try_reserve_exact(value.len())
            .map_err(|_| AxError::NoMemory)?;
        owned_value.extend_from_slice(value);
        entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        entries.push((owned_key, owned_value));
        Ok(())
    }
    fn delete(&self, key: &[u8]) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) || !self.key_valid(key) {
            return Err(AxError::InvalidInput);
        }
        if self.map_type == BPF_MAP_TYPE_PERCPU_ARRAY {
            return Err(AxError::InvalidInput);
        }
        let mut entries = self.entries.lock();
        let index = Self::find(&entries, key).ok_or(AxError::NotFound)?;
        entries.swap_remove(index);
        Ok(())
    }
    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>> {
        let entries = self.entries.lock();
        match key {
            None => entries.first().map(|(key, _)| key.clone()),
            Some(key) => Self::find(&entries, key)
                .and_then(|index| entries.get(index + 1).map(|(key, _)| key.clone()))
                .or_else(|| entries.first().map(|(key, _)| key.clone())),
        }
    }

    fn hash_batch_page(
        &self,
        bucket: u32,
        capacity: usize,
        delete: bool,
    ) -> AxResult<HashBatchPage> {
        if self.map_type != BPF_MAP_TYPE_PERCPU_HASH {
            return Err(AxError::OperationNotSupported);
        }
        if delete && self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        let bucket_count = self.max_entries.next_power_of_two().max(1);
        if bucket >= bucket_count {
            return Err(AxError::NotFound);
        }
        let mut entries = self.entries.lock();
        let count = entries
            .iter()
            .filter(|(key, _)| batch_bucket(key, bucket_count) == bucket)
            .count();
        if count > capacity {
            return Err(axerrno::LinuxError::ENOSPC.into());
        }
        let mut page = Vec::new();
        page.try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        for (key, value) in entries
            .iter()
            .filter(|(key, _)| batch_bucket(key, bucket_count) == bucket)
        {
            page.push((batch_owned(key)?, batch_owned(value)?));
        }
        if delete {
            entries.retain(|(key, _)| batch_bucket(key, bucket_count) != bucket);
        }
        let next_bucket = bucket + 1;
        Ok(HashBatchPage {
            entries: page,
            next_bucket,
            exhausted: next_bucket >= bucket_count,
        })
    }
    fn hash_batch_cursor_valid(&self, bucket: u32) -> bool {
        self.map_type == BPF_MAP_TYPE_PERCPU_HASH
            && bucket < self.max_entries.next_power_of_two().max(1)
    }
}

// ---------------------------------------------------------------------------
// LPM trie and queue/stack maps
// ---------------------------------------------------------------------------

pub struct LpmTrieMap {
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    entries: spin::Mutex<Vec<(Vec<u8>, Vec<u8>)>>,
}
impl LpmTrieMap {
    fn new(
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if key_size < 4 || value_size == 0 || max_entries == 0 || flags != BPF_F_NO_PREALLOC {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            key_size,
            value_size,
            max_entries,
            name,
            id,
            frozen: AtomicBool::new(false),
            entries: spin::Mutex::new(Vec::new()),
        })
    }
    fn prefix(key: &[u8]) -> Option<u32> {
        let raw: [u8; 4] = key.get(..4)?.try_into().ok()?;
        Some(u32::from_ne_bytes(raw))
    }
    fn valid(&self, key: &[u8]) -> bool {
        key.len() == self.key_size as usize
            && Self::prefix(key).is_some_and(|bits| bits <= (self.key_size - 4) * 8)
    }
    fn matches(candidate: &[u8], query: &[u8]) -> bool {
        let Some(bits) = Self::prefix(candidate) else {
            return false;
        };
        let Some(query_bits) = Self::prefix(query) else {
            return false;
        };
        if bits > query_bits {
            return false;
        }
        let bytes = (bits / 8) as usize;
        let tail = (bits % 8) as u8;
        candidate[4..4 + bytes] == query[4..4 + bytes]
            && (tail == 0
                || (candidate[4 + bytes] & (0xff << (8 - tail)))
                    == (query[4 + bytes] & (0xff << (8 - tail))))
    }
}
impl BpfMap for LpmTrieMap {
    fn map_type(&self) -> u32 {
        BPF_MAP_TYPE_LPM_TRIE
    }
    fn key_size(&self) -> u32 {
        self.key_size
    }
    fn value_size(&self) -> u32 {
        self.value_size
    }
    fn max_entries(&self) -> u32 {
        self.max_entries
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        BPF_F_NO_PREALLOC
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }
    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        if !self.valid(key) {
            return None;
        }
        let entries = self.entries.lock();
        entries
            .iter()
            .filter(|(candidate, _)| Self::matches(candidate, key))
            .max_by_key(|(candidate, _)| Self::prefix(candidate).unwrap_or(0))
            .map(|(_, value)| value.clone())
    }
    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if !self.valid(key)
            || value.len() != self.value_size as usize
            || !matches!(flags, BPF_ANY | BPF_NOEXIST | BPF_EXIST)
        {
            return Err(AxError::InvalidInput);
        }
        let mut entries = self.entries.lock();
        let present = entries
            .iter()
            .position(|(candidate, _)| candidate.as_slice() == key);
        if flags == BPF_NOEXIST && present.is_some() {
            return Err(AxError::AlreadyExists);
        }
        if flags == BPF_EXIST && present.is_none() {
            return Err(AxError::NotFound);
        }
        if let Some(index) = present {
            entries[index].1.copy_from_slice(value);
            return Ok(());
        }
        if entries.len() == self.max_entries as usize {
            return Err(AxError::StorageFull);
        }
        let mut owned_key = Vec::new();
        owned_key
            .try_reserve_exact(key.len())
            .map_err(|_| AxError::NoMemory)?;
        owned_key.extend_from_slice(key);
        let mut owned_value = Vec::new();
        owned_value
            .try_reserve_exact(value.len())
            .map_err(|_| AxError::NoMemory)?;
        owned_value.extend_from_slice(value);
        entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        entries.push((owned_key, owned_value));
        Ok(())
    }
    fn delete(&self, key: &[u8]) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if !self.valid(key) {
            return Err(AxError::InvalidInput);
        }
        let mut entries = self.entries.lock();
        let index = entries
            .iter()
            .position(|(candidate, _)| candidate.as_slice() == key)
            .ok_or(AxError::NotFound)?;
        entries.swap_remove(index);
        Ok(())
    }
    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>> {
        let entries = self.entries.lock();
        match key {
            None => entries.first().map(|(key, _)| key.clone()),
            Some(key) => entries
                .iter()
                .position(|(candidate, _)| candidate.as_slice() == key)
                .and_then(|index| entries.get(index + 1).map(|(key, _)| key.clone()))
                .or_else(|| entries.first().map(|(key, _)| key.clone())),
        }
    }
}

pub struct QueueStackMap {
    map_type: u32,
    value_size: u32,
    max_entries: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    values: spin::Mutex<VecDeque<Vec<u8>>>,
}
impl QueueStackMap {
    fn new(
        map_type: u32,
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if !matches!(map_type, BPF_MAP_TYPE_QUEUE | BPF_MAP_TYPE_STACK)
            || key_size != 0
            || value_size == 0
            || max_entries == 0
            || flags != 0
        {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            map_type,
            value_size,
            max_entries,
            name,
            id,
            frozen: AtomicBool::new(false),
            values: spin::Mutex::new(VecDeque::new()),
        })
    }
}
impl BpfMap for QueueStackMap {
    fn map_type(&self) -> u32 {
        self.map_type
    }
    fn key_size(&self) -> u32 {
        0
    }
    fn value_size(&self) -> u32 {
        self.value_size
    }
    fn max_entries(&self) -> u32 {
        self.max_entries
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        0
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }
    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        if !key.is_empty() {
            return None;
        }
        let values = self.values.lock();
        if self.map_type == BPF_MAP_TYPE_STACK {
            values.back().cloned()
        } else {
            values.front().cloned()
        }
    }
    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if !key.is_empty() || value.len() != self.value_size as usize || flags != BPF_ANY {
            return Err(AxError::InvalidInput);
        }
        let mut values = self.values.lock();
        if values.len() == self.max_entries as usize {
            return Err(AxError::StorageFull);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|_| AxError::NoMemory)?;
        owned.extend_from_slice(value);
        values.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        values.push_back(owned);
        Ok(())
    }
    fn delete(&self, key: &[u8]) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if !key.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let popped = if self.map_type == BPF_MAP_TYPE_STACK {
            self.values.lock().pop_back()
        } else {
            self.values.lock().pop_front()
        };
        popped.map(|_| ()).ok_or(AxError::NotFound)
    }
    fn lookup_and_delete(&self, key: &[u8]) -> AxResult<Vec<u8>> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if !key.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let value = if self.map_type == BPF_MAP_TYPE_STACK {
            self.values.lock().pop_back()
        } else {
            self.values.lock().pop_front()
        };
        value.ok_or(AxError::NotFound)
    }
    fn get_next_key(&self, _: Option<&[u8]>) -> Option<Vec<u8>> {
        None
    }
}

// ---------------------------------------------------------------------------
// SOCKMAP / SOCKHASH
// ---------------------------------------------------------------------------

/// One map entry owns the socket object, not merely the descriptor used at
/// update time. This makes close/dup/fd reuse irrelevant to later BPF lookup
/// or redirect resolution.
struct SocketMapEntry {
    fd: u32,
    socket: Arc<crate::file::Socket>,
}
pub struct SocketMap {
    map_type: u32,
    key_size: u32,
    max_entries: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    entries: spin::Mutex<Vec<(Vec<u8>, SocketMapEntry)>>,
}
impl SocketMap {
    fn new(
        map_type: u32,
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if !matches!(map_type, BPF_MAP_TYPE_SOCKMAP | BPF_MAP_TYPE_SOCKHASH)
            || value_size != 4
            || max_entries == 0
            || flags != 0
            || (map_type == BPF_MAP_TYPE_SOCKMAP && key_size != 4)
            || (map_type == BPF_MAP_TYPE_SOCKHASH && key_size == 0)
        {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            map_type,
            key_size,
            max_entries,
            name,
            id,
            frozen: AtomicBool::new(false),
            entries: spin::Mutex::new(Vec::new()),
        })
    }
    fn valid(&self, key: &[u8]) -> bool {
        key.len() == self.key_size as usize
    }
}
impl BpfMap for SocketMap {
    fn map_type(&self) -> u32 {
        self.map_type
    }
    fn key_size(&self) -> u32 {
        self.key_size
    }
    fn value_size(&self) -> u32 {
        4
    }
    fn max_entries(&self) -> u32 {
        self.max_entries
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        0
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }
    // Socket maps are execution-only object maps: Linux does not expose an
    // installed socket as the stale numeric FD used to populate the slot.
    // Keep descriptor lookup out of this generic byte-map path.
    fn lookup(&self, _key: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if !self.valid(key)
            || value.len() != 4
            || !matches!(flags, BPF_ANY | BPF_NOEXIST | BPF_EXIST)
        {
            return Err(AxError::InvalidInput);
        }
        let fd = i32::from_ne_bytes(value.try_into().map_err(|_| AxError::InvalidInput)?);
        let file = crate::file::get_file_like(fd)?;
        let _ = crate::file::Socket::from_file_handle(&file)?;
        let socket = crate::file::get_typed_file::<crate::file::Socket>(fd)?.clone_object();
        let mut entries = self.entries.lock();
        let present = entries
            .iter()
            .position(|(candidate, _)| candidate.as_slice() == key);
        if flags == BPF_NOEXIST && present.is_some() {
            return Err(AxError::AlreadyExists);
        }
        if flags == BPF_EXIST && present.is_none() {
            return Err(AxError::NotFound);
        }
        if let Some(index) = present {
            entries[index].1 = SocketMapEntry {
                fd: fd as u32,
                socket,
            };
            return Ok(());
        }
        if entries.len() == self.max_entries as usize {
            return Err(AxError::StorageFull);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(key.len())
            .map_err(|_| AxError::NoMemory)?;
        owned.extend_from_slice(key);
        entries.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        entries.push((
            owned,
            SocketMapEntry {
                fd: fd as u32,
                socket,
            },
        ));
        Ok(())
    }
    fn delete(&self, key: &[u8]) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if !self.valid(key) {
            return Err(AxError::InvalidInput);
        }
        let mut entries = self.entries.lock();
        let index = entries
            .iter()
            .position(|(candidate, _)| candidate.as_slice() == key)
            .ok_or(AxError::NotFound)?;
        entries.swap_remove(index);
        Ok(())
    }
    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>> {
        let entries = self.entries.lock();
        match key {
            None => entries.first().map(|(key, _)| key.clone()),
            Some(key) => entries
                .iter()
                .position(|(candidate, _)| candidate.as_slice() == key)
                .and_then(|index| entries.get(index + 1).map(|(key, _)| key.clone()))
                .or_else(|| entries.first().map(|(key, _)| key.clone())),
        }
    }
    fn socket_redirect_target(&self, key: &[u8]) -> Option<Arc<crate::file::Socket>> {
        if !self.valid(key) {
            return None;
        }
        self.entries
            .lock()
            .iter()
            .find(|(candidate, _)| candidate.as_slice() == key)
            .map(|(_, entry)| entry.socket.clone())
    }
}

// ---------------------------------------------------------------------------
// ArrayMap
// ---------------------------------------------------------------------------

/// AF_XDP redirect slots own typed endpoints.  Keeping an `Arc<XdpEndpoint>`
/// makes descriptor close/reuse and fd-table sharing irrelevant once a BPF
/// program executes `redirect_map`.
pub struct XskMap {
    max_entries: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    slots: spin::Mutex<Vec<Option<Arc<crate::file::af_xdp::XdpEndpoint>>>>,
}
impl XskMap {
    fn new(
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if key_size != 4 || value_size != 4 || max_entries == 0 || flags != 0 {
            return Err(AxError::InvalidInput);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(max_entries as usize)
            .map_err(|_| AxError::NoMemory)?;
        slots.resize_with(max_entries as usize, || None);
        Ok(Self {
            max_entries,
            name,
            id,
            frozen: AtomicBool::new(false),
            slots: spin::Mutex::new(slots),
        })
    }
    fn index(key: &[u8]) -> Option<usize> {
        usize::try_from(u32::from_ne_bytes(key.try_into().ok()?)).ok()
    }
}
impl BpfMap for XskMap {
    fn map_type(&self) -> u32 {
        BPF_MAP_TYPE_XSKMAP
    }
    fn key_size(&self) -> u32 {
        4
    }
    fn value_size(&self) -> u32 {
        4
    }
    fn max_entries(&self) -> u32 {
        self.max_entries
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        0
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }
    fn lookup(&self, _key: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn update(&self, _key: &[u8], _value: &[u8], _flags: u64) -> AxResult<()> {
        Err(AxError::OperationNotPermitted)
    }
    fn update_user(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire)
            || !matches!(flags, BPF_ANY | BPF_NOEXIST | BPF_EXIST)
            || value.len() != 4
        {
            return Err(AxError::InvalidInput);
        }
        let index = Self::index(key)
            .filter(|index| *index < self.max_entries as usize)
            .ok_or(AxError::InvalidInput)?;
        let fd = i32::from_ne_bytes(value.try_into().map_err(|_| AxError::InvalidInput)?);
        let target =
            crate::file::get_typed_file::<crate::file::af_xdp::XdpSocket>(fd)?.clone_object();
        let endpoint = target.endpoint();
        if !Arc::ptr_eq(target.net_namespace(), &current().as_thread().net_ns())
            || !endpoint.is_bound_live()
        {
            return Err(AxError::InvalidInput);
        }
        let mut slots = self.slots.lock();
        let exists = slots[index].is_some();
        if flags == BPF_NOEXIST && exists {
            return Err(AxError::AlreadyExists);
        }
        if flags == BPF_EXIST && !exists {
            return Err(AxError::NotFound);
        }
        slots[index] = Some(endpoint);
        Ok(())
    }
    fn delete(&self, key: &[u8]) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        let index = Self::index(key)
            .filter(|index| *index < self.max_entries as usize)
            .ok_or(AxError::InvalidInput)?;
        self.slots.lock()[index]
            .take()
            .ok_or(AxError::NotFound)
            .map(|_| ())
    }
    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>> {
        let start = key
            .and_then(Self::index)
            .map_or(0, |index| index.saturating_add(1));
        (start < self.max_entries as usize).then(|| (start as u32).to_ne_bytes().to_vec())
    }
    fn xsk_redirect_target(&self, index: u32) -> Option<Arc<crate::file::af_xdp::XdpEndpoint>> {
        self.slots.lock().get(index as usize)?.clone()
    }
}

pub struct ArrayMap {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    /// Generic storage is owned by axbpf; this wrapper adds Linux object
    /// identity, freezing and syscall update-flag semantics.
    data: spin::Mutex<axbpf::ArrayMap>,
}

impl ArrayMap {
    fn new(
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        map_flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        Self::new_kind(
            BPF_MAP_TYPE_ARRAY,
            key_size,
            value_size,
            max_entries,
            map_flags,
            name,
            id,
        )
    }

    fn new_kind(
        map_type: u32,
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        map_flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if !matches!(
            map_type,
            BPF_MAP_TYPE_ARRAY | BPF_MAP_TYPE_DEVMAP | BPF_MAP_TYPE_CPUMAP | BPF_MAP_TYPE_XSKMAP
        ) {
            return Err(AxError::InvalidInput);
        }
        let required_value_size = match map_type {
            BPF_MAP_TYPE_ARRAY => None,
            BPF_MAP_TYPE_DEVMAP | BPF_MAP_TYPE_CPUMAP => Some(8),
            BPF_MAP_TYPE_XSKMAP => Some(4),
            _ => unreachable!(),
        };
        if key_size != 4
            || value_size == 0
            || max_entries == 0
            || required_value_size.is_some_and(|size| value_size != size)
        {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            map_type,
            key_size,
            value_size,
            max_entries,
            map_flags,
            name,
            id,
            frozen: AtomicBool::new(false),
            data: spin::Mutex::new(
                axbpf::ArrayMap::new(max_entries as usize, value_size as usize)
                    .ok_or(AxError::NoMemory)?,
            ),
        })
    }

    fn index_range(&self, index: u32) -> Option<core::ops::Range<usize>> {
        if index >= self.max_entries {
            return None;
        }
        let start = index as usize * self.value_size as usize;
        let end = start + self.value_size as usize;
        Some(start..end)
    }

    fn key_to_index(key: &[u8]) -> Option<u32> {
        if key.len() != 4 {
            return None;
        }
        Some(u32::from_ne_bytes([key[0], key[1], key[2], key[3]]))
    }

    fn update_storage(&self, key: &[u8], value: &[u8]) -> AxResult<()> {
        axbpf::Map::update(&mut *self.data.lock(), key, value).map_err(|_| AxError::InvalidInput)
    }
}

impl BpfMap for ArrayMap {
    fn map_type(&self) -> u32 {
        self.map_type
    }
    fn key_size(&self) -> u32 {
        self.key_size
    }
    fn value_size(&self) -> u32 {
        self.value_size
    }
    fn max_entries(&self) -> u32 {
        self.max_entries
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        self.map_flags
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }

    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        let _index = Self::key_to_index(key)?;
        let data = self.data.lock();
        axbpf::Map::lookup(&*data, key).map(ToOwned::to_owned)
    }

    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        match flags {
            BPF_ANY | BPF_EXIST => {}
            BPF_NOEXIST => return Err(AxError::AlreadyExists),
            _ => return Err(AxError::InvalidInput),
        }
        let index = Self::key_to_index(key).ok_or(AxError::InvalidInput)?;
        let _ = self.index_range(index).ok_or(AxError::InvalidInput)?;
        if value.len() != self.value_size as usize {
            return Err(AxError::InvalidInput);
        }
        // Redirect-target maps are populated through their syscall-owned
        // typed update path.  A BPF helper may not reinterpret raw values as
        // file descriptors or namespace identities from a kernel worker.
        if matches!(
            self.map_type,
            BPF_MAP_TYPE_DEVMAP | BPF_MAP_TYPE_CPUMAP | BPF_MAP_TYPE_XSKMAP
        ) {
            return Err(AxError::OperationNotPermitted);
        }
        self.update_storage(key, value)
    }

    fn update_user(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if !matches!(
            self.map_type,
            BPF_MAP_TYPE_DEVMAP | BPF_MAP_TYPE_CPUMAP | BPF_MAP_TYPE_XSKMAP
        ) {
            return self.update(key, value, flags);
        }
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if !matches!(flags, BPF_ANY | BPF_EXIST) {
            return Err(AxError::InvalidInput);
        }
        let _ = Self::key_to_index(key)
            .and_then(|index| self.index_range(index))
            .ok_or(AxError::InvalidInput)?;
        if value.len() != self.value_size as usize {
            return Err(AxError::InvalidInput);
        }
        match self.map_type {
            BPF_MAP_TYPE_DEVMAP => {
                let ifindex =
                    u32::from_ne_bytes(value[..4].try_into().map_err(|_| AxError::InvalidInput)?);
                if ifindex == 0
                    || !current()
                        .as_thread()
                        .net_ns()
                        .stack()
                        .interfaces()
                        .iter()
                        .any(|entry| entry.index == ifindex)
                {
                    return Err(AxError::NoSuchDevice);
                }
            }
            BPF_MAP_TYPE_CPUMAP => {
                let cpu =
                    u32::from_ne_bytes(value[..4].try_into().map_err(|_| AxError::InvalidInput)?);
                if cpu >= axhal::cpu_num().max(1) as u32 {
                    return Err(AxError::InvalidInput);
                }
            }
            // AF_XDP has no endpoint class in this kernel yet.  Rejecting
            // every FD is intentional: accepting a generic file as XSKMAP
            // would publish an untyped, cross-namespace redirect target.
            BPF_MAP_TYPE_XSKMAP => return Err(AxError::InvalidInput),
            _ => unreachable!(),
        }
        self.update_storage(key, value)
    }

    fn delete(&self, key: &[u8]) -> AxResult<()> {
        let _ = key;
        Err(AxError::InvalidInput)
    }

    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>> {
        let next = match key {
            None => 0u32,
            Some(k) => match Self::key_to_index(k) {
                Some(index) if index < self.max_entries => index.wrapping_add(1),
                _ => 0,
            },
        };
        if next < self.max_entries {
            Some(next.to_ne_bytes().to_vec())
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// HashMap
// ---------------------------------------------------------------------------

pub struct BpfHashMap {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    /// Generic storage is owned by axbpf; this wrapper adds Linux object
    /// identity, freezing and syscall update-flag semantics.
    data: spin::Mutex<axbpf::HashMap>,
}

impl BpfHashMap {
    fn new(
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        map_flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        Self::new_kind(
            BPF_MAP_TYPE_HASH,
            key_size,
            value_size,
            max_entries,
            map_flags,
            name,
            id,
        )
    }

    fn new_lru(
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        map_flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        Self::new_kind(
            BPF_MAP_TYPE_LRU_HASH,
            key_size,
            value_size,
            max_entries,
            map_flags,
            name,
            id,
        )
    }

    fn new_kind(
        map_type: u32,
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        map_flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if key_size == 0 || value_size == 0 || max_entries == 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            map_type,
            key_size,
            value_size,
            max_entries,
            map_flags,
            name,
            id,
            frozen: AtomicBool::new(false),
            data: spin::Mutex::new(
                axbpf::HashMap::new(key_size as usize, value_size as usize, max_entries as usize)
                    .map_err(|error| match error {
                    axbpf::MapError::NoMemory => AxError::NoMemory,
                    _ => AxError::InvalidInput,
                })?,
            ),
        })
    }
}

impl BpfMap for BpfHashMap {
    fn map_type(&self) -> u32 {
        self.map_type
    }
    fn key_size(&self) -> u32 {
        self.key_size
    }
    fn value_size(&self) -> u32 {
        self.value_size
    }
    fn max_entries(&self) -> u32 {
        self.max_entries
    }
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }
    fn id(&self) -> u32 {
        self.id
    }
    fn map_flags(&self) -> u32 {
        self.map_flags
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }

    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        let mut data = self.data.lock();
        let value = axbpf::Map::lookup(&*data, key).map(ToOwned::to_owned);
        if value.is_some() && self.map_type == BPF_MAP_TYPE_LRU_HASH {
            data.touch(key);
        }
        value
    }

    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        if key.len() != self.key_size as usize || value.len() != self.value_size as usize {
            return Err(AxError::InvalidInput);
        }
        if !matches!(flags, BPF_ANY | BPF_NOEXIST | BPF_EXIST) {
            return Err(AxError::InvalidInput);
        }
        let mut data = self.data.lock();
        let exists = axbpf::Map::lookup(&*data, key).is_some();
        if flags == BPF_NOEXIST && exists {
            return Err(AxError::AlreadyExists);
        }
        if flags == BPF_EXIST && !exists {
            return Err(AxError::NotFound);
        }
        if !exists
            && self.map_type == BPF_MAP_TYPE_LRU_HASH
            && data.entries().count() == self.max_entries as usize
        {
            data.replace_lru_full(key, value)
                .map_err(|error| match error {
                    axbpf::MapError::NoMemory => AxError::NoMemory,
                    axbpf::MapError::Full => AxError::StorageFull,
                    _ => AxError::InvalidInput,
                })?;
            return Ok(());
        }
        axbpf::Map::update(&mut *data, key, value).map_err(|error| match error {
            axbpf::MapError::Full => AxError::StorageFull,
            axbpf::MapError::NoMemory => AxError::NoMemory,
            axbpf::MapError::KeySize | axbpf::MapError::ValueSize => AxError::InvalidInput,
        })?;
        if self.map_type == BPF_MAP_TYPE_LRU_HASH {
            data.touch(key);
        }
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        let mut data = self.data.lock();
        if axbpf::Map::remove(&mut *data, key) {
            Ok(())
        } else {
            Err(AxError::NotFound)
        }
    }

    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>> {
        let data = self.data.lock();
        match key {
            None => data.entries().next().map(|(key, _)| key.clone()),
            Some(k) => {
                let mut iter = data.entries().map(|(key, _)| key);
                // Find the key, then return the next one.
                // Since HashMap iteration order is arbitrary, this just returns
                // "some other key" which is valid for BPF iteration semantics.
                let mut found = false;
                for entry_key in iter.by_ref() {
                    if entry_key.as_slice() == k {
                        found = true;
                        break;
                    }
                }
                if found {
                    iter.next().cloned()
                } else {
                    // Key not found: return the first key (Linux behavior).
                    data.entries().next().map(|(key, _)| key.clone())
                }
            }
        }
    }
    fn hash_batch_page(
        &self,
        bucket: u32,
        capacity: usize,
        delete: bool,
    ) -> AxResult<HashBatchPage> {
        if delete && self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        let mut data = self.data.lock();
        let buckets = data.bucket_count();
        if bucket >= buckets {
            return Err(AxError::NotFound);
        }
        let source_len = data
            .bucket_entries(bucket)
            .ok_or(AxError::InvalidInput)?
            .count();
        if source_len > capacity {
            return Err(axerrno::LinuxError::ENOSPC.into());
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(source_len)
            .map_err(|_| AxError::NoMemory)?;
        for (key, value) in data.bucket_entries(bucket).ok_or(AxError::InvalidInput)? {
            entries.push((batch_owned(key)?, batch_owned(value)?));
        }
        if delete && !entries.is_empty() {
            let mut keys = Vec::new();
            keys.try_reserve_exact(entries.len())
                .map_err(|_| AxError::NoMemory)?;
            for (key, _) in &entries {
                keys.push(batch_owned(key)?);
            }
            data.remove_bucket_keys(bucket, &keys);
        }
        let next_bucket = bucket + 1;
        Ok(HashBatchPage {
            entries,
            next_bucket,
            exhausted: next_bucket >= buckets,
        })
    }
    fn hash_batch_cursor_valid(&self, bucket: u32) -> bool {
        bucket < self.data.lock().bucket_count()
    }
}

// ---------------------------------------------------------------------------
// RingBufMap (minimal create-only support for verifier-focused tests)
// ---------------------------------------------------------------------------

pub struct RingBufMap {
    max_entries: u32,
    map_flags: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    state: spin::Mutex<RingBufState>,
}

#[derive(Default)]
struct RingBufState {
    reserved_bytes: usize,
    records: VecDeque<Vec<u8>>,
    committed_bytes: usize,
}

impl RingBufMap {
    fn new(
        key_size: u32,
        value_size: u32,
        max_entries: u32,
        map_flags: u32,
        name: [u8; BPF_OBJ_NAME_LEN],
        id: u32,
    ) -> AxResult<Self> {
        if key_size != 0 || value_size != 0 || max_entries == 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            max_entries,
            map_flags,
            name,
            id,
            frozen: AtomicBool::new(false),
            state: spin::Mutex::new(RingBufState::default()),
        })
    }

    fn validate_ringbuf_flags(flags: u64) -> AxResult<()> {
        if flags & !(BPF_RB_NO_WAKEUP | BPF_RB_FORCE_WAKEUP) != 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(())
    }
}

impl BpfMap for RingBufMap {
    fn map_type(&self) -> u32 {
        BPF_MAP_TYPE_RINGBUF
    }

    fn key_size(&self) -> u32 {
        0
    }

    fn value_size(&self) -> u32 {
        0
    }

    fn max_entries(&self) -> u32 {
        self.max_entries
    }

    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN] {
        self.name
    }

    fn id(&self) -> u32 {
        self.id
    }

    fn map_flags(&self) -> u32 {
        self.map_flags
    }
    fn freeze_state(&self) -> &AtomicBool {
        &self.frozen
    }

    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }

    fn lookup(&self, _key: &[u8]) -> Option<Vec<u8>> {
        None
    }

    fn update(&self, _key: &[u8], _value: &[u8], _flags: u64) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    fn delete(&self, _key: &[u8]) -> AxResult<()> {
        Err(AxError::InvalidInput)
    }

    fn get_next_key(&self, _key: Option<&[u8]>) -> Option<Vec<u8>> {
        None
    }

    fn ringbuf_reserve(&self, size: usize, flags: u64) -> AxResult<()> {
        Self::validate_ringbuf_flags(flags)?;
        if size == 0 || size > self.max_entries as usize {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        if state
            .reserved_bytes
            .saturating_add(state.committed_bytes)
            .saturating_add(size)
            > self.max_entries as usize
        {
            return Err(AxError::NoMemory);
        }
        state.reserved_bytes += size;
        Ok(())
    }

    fn ringbuf_submit(&self, data: Vec<u8>, flags: u64) -> AxResult<()> {
        Self::validate_ringbuf_flags(flags)?;
        let size = data.len();
        let mut state = self.state.lock();
        if state.reserved_bytes < size {
            return Err(AxError::InvalidInput);
        }
        state.reserved_bytes -= size;
        state.committed_bytes = state.committed_bytes.saturating_add(size);
        state.records.push_back(data);
        while state.committed_bytes > self.max_entries as usize {
            if let Some(old) = state.records.pop_front() {
                state.committed_bytes = state.committed_bytes.saturating_sub(old.len());
            } else {
                break;
            }
        }
        Ok(())
    }

    fn ringbuf_discard(&self, size: usize, flags: u64) -> AxResult<()> {
        Self::validate_ringbuf_flags(flags)?;
        let mut state = self.state.lock();
        if state.reserved_bytes < size {
            return Err(AxError::InvalidInput);
        }
        state.reserved_bytes -= size;
        Ok(())
    }

    fn ringbuf_output(&self, data: &[u8], flags: u64) -> AxResult<()> {
        Self::validate_ringbuf_flags(flags)?;
        if data.len() > self.max_entries as usize {
            return Err(AxError::NoMemory);
        }
        self.ringbuf_submit(data.to_vec(), flags)
    }
}
