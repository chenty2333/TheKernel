//! Cgroup-v2 physical ownership budgets, independent of VMA and PID lifetime.
use alloc::{format, string::String, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use axalloc::{PageAccountingHooks, UsageKind};
use axerrno::{AxError, AxResult};
use hashbrown::HashMap;
use spin::{Lazy, Mutex};
use tk_linux_signal::{SignalInfo, Signo};

use crate::task::{AsThread, send_signal_to_process_data};

// No allocation or process signalling occurs under this hierarchy gate.
static BUDGET_GATE: Mutex<()> = Mutex::new(());
static ALLOCATIONS: Lazy<Mutex<HashMap<usize, MemoryCharge>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static TRACKED: AtomicU64 = AtomicU64::new(0);

pub(super) struct MemoryGroup {
    parent: Option<Arc<MemoryGroup>>,
    current: AtomicU64,
    peak: AtomicU64,
    maximum: AtomicU64,
    max_events: AtomicU64,
    oom: AtomicU64,
    oom_kill: AtomicU64,
    local_max_events: AtomicU64,
    local_oom: AtomicU64,
    local_oom_kill: AtomicU64,
}
impl MemoryGroup {
    pub(super) fn new(parent: Option<Arc<Self>>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            parent,
            current: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            maximum: AtomicU64::new(u64::MAX),
            max_events: AtomicU64::new(0),
            oom: AtomicU64::new(0),
            oom_kill: AtomicU64::new(0),
            local_max_events: AtomicU64::new(0),
            local_oom: AtomicU64::new(0),
            local_oom_kill: AtomicU64::new(0),
        })
        .map_err(|_| AxError::NoMemory)
    }
    fn each(&self, mut f: impl FnMut(&Self)) {
        let mut group = Some(self);
        while let Some(current) = group {
            f(current);
            group = current.parent.as_deref();
        }
    }
    pub(super) fn charge(self: &Arc<Self>, bytes: u64) -> Result<MemoryCharge, Arc<Self>> {
        let _gate = BUDGET_GATE.lock();
        let mut group = Some(self);
        while let Some(current) = group {
            if current
                .current
                .load(Ordering::Relaxed)
                .checked_add(bytes)
                .is_none_or(|next| next > current.maximum.load(Ordering::Relaxed))
            {
                current.local_max_events.fetch_add(1, Ordering::Relaxed);
                current.local_oom.fetch_add(1, Ordering::Relaxed);
                current.each(|ancestor| {
                    ancestor.max_events.fetch_add(1, Ordering::Relaxed);
                    ancestor.oom.fetch_add(1, Ordering::Relaxed);
                });
                return Err(current.clone());
            }
            group = current.parent.as_ref();
        }
        self.each(|current| {
            let next = current.current.fetch_add(bytes, Ordering::Relaxed) + bytes;
            current.peak.fetch_max(next, Ordering::Relaxed);
        });
        Ok(MemoryCharge {
            owner: self.clone(),
            bytes,
        })
    }
    pub(super) fn set_maximum(&self, value: Option<u64>) {
        let _gate = BUDGET_GATE.lock();
        self.maximum
            .store(value.unwrap_or(u64::MAX), Ordering::Relaxed);
    }
    pub(super) fn current(&self) -> u64 {
        let _gate = BUDGET_GATE.lock();
        self.current.load(Ordering::Relaxed)
    }
    pub(super) fn text(&self, name: &str) -> AxResult<String> {
        let _gate = BUDGET_GATE.lock();
        Ok(match name {
            "memory.current" => format!("{}\n", self.current.load(Ordering::Relaxed)),
            "memory.peak" => format!("{}\n", self.peak.load(Ordering::Relaxed)),
            "memory.max" => match self.maximum.load(Ordering::Relaxed) {
                u64::MAX => String::from("max\n"),
                n => format!("{n}\n"),
            },
            "memory.events" | "memory.events.local" => {
                let local = name == "memory.events.local";
                let max = if local {
                    &self.local_max_events
                } else {
                    &self.max_events
                };
                let oom = if local { &self.local_oom } else { &self.oom };
                let kill = if local {
                    &self.local_oom_kill
                } else {
                    &self.oom_kill
                };
                // No low/high policy, group-kill policy or socket throttling is
                // enabled. Those event classes cannot fire in this controller.
                format!(
                    "low 0\nhigh 0\nmax {}\noom {}\noom_kill {}\noom_group_kill 0\nsock_throttled \
                     0\n",
                    max.load(Ordering::Relaxed),
                    oom.load(Ordering::Relaxed),
                    kill.load(Ordering::Relaxed)
                )
            }
            _ => return Err(AxError::NotFound),
        })
    }
    fn killed(&self) {
        let _gate = BUDGET_GATE.lock();
        self.local_oom_kill.fetch_add(1, Ordering::Relaxed);
        self.each(|ancestor| {
            ancestor.oom_kill.fetch_add(1, Ordering::Relaxed);
        });
    }
}
pub(super) struct MemoryCharge {
    owner: Arc<MemoryGroup>,
    bytes: u64,
}
impl Drop for MemoryCharge {
    fn drop(&mut self) {
        let _gate = BUDGET_GATE.lock();
        self.owner.each(|group| {
            let old = group.current.fetch_sub(self.bytes, Ordering::Relaxed);
            assert!(
                old >= self.bytes,
                "cgroup physical ownership refund underflow"
            );
        });
    }
}

