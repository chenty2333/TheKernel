//! BPF helper function dispatch table.
//!
//! Provides the runtime helper functions that BPF programs invoke via the
//! `CALL` instruction. Each helper has a numeric ID matching the Linux kernel's
//! `bpf_func_id` enum.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::ops::Range;

use axerrno::{AxError, AxResult};
use axhal::time::monotonic_time_nanos;
use axtask::current;

use super::{defs::*, map::BpfMap};
use crate::task::AsThread;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelperMemMask(u8);

impl HelperMemMask {
    pub const STACK: Self = Self(1 << 0);
    pub const CTX: Self = Self(1 << 1);
    pub const MAP_VALUE: Self = Self(1 << 2);

    pub const READABLE: Self = Self(Self::STACK.0 | Self::CTX.0 | Self::MAP_VALUE.0);
    pub const WRITABLE: Self = Self(Self::STACK.0 | Self::MAP_VALUE.0);

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

pub fn charge_aux_budget(remaining: &mut u64, bytes: usize) -> AxResult<()> {
    let bytes = bytes as u64;
    if *remaining < bytes {
        return Err(AxError::ResourceBusy);
    }
    *remaining -= bytes;
    Ok(())
}

/// Stable VM region backing a pointer returned by `bpf_map_lookup_elem`.
pub struct MapValueRegion {
    map: Arc<dyn BpfMap>,
    key: Vec<u8>,
    data: Box<[u8]>,
}

impl MapValueRegion {
    pub fn new(map: Arc<dyn BpfMap>, key: Vec<u8>, value: Vec<u8>) -> Self {
        Self {
            map,
            key,
            data: value.into_boxed_slice(),
        }
    }

    fn base(&self) -> usize {
        self.data.as_ptr() as usize
    }

    fn range_for(&self, ptr: usize, len: usize) -> Option<Range<usize>> {
        checked_region(ptr, len, self.base(), self.data.len())
    }

    pub fn contains_range(&self, ptr: usize, len: usize) -> bool {
        self.range_for(ptr, len).is_some()
    }

    fn matches(&self, map_id: u32, key: &[u8]) -> bool {
        self.map.id() == map_id && self.key.as_slice() == key
    }

    pub fn refresh(&mut self, value: &[u8]) -> AxResult<()> {
        if value.len() != self.data.len() {
            return Err(AxError::InvalidInput);
        }
        self.data.copy_from_slice(value);
        Ok(())
    }

    pub fn read_bytes(&self, ptr: usize, len: usize) -> Option<Vec<u8>> {
        let range = self.range_for(ptr, len)?;
        Some(self.data[range].to_vec())
    }

    pub fn write_bytes(
        &mut self,
        ptr: usize,
        bytes: &[u8],
        aux_budget_remaining: &mut u64,
    ) -> AxResult<()> {
        let range = self
            .range_for(ptr, bytes.len())
            .ok_or(AxError::BadAddress)?;
        self.data[range].copy_from_slice(bytes);
        charge_aux_budget(aux_budget_remaining, self.key.len() + self.data.len())?;
        self.map.update(&self.key, &self.data, BPF_ANY)
    }
}

/// Runtime context passed to helper functions, providing access to maps and
/// BPF-visible memory regions.
pub struct HelperContext<'a> {
    /// Maps referenced by the running program.
    pub maps: &'a [Arc<dyn BpfMap>],
    /// Stable regions for map values returned by lookup helpers.
    pub map_value_regions: &'a mut Vec<MapValueRegion>,
    /// VM stack backing storage.
    pub stack: &'a mut [u8; BPF_STACK_SIZE],
    /// Base address of the context buffer.
    pub ctx_base: u64,
    /// Size of the context buffer.
    pub ctx_size: usize,
    /// Remaining auxiliary copy/allocation budget for this execution.
    pub aux_budget_remaining: &'a mut u64,
}

impl<'a> HelperContext<'a> {
    pub fn map(&self, map_idx: u64) -> Option<&Arc<dyn BpfMap>> {
        self.maps.get(map_idx as usize)
    }

