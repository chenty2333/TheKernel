//! Snapshot slot ownership without VFS operations or per-slot scans on reads.
use super::*;

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct SwapUsage {
    pub(crate) total_bytes: usize,
    pub(crate) free_bytes: usize,
}
pub(crate) struct SwapDeviceInfo {
    id: u16,
    pub(crate) location: Location,
    pub(crate) total_bytes: usize,
    pub(crate) used_bytes: usize,
    pub(crate) priority: i16,
}

pub(crate) fn swap_usage() -> SwapUsage {
    let registry = SWAPS.lock();
    let mut usage = SwapUsage::default();
    for area in registry.areas.values().filter(|area| !area.draining) {
        usage.total_bytes = usage
            .total_bytes
            .saturating_add(area.refs.len().saturating_mul(PAGE));
        usage.free_bytes = usage
            .free_bytes
            .saturating_add((area.refs.len() - area.used_slots).saturating_mul(PAGE));
    }
    usage
}

pub(crate) fn swap_devices() -> AxResult<Vec<SwapDeviceInfo>> {
    let mut entries = {
        let registry = SWAPS.lock();
        let mut entries = Vec::new();
        entries
            .try_reserve(registry.areas.len())
            .map_err(|_| AxError::NoMemory)?;
        for area in registry.areas.values() {
            entries.push(SwapDeviceInfo {
                id: area.id,
                location: area.location.clone(),
                total_bytes: area.refs.len().saturating_mul(PAGE),
                used_bytes: area.used_slots.saturating_mul(PAGE),
                priority: area.priority,
            });
        }
        entries
    };
    entries.sort_unstable_by_key(|entry| entry.id);
    Ok(entries)
}
