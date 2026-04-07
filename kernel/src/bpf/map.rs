//! BPF map implementations (ArrayMap, HashMap).

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use axerrno::{AxError, AxResult};

use super::defs::*;

/// Trait for all BPF map types.
pub trait BpfMap: Send + Sync {
    fn map_type(&self) -> u32;
    fn key_size(&self) -> u32;
    fn value_size(&self) -> u32;
    fn max_entries(&self) -> u32;
    fn name(&self) -> [u8; BPF_OBJ_NAME_LEN];
    fn id(&self) -> u32;
    fn map_flags(&self) -> u32;
    fn freeze(&self) -> AxResult<()>;

    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn update(&self, key: &[u8], value: &[u8], flags: u64) -> AxResult<()>;
    fn delete(&self, key: &[u8]) -> AxResult<()>;
    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>>;

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
    if flags != 0 {
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
        BPF_MAP_TYPE_HASH => Ok(Arc::new(BpfHashMap::new(
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
        _ => Err(AxError::InvalidInput),
    }
}

// ---------------------------------------------------------------------------
// ArrayMap
// ---------------------------------------------------------------------------

pub struct ArrayMap {
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    data: spin::Mutex<Vec<u8>>,
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
        if key_size != 4 || value_size == 0 || max_entries == 0 {
            return Err(AxError::InvalidInput);
        }
        let total = (max_entries as usize)
            .checked_mul(value_size as usize)
            .ok_or(AxError::NoMemory)?;
        Ok(Self {
            key_size,
            value_size,
            max_entries,
            map_flags,
            name,
            id,
            frozen: AtomicBool::new(false),
            data: spin::Mutex::new(alloc::vec![0u8; total]),
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
}

impl BpfMap for ArrayMap {
    fn map_type(&self) -> u32 {
        BPF_MAP_TYPE_ARRAY
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
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }

    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        let index = Self::key_to_index(key)?;
        let range = self.index_range(index)?;
        let data = self.data.lock();
        Some(data[range].to_vec())
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
        let range = self.index_range(index).ok_or(AxError::InvalidInput)?;
        if value.len() != self.value_size as usize {
            return Err(AxError::InvalidInput);
        }
        let mut data = self.data.lock();
        data[range].copy_from_slice(value);
        Ok(())
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
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
    name: [u8; BPF_OBJ_NAME_LEN],
    id: u32,
    frozen: AtomicBool,
    data: spin::Mutex<hashbrown::HashMap<Vec<u8>, Vec<u8>>>,
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
        if key_size == 0 || value_size == 0 || max_entries == 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(Self {
            key_size,
            value_size,
            max_entries,
            map_flags,
            name,
            id,
            frozen: AtomicBool::new(false),
            data: spin::Mutex::new(hashbrown::HashMap::new()),
        })
    }
}

impl BpfMap for BpfHashMap {
    fn map_type(&self) -> u32 {
        BPF_MAP_TYPE_HASH
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
    fn freeze(&self) -> AxResult<()> {
        self.frozen.store(true, Ordering::Release);
        Ok(())
    }

    fn lookup(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.data.lock().get(key).cloned()
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
        let exists = data.contains_key(key);
        if flags == BPF_NOEXIST && exists {
            return Err(AxError::AlreadyExists);
        }
        if flags == BPF_EXIST && !exists {
            return Err(AxError::NotFound);
        }
        if !exists && data.len() >= self.max_entries as usize {
            return Err(AxError::StorageFull);
        }
        data.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &[u8]) -> AxResult<()> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(AxError::OperationNotPermitted);
        }
        let mut data = self.data.lock();
        if data.remove(key).is_some() {
            Ok(())
        } else {
            Err(AxError::NotFound)
        }
    }

    fn get_next_key(&self, key: Option<&[u8]>) -> Option<Vec<u8>> {
        let data = self.data.lock();
        match key {
            None => data.keys().next().cloned(),
            Some(k) => {
                let mut iter = data.keys();
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
                    data.keys().next().cloned()
                }
            }
        }
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