    pub fn read_bytes(&mut self, ptr: u64, len: usize, allowed: HelperMemMask) -> AxResult<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }

        let ptr = ptr as usize;
        if allowed.contains(HelperMemMask::STACK) {
            if let Some(bytes) = self.read_stack_bytes(ptr, len) {
                charge_aux_budget(self.aux_budget_remaining, bytes.len())?;
                return Ok(bytes);
            }
        }
        if allowed.contains(HelperMemMask::CTX) {
            if let Some(bytes) = self.read_ctx_bytes(ptr, len) {
                charge_aux_budget(self.aux_budget_remaining, bytes.len())?;
                return Ok(bytes);
            }
        }
        if allowed.contains(HelperMemMask::MAP_VALUE) {
            if let Some(bytes) = self.read_map_value_bytes(ptr, len) {
                charge_aux_budget(self.aux_budget_remaining, bytes.len())?;
                return Ok(bytes);
            }
        }

        Err(AxError::BadAddress)
    }

    pub fn write_bytes(&mut self, ptr: u64, bytes: &[u8], allowed: HelperMemMask) -> AxResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }

        let ptr = ptr as usize;
        if allowed.contains(HelperMemMask::STACK) {
            if self.write_stack_bytes(ptr, bytes) {
                return Ok(());
            }
        }
        if allowed.contains(HelperMemMask::CTX) && self.write_ctx_bytes(ptr, bytes)? {
            return Ok(());
        }
        if allowed.contains(HelperMemMask::MAP_VALUE) && self.write_map_value_bytes(ptr, bytes)? {
            return Ok(());
        }

        Err(AxError::BadAddress)
    }

    pub fn push_map_value_region(
        &mut self,
        map: Arc<dyn BpfMap>,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> u64 {
        if let Some(region) = self
            .map_value_regions
            .iter_mut()
            .find(|region| region.matches(map.id(), &key))
        {
            if region.refresh(&value).is_err() {
                return 0;
            }
            return region.base() as u64;
        }

        self.map_value_regions
            .push(MapValueRegion::new(map, key, value));
        self.map_value_regions.last().unwrap().base() as u64
    }

    pub fn invalidate_all_map_value_regions(&mut self) {
        self.map_value_regions.clear();
    }

    fn read_stack_bytes(&self, ptr: usize, len: usize) -> Option<Vec<u8>> {
        let base = self.stack.as_ptr() as usize;
        let range = checked_region(ptr, len, base, self.stack.len())?;
        Some(self.stack[range].to_vec())
    }

    fn write_stack_bytes(&mut self, ptr: usize, bytes: &[u8]) -> bool {
        let base = self.stack.as_ptr() as usize;
        let Some(range) = checked_region(ptr, bytes.len(), base, self.stack.len()) else {
            return false;
        };
        self.stack[range].copy_from_slice(bytes);
        true
    }

    fn read_ctx_bytes(&self, ptr: usize, len: usize) -> Option<Vec<u8>> {
        let base = self.ctx_base as usize;
        checked_region(ptr, len, base, self.ctx_size)?;
        Some(unsafe { core::slice::from_raw_parts(ptr as *const u8, len).to_vec() })
    }

    fn write_ctx_bytes(&mut self, ptr: usize, bytes: &[u8]) -> AxResult<bool> {
        let base = self.ctx_base as usize;
        let Some(_range) = checked_region(ptr, bytes.len(), base, self.ctx_size) else {
            return Ok(false);
        };
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        }
        Ok(true)
    }

    fn read_map_value_bytes(&self, ptr: usize, len: usize) -> Option<Vec<u8>> {
        self.map_value_regions
            .iter()
            .find_map(|region| region.read_bytes(ptr, len))
    }

    fn write_map_value_bytes(&mut self, ptr: usize, bytes: &[u8]) -> AxResult<bool> {
        for region in self.map_value_regions.iter_mut() {
            if region.range_for(ptr, bytes.len()).is_some() {
                region.write_bytes(ptr, bytes, self.aux_budget_remaining)?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Dispatch a helper function call.
///
/// `regs` contains R1..R5 as arguments. Returns the value for R0.
pub fn call_helper(
    helper_id: u32,
    r1: u64,
    r2: u64,
    r3: u64,
    r4: u64,
    r5: u64,
    hctx: &mut HelperContext,
) -> AxResult<u64> {
    match helper_id {
        BPF_FUNC_MAP_LOOKUP_ELEM => helper_map_lookup_elem(r1, r2, hctx),
        BPF_FUNC_MAP_UPDATE_ELEM => helper_map_update_elem(r1, r2, r3, r4, hctx),
        BPF_FUNC_MAP_DELETE_ELEM => helper_map_delete_elem(r1, r2, hctx),
        BPF_FUNC_KTIME_GET_NS => Ok(monotonic_time_nanos() as u64),
        BPF_FUNC_GET_CURRENT_PID_TGID => Ok(helper_get_current_pid_tgid()),
        BPF_FUNC_GET_CURRENT_UID_GID => Ok(helper_get_current_uid_gid()),
        BPF_FUNC_GET_CURRENT_COMM => helper_get_current_comm(r1, r2, hctx),
        BPF_FUNC_TRACE_PRINTK => helper_trace_printk(r1, r2, r3, r4, r5, hctx),
        BPF_FUNC_GET_PRANDOM_U32 => Ok(helper_get_prandom_u32()),
        BPF_FUNC_GET_SMP_PROCESSOR_ID => Ok(0), // unikernel: always CPU 0
        _ => Ok(helper_error()),
    }
}

// ---------------------------------------------------------------------------
// Map helpers
// ---------------------------------------------------------------------------

fn helper_map_lookup_elem(map_idx: u64, key_ptr: u64, hctx: &mut HelperContext) -> AxResult<u64> {
    let Some(map) = hctx.map(map_idx).cloned() else {
        return Ok(0);
    };

    let key_size = map.key_size() as usize;
    let Ok(key) = hctx.read_bytes(key_ptr, key_size, HelperMemMask::READABLE) else {
        return Ok(0);
    };

    match map.lookup(&key) {
        Some(value) => {
            charge_aux_budget(hctx.aux_budget_remaining, value.len())?;
            Ok(hctx.push_map_value_region(map, key, value))
        }
        None => Ok(0),
    }
}

fn helper_map_update_elem(
    map_idx: u64,
    key_ptr: u64,
    value_ptr: u64,
    flags: u64,
    hctx: &mut HelperContext,
) -> AxResult<u64> {
    let Some(map) = hctx.map(map_idx) else {
        return Ok(helper_error());
    };

    let key_size = map.key_size() as usize;
    let value_size = map.value_size() as usize;
    let Ok(key) = hctx.read_bytes(key_ptr, key_size, HelperMemMask::READABLE) else {
        return Ok(helper_error());
    };
    let Ok(value) = hctx.read_bytes(value_ptr, value_size, HelperMemMask::READABLE) else {
        return Ok(helper_error());
    };
    charge_aux_budget(hctx.aux_budget_remaining, key.len() + value.len())?;

    match map.update(&key, &value, flags) {
        Ok(()) => {
            hctx.invalidate_all_map_value_regions();
            Ok(0)
        }
        Err(_) => Ok(helper_error()),
    }
}

fn helper_map_delete_elem(map_idx: u64, key_ptr: u64, hctx: &mut HelperContext) -> AxResult<u64> {
    let Some(map) = hctx.map(map_idx) else {
        return Ok(helper_error());
    };

    let key_size = map.key_size() as usize;
    let Ok(key) = hctx.read_bytes(key_ptr, key_size, HelperMemMask::READABLE) else {
        return Ok(helper_error());
    };

    match map.delete(&key) {
        Ok(()) => {
            hctx.invalidate_all_map_value_regions();
            Ok(0)
        }
        Err(_) => Ok(helper_error()),
    }
}

// ---------------------------------------------------------------------------
// Process context helpers
// ---------------------------------------------------------------------------

fn helper_get_current_pid_tgid() -> u64 {
    let curr = current();
    if let Some(thr) = curr.try_as_thread() {
        let pid = thr.proc_data.proc.pid() as u64;
        let tid = thr.tid() as u64;
        (pid << 32) | (tid & 0xFFFF_FFFF)
    } else {
        0
    }
}

fn helper_get_current_uid_gid() -> u64 {
    // StarryOS runs as root (uid=0, gid=0)
    0
}

fn helper_get_current_comm(buf_ptr: u64, buf_size: u64, hctx: &mut HelperContext) -> AxResult<u64> {
    let curr = current();
    let name = curr.name();
    let name_bytes = name.as_bytes();

    let out_size = (buf_size as usize).min(16); // TASK_COMM_LEN = 16
    let copy_len = name_bytes.len().min(out_size.saturating_sub(1));
    charge_aux_budget(hctx.aux_budget_remaining, out_size)?;
    let mut out = vec![0u8; out_size];
    out[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

    if hctx
        .write_bytes(buf_ptr, &out, HelperMemMask::WRITABLE)
        .is_err()
    {
        return Ok(helper_error());
    }

    Ok(0)
}

// ---------------------------------------------------------------------------
// Debug helpers
// ---------------------------------------------------------------------------

fn helper_trace_printk(
    fmt_ptr: u64,
    fmt_size: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    hctx: &mut HelperContext,
) -> AxResult<u64> {
    let fmt_len = (fmt_size as usize).min(256);
    if fmt_len > 0
        && hctx
            .read_bytes(fmt_ptr, fmt_len, HelperMemMask::READABLE)
            .is_err()
    {
        return Ok(helper_error());
    }

    // Simplified: just print the arguments since parsing format strings from
    // BPF memory is complex and error-prone.
    warn!("bpf_trace_printk: arg1={arg1:#x}, arg2={arg2:#x}, arg3={arg3:#x}");
    Ok(0)
}

fn helper_get_prandom_u32() -> u64 {
    // Simple PRNG — use monotonic time as entropy source.
    // Not cryptographically secure, but sufficient for BPF use cases.
    let t = monotonic_time_nanos() as u64;
    // xorshift32-ish
    let mut x = t as u32;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x as u64
}

fn helper_error() -> u64 {
    u64::MAX
}

pub(crate) fn checked_region(
    ptr: usize,
    len: usize,
    base: usize,
    size: usize,
) -> Option<Range<usize>> {
    let end = ptr.checked_add(len)?;
    let region_end = base.checked_add(size)?;
    if ptr < base || end > region_end {
        return None;
    }
    Some((ptr - base)..(end - base))
}