pub(super) fn parse_maximum(data: &[u8]) -> AxResult<Option<u64>> {
    let text = core::str::from_utf8(data)
        .map_err(|_| AxError::InvalidInput)?
        .trim();
    if text == "max" {
        return Ok(None);
    }
    let (digits, shift) = match text.as_bytes().last() {
        Some(b'k' | b'K') => (&text[..text.len() - 1], 10),
        Some(b'm' | b'M') => (&text[..text.len() - 1], 20),
        Some(b'g' | b'G') => (&text[..text.len() - 1], 30),
        Some(b't' | b'T') => (&text[..text.len() - 1], 40),
        _ => (text, 0),
    };
    let number = digits.parse::<u64>().map_err(|_| AxError::InvalidInput)?;
    let bytes = number
        .checked_mul(1u64 << shift)
        .ok_or(AxError::InvalidInput)?;
    Ok(Some(bytes & !4095))
}

fn allocated(addr: usize, pages: usize, _kind: UsageKind) -> bool {
    let process = axtask::current_may_uninit()
        .and_then(|task| task.try_as_thread().map(|thread| thread.proc_data.clone()));
    let Some(process) = process else {
        return true;
    };
    // Read-only identity-bound snapshot; do not purge or acquire the migration
    // operation gate from the allocator. A migration may choose old/new owner,
    // but later PID reuse cannot substitute another process's membership.
    let group = {
        let registry = super::PID_CGROUPS.by_pid.lock();
        registry.get(&process.proc.pid()).and_then(|memberships| {
            memberships
                .iter()
                .find(|(key, entry)| {
                    key.version == super::CgroupVersion::V2
                        && entry.is_visible()
                        && super::membership_matches_process(entry, &process.proc)
                })
                .and_then(|(_, entry)| entry.target.upgrade())
                .map(|dir| dir.memory.clone())
        })
    };
    let Some(group) = group else {
        return true;
    };
    if group.parent.is_none() {
        return true;
    } // No root limit/control files on Linux.
    let Some(bytes) = (pages as u64).checked_mul(4096) else {
        return false;
    };
    let mut allocations = ALLOCATIONS.lock();
    if allocations.try_reserve(1).is_err() {
        return false;
    }
    match group.charge(bytes) {
        Ok(charge) => {
            assert!(
                !allocations.contains_key(&addr),
                "physical allocation ownership reused before return"
            );
            allocations.insert(addr, charge);
            TRACKED.fetch_add(1, Ordering::Release);
            true
        }
        Err(_) => {
            drop(allocations);
            // OOM policy selects the allocating member, never an unrelated
            // namespace/PID or host-root victim. SIGKILL publication is outside
            // quota/ownership locks and at most once for this process identity.
            if !process.memory_oom_killed.swap(true, Ordering::AcqRel) {
                if send_signal_to_process_data(
                    &process,
                    Some(SignalInfo::new_kernel(Signo::SIGKILL)),
                )
                .is_ok()
                {
                    group.killed();
                } else {
                    process.memory_oom_killed.store(false, Ordering::Release);
                }
            }
            false
        }
    }
}
fn deallocated(addr: usize, pages: usize, _kind: UsageKind) {
    if TRACKED.load(Ordering::Acquire) == 0 {
        return;
    }
    let charge = ALLOCATIONS.lock().remove(&addr);
    if let Some(charge) = charge {
        assert_eq!(
            charge.bytes,
            pages as u64 * 4096,
            "partial physical return lost cgroup ownership"
        );
        TRACKED.fetch_sub(1, Ordering::Release);
        drop(charge);
    }
}
static HOOKS: PageAccountingHooks = PageAccountingHooks {
    allocated,
    deallocated,
};
pub(crate) fn install() {
    assert!(
        axalloc::global_allocator().install_page_accounting(&HOOKS),
        "cgroup page accounting installed twice"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budget_denial_is_atomic_and_physical_token_lifetime_refunds_ancestors() {
        let root = MemoryGroup::new(None).unwrap();
        let parent = MemoryGroup::new(Some(root.clone())).unwrap();
        let child = MemoryGroup::new(Some(parent.clone())).unwrap();
        parent.set_maximum(Some(8192));
        let charge = child.charge(8192).ok().unwrap();
        assert_eq!(root.current(), 8192);
        assert_eq!(parent.current(), 8192);
        assert!(child.charge(4096).is_err());
        assert_eq!(child.current(), 8192);
        assert!(
            parent
                .text("memory.events.local")
                .unwrap()
                .contains("max 1\noom 1")
        );
        assert!(root.text("memory.events").unwrap().contains("max 1\noom 1"));
        child.killed();
        assert!(child.text("memory.events.local").unwrap().contains("oom_kill 1"));
        assert!(parent.text("memory.events.local").unwrap().contains("oom_kill 0"));
        assert!(parent.text("memory.events").unwrap().contains("oom_kill 1"));
        let retained = charge.owner.clone();
        drop(child);
        drop(charge);
        assert_eq!(retained.current(), 0);
        assert_eq!(root.current(), 0);
        assert_eq!(retained.text("memory.peak").unwrap(), "8192\n");
    }
    #[test]
    fn event_formatter_and_maximum_parser_match_linux_numeric_page_units() {
        assert_eq!(parse_maximum(b"max\n").unwrap(), None);
        assert_eq!(parse_maximum(b"8193\n").unwrap(), Some(8192));
        assert_eq!(parse_maximum(b"32M").unwrap(), Some(32 * 1024 * 1024));
        assert!(parse_maximum(b"garbage").is_err());
        let group = MemoryGroup::new(None).unwrap();
        assert_eq!(
            group.text("memory.events").unwrap(),
            "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\nsock_throttled 0\n"
        );
    }
}
